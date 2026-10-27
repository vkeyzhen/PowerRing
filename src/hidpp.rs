//! Logitech HID++ 2.0 最小实现
//!
//! 协议常量与电压-电量曲线为事实信息，参考 Solaar（GPL-3.0）公开文档整理，
//! 本项目未复制其任何源码。

// M1 起设备由 devices/*.yaml 声明驱动，这里的常量不再被代码直接引用，
// 但保留作为协议速查（PRD §13.1），故允许未被使用。
#![allow(dead_code)]

use hidapi::HidDevice;

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

/// 一次读取的结果
#[derive(Debug, Clone, Copy)]
pub struct Battery {
    /// 电量百分比
    pub percent: u8,
    /// 毫伏（电压式设备才有意义）
    pub mv: u16,
    /// 是否正在充电
    pub charging: bool,
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
        match self.dev.read_timeout(&mut tmp, wait_ms) {
            Ok(k) if k > 0 => Some(tmp[..k].to_vec()),
            _ => None,
        }
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
        Some(Battery {
            percent: mv_to_percent(mv),
            mv,
            charging: r[6] & 0x80 != 0,
        })
    }

    /// 按声明指定的 feature 读统一电量（0x1004 类）：[4] 直接就是百分比
    pub fn battery_percent(&self, dev_idx: u8, feature: u16) -> Option<Battery> {
        let i = self.feature_index(dev_idx, feature)?;
        let r = self.call(dev_idx, i, 0x00, &[0; 16])?;
        if r.len() < 6 {
            return None;
        }
        Some(Battery {
            percent: r[4],
            mv: 0,
            charging: r[5] & 0x80 != 0,
        })
    }
}
