//! 界面配色：浅色 / 深色 / 跟随系统
//!
//! 三档而不是两档，是因为「系统怎么说」和「用户想要什么」经常不是一回事：
//! 实测这台机器 AppsUseLightTheme=1（应用走浅色）而 SystemUsesLightTheme=0（任务栏深色），
//! 托盘在深色的任务栏上，旁边弹出一张白卡片就很刺眼 —— 只给「跟随系统」的话没法解决。
//!
//! 跟不跟系统由 Windows 自己那把钥匙决定：
//! HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize 的 AppsUseLightTheme
//! （0 = 深色应用）。读不到就按浅色处理 —— 那是个只读的偏好，不值得为它报错。

use crate::{info, warn};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use windows::core::PCWSTR;
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, REG_DWORD,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// 跟着 Windows 的设置走
    System,
    Light,
    Dark,
}

impl Mode {
    /// 配置里存的是字符串（用户可能手改 config.json），认不出来就退回跟随系统
    pub fn from_str(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "light" => Mode::Light,
            "dark" => Mode::Dark,
            _ => Mode::System,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::System => "system",
            Mode::Light => "light",
            Mode::Dark => "dark",
        }
    }

    /// 按钮上显示的名字，后面括号里补一句实际效果，免得「跟随系统」像个黑盒
    pub fn label(self) -> String {
        match self {
            Mode::System => format!(
                "界面配色：跟随系统（当前{}）",
                if is_dark() { "深色" } else { "浅色" }
            ),
            Mode::Light => "界面配色：浅色".to_string(),
            Mode::Dark => "界面配色：深色".to_string(),
        }
    }

    pub fn next(self) -> Self {
        match self {
            Mode::System => Mode::Light,
            Mode::Light => Mode::Dark,
            Mode::Dark => Mode::System,
        }
    }
}

/// 一套配色。凡是要上色的地方都从这里取，不要在绘制代码里写死颜色 ——
/// 否则加了深色模式，改一处漏一处，界面会变成一半深一半浅。
pub struct Colors {
    /// 托盘弹窗卡片底色
    pub card_bg: (u8, u8, u8),
    /// 卡片标题（次要文字）
    pub card_title: (u8, u8, u8),
    /// 卡片正文
    pub card_text: (u8, u8, u8),
    /// 设置窗口底色
    pub win_bg: (u8, u8, u8),
    /// 设置窗口文字
    pub win_fg: (u8, u8, u8),
    /// 按钮底色 / 按下时的底色 / 边框
    pub btn_bg: (u8, u8, u8),
    pub btn_pressed: (u8, u8, u8),
    pub btn_border: (u8, u8, u8),
    /// 输入框底色。刻意和窗口底色不同：不然看不出哪里能填
    pub edit_bg: (u8, u8, u8),
    /// 托盘电量环的未填充部分，以及「未知」时的整环
    pub ring_empty: (u8, u8, u8),
    pub ring_unknown: (u8, u8, u8),
    /// 提示里的「有事但不算失败」那一档：同步时部分声明没写进磁盘、
    /// 拉不到更新一类的提示点。与电量环的警告色同源，用户不用重新学一套语义
    pub warn: (u8, u8, u8),
}

const LIGHT: Colors = Colors {
    card_bg: (250, 250, 252),
    card_title: (70, 70, 70),
    card_text: (40, 40, 40),
    win_bg: (255, 255, 255),
    win_fg: (26, 26, 26),
    btn_bg: (240, 240, 244),
    btn_pressed: (214, 214, 222),
    btn_border: (176, 176, 186),
    edit_bg: (255, 255, 255),
    ring_empty: (75, 75, 75),
    ring_unknown: (110, 110, 110),
    warn: (200, 105, 20),
};

// 深色不是把浅色反转：卡片用 #2D2D30（VS / Win10 深色面板的常见底色），
// 纯黑 #000 在 OLED 上会拖影，也压不住白色文字的对比度
const DARK: Colors = Colors {
    card_bg: (45, 45, 48),
    card_title: (158, 158, 165),
    card_text: (232, 232, 238),
    win_bg: (45, 45, 48),
    win_fg: (232, 232, 238),
    btn_bg: (66, 66, 72),
    btn_pressed: (92, 92, 100),
    btn_border: (102, 102, 112),
    edit_bg: (30, 30, 33),
    // 环要在深色任务栏上看得见，未填充部分得比浅色主题时浅一档
    ring_empty: (112, 112, 118),
    ring_unknown: (160, 160, 168),
    // 深底上要亮一档，否则这行橙色提示在卡片里几乎读不出来
    warn: (240, 160, 70),
};

// 存 u8 而不是 Mode：静态变量要在编译期初始化，Mode 是枚举也行，
// 但 AtomicU8 转换起来比 AtomicPtr 直白
static MODE: AtomicU8 = AtomicU8::new(0);
// 不叫 DARK：上面那套深色配色已经叫这个名字了
static IS_DARK: AtomicBool = AtomicBool::new(false);

fn mode() -> Mode {
    match MODE.load(Ordering::SeqCst) {
        1 => Mode::Light,
        2 => Mode::Dark,
        _ => Mode::System,
    }
}

pub fn set_mode(m: Mode) {
    let v = match m {
        Mode::System => 0,
        Mode::Light => 1,
        Mode::Dark => 2,
    };
    MODE.store(v, Ordering::SeqCst);
    reload();
}

pub fn current_mode() -> Mode {
    mode()
}

pub fn is_dark() -> bool {
    IS_DARK.load(Ordering::SeqCst)
}

pub fn colors() -> &'static Colors {
    if is_dark() {
        &DARK
    } else {
        &LIGHT
    }
}

/// 重新算一次「现在该用深色吗」。
/// 两个时机要调它：改了主题设置、以及系统主题被改（WM_SETTINGCHANGE）。
pub fn reload() {
    let dark = match mode() {
        Mode::Light => false,
        Mode::Dark => true,
        Mode::System => apps_use_light_theme().map(|v| v == 0).unwrap_or(false),
    };
    let before = IS_DARK.swap(dark, Ordering::SeqCst);
    if before != dark {
        info!(
            "[theme] 配色切到{}（模式={}）",
            if dark { "深色" } else { "浅色" },
            mode().as_str()
        );
    } else {
        info!(
            "[theme] 配色维持{}（模式={}）",
            if dark { "深色" } else { "浅色" },
            mode().as_str()
        );
    }
}

fn wide_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Windows 的「应用使用浅色主题」开关。None 表示读不到（键不存在 / 被策略挡住）
fn apps_use_light_theme() -> Option<u32> {
    let key = wide_nul("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    let name = wide_nul("AppsUseLightTheme");
    let mut hk = HKEY::default();
    let err = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            Some(0),
            KEY_READ,
            &mut hk,
        )
    };
    if err.0 != 0 {
        warn!("[theme] 读不到系统主题设置（错误 {}），按浅色处理", err.0);
        return None;
    }
    let mut val: u32 = 1;
    let mut size: u32 = 4;
    let mut ty = REG_DWORD;
    let err = unsafe {
        RegQueryValueExW(
            hk,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut ty),
            Some(&mut val as *mut u32 as *mut u8),
            Some(&mut size),
        )
    };
    unsafe {
        let _ = RegCloseKey(hk);
    }
    if err.0 != 0 || size != 4 {
        return None;
    }
    Some(val)
}
