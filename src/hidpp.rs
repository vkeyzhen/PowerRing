//! Logitech HID++ 2.0 最小实现
//!
//! 协议常量与电压-电量曲线为事实信息，参考 Solaar（GPL-3.0）公开文档整理，
//! 本项目未复制其任何源码。

// M1 起设备由 devices/*.yaml 声明驱动，这里的常量不再被代码直接引用，
// 但保留作为协议速查（PRD §13.1），故允许未被使用。
#![allow(dead_code)]

use hidapi::HidDevice;
use std::time::{Duration, Instant};

use crate::debug;

pub const F_BATTERY_VOLTAGE: u16 = 0x1001; // 电压式电量（GPW 走这条）
pub const F_UNIFIED_BATTERY: u16 = 0x1004; // 统一电量（GPW 不支持）
pub const F_DEVICE_NAME: u16 = 0x0005;

/// 电压(mV) → 电量百分比，线性插值。与 Python M0 验证版同一条曲线。
const CURVE: [(u16, u8); 13] = [
    (4186, 100),
    (4067, 90),
    (3989, 80),
    (3922, 70),
    (3859, 60),
    (3811, 50),
    (3778, 40),
    (3751, 30),
    (3717, 20),
    (3671, 10),
    (3646, 5),
    (3579, 2),
    (3500, 0),
];

pub fn mv_to_percent(mv: u16) -> u8 {
    if mv >= CURVE[0].0 {
        return CURVE[0].1;
    }
    if mv <= CURVE[CURVE.len() - 1].0 {
        return CURVE[CURVE.len() - 1].1;
    }
    for w in CURVE.windows(2) {
        let (v_hi, p_hi) = (w[0].0, w[0].1);
        let (v_lo, p_lo) = (w[1].0, w[1].1);
        if v_lo <= mv && mv <= v_hi {
            let t = (mv - v_lo) as f32 / (v_hi - v_lo) as f32;
            return (p_lo as f32 + (p_hi - p_lo) as f32 * t).round() as u8;
        }
    }
    0
}

/// 声明里 `battery.kind` 的取值：读到的数据是什么形式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatteryKind {
    /// 读到的是电压 mV，需查放电曲线换算
    Voltage,
    /// 读到的是直接百分比
    Percent,
}

/// 这个数字**是从哪儿来的**。
///
/// 面板上的百分比有三种来源，用户看不出区别，但可信度差很远：
/// 真的问了设备一次、读的是系统缓存的旧值、还是这一轮没读到所以沿用上次。
/// 曾经只有数字、没有来源，于是「设备明明关机了还显示 86%」这种事
/// 只能靠用户自己猜。带上来源，界面才说得清「这个数是刚才读的」还是「这是旧账」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReadingSource {
    /// 刚向设备要了一次
    #[default]
    Live,
    /// 系统缓存里的值（蓝牙 GATT 缓存，可能与系统蓝牙设置同源）
    SystemCache,
    /// 这一轮没读到，沿用上一次的读数
    Previous,
}

/// 一次读取的结果
#[derive(Debug, Clone, Copy)]
pub struct Battery {
    /// 电量百分比。
    ///
    /// `None` 不是「0%」，而是**读到了设备、但拿不到可信的百分比** ——
    /// 比如 0x1004 那类只报粗档位、不报百分比的设备，或者充电中数值字节
    /// 不代表电量。这种时候该说「不知道」，不能编一个数顶上。
    pub percent: Option<u8>,
    /// 毫伏（电压式设备才有意义）
    pub mv: u16,
    /// 是否正在充电
    pub charging: bool,
    /// 这个数字的来源，用来在界面上标注可信度
    pub source: ReadingSource,
}

/// 一条应答和我们的请求是什么关系。
///
/// HID++ 是一条总线上多台设备共用（接收器下面最多挂 6 个），
/// **别的设备的应答、设备主动上报的通知都会混进同一个读口**。
/// 以前只要读到包就当成自己的应答，于是「上一轮别的 slot 的回复」
/// 会被当成本轮的读数 —— 表现是电量数字偶尔跳一下又跳回来。
#[derive(Debug, PartialEq, Eq)]
enum Reply {
    /// 就是这条请求的应答
    Match,
    /// 是我们这条请求的错误应答（0x8F / 0xFF），不用再等
    Error,
    /// 别的设备、别的功能、或者根本不是应答包
    Unrelated,
}

/// 判断一个读到的包是不是 `req` 的应答。
///
/// 抽成纯函数是为了能测：这类判断在真机上要靠「两台设备同时上报」才复现得出来，
/// 而它本身只是一段字节比较 —— 有合成的报文就够验了。
fn classify_reply(req: &[u8], reply: &[u8]) -> Reply {
    // 报告 ID 与设备号都得对上：[0] 是报文长度类别，[1] 是 device index
    if req.len() < 4 || reply.len() < 4 || reply[1] != req[1] {
        return Reply::Unrelated;
    }
    let size = match reply[0] {
        0x10 => 7,  // 短报文
        0x11 => 20, // 长报文
        _ => return Reply::Unrelated,
    };
    // 长度不够说明这个包本身不完整，不能拿来解析
    if reply.len() < size {
        return Reply::Unrelated;
    }
    // 错误应答的布局不一样：[2] 是 0x8F/0xFF 表示出错，原来的 feature/function
    // 被挪到 [3]/[4]
    if matches!(reply[2], 0x8F | 0xFF) && reply[3..5] == req[2..4] {
        return Reply::Error;
    }
    if reply[2..4] == req[2..4] {
        Reply::Match
    } else {
        Reply::Unrelated
    }
}

pub struct Hidpp {
    dev: HidDevice,
}

impl Hidpp {
    pub fn new(dev: HidDevice) -> Self {
        // 非阻塞读：收不到就超时返回，不卡死主线程
        let _ = dev.set_blocking_mode(false);
        Self { dev }
    }

    /// 发一条 HID++ 报文并等待应答。
    /// 坑（M0 实测）：长报文必须写满 20 字节，短报文固定 7 字节，长度不对设备不回或回 0x8F。
    fn request(&self, req: &[u8], wait_ms: i32) -> Option<Vec<u8>> {
        let size = if req[0] == 0x11 { 20 } else { 7 };
        let mut out = vec![0u8; size];
        let n = req.len().min(size);
        out[..n].copy_from_slice(&req[..n]);

        // 清空遗留应答，避免读到上一轮的回复。
        // **必须有次数上限**：若匹配到的接口会持续上报（键盘、鼠标的输入接口就是这样，
        // 一份写错的声明完全可能指到那种接口），「读到 0 才停」的循环会永远转下去，
        // 轮询线程直接卡死 —— 表现是托盘再也不更新，看着像程序死了。
        // 64 次远超正常残留包的个数，正常情况下一两次就 break
        let mut tmp = [0u8; 64];
        for _ in 0..64 {
            match self.dev.read_timeout(&mut tmp, 5) {
                Ok(k) if k > 0 => continue,
                _ => break,
            }
        }

        if self.dev.write(&out).is_err() {
            return None;
        }

        // 等到「自己那条应答」为止，而不是「随便哪个包」。
        // 同一个读口上可能先来别人的应答、或者设备主动上报的通知（电量变化、
        // 按键事件都会发），那些都不能当成本次请求的结果 —— 以前就是这么错的：
        // 取第一个包，于是偶尔读到上一轮别的 slot 的回复，数字跳一下又跳回来。
        let deadline = Instant::now() + Duration::from_millis(wait_ms.max(1) as u64);
        let mut dropped = 0usize;
        let mut found: Option<Vec<u8>> = None;
        while Instant::now() < deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            // 剩下的时间不足 1ms 时按 1ms 试，别传 0（0 在 hidapi 里是无限等）
            let ms = left.as_millis().max(1) as i32;
            let k = match self.dev.read_timeout(&mut tmp, ms) {
                Ok(k) if k > 0 => k,
                _ => break,
            };
            match classify_reply(req, &tmp[..k]) {
                Reply::Match => {
                    found = Some(tmp[..k].to_vec());
                    break;
                }
                // 设备明确说这条请求不行，再等也不会有别的答复
                Reply::Error => break,
                Reply::Unrelated => dropped += 1,
            }
        }
        if found.is_none() && dropped > 0 {
            // 只在「有流量但没有一条是我们的」时记：这正是声明指错接口、
            // 或者设备回包格式不对的特征。设备睡着时根本没流量，不会刷日志
            debug!(
                "[hidpp] 丢了 {dropped} 个不相干的报文，始终没等到 idx={:#04X} feat={:#04X} 的应答",
                req.get(1).copied().unwrap_or(0),
                req.get(2).copied().unwrap_or(0)
            );
        }
        found
    }

    /// 这个 USB 接口在系统上还在不在。
    ///
    /// 判据是**写不写得进去**，不是「有没有回答」—— 这两件事必须分开看：
    /// 接收器插着、鼠标睡着时，写入照样成功，只是没人应答；
    /// 写不进去只可能是那个接口已经不在了（接收器被拔掉、线被拔掉）。
    /// `request()` 把两者合并成一个 `None`，于是「睡着的鼠标」会被当成
    /// 「设备没了」而从托盘消失 —— 2026-10-25 实测到的就是这条。
    ///
    /// 报文用一条合法的 getFeature（问 0xFF 有没有设备名 feature），没有副作用。
    /// 直连设备也认 0xFF（M2 实测：0x00/0x01/0x02/0x03/0xFF 全响应），所以两种
    /// 连接方式都能用同一条判据。
    pub fn attached(&self) -> bool {
        let req = [0x11u8, 0xFF, 0x00, 0x0D, 0x00, 0x05];
        let mut out = vec![0u8; 20];
        out[..req.len()].copy_from_slice(&req);
        // 不等应答：只关心写入本身成不成。遗留的包会被下一次 request() 排空
        self.dev.write(&out).is_ok()
    }

    /// 查询某个 feature 在设备上的 index；返回 None 表示不支持。
    /// 注意：不能用 ping 判断 slot 是否有设备（M0 实测不可靠），改用 feature 查询。
    pub fn feature_index(&self, dev_idx: u8, feature: u16) -> Option<u8> {
        let r = self.request(
            &[
                0x11,
                dev_idx,
                0x00,
                0x0D,
                (feature >> 8) as u8,
                (feature & 0xFF) as u8,
            ],
            300,
        )?;
        if r.len() > 4 && r[0] == 0x11 && r[3] == 0x0D && r[4] != 0 {
            Some(r[4])
        } else {
            None
        }
    }

    fn call(&self, dev_idx: u8, f_idx: u8, func: u8, params: &[u8]) -> Option<Vec<u8>> {
        let sw = 0x0D;
        let mut req = vec![0x11, dev_idx, f_idx, ((func & 0x0F) << 4) | (sw & 0x0F)];
        req.extend_from_slice(params);
        self.request(&req, 300)
    }

    /// 把 HID 报文里的定长字节串转成文本。
    /// 坑（M0.5 实测）：报文里的字符串是定长并用 0x00 填充的，Rust 的 trim() 只去空白、
    /// **不去空字符**。残留的 NUL 会让 Windows tooltip 把后面的内容全部截掉
    /// （szTip 以 NUL 为结尾），表现为「悬停只看得到设备名、看不到百分比」。
    fn text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes)
            .replace('\0', "")
            .trim()
            .to_string()
    }

    pub fn device_name(&self, dev_idx: u8) -> Option<String> {
        let i = self.feature_index(dev_idx, F_DEVICE_NAME)?;
        self.device_name_at(dev_idx, i)
    }

    /// 已知 name feature 的 index 时直接读，省掉一次查询
    pub fn device_name_at(&self, dev_idx: u8, i: u8) -> Option<String> {
        let head = self.call(dev_idx, i, 0x00, &[0; 16])?;
        let len = *head.get(4)? as usize;
        let mut name = Vec::with_capacity(len);
        let mut off = 0usize;
        while off < len {
            let r = self.call(dev_idx, i, 0x01, &{
                let mut p = [0u8; 16];
                p[0] = off as u8;
                p
            })?;
            let take = (len - off).min(15);
            name.extend_from_slice(r.get(5..5 + take)?);
            off += take;
        }
        Some(Self::text(&name))
    }

    /// 按声明指定的 feature 读电压式电量（0x1001 类）：
    /// 参数区 [4:6] = mV 大端，[6] = flags（bit7 表示充电中）
    pub fn battery_voltage(&self, dev_idx: u8, feature: u16) -> Option<Battery> {
        let i = self.feature_index(dev_idx, feature)?;
        let r = self.call(dev_idx, i, 0x00, &[0; 16])?;
        if r.len() < 7 {
            return None;
        }
        let mv = ((r[4] as u16) << 8) | r[5] as u16;
        // 电量曲线只覆盖 3500..4200 mV。超出这个范围说明读到的不是电压
        // （通常是对着错误的接口问了一句，回来了别的数据），
        // 硬查表会把它算成 0% 或 100% —— 那是在编数字，不如承认读不到
        if !(2500..=5000).contains(&mv) {
            return None;
        }
        Some(Battery {
            percent: Some(mv_to_percent(mv)),
            mv,
            charging: r[6] & 0x80 != 0,
            source: ReadingSource::Live,
        })
    }

    /// 按声明指定的 feature 读统一电量（0x1004 类）。
    ///
    /// 这个 feature 要问两次，以前只问一次就把 [4] 当百分比读 —— 那读到的其实是
    /// **能力位**，于是多数字节被当成电量（常见表现是恒为某个固定值或 0）。
    /// function 0 查能力（[5] 的 bit1 = 支持百分比），function 1 才是状态：
    /// [4] = 百分比，[6] = 充电状态（0 未充电，1..=3 各种充电中）。
    pub fn battery_percent(&self, dev_idx: u8, feature: u16) -> Option<Battery> {
        let i = self.feature_index(dev_idx, feature)?;
        let caps = self.call(dev_idx, i, 0x00, &[0; 16])?;
        let capable = *caps.get(5)? & 0x02 != 0;
        let r = self.call(dev_idx, i, 0x01, &[0; 16])?;
        Self::parse_percent(&r, capable)
    }

    /// 解析统一电量的状态应答。抽出成关联函数，好让合成报文能直接测 ——
    /// 手上没有 0x1004 的设备，只有把解析和取数分开才验得了
    fn parse_percent(r: &[u8], capable: bool) -> Option<Battery> {
        // [4] 是百分比、[6] 是充电状态，至少要 7 个字节才拿得到
        if r.len() < 7 || (capable && r[4] > 100) {
            return None;
        }
        Some(Battery {
            // 设备说自己不支持百分比：那不叫 0%，叫不知道。
            // 以前这里直接返回 0，界面上就成了「这台永远 0%」
            percent: capable.then_some(r[4]),
            mv: 0,
            charging: matches!(r[6], 1..=3),
            source: ReadingSource::Live,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把「有效字节」补成一条完整的包：0x10 短报文 7 字节，0x11 长报文 20 字节
    fn packet(bytes: &[u8]) -> Vec<u8> {
        let mut out = vec![0; if bytes[0] == 0x10 { 7 } else { 20 }];
        out[..bytes.len()].copy_from_slice(bytes);
        out
    }

    /// 守的是「读到的第一个包就当应答」这个老毛病：接收器下挂着多台设备时，
    /// 别人的应答会先到，取第一个包就会读到别人的电量
    #[test]
    fn 只接受当前设备和函数的应答() {
        let req = [0x11, 2, 7, 0x0D];
        assert_eq!(
            classify_reply(&req, &packet(&[0x11, 2, 7, 0x0D, 80])),
            Reply::Match
        );
        for reply in [
            packet(&[0x11, 1, 7, 0x0D, 80]), // 别的 device index（另一个 slot）
            packet(&[0x11, 2, 8, 0x0D, 80]), // 别的 feature
            packet(&[0x11, 2, 7, 0x1D, 80]), // 别的 function
            vec![0x11, 2, 7, 0x0D, 80],      // 长度不足一条长报文
            vec![0x11, 2],                   // 残包
            packet(&[0x12, 2, 7, 0x0D, 80]), // 既不是 0x10 也不是 0x11
        ] {
            assert_eq!(
                classify_reply(&req, &reply),
                Reply::Unrelated,
                "这个包不该被当成应答：{reply:?}"
            );
        }
    }

    /// 错误应答要单独认出来：它意味着「设备明确拒绝了」，再等下去没有意义，
    /// 但也不能当成正常数据去解析
    #[test]
    fn 协议错误不吃成电量() {
        let req = [0x11, 2, 7, 0x0D];
        assert_eq!(
            classify_reply(&req, &packet(&[0x11, 2, 0xFF, 7, 0x0D, 6])),
            Reply::Error
        );
        assert_eq!(
            classify_reply(&req, &packet(&[0x10, 2, 0x8F, 7, 0x0D, 6])),
            Reply::Error
        );
        // 错误应答也对不上我们的 feature，那就只是别人的错误
        assert_eq!(
            classify_reply(&req, &packet(&[0x11, 2, 0xFF, 8, 0x0D, 6])),
            Reply::Unrelated
        );
    }

    /// 0x1004 的状态解析：[4] 百分比、[6] 充电状态
    #[test]
    fn 统一电量按状态报文解析() {
        let mut reply = [0x11, 1, 7, 0x1D, 42, 8, 1, 0];
        let b = Hidpp::parse_percent(&reply, true).expect("应当解析出电量");
        assert_eq!(b.percent, Some(42));
        assert!(b.charging);
        assert_eq!(b.source, ReadingSource::Live);

        reply[6] = 0;
        assert!(!Hidpp::parse_percent(&reply, true).unwrap().charging);

        // 声称支持百分比却给出超过 100 的数：这是坏数据，不是满电
        reply[4] = 255;
        assert!(Hidpp::parse_percent(&reply, true).is_none());

        // 连充电状态字节都没有
        assert!(Hidpp::parse_percent(&reply[..6], true).is_none());
    }

    /// 只报粗档位的设备没有百分比可报。返回 0 会被界面画成「空电」，
    /// 那是把「不知道」说成「没电了」
    #[test]
    fn 不支持百分比的设备如实返回未知() {
        let reply = [0x11, 1, 7, 0x1D, 0, 4, 0, 0];
        let b = Hidpp::parse_percent(&reply, false).expect("仍然是一次有效读取");
        assert_eq!(b.percent, None);
        assert!(!b.charging);
    }

    /// 电压超出电量曲线的覆盖范围时不能硬算
    #[test]
    fn 离谱的电压不算电量() {
        // 直接验曲线边界：0 mV 不该被算成 0%，65535 也不该算成 100%
        // （真实路径上由 battery_voltage 的范围夹取拦住）
        assert!(mv_to_percent(2550) <= 2);
        assert_eq!(mv_to_percent(4186), 100);
    }
}
