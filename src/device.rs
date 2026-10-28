//! 设备扫描与读取：把「声明」变成「活的设备」
//!
//! 一台逻辑设备 = 一份声明 + 一条实际链路（如接收器的某个 slot）。
//! 扫描只依赖声明里的 VID/PID/usage，不硬编码任何厂商常量。

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use hidapi::HidApi;

use crate::debug;
use crate::hidpp::{Battery, BatteryKind, Hidpp};
use crate::profile::{Profile, Protocol, Transport};
use crate::raw::RawDev;
use crate::warn;

// 这里用 Rc 而不是 Arc：hidapi 的句柄不是 Send，这些对象自始至终活在轮询线程里，
// 用 Arc 只是白白付原子操作的代价。
/// 一条实际链路。feature / kind / 报文格式都来自声明 ——
/// 读什么、怎么解读，不再由代码硬编码。
enum Link {
    Hidpp {
        conn: Rc<Hidpp>,
        dev_idx: u8,
        feature: u16,
        kind: BatteryKind,
    },
    Ble {
        dev: Rc<crate::ble::Dev>,
    },
    Raw {
        dev: Rc<RawDev>,
    },
}

/// 一台设备的**物理身份**：不看名字，只看它是从哪个接口、哪个槽位读到的。
///
/// 为什么需要它：两只同型号手柄的显示名一模一样（设备自报的名字、声明名都一样），
/// 按名字认设备会把第二只当成第一只 —— 第二只的读数永远不出现，而第一只被拔掉时
/// 第二只还会接管它的句柄。名字只该用来显示，不该用来认人。
///
/// 代价：接口路径在 USB 重枚举、蓝牙重连之后可能变。那时这台设备会被当成新的一台，
/// 旧那一行靠原有的「读不到就挪走」机制消失，中间会短暂看到两行同名。
/// 这是有意选的：宁可闪一下，也不能把两台真设备当成一台、让其中一台永远读不到。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceId {
    /// HID++：同一个接收器下只认 slot（GPW 的 slot 会漂移，这里记的是实际答话的那个）
    Hid { path: String, slot: u8 },
    /// raw：一份声明对应一种报文格式，一个接口只认一份声明（见 scan 里的说明）
    Raw { path: String },
    /// 蓝牙：系统枚举出来的接口标识
    Ble(String),
}

pub struct Device {
    /// 显示名：优先用设备自报的名字，取不到才用声明名
    pub name: String,
    /// 托盘悬停用的短名。来自声明的 short，没有就用声明名
    pub short: String,
    /// 构造这台设备时用的是哪一份声明。
    ///
    /// 存一份快照是为了在重扫时对比：同步来了新声明之后，设备本身没变、
    /// 句柄也还活着，但「怎么读它」已经不一样了。没有这一份就只能靠重启，
    /// 而界面明明说着「更新了 1 份，现在就能用」。
    pub profile: Profile,
    /// 物理身份，重扫时用来认「是不是同一台」
    pub id: DeviceId,
    link: Link,
}

impl Device {
    pub fn read(&self) -> Option<Battery> {
        match &self.link {
            Link::Hidpp {
                conn,
                dev_idx,
                feature,
                kind,
            } => match kind {
                BatteryKind::Voltage => conn.battery_voltage(*dev_idx, *feature),
                BatteryKind::Percent => conn.battery_percent(*dev_idx, *feature),
            },
            Link::Raw { dev } => dev.battery(),
            Link::Ble { dev } => dev.battery(),
        }
    }

    /// 这台设备此刻**还在不在**（发一条探测报文，问一句就回来）。
    ///
    /// 它和 `read()` 是两件事，别混用：设备休眠时照样答应探测 ——
    /// 接收器认得这台设备、连名字都读得到 —— 但就是不回答电量请求。
    /// 2026-10-25 实测 GPW 静止一段时间后正是这样：同一时刻探测每次都成功，
    /// 而读电量连续 20 次全部无应答。把「读不到电量」当成「设备没了」，
    /// 会让只是睡着的鼠标直接从托盘消失，等用户回来动一下才再出现。
    ///
    /// 蓝牙没有「发条探测报文」这回事，但系统直接告诉我们连没连上，
    /// 用那个当答案 —— 不能一律返回 true，否则关机（仍配对、只是没连）
    /// 的手柄会一直挂着缓存里的旧电量被当成「设备还在」。
    pub fn present(&self) -> bool {
        match &self.link {
            Link::Hidpp {
                conn,
                dev_idx,
                feature,
                ..
            } => conn.feature_index(*dev_idx, *feature).is_some(),
            // raw 协议没有「探测」这条独立报文：能问的只有电量那一条，所以这里会
            // 把同样的报文再发一次（刚读过一次，这一次注定也失败，白等 400 ms）。
            // 明知多余也没有省掉，因为省掉就得改成只问「接口在不在」——
            // 那样关机的耳机将被判成「还在、只是联系不上」，从此一直挂在托盘上。
            // 那是另一种语义（GPW 那种分不清深睡与关机的设备才该那样），
            // 不能顺手把耳机也归进去：400 ms 换「关机就从列表里下去」更划算
            Link::Raw { dev } => dev.probe(),
            Link::Ble { dev } => dev.connected(),
        }
    }

    /// 收不到应答时，是不是**判断不了**它究竟关机了还是睡着了。
    ///
    /// HID 接收器设备：判断不了。接收器一直插在机器上，鼠标休眠和鼠标关机
    /// 在协议层长得一模一样（都是不回答），只能留在列表里标成未连接 ——
    /// 用户 2026-10-25 明确选了这个：睡着的鼠标不该从托盘消失。
    ///
    /// 蓝牙：判断得了。`ConnectionStatus` 直接说有没有连上，没连上就是关了
    /// （或不在范围内），那就照旧走连续失败计数把它请出去。
    pub fn silence_is_ambiguous(&self) -> bool {
        !matches!(self.link, Link::Ble { .. })
    }

    /// 承载这台设备的那个 USB 接口还在不在系统上（判据是写入成不成功）。
    ///
    /// 它比 `present()` 问得更浅一层，但答案更硬：
    /// `present()` 问的是「设备有没有理我」，`attached()` 问的是「接口还在不在」。
    /// 于是三种情况能分开 —— 接口没了 = 拔了（硬证据，可以立刻移出）；
    /// 接口在但不理我 = 关机或深睡（HID 协议层区分不了，只能标未连接）；
    /// 接口在且答应探测但不给电量 = 浅睡（`present()` 的场景）。
    pub fn attached(&self) -> bool {
        match &self.link {
            Link::Hidpp { conn, .. } => conn.attached(),
            Link::Raw { dev } => dev.attached(),
            // 蓝牙没有「接口」这回事，它用自己的在线判定（见 `gone`）
            Link::Ble { .. } => true,
        }
    }

    /// 这台设备是不是已经**从系统里彻底消失了**。
    ///
    /// 蓝牙：连设备对象都构造不出来（Gone）。
    /// HID：接口被拔掉 —— 同样是硬证据，不需要攒失败次数。
    /// 其余情况一律按「还在」处理：接收器插着却读不到，那是关机或休眠，
    /// 上层应当标成未连接留着，而不是把它删掉。
    pub fn gone(&self) -> bool {
        match &self.link {
            Link::Ble { dev } => dev.gone(),
            Link::Hidpp { .. } | Link::Raw { .. } => !self.attached(),
        }
    }
}

/// 按声明扫描本机所有受支持设备
pub fn scan(api: &HidApi, profiles: &[Profile]) -> Vec<Device> {
    // 同一个 HID 接口可能被多份声明引用（一个接收器上挂着多台设备），只打开一次
    let mut opened: HashMap<String, Rc<Hidpp>> = HashMap::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<Device> = Vec::new();

    for prof in profiles {
        // 蓝牙设备压根不走 HID 枚举，单独处理
        if prof.protocol == Protocol::Ble {
            scan_ble(prof, &mut seen, &mut out);
            continue;
        }
        for info in api.device_list() {
            if info.vendor_id() != prof.vid || info.product_id() != prof.pid {
                continue;
            }
            if let Some(up) = prof.usage_page {
                if info.usage_page() != up {
                    continue;
                }
            }
            if let Some(u) = prof.usage {
                if info.usage() != u {
                    continue;
                }
            }

            let key = info.path().to_string_lossy().to_string();

            match prof.protocol {
                Protocol::Ble => unreachable!("蓝牙在上面已单独处理"),
                Protocol::Hidpp20 => {
                    let conn = match opened.get(&key) {
                        Some(c) => c.clone(),
                        None => {
                            let Ok(dev) = info.open_device(api) else {
                                continue;
                            };
                            let conn = Rc::new(Hidpp::new(dev));
                            opened.insert(key.clone(), conn.clone());
                            conn
                        }
                    };
                    scan_hidpp(&conn, prof, &key, &mut seen, &mut out);
                }
                Protocol::Raw => {
                    let Some(spec) = prof.raw.clone() else {
                        continue;
                    };
                    // 同一个接口只认一份声明。raw 是一问一答的固定报文，
                    // 一个接口上的设备就只有一套读法；若用户对同一接口写了两份，
                    // 那两份读的也是同一台设备，第二份进列表只会变成重复行。
                    // 声明已按可信度排过序，先到的那份就是优先级高的
                    if seen.contains(&key) {
                        debug!("[scan] 接口 {} 已有 raw 声明生效，跳过 {}", key, prof.name);
                        continue;
                    }
                    let Ok(h) = info.open_device(api) else {
                        continue;
                    };
                    let dev = Rc::new(RawDev::new(h, spec));
                    scan_raw(&dev, prof, &key, &mut seen, &mut out);
                }
            }
        }
    }
    out
}

/// 蓝牙设备的扫描。**每轮都重新枚举**，不像 HID 那样跨轮缓存句柄 ——
/// 接口路径里带着会话相关的部分，断开重连后会变，攥着旧路径去打开只会失败。
/// 开一次 GATT 服务几百毫秒的量级，一分钟扫一次不值得为此优化。
///
/// 同样遵循「读得到才算它在」：枚举里挂着不代表连着，手柄关机后服务还会留一会儿，
/// 用一次读取来定性，比枚举结果诚实。
fn scan_ble(prof: &Profile, seen: &mut HashSet<String>, out: &mut Vec<Device>) {
    for node in crate::ble::enumerate().unwrap_or_default() {
        if node.vid != prof.vid || node.pid != prof.pid {
            continue;
        }
        let dev = match crate::ble::Dev::open(&node.id) {
            Ok(d) => Rc::new(d),
            Err(e) => {
                warn!("[ble] {} 打开失败：{e}", node.name);
                continue;
            }
        };
        if !dev.probe() {
            continue;
        }
        // 去重键用接口路径而不是「VID-PID」：两只同型号手柄（比如一黑一白）
        // VID/PID 完全一样，按型号去重会让其中一只永远不出现在面板上。
        // 同一物理设备被多份声明匹配时路径相同，照样只留一台。
        // `seen` 是每轮扫描新建的（见 `scan`），所以这里不需要跨轮稳定
        let key = format!("ble#{}", node.id);
        if !seen.insert(key) {
            continue;
        }
        out.push(Device {
            profile: prof.clone(),
            id: DeviceId::Ble(node.id.clone()),
            // 用系统在蓝牙里认到的名字，比声明里的名字更贴用户看到的那台设备
            name: if node.name.is_empty() {
                prof.name.clone()
            } else {
                node.name
            },
            short: prof.short_name.clone().unwrap_or_else(|| prof.name.clone()),
            link: Link::Ble { dev },
        });
    }
}

/// raw 设备的扫描：**能读到一条有效响应才算它在**。
///
/// 只比对 VID/PID 是不够的 —— 无线接收器是插在机器上的，耳机开机与否
/// 枚举结果完全一样。靠一次探测请求区分：耳机没开就没有响应，
/// 于是它自然从列表里消失，而不是显示一个假的 0%。
fn scan_raw(
    dev: &Rc<RawDev>,
    prof: &Profile,
    key: &str,
    seen: &mut HashSet<String>,
    out: &mut Vec<Device>,
) {
    // 探测失败**不**记进 seen：这一轮没开机，下一轮还可能开机，别把它永久划掉
    if !dev.probe() {
        return;
    }
    if !seen.insert(key.to_string()) {
        return;
    }
    out.push(Device {
        profile: prof.clone(),
        id: DeviceId::Raw {
            path: key.to_string(),
        },
        short: prof.short_name.clone().unwrap_or_else(|| prof.name.clone()),
        name: prof.name.clone(),
        link: Link::Raw { dev: dev.clone() },
    });
}

fn scan_hidpp(
    conn: &Rc<Hidpp>,
    prof: &Profile,
    path_key: &str,
    seen: &mut HashSet<String>,
    out: &mut Vec<Device>,
) {
    // 该 slot 上有没有设备，靠「电量 feature 是否存在」判断 —— M0 实测 ping 不可靠
    let Some(feat) = prof.battery_feature else {
        return;
    };
    let idxs: Vec<u8> = match prof.transport {
        Transport::Receiver => prof.slots.clone(),
        Transport::Direct => vec![1],
    };

    for dev_idx in idxs {
        if conn.feature_index(dev_idx, feat).is_none() {
            continue;
        }

        // 识别：读设备名并与声明里的关键字比对，避免同接收器上的其它设备被误认
        let reported = prof
            .name_feature
            .and_then(|f| conn.feature_index(dev_idx, f))
            .and_then(|i| conn.device_name_at(dev_idx, i));
        if let Some(want) = &prof.identify_contains {
            let hit = reported
                .as_deref()
                .map(|n| n.to_ascii_lowercase().contains(&want.to_ascii_lowercase()))
                .unwrap_or(false);
            if !hit {
                continue;
            }
        }

        // 占坑必须放在识别**之后**。以前放在前面，名称对不上的声明也把这个
        // path#slot 占了，后面那份真正匹配的声明就被当成「重复」跳过 ——
        // 表现是「明明有两份声明覆盖它，却一台都认不出」，而且看起来特别像设备的问题
        let uniq = format!("{path_key}#{dev_idx}");
        if !seen.insert(uniq) {
            continue;
        }

        let name = reported
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| prof.name.clone());

        out.push(Device {
            profile: prof.clone(),
            id: DeviceId::Hid {
                path: path_key.to_string(),
                slot: dev_idx,
            },
            short: prof.short_name.clone().unwrap_or_else(|| name.clone()),
            name,
            link: Link::Hidpp {
                conn: conn.clone(),
                dev_idx,
                feature: feat,
                kind: prof.battery_kind,
            },
        });
    }
}
