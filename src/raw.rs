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
        let r = match self.dev.read_timeout(&mut tmp, 400) {
            Ok(k) if k > 0 => &tmp[..k],
            _ => return None,
        };
        if r.len() <= s.value_index {
            return None;
        }

        let value = r[s.value_index];
        let state = r.get(s.state_index).copied().unwrap_or(0);
        let charging = s.charging_states.contains(&state);

        let percent = if charging {
            // 充电中：数值字节在这个状态下不代表电量（它是另一张表的档位），
            // 直接按满格显示。**这是已知问题**，仓库 issue #10 记着：
            // 准确的做法是「不知道」，但它会让充电中的耳机从蓝环变成灰环，
            // 显示语义要先定下来再改
            100
        } else {
            lookup(&s.table, state, value)?
        };
        Some(Battery {
            percent: Some(percent),
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
    use super::lookup;

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
