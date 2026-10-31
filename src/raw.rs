//! 原始报文设备（protocol: raw）的读取
//!
//! 给那些报文格式各家自定义、不属于任何标准协议族的设备准备的。
//! 发什么字节、读哪几个字节、怎么换算成百分比，**全部来自声明**
//! （src/profile.rs 的 RawSpec），这个文件里没有任何厂商常量。
//!
//! 为什么要做成通用能力而不是「HyperX 专用」：
//! PRD 的愿景是「加一台设备 = 加一份 YAML」。每支持一家就写一份 Rust，
//! 那愿景就作废了 —— 而且各家报文的差别其实就那几处（请求字节、字段下标、
//! 换算表），正好是能声明化的东西。

use std::time::{Duration, Instant};

use hidapi::HidDevice;

use crate::hidpp::{Battery, ReadingSource};
use crate::profile::RawSpec;

pub struct RawDev {
    dev: HidDevice,
    spec: RawSpec,
}

impl RawDev {
    pub fn new(dev: HidDevice, spec: RawSpec) -> Self {
        // 非阻塞读，和 HID++ 那边一样：设备不回就超时返回，不拖住轮询线程
        let _ = dev.set_blocking_mode(false);
        Self { dev, spec }
    }

    /// 发一次请求并解析电量。设备没开机/没连上时不会有响应，返回 None ——
    /// 这正是我们希望的行为：关机就该从列表里消失，而不是显示 0%
    pub fn battery(&self) -> Option<Battery> {
        let s = &self.spec;
        let mut out = vec![0u8; s.reply_len];
        let n = s.request.len().min(out.len());
        out[..n].copy_from_slice(&s.request[..n]);

        // 清空遗留应答：上一次读留下的包会被当成这次的响应。
        // 同 hidpp.rs 那边的道理，**必须有次数上限** —— 声明能匹配到会持续上报的
        // 接口，无上限的「读到 0 才停」会让轮询线程永远转在这里
        let mut tmp = vec![0u8; 64];
        for _ in 0..64 {
            match self.dev.read_timeout(&mut tmp, 5) {
                Ok(k) if k > 0 => continue,
                _ => break,
            }
        }

        if self.dev.write(&out).is_err() {
            return None;
        }

        // 读到「长得对的那一条」为止，而不是读一条就当成答案：
        // 有的设备一问会先回一条状态帧、再回真正带数据的帧。只读一条的话，
        // 拿到的是状态帧里那个恒为 0 的字节，界面会一直显示 0%。
        // 总时限仍是 400ms，多读几条不会拖慢轮询
        let deadline = Instant::now() + Duration::from_millis(400);
        let (value, state) = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            let ms = (left.as_millis() as i32).clamp(1, 400);
            let r = match self.dev.read_timeout(&mut tmp, ms) {
                Ok(k) if k > 0 => &tmp[..k],
                _ => return None,
            };
            // 前缀不符就是别人的帧（状态帧、按键上报……），继续等自己那条
            if !s.reply_prefix.is_empty() && !r.starts_with(&s.reply_prefix) {
                continue;
            }
            // 太短：也没法取数，继续等
            if r.len() <= s.value_index {
                continue;
            }
            break (r[s.value_index], r.get(s.state_index).copied().unwrap_or(0));
        };

        let charging = is_charging(s.charging_mask, &s.charging_states, state);
        // 数值不可用时 percent 是 None，但这次读取本身是**成功**的：
        // 它确实答了话、也确实在充电。返回 None 会被上层当成「读不到」，
        // 于是保留上一轮的旧读数 —— 那是个更旧的数字，比「不知道」更糟
        let percent = if value_is_usable(s, state) {
            Some(lookup(&s.table, state, value)?)
        } else {
            None
        };
        Some(Battery {
            percent,
            mv: 0,
            charging,
            source: ReadingSource::Live,
        })
    }

    /// 扫描时用：能不能读到一条能解析的响应，就是「这台设备在不在」的证据。
    /// 比只比对 VID/PID 可靠得多 —— 接收器插着但耳机没开时，这里会返回 None
    pub fn probe(&self) -> bool {
        self.battery().is_some()
    }

    /// 这个 USB 接口在系统上还在不在（判据是写入成不成功，见 `Hidpp::attached`）。
    /// 接收器插着但耳机没开时它仍然是 true —— 那是「设备在但没开机」，
    /// 跟「接收器被拔了」不是一回事。
    pub fn attached(&self) -> bool {
        let mut out = vec![0u8; self.spec.reply_len];
        let n = self.spec.request.len().min(out.len());
        out[..n].copy_from_slice(&self.spec.request[..n]);
        self.dev.write(&out).is_ok()
    }
}

/// 判断当前是否在充电。两种判据方向不同，必须由声明指定用哪一种：
///
/// - `mask`：状态字节按位判（`state & mask != 0`）。充电只是多置一个位，
///   **电量照样有效** —— 凌豹 MK75 就是这样，充电中 89%→93% 一路涨
/// - `states`：整个状态字节落在给定集合里。这类设备充电时数值字节整体
///   换成了另一张表的档位，**电量不可用**（天箭）
fn is_charging(mask: Option<u8>, states: &[u8], state: u8) -> bool {
    match mask {
        Some(m) => state & m != 0,
        None => states.contains(&state),
    }
}

/// 这次响应里的数值字节，还能不能当电调用。
///
/// 按位判充电的设备（`charging_mask`）：充电只是多置一个位，数值照旧有效。
/// 按档位判充电的设备（`charging_states`）：那个档位下数值字节换成了另一张表的
/// 东西，**不代表电量**。
///
/// 这条判据原来是写成「充电就返回 100%」的（issue #10）。那是个纯属编造的读数：
/// 一台充到 40% 的设备会显示满格，用户据此拔线只会发现掉了一大截。
/// 现在如实返回「不知道」，由界面说「充电中（电量未知）」。
fn value_is_usable(s: &RawSpec, state: u8) -> bool {
    s.charging_mask.is_some() || !s.charging_states.contains(&state)
}

/// 在分段表里查百分比：找到第一条「数值 ≤ 上界」就取它对应的百分比。
/// 表为空时把数值当百分比直接返回（有些设备读到就是百分比，不需要换算）
fn lookup(table: &[(u8, Vec<(u8, u8)>)], state: u8, value: u8) -> Option<u8> {
    if table.is_empty() {
        return Some(value.min(100));
    }
    let rows = table.iter().find(|(s, _)| *s == state)?.1.as_slice();
    for (hi, pct) in rows {
        if value <= *hi {
            return Some(*pct);
        }
    }
    // 数值超过了表里最大的上界：取最后一档（表应当覆盖到 255）
    rows.last().map(|(_, pct)| *pct)
}

#[cfg(test)]
mod tests {
    use super::{is_charging, lookup, value_is_usable};
    use crate::profile::RawSpec;

    /// 凌豹 MK75 实测的那份规格：17 字节请求补到 20，电量在第 6 位，
    /// 第 7 位是状态（0x01 电池 / 0x10 充电），数值本身就是百分比
    fn mk75() -> RawSpec {
        RawSpec {
            request: vec![0x13, 0x05],
            reply_len: 20,
            reply_prefix: vec![0x13, 0x0A, 0x01, 0x00, 0x04, 0x05],
            state_index: 7,
            value_index: 6,
            charging_states: Vec::new(),
            charging_mask: Some(0x10),
            table: Vec::new(),
        }
    }

    /// 按位判充电：拔线是 0x01、插线是 0x10，两种都要认对。
    /// 若写成「整字节相等」，充电时若再有别的位被置上（0x11），就会判成没充电
    #[test]
    fn 充电位按位判断() {
        let m = Some(0x10);
        assert!(!is_charging(m, &[], 0x01), "拔线状态不该算充电");
        assert!(is_charging(m, &[], 0x10), "插线状态要算充电");
        assert!(is_charging(m, &[], 0x11), "别的位一起置上也要算充电");
        assert!(!is_charging(m, &[], 0x00));
    }

    /// 用充电位的那类设备，充电中电量照样是真实读数。
    /// 以前一律按满格 100% 处理，会把 89% 显示成 100%
    #[test]
    fn 充电位不覆盖电量读数() {
        let s = mk75();
        assert!(value_is_usable(&s, 0x10), "按位判充电时数值照读");
        assert_eq!(lookup(&s.table, 0x10, 89), Some(89), "充电中照读 89%");
        assert_eq!(lookup(&s.table, 0x01, 87), Some(87));
    }

    /// 天箭那类设备相反：充电档位下数值字节不代表电量，按满格处理
    #[test]
    fn 充电档位下数值作废() {
        let s = RawSpec {
            request: vec![0x21, 0xFF, 0x05],
            reply_len: 20,
            reply_prefix: Vec::new(),
            state_index: 3,
            value_index: 4,
            charging_states: vec![0x10, 0x11],
            charging_mask: None,
            table: vec![(0x0E, vec![(255, 60)])],
        };
        // 充电档位：数值不可用，如实「不知道」而不是编一个 100%
        assert!(!value_is_usable(&s, 0x10));
        assert!(!value_is_usable(&s, 0x11));
        assert!(
            is_charging(None, &s.charging_states, 0x10),
            "充电状态本身是确定的"
        );
        // 非充电档位：照常查表
        assert!(value_is_usable(&s, 0x0E));
        assert_eq!(lookup(&s.table, 0x0E, 231), Some(60));
    }

    #[test]
    fn 查表落在区间内() {
        let t = vec![
            (0x0eu8, vec![(89u8, 10u8), (119, 15), (239, 60), (255, 65)]),
            (
                0x0f,
                vec![
                    (19, 70),
                    (49, 75),
                    (69, 80),
                    (99, 85),
                    (119, 90),
                    (129, 95),
                    (255, 100),
                ],
            ),
        ];
        assert_eq!(lookup(&t, 0x0e, 231), Some(60));
        assert_eq!(lookup(&t, 0x0f, 60), Some(80));
    }

    #[test]
    fn 数值超过最大上界时取最后一档() {
        let t = vec![(0x0eu8, vec![(89u8, 10u8), (119, 15)])];
        assert_eq!(lookup(&t, 0x0e, 250), Some(15));
    }

    #[test]
    fn 表里没有这个档位时返回空() {
        let t = vec![(0x0eu8, vec![(255u8, 50u8)])];
        assert_eq!(lookup(&t, 0x77, 10), None);
    }
}
