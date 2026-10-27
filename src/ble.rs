//! 蓝牙 BLE 电量：**标准** Battery Service
//!
//! 蓝牙 SIG 定了两个公开 UUID：
//!   服务 0x180F（Battery Service）
//!   通道 0x2A19（Battery Level）—— 读出来一个字节，就是 0..100 的百分比
//!
//! 这条路的特殊之处在于**完全不需要逆向**：不像罗技的 HID++、HyperX 的私有报文
//! 得挨家猜，只要是符合标准的蓝牙设备，系统已经把通道开好了。
//! 代价是只能在走 BLE 协议的设备上用 —— 大部分无线键鼠用的是私有 2.4G，不在此列。
//!
//! WinRT 异步一律用 `get()` 同步等结果：轮询线程每几分钟才调一次，
//! 为它引入一套异步运行时不值得，阻塞等待反而让出错路径简单得多。
//!
//! ## 两种读法，先实时后缓存
//!
//! WinRT 给的 `ReadValueWithCacheModeAsync` 有两种模式：
//!   `Uncached` —— 真的往设备发一次 GATT 读请求，拿到当下这个值
//!   `Cached`   —— 取系统缓存里那份，可能是几分钟前同步的
//!
//! 所以取值策略是**先实时、读不到再回落到缓存**。实时读实测十几毫秒（首次
//! 建立连接约 80 ms），一分钟一次完全负担得起；而设备正连着时它总是成功。
//!
//! 曾经在注释里写过「Uncached 不可用」，那是错的，错得很有代表性：当时试的
//! 两台设备**恰好都不在线**（一台正在关机、一台远在公司），全阴性样本使得
//! 「全都返回 Unreachable」看起来像「这条路根本不通」。后来有在线设备可对照，
//! 同一个 API 立刻就读出了 91%。**判 API 是否可行之前，先确认手上有能成功的样本。**
//!
//! ## 在线判定：缓存会留住已经不在的设备
//!
//! 缓存的另一个副作用更要紧：系统会长期留着已配对设备的最后读数，哪怕那台设备
//! 已经关机、甚至早就不在身边（本机有一台放在公司的键盘，长年能枚举到、能读出
//! 100%）。于是「读得到」不再等于「它在」，得额外看一眼蓝牙连接状态。
//! `connected()` 干的就是这件事，上下两半都验证过了：在线的设备是 Connected，
//! 不在的是 Disconnected。

use windows::core::GUID;
use windows::Devices::Bluetooth::{
    BluetoothCacheMode, BluetoothConnectionStatus, BluetoothLEDevice, GenericAttributeProfile as GP,
};
use windows::Devices::Enumeration::DeviceInformation;
use windows::Storage::Streams::DataReader;

use crate::hidpp::{Battery, ReadingSource};
use crate::{info, warn};

/// 0x2A19 Battery Level
const BATTERY_LEVEL: GUID = GUID::from_u128(0x00002A19_0000_1000_8000_00805F9B34FB);

/// 枚举出的一个「带电池服务的蓝牙设备」
pub struct Node {
    /// 设备接口路径，打开服务时要用
    pub id: String,
    /// 系统显示名
    pub name: String,
    pub vid: u16,
    pub pid: u16,
}

/// 列出本机所有暴露 Battery Service 的 BLE 设备。
///
/// 失败返回 None 而不 panic：蓝牙无线电被关掉、系统没有蓝牙时都会失败，
/// 这只是「这类设备暂时不可用」，不该把整个程序拖下水。
pub fn enumerate() -> Option<Vec<Node>> {
    let selector =
        GP::GattDeviceService::GetDeviceSelectorFromUuid(GP::GattServiceUuids::Battery().ok()?)
            .ok()?;
    let devs = DeviceInformation::FindAllAsyncAqsFilter(&selector)
        .ok()?
        .get()
        .ok()?;
    let mut out = Vec::new();
    for d in devs {
        let Ok(id) = d.Id() else { continue };
        let id = id.to_string();
        let name = d.Name().map(|n| n.to_string()).unwrap_or_default();
        let Some((vid, pid)) = ids_from(&id) else {
            // 解析不出厂商号的设备没法跟任何声明对上，只能跳过
            warn!("[ble] 设备 {name} 的路径里没有可用的 VID/PID，跳过");
            continue;
        };
        info!("[ble] 发现 {name} (VID {vid:#06X} PID {pid:#06X})");
        out.push(Node { id, name, vid, pid });
    }
    Some(out)
}

/// 从设备接口路径里抠出厂商号和产品号。
///
/// 路径形如：
///   \\?\BTHLEDevice#{0000180f-...}_Dev_VID&02045e_PID&0b13_REV&0517_3cfa0673b8b8#...
///
/// 注意 VID 字段是 **6 位**十六进制（前 4 位前缀 + 真正的 VID），Xbox 的 0x045E
/// 在这里写成 02045E —— 所以只取**最后 4 位**，照抄整个字段会得到一个不存在的厂商号
fn ids_from(id: &str) -> Option<(u16, u16)> {
    let lower = id.to_ascii_lowercase();
    let vid = field(&lower, "vid&")?;
    let pid = field(&lower, "pid&")?;
    Some((last4_hex(&vid)?, hex16(&pid)?))
}

/// 取出 `key` 后面那段连续字符（到下划线为止），如 "vid&02045e"
fn field(id: &str, key: &str) -> Option<String> {
    let rest = id.split(key).nth(1)?;
    Some(rest.split('_').next()?.to_string())
}

/// "02045e" -> 0x045E（取末尾 4 位）
fn last4_hex(s: &str) -> Option<u16> {
    let tail = s.get(s.len().checked_sub(4)?..)?;
    u16::from_str_radix(tail, 16).ok()
}

fn hex16(s: &str) -> Option<u16> {
    u16::from_str_radix(s, 16).ok()
}

/// 一台蓝牙设备此刻在不在
pub enum Presence {
    /// 连着，可以读
    Connected,
    /// 系统里还在（多半是配对过），但没连上
    Disconnected,
    /// 系统里已经没有这台了 —— 连设备对象都构造不出来
    Gone,
}

/// 一个已经打开的电池通道。持有它就不用每轮重新发现一次 GATT
pub struct Dev {
    /// 设备接口路径。查连接状态时要用，所以留在手上
    id: String,
    ch: GP::GattCharacteristic,
}

impl Dev {
    /// 打开设备的电池服务并找到 Battery Level 通道。
    /// 打开了服务但里面没有那个通道也算失败 —— 有些厂商挂了 0x180F 却不放 0x2A19
    pub fn open(id: &str) -> Result<Dev, String> {
        let svc = GP::GattDeviceService::FromIdAsync(&windows::core::HSTRING::from(id))
            .map_err(|e| format!("打开 GATT 服务失败：{e}"))?
            .get()
            .map_err(|e| format!("等待 GATT 服务超时：{e}"))?;
        let chars = svc
            .GetCharacteristicsAsync()
            .map_err(|e| format!("列通道失败：{e}"))?
            .get()
            .map_err(|e| format!("列通道超时：{e}"))?
            .Characteristics()
            .map_err(|e| format!("取通道列表失败：{e}"))?;
        for ch in chars {
            if let Ok(u) = ch.Uuid() {
                if u == BATTERY_LEVEL {
                    return Ok(Dev {
                        id: id.to_string(),
                        ch,
                    });
                }
            }
        }
        Err("设备挂了 Battery Service 但里面没有 Battery Level 通道".to_string())
    }

    /// 读一次电量。标准只给百分比，**不含是否在充电的信息** —— 所以 charging 恒为 false
    ///
    /// 设备不在就不给值。这不是多此一举：缓存会把关机设备的最后读数一直留着，
    /// 光看「读没读到」根本分不出它在不在（详见模块头注释），所以这里额外看一眼
    /// 连接状态，交给上层用「连续几次读不到」的既有机制把它请出列表。
    pub fn battery(&self) -> Option<Battery> {
        if !matches!(self.presence(), Presence::Connected) {
            return None;
        }
        let (percent, source) = self.raw()?;
        Some(Battery {
            percent: Some(percent),
            mv: 0,
            charging: false,
            source,
        })
    }

    /// 此刻连没连上。和 `gone()` 的分工：这个说「现在没连上」，
    /// 那个说「系统里已经没有这台设备」。前者可能只是 BLE 空闲挂起，
    /// 后者才是硬证据 —— 所以上层对两者的处理不一样。
    pub fn connected(&self) -> bool {
        matches!(self.presence(), Presence::Connected)
    }

    /// 这台设备是不是已经**从系统里彻底消失了**。
    ///
    /// 比「读不到」强得多：GATT 缓存会给早已消失的设备继续报值，
    /// `ConnectionStatus` 也只是说明那一刻没连上（BLE 空闲挂起也会这样），
    /// 唯独这个是硬证据 —— 上层可以据此**立即**把设备请出列表，不必再等容错。
    pub fn gone(&self) -> bool {
        matches!(self.presence(), Presence::Gone)
    }

    /// 扫描时用：既要读得到值，也要设备在线。两条挨在一起讲清楚它们的分工 ——
    /// `raw()` 只能证明「缓存里有个数」，`presence()` 才能证明「这台设备还连着」
    pub fn probe(&self) -> bool {
        matches!(self.presence(), Presence::Connected) && self.raw().is_some()
    }

    /// 设备此刻在不在。做成三态而不是布尔值，是因为「下线」和「彻底没了」
    /// 值得区别对待：前者可能只是 BLE 空闲挂起，还算不上证据；
    /// 后者是「系统里已经没有这台设备」，可以立即下结论。
    fn presence(&self) -> Presence {
        let Ok(op) = BluetoothLEDevice::FromIdAsync(&windows::core::HSTRING::from(&self.id)) else {
            warn!("[ble] 建 BluetoothLEDevice 失败，按在线处理");
            return Presence::Connected;
        };
        match op.get() {
            Ok(dev) => match dev.ConnectionStatus() {
                // 不记 normal 情况：轮询每几秒一次，正常情况的日志会把滚动日志冲干净，
                // 而「设备离线」这件事上层本来就有「读取失败」的日志在讲
                Ok(s) => match s == BluetoothConnectionStatus::Connected {
                    true => Presence::Connected,
                    false => Presence::Disconnected,
                },
                Err(e) => {
                    warn!("[ble] 取连接状态失败 {e}，按在线处理");
                    Presence::Connected
                }
            },
            Err(e) => {
                // ⚠️ WinRT 有个很会骗人的地方：**查无此设备时 `FromIdAsync` 会以
                // S_OK 的姿态交付一个空对象**，绑定层把它翻成 Err，于是看起来像
                // 「调用失败」而 HRESULT 却是 0。事实上这正是最该判离线的情形 ——
                // 设备已经从系统里消失了，连 GATT 服务都不再枚举得到。
                // 2026-10-23 就踩过：手柄手动关机后只剩这一个信号，当时一律按在线
                // 兜底，结果它攥着缓存里的 86% 永远不消失。
                // 用不存在的蓝牙 id 喂同一个 API 验证过：拿回来的正是同一个
                // `HRESULT(0x00000000) / 操作成功完成。`。
                //
                // 所以按 HRESULT 区分：S_OK + Err = 这台设备没了；
                // 真正的调用失败（HRESULT 非 0）才保守按在线，交给上层三次容错兜底。
                match e.code().is_ok() {
                    true => Presence::Gone,
                    false => {
                        warn!("[ble] FromIdAsync 失败 {e}，按在线处理");
                        Presence::Connected
                    }
                }
            }
        }
    }

    fn raw(&self) -> Option<(u8, ReadingSource)> {
        // 先真去设备上读一次；连不上（比如它刚好溜去休眠了）就退回系统缓存里
        // 那个值 —— 陈旧，但比让设备凭空消失好。
        // 两条路都带上来源：实时读和缓存值在界面上长得一样，
        // 而「这个 100% 是刚才读的，还是系统三个月前的旧账」差别很大
        if let Some(v) = self.read(BluetoothCacheMode::Uncached) {
            return Some((v, ReadingSource::Live));
        }
        self.read(BluetoothCacheMode::Cached)
            .map(|v| (v, ReadingSource::SystemCache))
    }

    fn read(&self, mode: BluetoothCacheMode) -> Option<u8> {
        let r = self.ch.ReadValueWithCacheModeAsync(mode).ok()?.get().ok()?;
        if r.Status().ok()? != GP::GattCommunicationStatus::Success {
            return None;
        }
        let buf = r.Value().ok()?;
        let len = buf.Length().ok()? as usize;
        if len == 0 {
            return None;
        }
        let rd = DataReader::FromBuffer(&buf).ok()?;
        // 标准规定至少 1 字节，但也有设备多给几位，这里只读第一个
        let b = rd.ReadByte().ok()?;
        // 0xFF 是蓝牙规范里「电量未知」的取值。不挡的话托盘会显示 255%，
        // 而电量环会被 min(100) 兜住画成满圈 —— 等于把「没有数据」画成「满电」，
        // 正是这个项目最不该犯的错。返回 None 就走既有的「读不到」路径：
        // 灰环、两个短横，绝不用假数字凑
        if b > 100 {
            return None;
        }
        Some(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 真机的设备路径（Xbox 手柄，键名大小写是系统给的）
    const XBOX: &str = r"\\?\BTHLEDevice#{0000180f-0000-1000-8000-00805f9b34fb}_Dev_VID&02045e_PID&0b13_REV&0517_3cfa0673b8b8#8&19a62b31&1&0012#{6e3bb679-4372-40c8-9eaa-4509df260cd8}";

    #[test]
    fn vid字段取末尾四位才是真厂商号() {
        assert_eq!(ids_from(XBOX), Some((0x045E, 0x0B13)));
    }

    #[test]
    fn pid字段直接就是产品号() {
        assert_eq!(ids_from(XBOX).map(|(_, pid)| pid), Some(0x0B13));
    }

    #[test]
    fn 路径里没有厂商号就返回空() {
        assert_eq!(ids_from(r"\\?\BTHLEDevice#{0000180f}_Dev_x"), None);
    }

    #[test]
    fn 另一位设备也能解析() {
        // MK75 键盘：VID 字段写成 023554，真正厂商号是 0x3554
        let id = r"\\?\BTHLEDevice#{0000180f-0000-1000-8000-00805f9b34fb}_Dev_VID&023554_PID&fa07_REV&6701_c99d93fe81ff#8&1f08d058&0&000f#{6e3bb679}";
        assert_eq!(ids_from(id), Some((0x3554, 0xFA07)));
    }
}
