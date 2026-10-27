//! 导出设备信息：给「我的设备没被认出来」这条上报通道用的
//!
//! 为什么需要它：自动发现（discover.rs）能接住走公开标准的设备，
//! 但私有协议那类程序认不出来 —— 它既不知道该发什么报文，也没法猜。
//! 这类设备只能靠用户把「本机有什么」发过来，由维护者写声明。
//!
//! 所以这里做的是**把本机设备情况写成一段纯文本**：用户点一下菜单，
//! 文本进剪贴板，粘贴到 Issue 或者发给维护者就完了。不需要他懂 VID/PID 是什么。
//!
//! 隐私红线（改这里前先看，这三条是对用户的承诺，不是建议）：
//! 1. **不写序列号** —— 它是设备唯一标识，跟人能对应上
//! 2. **不写接口路径** —— 接口路径里带着机器相关的会话 ID
//! 3. **不写蓝牙地址** —— 蓝牙设备的标识就是 MAC，等同于可追踪标识
//!
//! 设备名是要写的（型号靠它认），但开头会提示用户自己检查有没有昵称。
//!
//! 另一条纪律：探测只对罗技发 HID++ 报文（同 discover.rs）。
//! 对着别人的设备乱发私有请求，可能把休眠的设备叫醒，也可能有别的后果 ——
//! 认不出来就认不出来，交给维护者看 VID/PID 判断，不在这里冒险。

use std::fmt::Write as _;

use hidapi::{DeviceInfo, HidApi};

use crate::discover::{best_name, looks_direct, try_direct, try_receiver, HIDPP_VENDORS};
use crate::hidpp::{BatteryKind, Hidpp};
use crate::warn;

/// 厂商自定义 usage page 的起点。HID 规范里 0xFF00 以上是留给厂商的，
/// HID++ 这类私有协议只会挂在那些接口上。
const VENDOR_PAGE: u16 = 0xFF00;

/// HID++ 2.0 长报文接口的 usage。
///
/// 罗技接收器会同时暴露 0xFF00:0x0001（短报文 7 字节）、0xFF00:0x0002（长报文 20 字节）
/// 和 0xFF00:0x0004 三个厂商接口，**只有 0x0002 应答 HID++**。
/// 对这个值做精确匹配，是为了让「导出一份设备信息」不必在每个不答话的接口上
/// 白等一整轮超时
const HIDPP_LONG_REPORT_USAGE: u16 = 0x0002;

/// 程序当前显示着的设备（由 main 传进来，这里不自己去读一遍 ——
/// 用户看到的和他要上报的必须是同一份事实）
pub struct Known {
    pub name: String,
    pub percent: Option<u8>,
    pub charging: bool,
}

/// 生成报表全文
pub fn build(api: &HidApi, known: &[Known]) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "PowerRing 设备信息（用于申请适配新设备）");
    let _ = writeln!(s, "导出时间：{}", crate::log::stamp());
    let _ = writeln!(s, "程序版本：{}", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "不含序列号、接口路径、蓝牙地址。设备名如果带昵称，提交前自己删一下。"
    );
    let _ = writeln!(s);

    // ---- 第一段：程序现在显示了什么 ----
    // 放最前面是因为它最直接回答「为什么我的设备没显示」
    let _ = writeln!(s, "【当前显示的设备】");
    if known.is_empty() {
        let _ = writeln!(s, "  （一台都没有）");
    } else {
        for k in known {
            let pct = match k.percent {
                Some(p) => format!("{p}%"),
                None => "未连接".to_string(),
            };
            let _ = writeln!(
                s,
                "  {}  {}{}",
                k.name,
                pct,
                if k.charging { "  充电中" } else { "" }
            );
        }
    }
    let _ = writeln!(s);

    // ---- 第二段：蓝牙 ----
    let _ = writeln!(s, "【蓝牙设备：系统可见、暴露标准电池服务】");
    let mut ble_count = 0;
    for node in crate::ble::enumerate().unwrap_or_default() {
        ble_count += 1;
        // 注意：只输出名字与 VID/PID。node.id 是设备接口路径，里面就是蓝牙 MAC，不能写
        let name = if node.name.is_empty() {
            "（未命名）".to_string()
        } else {
            node.name.clone()
        };
        let _ = writeln!(s, "  {}  {:04X}:{:04X}", name, node.vid, node.pid);
    }
    if ble_count == 0 {
        let _ = writeln!(s, "  （系统里没有这类设备）");
    }
    let _ = writeln!(s);

    // ---- 第三段：USB HID 接口 ----
    // 全列而不是只列「像外设的」：程序认设备靠的就是 VID/PID，
    // 维护者要判断该加哪一条，就得看到完整画面。键盘鼠标也在里面 ——
    // 它们本来就是可能被支持的对象。
    let mut list: Vec<&DeviceInfo> = api.device_list().collect();
    list.sort_by_key(|i| (i.vendor_id(), i.product_id(), i.interface_number()));
    let _ = writeln!(s, "【USB HID 接口】共 {} 个", list.len());
    for info in list {
        let _ = writeln!(s, "  {}", interface_line(info));
        // 只对罗技问 HID++：这条纪律同 discover.rs，不因为「反正要上报」就放开
        if HIDPP_VENDORS.contains(&info.vendor_id()) {
            // HID++ 走厂商自定义接口（usage_page 从 0xFF00 起）。标准键鼠接口
            // （0x0001 通用桌面、0x000C 消费控制）不可能是它 —— 对着十几个标准接口
            // 逐个发探测，只是让「点一下导出」多等十几秒，信息量却是零
            //
            // 再收紧一层：厂商接口里也只有**长报文**那个（usage 0x0002）会答 HID++。
            // 实测罗技接收器暴露的 0xFF00:0x0001（短报文 7 字节）与 0xFF00:0x0004
            // 一律不回，问一次就是白等 300 ms × 槽位数。报告里如实写「未探测」，
            // 而不是伪装成「没答应探测」—— 后者会让人以为设备坏了
            if info.usage_page() >= VENDOR_PAGE && info.usage() == HIDPP_LONG_REPORT_USAGE {
                let _ = writeln!(s, "      {}", hidpp_line(api, info));
            } else if info.usage_page() >= VENDOR_PAGE {
                let _ = writeln!(
                    s,
                    "      HID++：未探测（不是长报文接口，实测不应答；换用长报文接口那一条才问得出来）"
                );
            } else {
                let _ = writeln!(
                    s,
                    "      HID++：未探测（标准 HID 接口，HID++ 走厂商自定义接口）"
                );
            }
        }
    }

    s
}

/// 一个接口的一行描述。
///
/// 刻意**不写** `path()` 和 `serial_number()`：前者含机器会话 ID，后者是设备唯一标识。
/// 这两样对适配设备没有任何帮助 —— 认设备靠 VID/PID，不是靠这台机器上的第几个接口。
fn interface_line(info: &DeviceInfo) -> String {
    let mut s = format!("{:04X}:{:04X}", info.vendor_id(), info.product_id());
    if let Some(p) = info.product_string() {
        if !p.trim().is_empty() {
            let _ = write!(s, "  “{}”", p.trim());
        }
    }
    if let Some(m) = info.manufacturer_string() {
        if !m.trim().is_empty() {
            let _ = write!(s, "  厂商 “{}”", m.trim());
        }
    }
    let _ = write!(
        s,
        "  usage_page=0x{:04X} usage=0x{:04X} 接口 {}",
        info.usage_page(),
        info.usage(),
        info.interface_number()
    );
    s
}

/// 这个接口答不答 HID++、支持哪个电量 feature。
///
/// 打开失败一律说「打不开」：系统占着接口、权限不足都会失败，
/// 那是本机此刻的状态，值得让维护者看见（比如「设备正被官方软件独占」）。
fn hidpp_line(api: &HidApi, info: &DeviceInfo) -> String {
    let dev = match info.open_device(api) {
        Ok(d) => d,
        Err(_) => return "HID++：接口打不开（可能被别的程序占着）".to_string(),
    };
    let conn = Hidpp::new(dev);
    // 和 discover 用同一套判断：先分清接收器还是直连，再按各自的方式问。
    // 这里问得更粗一点（只看支不支持某个 feature），因为上报不需要精确到 slot
    if looks_direct(&conn) {
        match try_direct(&conn) {
            Some((feat, kind)) => format!(
                "HID++：直连，支持电量 {feat:#06X}（{}），设备自报“{}”",
                kind_name(kind),
                best_name(&conn, 1, "（读不到名字）".into())
            ),
            None => "HID++：答应探测，但读不到电量".to_string(),
        }
    } else {
        // 全 slot 都问，因为一个接收器上可能挂好几台（鼠标 + 键盘），
        // 只报第一个等于替维护者决定「这台不重要」—— 上报要的是完整画面
        let found = try_receiver(&conn);
        if found.is_empty() {
            return "HID++：没答应探测".to_string();
        }
        let mut s = format!("HID++：接收器，{} 个 slot 有回应", found.len());
        for (slot, feat, kind) in found {
            let name = best_name(&conn, slot, "（读不到名字）".into());
            let _ = write!(
                s,
                "；slot {slot} 电量 {feat:#06X}（{}）设备自报“{name}”",
                kind_name(kind)
            );
        }
        s
    }
}

fn kind_name(kind: BatteryKind) -> &'static str {
    match kind {
        BatteryKind::Voltage => "电压式，读出来是毫伏",
        BatteryKind::Percent => "百分比式，设备直接报百分比",
    }
}

/// 把文本放进剪贴板。
///
/// 失败是常有的事（别的程序正占着剪贴板），所以返回 Result 让调用方决定怎么告诉用户，
/// 而不是自己吞掉 —— 用户点了没反应，比看到一句「失败了」更糟。
pub fn copy(text: &str) -> Result<(), String> {
    // GlobalFree 挂在 Foundation 下，不跟 GlobalAlloc 在一处 —— 这是 windows crate 的分法
    use windows::Win32::Foundation::{GlobalFree, HANDLE};
    use windows::Win32::System::DataExchange::{CloseClipboard, OpenClipboard, SetClipboardData};
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    // CF_UNICODETEXT 要求一段以 NUL 结尾的 UTF-16
    let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    // 空文本不能往里写：那会把用户剪贴板里已有的内容清掉，
    // 后果比「复制失败」严重得多
    if text.is_empty() {
        return Err("没有内容可复制".into());
    }
    let bytes = units.len() * std::mem::size_of::<u16>();

    unsafe {
        let mem =
            GlobalAlloc(GMEM_MOVEABLE, bytes).map_err(|e| format!("剪贴板内存分配失败：{e}"))?;
        let ptr = GlobalLock(mem) as *mut u16;
        if ptr.is_null() {
            let _ = GlobalFree(Some(mem));
            return Err("剪贴板内存锁定失败".into());
        }
        std::ptr::copy_nonoverlapping(units.as_ptr(), ptr, units.len());
        let _ = GlobalUnlock(mem);

        // 传 None 表示「不把剪贴板所有权挂到某个窗口上」——
        // 我们的窗口是隐藏的，给了反而可能在退出时出问题
        if let Err(e) = OpenClipboard(None) {
            let _ = GlobalFree(Some(mem));
            return Err(format!("剪贴板打不开（可能被别的程序占着）：{e}"));
        }
        let set = SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(mem.0)));
        let _ = CloseClipboard();
        match set {
            // 成功后内存归系统所有，不能再 GlobalFree
            Ok(_) => Ok(()),
            Err(e) => {
                let _ = GlobalFree(Some(mem));
                Err(format!("写入剪贴板失败：{e}"))
            }
        }
    }
}

/// 剪贴板用不了时的退路：写进文件，用户至少能把文件发出去。
///
/// 路径放在 %APPDATA%\PowerRing 下，跟日志一处 ——
/// 用户知道去哪找，不用猜。
pub fn write_file(text: &str) -> Result<std::path::PathBuf, String> {
    let dir = std::env::var("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("PowerRing");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Err(format!("创建目录失败：{e}"));
    }
    let path = dir.join("device-report.txt");
    match std::fs::write(&path, text) {
        Ok(()) => Ok(path),
        Err(e) => {
            warn!("[report] 写文件失败：{e}");
            Err(format!("写入文件失败：{e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 隐私红线：报表里绝不能出现这三样。
    /// 这条测试守的不是代码正确性，是对用户的承诺 ——
    /// 哪天有人顺手加了 serial_number()，它必须亮红灯。
    #[test]
    fn 报表不含隐私字段() {
        // 查源码比查输出可靠：输出依赖本机插着什么，测不出「要是有人加了个字段怎么办」。
        // 只看 mod tests 之前的那段 —— 下面几条断言自己就写着这些字段名，
        // 不分开会永远命中自己。
        let src = include_str!("report.rs");
        let code = src.split("#[cfg(test)]").next().unwrap_or("");
        let code: String = code
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !code.contains("serial_number"),
            "报表不能写序列号：它是设备唯一标识"
        );
        assert!(
            !code.contains(".path()"),
            "报表不能写接口路径：里面含机器会话 ID"
        );
        // 蓝牙那段只允许用名字与 VID/PID：设备标识里就是 MAC 地址
        assert!(code.contains("node.name"), "蓝牙段应该用到设备名");
        assert!(!code.contains("node.id"), "报表不能写蓝牙设备标识");
    }

    /// 报表要自带说明：用户是把它整段贴出去的，
    /// 那句「不含序列号、如果名字里有昵称自己删」必须跟着一起走
    #[test]
    fn 报表自带隐私提示与分段() {
        let Ok(api) = HidApi::new() else { return };
        let text = build(&api, &[]);
        // 带 --nocapture 跑时能看到真实报表长什么样，排查用户发来的那份时很有用
        eprintln!("{text}");
        assert!(text.contains("不含序列号"), "开头要有隐私说明");
        assert!(text.contains("【当前显示的设备】"));
        assert!(text.contains("（一台都没有）"), "没有设备时要说明白");
        assert!(text.contains("【蓝牙设备"));
        assert!(text.contains("【USB HID 接口】"));
    }

    #[test]
    fn 剪贴板拒绝空内容() {
        // 空文本进剪贴板会把用户已有的内容清掉，后果比报错严重
        assert!(copy("").is_err());
    }

    /// 写进去再读出来：这条链路真正出错的地方在 Win32 那几步
    /// （内存要可移动、要以 NUL 结尾、格式号要对），不端到端跑一遍看不出来
    #[test]
    fn 剪贴板能写进去再读出来() {
        use windows::Win32::Foundation::HGLOBAL;
        use windows::Win32::System::DataExchange::{
            CloseClipboard, GetClipboardData, OpenClipboard,
        };
        use windows::Win32::System::Memory::GlobalLock;
        use windows::Win32::System::Ole::CF_UNICODETEXT;

        let text = "PowerRing 剪贴板自检";
        // 剪贴板被别的程序占着时跳过，不算失败 —— 这是环境状态，不是代码问题
        if copy(text).is_err() {
            return;
        }
        unsafe {
            if OpenClipboard(None).is_err() {
                return;
            }
            let got = match GetClipboardData(CF_UNICODETEXT.0 as u32) {
                Ok(h) => {
                    let p = GlobalLock(HGLOBAL(h.0)) as *const u16;
                    if p.is_null() {
                        None
                    } else {
                        let mut len = 0;
                        while *p.add(len) != 0 {
                            len += 1;
                        }
                        Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, len)))
                    }
                }
                Err(_) => None,
            };
            let _ = CloseClipboard();
            assert_eq!(got.as_deref(), Some(text), "写进去的和读出来的应该是同一段");
        }
    }
}
