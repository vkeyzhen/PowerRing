//! 自动发现：没有声明的设备，用**公开标准**试着认一认
//!
//! 为什么需要它：声明目录里只有几台设备，一个陌生人第一次打开几乎必然是空壳。
//! 但「读电量」这件事在两条路上是公开标准，不需要任何人来逆向：
//!   · 蓝牙 Battery Service（0x180F / 0x2A19）—— 蓝牙 SIG 定的，设备自己报百分比
//!   · HID++ 2.0 的电量 feature —— 罗技自家标准，feature 号可以问出来
//! 所以与其等用户报 VID/PID 过来，不如让程序自己问一遍。
//!
//! 三条纪律（改这里前先看）：
//!   1. **必须真的读到一次合理的电量**才算发现。光答应探测不算 ——
//!      应答了却读出 0 mV 的设备写进声明，就是给用户看一个错数字
//!   2. 只对走这两条公开标准的设备动手，**不猜私有报文**。猜不出来就交给用户上报
//!   3. 探测要发报文，会唤醒睡着的设备，所以调用频率由调用方控制（见 main.rs）：
//!      启动第一轮无条件试一次，之后只在「一台设备都没有」时定期重试

use hidapi::HidApi;

use crate::hidpp::{
    Battery, BatteryKind, Hidpp, F_BATTERY_VOLTAGE, F_DEVICE_NAME, F_UNIFIED_BATTERY,
};
use crate::profile::{Profile, Protocol, Transport};
use crate::{info, warn};

/// 会答 HID++ 的厂商。HID++ 是罗技的协议，只对它家设备发这类报文 ——
/// 对着别人的设备乱发私有请求，轻则没反应，重则把正在休眠的设备叫醒。
/// 这条清单只增不减：加一个厂商意味着确认它真的实现了 HID++ 2.0。
pub(crate) const HIDPP_VENDORS: [u16; 1] = [0x046D];

/// 候选电量 feature，按「老设备多、新设备准」排序：
/// 0x1001 电压式（罗技老设备普遍支持，读出来是毫伏，走共用放电曲线换算）
/// 0x1004 统一电量式（较新的设备，直接给百分比和充电状态）
/// 两者都是罗技公开文档写明编号的 feature，不是猜出来的。
const BATTERY_FEATURES: [(u16, BatteryKind); 2] = [
    (F_BATTERY_VOLTAGE, BatteryKind::Voltage),
    (F_UNIFIED_BATTERY, BatteryKind::Percent),
];

/// 直连设备（插线、或接收器已与设备配对成一台）用的 device index。
/// M2 实测：直连时不挑 index，0x00–0xFF 全都响应，用 1 即可。
pub(crate) const DIRECT_IDX: u8 = 1;
/// 用来分辨接收器与直连设备的两个 index：刻意取不是 1 的值，
/// 只有「不挑 index 的直连设备」才会**两个都**理（见 `looks_direct`）
const PROBE_IDX: u8 = 2;
const PROBE_IDX_2: u8 = 3;
/// 接收器上要逐个问的 slot。slot 会漂移（实测 GPW 从 3 漂到 2），所以全覆盖
const RECEIVER_SLOTS: std::ops::RangeInclusive<u8> = 1..=6;

/// 电压式读数的合理区间（毫伏）。
///
/// 一节锂电是 3.0–4.2 V，两串是 6.0–8.4 V，这里按单节取宽松边界。
/// 作用是把「应答了但读出一堆零」挡在外面 —— 那种设备写进声明就是错的。
/// 区间放得很宽是有意的：窄一点就可能把某个型号的低电量状态判成无效。
const PLAUSIBLE_MV: std::ops::RangeInclusive<u16> = 2500..=5000;

/// 这个读数能不能信。
///
/// 百分比式没有可校验的东西（0..100 本来就全覆盖），只有电压式能看数值像不像电压。
fn plausible(b: &Battery, kind: BatteryKind) -> bool {
    match kind {
        BatteryKind::Voltage => PLAUSIBLE_MV.contains(&b.mv),
        // 百分比式必须**真的给出一个百分比**。应答了却没给数（比如设备只支持
        // 粗档位、能力位说不支持百分比），那不叫「读到了」，认下来只会让用户
        // 看到一台永远「--」的设备
        BatteryKind::Percent => b.percent.is_some_and(|p| p <= 100),
    }
}

/// 按 kind 读一次电量
fn read_battery(conn: &Hidpp, idx: u8, feature: u16, kind: BatteryKind) -> Option<Battery> {
    match kind {
        BatteryKind::Voltage => conn.battery_voltage(idx, feature),
        BatteryKind::Percent => conn.battery_percent(idx, feature),
    }
}

/// 试认这台设备走的是不是「直连的 HID++ 设备」
pub(crate) fn try_direct(conn: &Hidpp) -> Option<(u16, BatteryKind)> {
    for (feat, kind) in BATTERY_FEATURES {
        // feature_index 问的是「你支持这个 feature 吗」，答了才算候选
        if conn.feature_index(DIRECT_IDX, feat).is_none() {
            continue;
        }
        if let Some(b) = read_battery(conn, DIRECT_IDX, feat, kind) {
            if plausible(&b, kind) {
                return Some((feat, kind));
            }
        }
    }
    None
}

/// 试认这个接口是不是「接收器」，上面挂着几台设备。
///
/// 返回**所有**读到合理电量的 slot，而不是第一个就收工 —— 一个接收器可以
/// 同时挂着鼠标和键盘，而这两台未必用同一个电量 feature（实测过电压式与
/// 百分比式混挂的情况）。只认第一台的话，后面那台会因为 feature 对不上号被漏掉。
///
/// 代价是每个 slot 都要问一遍，等于多唤醒几次设备。自动发现只在启动首轮和
/// 「一台设备都没有」的时候跑，认出来就落盘、之后不再探，所以这笔账只付一次。
pub(crate) fn try_receiver(conn: &Hidpp) -> Vec<(u8, u16, BatteryKind)> {
    let mut out = Vec::new();
    for slot in RECEIVER_SLOTS {
        for (feat, kind) in BATTERY_FEATURES {
            if conn.feature_index(slot, feat).is_none() {
                continue;
            }
            if let Some(b) = read_battery(conn, slot, feat, kind) {
                if plausible(&b, kind) {
                    out.push((slot, feat, kind));
                    // 这个 slot 已经认下了，没必要再拿另一个 feature 问它一遍
                    break;
                }
            }
        }
    }
    out
}

/// 把「每个 slot 各自支持哪个 feature」归并成「每种 feature 一份声明」。
///
/// 为什么要归并：主循环会遍历声明里的每个 slot，所以挂在同一个 feature 上的
/// 几台设备共用一份声明就够了，分开写只会让面板里出现重复的行。
/// 而**用另一种 feature 的那台必须单独一份** —— 主循环拿着 0x1001 去问它，
/// 它对不上号，等于漏掉一台。
///
/// 返回 (feature, 取值方式, 命中的 slot 列表)，slot 保持发现时的顺序。
fn merge_by_feature(found: &[(u8, u16, BatteryKind)]) -> Vec<(u16, BatteryKind, Vec<u8>)> {
    let mut out: Vec<(u16, BatteryKind, Vec<u8>)> = Vec::new();
    for (slot, feat, kind) in found {
        match out.iter_mut().find(|(f, k, _)| *f == *feat && *k == *kind) {
            Some((_, _, slots)) => slots.push(*slot),
            None => out.push((*feat, *kind, vec![*slot])),
        }
    }
    out
}

/// 这个接口是**接收器**还是**直连设备**？
///
/// 判据：直连设备对每个 device index 都应答（M2 实测 0x00–0xFF 全响应，
/// 回显请求里的 index），而接收器只对**已配对**的 slot 应答。
///
/// 所以不能只问一个 index 就下结论 —— 接收器的 slot 2 上正好挂着一台设备时，
/// `index 2` 也会答应，于是一个 slot 1 + slot 2 各有一台设备的接收器会被判成直连，
/// 结果只按 index 1 读，slot 2 那台永远发现不了。要**两个不同的非 1 index 都答应**
/// 才算直连：接收器的 slot 2 和 slot 3 同时都有设备才会误判，概率低得多。
///
/// 分错的代价并不对称：接收器被当成直连，就只按 index 1 读 —— 设备漂到别的 slot
/// 就再也读不到（实测 GPW 的 slot 会漂）；直连被当成接收器，只是多问几个空 slot，
/// 慢一点但结果一样。所以拿不准时宁可判成接收器。
///
/// 代价是多一次探测报文，但只在「自动发现」里走（启动首轮或列表为空时），
/// 不是每轮轮询都付。
pub(crate) fn looks_direct(conn: &Hidpp) -> bool {
    BATTERY_FEATURES.iter().any(|(f, _)| {
        conn.feature_index(PROBE_IDX, *f).is_some() && conn.feature_index(PROBE_IDX_2, *f).is_some()
    })
}

/// 显示名：**设备自报的**那个优先。
///
/// 接收器的 USB 产品串一律叫 "USB Receiver"，用户在意的是挂在它上面的那台鼠标叫什麼。
/// 设备名走 HID++ 的 device name feature（0x0005），读不到才退回产品串。
pub(crate) fn best_name(conn: &Hidpp, idx: u8, fallback: String) -> String {
    match conn.device_name(idx) {
        Some(n) if !n.is_empty() => n,
        _ => fallback,
    }
}

/// 一份声明里已经写过的 VID/PID：别去重复发现，用户手动加的声明优先
fn declared_key(prof: &Profile) -> (u16, u16) {
    (prof.vid, prof.pid)
}

/// 扫一遍本机，找出「没写进声明、但能用公开标准读到电量」的设备。
///
/// 返回的是可以直接用的声明（还没落盘），由调用方决定要不要写进用户目录。
/// 打不开的接口一律跳过：系统占着不放、权限不足都会失败，
/// 那是「这台暂时碰不到」，不是错误 —— 与其报错，不如安静地跳过。
pub fn find(api: &HidApi, declared: &[Profile]) -> Vec<Profile> {
    let known: Vec<(u16, u16)> = declared.iter().map(declared_key).collect();
    let mut out = Vec::new();
    let mut seen: Vec<(u16, u16)> = Vec::new();

    // ---- 蓝牙：能枚举到就已经是「暴露了标准电池服务」的设备 ----
    // ble::enumerate() 用的是系统按 Battery Service UUID 过滤的选择器，
    // 所以这里不需要再挑一遍「它有没有电池服务」，只要没被声明覆盖过就值得一试。
    for node in crate::ble::enumerate().unwrap_or_default() {
        if known.contains(&(node.vid, node.pid)) || seen.contains(&(node.vid, node.pid)) {
            continue;
        }
        let dev = match crate::ble::Dev::open(&node.id) {
            Ok(d) => d,
            Err(e) => {
                warn!("[discover] 蓝牙设备 {} 打开失败：{e}", node.name);
                continue;
            }
        };
        // probe() 会真读一次：枚举里挂着不代表此刻连着（系统缓存会留住已关机的设备），
        // 读不到就当它不在，别给一台关机的手柄建声明
        if !dev.probe() {
            continue;
        }
        seen.push((node.vid, node.pid));
        // 名字用系统在蓝牙里认到的那个：用户看到的就是这个名字
        let name = if node.name.is_empty() {
            format!("蓝牙设备 {:04X}:{:04X}", node.vid, node.pid)
        } else {
            node.name.clone()
        };
        info!(
            "[discover] 蓝牙：{}（{:04X}:{:04X}）走标准电池服务，自动加入",
            name, node.vid, node.pid
        );
        out.push(Profile {
            name: name.clone(),
            short_name: None,
            protocol: Protocol::Ble,
            transport: Transport::Direct,
            vid: node.vid,
            pid: node.pid,
            usage_page: None,
            usage: None,
            battery_feature: None,
            battery_kind: BatteryKind::Percent,
            raw: None,
            slots: vec![1],
            name_feature: None,
            identify_contains: None,
            protocol_source: "蓝牙 SIG 公开标准：Battery Service 0x180F / Battery Level 0x2A19"
                .into(),
            verification: "本机自动发现：设备自报百分比，未与官方软件对照".into(),
        });
    }

    // ---- HID++：只对罗技设备发这类报文 ----
    for info in api.device_list() {
        let (vid, pid) = (info.vendor_id(), info.product_id());
        if !HIDPP_VENDORS.contains(&vid) {
            continue;
        }
        if known.contains(&(vid, pid)) || seen.contains(&(vid, pid)) {
            continue;
        }
        // 同一个 USB 设备往往暴露好几个接口，全试一遍没有意义，
        // 认出一个就够了（剩下的接口读出来也是同一台设备的同一份电量）
        let Ok(dev) = info.open_device(api) else {
            continue;
        };
        let conn = Hidpp::new(dev);

        // 先分清它是接收器还是直连设备，再决定怎么问 —— 顺序反了会在接收器上
        // 只认住 index 1，设备漂到别的 slot 就再也读不到
        let fallback = hid_name(info);
        // 实际应答的这个接口；下面两种模式都会把它写进声明
        let at = Found {
            vid,
            pid,
            usage_page: info.usage_page(),
            usage: info.usage(),
        };

        if looks_direct(&conn) {
            if let Some((feat, kind)) = try_direct(&conn) {
                let name = best_name(&conn, DIRECT_IDX, fallback);
                info!(
                    "[discover] HID++：{}（{vid:04X}:{pid:04X}）电量 feature {feat:#06X}，按直连模式自动加入",
                    name
                );
                seen.push((vid, pid));
                out.push(hidpp_profile(
                    name,
                    &at,
                    feat,
                    kind,
                    Transport::Direct,
                    vec![DIRECT_IDX],
                ));
                continue;
            }
        }

        let found = try_receiver(&conn);
        if found.is_empty() {
            continue;
        }
        let slots: Vec<u8> = RECEIVER_SLOTS.collect();
        for (feat, kind, hit) in merge_by_feature(&found) {
            // 名字只是兜底：主循环每轮都会去问设备自报的名字，
            // 面板上显示的是各自那一台，不是「发现时恰好排第一的那台」
            let name = best_name(&conn, hit[0], fallback.clone());
            info!(
                "[discover] HID++：{}（{vid:04X}:{pid:04X}）slot {:?} 支持电量 feature {feat:#06X}，按接收器模式自动加入",
                name, hit
            );
            out.push(hidpp_profile(
                name,
                &at,
                feat,
                kind,
                Transport::Receiver,
                // slot 会漂移（GPW 实测漂过），所以还是把 1..6 全写进声明：
                // 空 slot 不应答，写了不会多出一台设备，反而能接住漂移
                slots.clone(),
            ));
        }
        seen.push((vid, pid));
    }
    out
}

fn hid_name(info: &hidapi::DeviceInfo) -> String {
    let s = info.product_string().unwrap_or("").trim();
    if s.is_empty() {
        format!(
            "USB 设备 {:04X}:{:04X}",
            info.vendor_id(),
            info.product_id()
        )
    } else {
        s.to_string()
    }
}

/// 发现到的一台设备定位：VID/PID 加上**实际应答的那个接口**。
///
/// 接口这一层不能省：写进声明之后，扫描只打开这一个接口。
/// 不写的话声明会匹配该 VID/PID 的所有接口，之后每轮都会对着那些不答话的接口
/// 把每个 slot 问一遍，白等一串超时
struct Found {
    vid: u16,
    pid: u16,
    usage_page: u16,
    usage: u16,
}

fn hidpp_profile(
    name: String,
    at: &Found,
    feature: u16,
    kind: BatteryKind,
    transport: Transport,
    slots: Vec<u8>,
) -> Profile {
    let (vid, pid) = (at.vid, at.pid);
    Profile {
        name,
        short_name: None,
        protocol: Protocol::Hidpp20,
        transport,
        vid,
        pid,
        usage_page: Some(at.usage_page),
        usage: Some(at.usage),
        battery_feature: Some(feature),
        battery_kind: kind,
        raw: None,
        slots,
        // 让它每轮都去问设备自己的名字：面板里显示的名字才是用户认得的那台，
        // 而不是「第一次发现时恰好挂在 slot 1 的那台」的名字
        name_feature: Some(F_DEVICE_NAME),
        identify_contains: None,
        protocol_source: format!(
            "HID++ 2.0 公开 feature {feature:#06X}（本机自动探测，设备自报支持）"
        ),
        verification: match kind {
            BatteryKind::Voltage => {
                "本机自动发现：读到的是电压，按共用放电曲线换算，未与官方软件对照".into()
            }
            BatteryKind::Percent => "本机自动发现：设备自报百分比，未与官方软件对照".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hidpp::{Battery, ReadingSource};

    #[test]
    fn 电压式只认像电压的数() {
        let good = Battery {
            percent: Some(88),
            mv: 4050,
            charging: false,
            source: ReadingSource::Live,
        };
        let zero = Battery {
            percent: Some(0),
            mv: 0,
            charging: false,
            source: ReadingSource::Live,
        };
        assert!(plausible(&good, BatteryKind::Voltage));
        assert!(!plausible(&zero, BatteryKind::Voltage));
        // 边界：锂电放空与满电的两端都要算合理
        let low = Battery {
            percent: Some(1),
            mv: 2500,
            charging: false,
            source: ReadingSource::Live,
        };
        let high = Battery {
            percent: Some(100),
            mv: 5000,
            charging: false,
            source: ReadingSource::Live,
        };
        assert!(plausible(&low, BatteryKind::Voltage));
        assert!(plausible(&high, BatteryKind::Voltage));
    }

    /// 应答了但没有百分比，不能算「认出来了」：
    /// 认下来会写进声明，之后这台设备就一直显示「--」
    #[test]
    fn 没有百分比的百分比式设备不算认出来() {
        let unknown = Battery {
            percent: None,
            mv: 0,
            charging: true,
            source: ReadingSource::Live,
        };
        assert!(!plausible(&unknown, BatteryKind::Percent));
        let real = Battery {
            percent: Some(64),
            ..unknown
        };
        assert!(plausible(&real, BatteryKind::Percent));
    }

    /// 归并规则是这次改动的全部内容，而它没法靠「插两台设备」稳定复现
    /// （要一收多发、两台还得用不同的电量 feature），所以用构造数据守住：
    /// 同 feature 合并成一份、不同 feature 各出一份、顺序不乱
    #[test]
    fn 同一个接收器上混挂两种电量方式时各出一份声明() {
        let found = vec![
            (1, F_BATTERY_VOLTAGE, BatteryKind::Voltage),
            (2, F_UNIFIED_BATTERY, BatteryKind::Percent),
            (4, F_BATTERY_VOLTAGE, BatteryKind::Voltage),
        ];
        let merged = merge_by_feature(&found);
        assert_eq!(merged.len(), 2, "两种电量方式要有两份声明，否则会漏一台");

        let volt = merged
            .iter()
            .find(|(f, _, _)| *f == F_BATTERY_VOLTAGE)
            .expect("电压式那一份不能少");
        assert_eq!(volt.2, vec![1, 4], "同 feature 的 slot 要合到一份里");
        assert_eq!(volt.1, BatteryKind::Voltage);

        let pct = merged
            .iter()
            .find(|(f, _, _)| *f == F_UNIFIED_BATTERY)
            .expect("百分比式那一份不能少");
        assert_eq!(pct.2, vec![2]);
        assert_eq!(pct.1, BatteryKind::Percent);
    }

    #[test]
    fn 归并结果为空时不出声明() {
        // 一个都没读到就不该写出任何东西：声明不是「猜的」，是读到过才算
        assert!(merge_by_feature(&[]).is_empty());
    }

    #[test]
    fn 只有一台设备时归并结果仍是它自己() {
        let found = vec![(3, F_BATTERY_VOLTAGE, BatteryKind::Voltage)];
        let merged = merge_by_feature(&found);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].2, vec![3]);
    }

    #[test]
    fn 对非罗技厂商不动手() {
        // 这条守的是「别对着别人的设备发私有报文」：
        // HID++ 是罗技的协议，厂商清单里没有的 VID 一律跳过
        assert!(HIDPP_VENDORS.contains(&0x046D));
        assert!(!HIDPP_VENDORS.contains(&0x0951));
    }
}
