//! PowerRing M1（第一块）：从声明驱动的多设备轮询
//!
//! 与 M0.5 的区别：设备不再硬编码，全部来自 devices/*.yaml 声明；
//! 支持多设备；读数做滞回平滑；读不到时显示「未知」而不是 0%。
//!
//! 线程模型（2026-10-20 修右键菜单时确立）：
//!   · 主线程  —— 创建托盘并跑 Win32 消息泵。tray-icon 自己不带事件循环，
//!                右键菜单靠 Shell 投递的 WM_RBUTTONUP 触发，没泵就永远不弹。
//!   · 轮询线程 —— 读写 HID、算百分比，结果通过 channel 交给主线程渲染。
//!   · 菜单线程 —— 监听 MenuEvent，只负责把退出标志置起来。
//! 把 HID 读写挪出主线程，是为了让消息泵不被几百毫秒的设备响应卡住。

// 没有控制台窗口：所有输出走 %APPDATA%\PowerRing\log.txt。
// 代价是调试时看不到实时输出，换来的是双击运行不再弹黑框。
#![cfg_attr(windows, windows_subsystem = "windows")]

mod autostart;
mod ble;
mod config;
mod device;
mod discover;
mod hidpp;
mod log;
mod popup;
mod profile;
mod raw;
mod report;
mod settings;
mod sync;
mod theme;
mod ui;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use hidapi::HidApi;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::config::Config;
use crate::hidpp::ReadingSource;
use crate::popup::Line;
use crate::profile::Profile;

/// 轮询线程交给主线程的一次快照
struct Status {
    name: String,
    /// 托盘悬停用的短名。完整名单在左键面板里
    short: String,
    percent: Option<u8>,
    charging: bool,
    /// 这个百分比是从哪儿来的：刚读的 / 系统缓存 / 上一轮的旧账。
    /// 面板与悬停文字都要标出来 —— 三种来源在界面上原本长得一模一样
    source: ReadingSource,
    /// 联系不上，但承载它的接收器还在机器上 —— 多半是休眠或关机。
    /// 显示为「未连接」而不是把设备删掉：删了的话鼠标每次睡深一点都要重新出现一次
    offline: bool,
}

/// 一次低电量提醒。是「事件」不是「状态」—— 一轮轮询最多产生一次
struct Alert {
    name: String,
    percent: u8,
    critical: bool,
}

/// 某档提醒的弹药状态。
///
/// 为什么要区分「已弹过」和「没弹过」：防抖缓冲只能用在**弹过之后**的重新装弹上。
/// 若一律要求「电量高于阈值+3 才装弹」，电量停在阈值上方 3 点以内时那档就永远不装弹，
/// 真正跌破时不响 —— 漏报比多弹一次严重得多。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Arm {
    /// 可以弹
    Armed,
    /// 弹过了（或开局就在阈值之下），等回升到阈值 +3 才重新装弹
    Spent,
}

/// 重扫时碰上一台已经在列表里的设备，该做什么。
///
/// 抽出来是为了能单测：三种情况的区别很细（换不换句柄、清不清旧读数），
/// 而它们出错的表现都是「数字看着不太对」，现场很难判断是哪一种。
#[derive(Debug, PartialEq, Eq)]
enum Refresh {
    /// 同一份声明、也还在线：什么都不用做。
    /// 特别**不要**换句柄 —— 每轮都换等于每轮重建连接
    Keep,
    /// 离线过又回来了：只换句柄，上次的读数还能用
    Reconnect,
    /// 声明变了：换读取器，并且把旧读数清掉 —— 那个数是按旧规则算的
    Redefined,
}

/// 由「是否离线」「声明是否变了」决定动作。声明变了优先：
/// 这时无论设备在不在线，旧读数都不能留
fn refresh_of(offline: bool, redefined: bool) -> Refresh {
    if redefined {
        Refresh::Redefined
    } else if offline {
        Refresh::Reconnect
    } else {
        Refresh::Keep
    }
}

/// 身份对不上时，列表里这一行还算不算「就是它回来了」。
///
/// 只在三种条件同时成立时才认：这条记录标着未连接、名字一样、声明也一样。
///
/// 为什么要有这个兜底：接收器拔了又插回来、蓝牙重连之后，接口路径会变，
/// 物理身份就跟着变了，可它明明还是同一台设备 —— 不认的话，用户每次拔插
/// 都要多等一轮「移出列表、再发现」，中间白闪一次。
///
/// 为什么必须限死在「未连接」：那正是「第二只同名设备」会被吞掉的那个入口。
/// 一只新的同型号手柄插上来时，列表里那台在线设备名字、声明全都一样，
/// 一放宽就会把新手柄的读数记到老手柄头上。
fn is_returning(
    offline: bool,
    name: &str,
    profile: &Profile,
    dev_name: &str,
    dev: &Profile,
) -> bool {
    offline && name == dev_name && profile == dev
}

/// 首次读数最多连着走几轮 5 秒短周期。
///
/// 与「空列表只连续快扫 4 轮」同一个道理：短周期每轮都要刷总线、开 GATT、
/// 发探测报文（还会叫醒睡着的设备），不能无限期地跑下去。4 轮 ≈ 20 秒，
/// 盖得住开机时接收器晚应答那一段。
const FAST_RETRY_LIMIT: u8 = 4;

/// 「这台设备的第一次读数还没拿到」的记账。
///
/// 关键点：**读到设备**和**读到百分比**是两件事。只报粗档位、不报百分比的设备
/// 每一轮都在答应，可它永远不会给出一个数字 —— 以前拿 `percent.is_none()`
/// 当「还没初始化」，这类设备就让整条轮询线永远停在 5 秒周期，
/// 顺带把别的设备也拖成 5 秒读一次。
///
/// 抽成独立类型是为了能单测：这个判断决定整条轮询线的节奏，
/// 写错的表现是「费电、设备老被叫醒」，不会有人报成 bug。
#[derive(Debug, Default)]
struct ReadingState {
    /// 成功读到过一次（有没有百分比都算）
    received: bool,
    /// 参与过快轮询的轮数
    attempts: u8,
}

impl ReadingState {
    /// 每轮读取前调用一次
    fn begin_attempt(&mut self) {
        self.attempts = self.attempts.saturating_add(1);
    }

    /// 读到设备了 —— 哪怕这次没给出百分比，也算「不需要再快试了」
    fn record_success(&mut self) {
        self.received = true;
    }

    fn pending(&self) -> bool {
        !self.received && self.attempts < FAST_RETRY_LIMIT
    }
}

/// 手绘电量环图标：32×32 RGBA，零图像处理依赖（这正是「PowerRing」的由来）
/// percent 为 None 表示「未知」—— 画成全灰环，绝不用 0% 冒充读不到（PRD F2）
fn ring_icon(percent: Option<u8>, charging: bool) -> Icon {
    const S: usize = 32;
    let mut rgba = vec![0u8; S * S * 4];
    let c = (S as f32 - 1.0) / 2.0;
    let r_out = 15.2_f32;
    let r_in = 10.8_f32;
    let filled_deg = percent.unwrap_or(0).min(100) as f32 / 100.0 * 360.0;
    // 未填充部分和「未知」整环取当前主题的灰：深色任务栏上用浅色主题那种深灰会看不清
    let dim = crate::theme::colors().ring_empty;
    let unknown = crate::theme::colors().ring_unknown;

    for y in 0..S {
        for x in 0..S {
            let dx = x as f32 - c;
            let dy = y as f32 - c;
            let d = (dx * dx + dy * dy).sqrt();
            if d < r_in || d > r_out {
                continue;
            }
            // 以 12 点方向为 0°，顺时针填充
            let mut ang = dy.atan2(dx).to_degrees() + 90.0;
            if ang < 0.0 {
                ang += 360.0;
            }
            let (r, g, b) = match percent {
                None => unknown, // 未知：整环灰
                Some(p) if ang <= filled_deg => color_of(p, charging),
                Some(_) => dim, // 未填充部分
            };
            let i = (y * S + x) * 4;
            rgba[i..i + 4].copy_from_slice(&[r, g, b, 255]);
        }
    }
    Icon::from_rgba(rgba, S as u32, S as u32).expect("构造图标失败")
}

/// 读数滞回：变化不足 2 个百分点就不动。
/// 原因（M0.5 实测）：电压有 1mV 抖动，正好压在放电曲线档位边界上，
/// 不做平滑会让百分比在 27% 和 28% 之间反复横跳。
fn smooth(shown: Option<u8>, fresh: u8) -> u8 {
    match shown {
        None => fresh,
        Some(s) if (fresh as i16 - s as i16).abs() >= 2 => fresh,
        // 跨档例外：21% → 20% 只差 1，但 20% 正好压在默认警告线上。
        // 压着不动的话界面停在 21%、提醒也不触发，而用户看一眼系统电量就是 20% ——
        // 滞回是为了防抖，不该把「刚好跨过那条线」也一起吞掉。
        // 这里用 charging=false 只比电量档，充电与否不参与判断
        Some(s) if tier_of(Some(s), false) != tier_of(Some(fresh), false) => fresh,
        Some(s) => s,
    }
}

/// 取走并处理本线程消息队列里的所有消息。
///
/// 这是右键菜单能弹出的**唯一**前提：tray-icon（0.26）在 Windows 上不自带事件循环
/// （其源码内没有任何 GetMessageW）。set_icon / set_tooltip 之所以一直正常，是因为它们
/// 直接调 Shell_NotifyIconW，不走消息队列；而右键点击是 Shell 把 WM_RBUTTONUP 投递到
/// tray-icon 的隐藏窗口，只有把消息取出来派发，它内部的 wndproc 才会调 show_tray_menu。
unsafe fn pump_messages() {
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };

    let mut msg: MSG = std::mem::zeroed();
    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
        // 设置窗口是自建的顶层窗口，不是对话框：Tab 换焦点、Enter 保存、Esc 关闭
        // 这些「对话框肌肉记忆」得靠对话框管理器补上，否则一组 WS_TABSTOP 控件
        // 只能用鼠标点，Esc 更是按了没反应。返回 true 表示消息已被处理完
        if settings::handle_dialog_keys(&msg) {
            continue;
        }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

/// 托盘悬停文字最多列几台。悬停只有一两秒的阅读时间，
/// 列再多也看不完；完整名单在左键面板里
const TIP_MAX_ITEMS: usize = 3;
/// Windows 的 szTip 最多 128 字符，留出余量。超了会被系统从中间截断成半句话
const TIP_MAX_CHARS: usize = 100;

/// 托盘悬停文字。设备一多，全名逐个拼起来会长到既读不完、又会被系统截断，
/// 所以多设备时改用短名，并且最多列 3 台。单设备时用全名 —— 那点长度不值一提，
/// 反而是「这是哪台」最该说清楚的时候。
fn tooltip_of(list: &[Status]) -> String {
    if list.is_empty() {
        return "PowerRing — 未检测到设备".to_string();
    }
    if list.len() == 1 {
        return tip_line(&list[0], &list[0].name);
    }

    // 电量低的排前面：悬停最可能想看的就是「哪台该充电了」。
    // 读不到电量的排最后，未连接的排在它们之后 —— 它是已知的「不在线」，
    // 不该跟「在线但读不到」混在一起
    let mut sorted: Vec<&Status> = list.iter().collect();
    sorted.sort_by_key(|s| (s.offline, s.percent.unwrap_or(u8::MAX)));

    let mut parts: Vec<String> = sorted
        .iter()
        .take(TIP_MAX_ITEMS)
        .map(|s| tip_line(s, &s.short))
        .collect();
    if sorted.len() > TIP_MAX_ITEMS {
        parts.push(format!("等 {} 台", sorted.len()));
    }
    // 装不下就砍掉末尾。宁可少列一台，也不要被系统截成半句话
    while parts.len() > 1 && parts.join(" · ").chars().count() > TIP_MAX_CHARS {
        parts.pop();
    }
    parts.join(" · ")
}

/// 读书来源在界面上怎么标。实时读数不标 —— 那是默认情况，标了只是噪音；
/// 另外两种必须标，因为用户看到的数字其实是旧值
fn source_note(source: ReadingSource) -> &'static str {
    match source {
        ReadingSource::Live => "",
        ReadingSource::SystemCache => "（系统缓存）",
        ReadingSource::Previous => "（上次读数）",
    }
}

/// 日志里用的说法，比界面上的短
fn source_name(source: ReadingSource) -> &'static str {
    match source {
        ReadingSource::Live => "实时",
        ReadingSource::SystemCache => "系统缓存",
        ReadingSource::Previous => "上次读数",
    }
}

fn tip_line(s: &Status, name: &str) -> String {
    if s.offline {
        // 说「未连接」而不是给个数：那个数是它睡着之前读到的，
        // 摆出来像现在还是这么多，而它其实可能已经充了一夜
        return format!("{}（未连接）", name);
    }
    match s.percent {
        Some(p) => format!(
            "{} {}%{}{}",
            name,
            p,
            if s.charging { " 充电" } else { "" },
            source_note(s.source)
        ),
        // 充着电但拿不到百分比：别写「--」让人以为是没读到，
        // 也别编个 100% —— 它就是在充电，只是不知道充到哪儿了
        None if s.charging => format!("{} 充电中（电量未知）", name),
        None => format!("{} --", name),
    }
}

/// 弹窗里的行。色块颜色与托盘电量环共用同一套语义，
/// 用户看环和看面板得到的是同一个信号，不需要重新学一遍。
fn popup_lines(list: &[Status]) -> Vec<Line> {
    if list.is_empty() {
        return vec![Line {
            text: "未检测到设备".to_string(),
            swatch: (150, 150, 150),
        }];
    }
    list.iter()
        .map(|s| Line {
            text: if s.offline {
                format!("{}  未连接", s.name)
            } else {
                match s.percent {
                    Some(p) => format!(
                        "{}  {}%{}{}",
                        s.name,
                        p,
                        if s.charging { "  充电中" } else { "" },
                        source_note(s.source)
                    ),
                    None if s.charging => format!("{}  充电中（电量未知）", s.name),
                    None => format!("{}  --", s.name),
                }
            },
            swatch: swatch_of(s),
        })
        .collect()
}

/// 电量档位 → 颜色。电量环和弹窗色块共用这一处，改配色只改这里。
///
/// 阈值读的是设置窗口里当前的值（settings::low_*），不写死 10/20 ——
/// 否则用户把阈值改成 30，环色还是按 20 变橙，设置等于白改。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tier {
    Critical = 0,
    Warn = 1,
    Normal = 2,
    Charging = 3,
    Unknown = 4,
}

fn tier_of(percent: Option<u8>, charging: bool) -> Tier {
    // 顺序有讲究：**先比电量档，充电只在「不缺电」时才算一种状态**。
    // 反过来的话，一台 5% 的鼠标一插上线就变蓝环 —— 红环本该说的是
    // 「快没电了」，充电并不改变它现在缺电这个事实，蓝色反而把最该被看见的信号抹掉。
    // （真事：另一台耳机在充电，把 5% 的鼠标红环染成了蓝色）
    match percent {
        None => Tier::Unknown,
        Some(p) if p <= settings::low_critical() => Tier::Critical,
        Some(p) if p <= settings::low_warn() => Tier::Warn,
        Some(_) if charging => Tier::Charging,
        Some(_) => Tier::Normal,
    }
}

fn color_of(percent: u8, charging: bool) -> (u8, u8, u8) {
    match tier_of(Some(percent), charging) {
        Tier::Charging => (52, 152, 219), // 充电中：蓝
        Tier::Critical => (192, 57, 43),  // 严重：红
        Tier::Warn => (230, 126, 34),     // 警告：橙
        _ => (39, 174, 96),               // 正常：绿
    }
}

/// 托盘图标此刻的屏幕位置 (x, y, w, h)。
///
/// 之前这个位置只从「用户点击托盘」的事件里拿，没人点过就没有 —— 于是提醒卡片
/// 只能退回光标位置，用户在副屏操作它就弹到副屏去，跟托盘脱节。
/// 直接问 Shell 要，任何时刻都拿得到。拿不到（图标被折叠进 ^ 里、或托盘还没就绪）
/// 就返回 None，由调用方决定退路。
fn icon_rect(tray: &TrayIcon) -> Option<(i32, i32, i32, i32)> {
    let r = tray.rect()?;
    Some((
        r.position.x as i32,
        r.position.y as i32,
        r.size.width as i32,
        r.size.height as i32,
    ))
}

fn swatch_of(s: &Status) -> (u8, u8, u8) {
    match s.percent {
        None => (150, 150, 150), // 未知：灰
        Some(p) => color_of(p, s.charging),
    }
}

/// 托盘那一圈该画成什么样：(显示的电量, 是否按「充电中」着色)。
///
/// 环只画一台 —— 电量最低的那台，因为它是唯一需要人动手的。颜色必须跟着**同一台**：
/// 以前这里是「任意一台在充电就整环变蓝」，于是 5% 的鼠标会被旁边正在充电的耳机
/// 染成蓝色，最该被看见的红环就这么没了。
fn tray_ring(latest: &[Status]) -> (Option<u8>, bool) {
    match latest
        .iter()
        .filter_map(|s| s.percent.map(|p| (p, s.charging)))
        .min_by_key(|(p, _)| *p)
    {
        Some((p, c)) => (Some(p), c),
        None => (None, false),
    }
}

/// 只允许一个 PowerRing 在跑。返回 false 表示已经有实例了，本进程该退出。
///
/// 为什么非做不可：开了开机自启之后，用户很可能再手动双击一次 exe ——
/// 没有这道闸就会冒出第二个托盘图标，两个进程还同时轮询同一台设备。
/// 用命名互斥量而不是「查进程列表」：进程一旦结束（哪怕是崩的），
/// 内核会把它收回去，不会留下「下次再也启动不了」的残局。
fn claim_single_instance() -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;

    let name: Vec<u16> = "PowerRing.SingleInstance"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        match CreateMutexW(None, true, PCWSTR(name.as_ptr())) {
            // 句柄故意不关：关了锁就放了，得让它活到进程结束。
            // 互斥量是内核对象，进程退出时自动释放
            Ok(_) => GetLastError() != ERROR_ALREADY_EXISTS,
            Err(e) => {
                // 建不了锁只影响「防多开」，不影响电量监测本身，让它继续启动
                warn!("[startup] 建不了互斥量：{e}，按单实例处理失败，继续启动");
                true
            }
        }
    }
}

/// 设备信息收集页。不上 GitHub 的人也需要一条路把设备报过来，
/// 所以它是一个免登录问卷而不是 Issue 模板。
///
/// 换链接时记得同步三处：README.md、README.en.md、.github/ISSUE_TEMPLATE/device-request.yml
const SUBMIT_URL: &str = "https://wj.qq.com/s2/28339156/r6r6";

/// 「检查设备声明更新…」是不是还有一次没回来。
/// 一次最多要试三个地址、每个最多 8 秒，这期间再点一次不该又发一轮 ——
/// 重复拉不会带来任何新东西，只会让人等得更久
static SYNC_BUSY: AtomicBool = AtomicBool::new(false);

/// 起一个线程去拉远程声明，结果送进 `tx`。
///
/// 必须另起线程：一次同步最多要试三个地址、每个最多 8 秒，
/// 放在跑消息泵的那个线程上做，窗口就二十几秒不响应，看起来和卡死没区别
fn start_sync(tx: mpsc::Sender<Result<sync::Outcome, String>>) {
    let url = settings::sync_url();
    info!("[sync] 开始，自定义地址={:?}", url);
    thread::spawn(move || {
        // 留空表示「用内置的两个源」—— 判断放在这里，
        // 免得每个调用点都要记得区分「没填」和「填了空串」
        let custom = if url.trim().is_empty() {
            None
        } else {
            Some(url.as_str())
        };
        let out = sync::run(custom);
        // 收的人可能已经退出了，发不出去就算了
        let _ = tx.send(out);
    });
}

/// 用系统默认浏览器打开一个网址。
///
/// 为什么用 ShellExecuteW 而不是拉起 chrome.exe：默认浏览器不一定是谁，
/// 硬编码只会打开错的那个（甚至根本没装）。交给 Shell 就永远是对的。
fn open_url(url: &str) -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let op: Vec<u16> = "open\0".encode_utf16().collect();
    let wide: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    let r = unsafe {
        ShellExecuteW(
            None, // 不挂父窗口：托盘程序没有主窗口，硬造一个反而容易把浏览器挡住
            PCWSTR(op.as_ptr()),
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // 约定：返回值大于 32 是成功，否则是错误码（文件没关联、内存不足之类）
    let ok = r.0 as isize > 32;
    if !ok {
        warn!("[tray] 打不开浏览器，ShellExecuteW 返回 {}", r.0 as isize);
    }
    ok
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 日志放在最前面：后面任何一步出问题，都要有地方可查
    log::init();

    // 开机自启的配套：已经有一个在跑就直接退出，别再挂一个托盘图标。
    // 放在日志之后，好让「双击了却没反应」这件事在日志里留得下痕迹
    if !claim_single_instance() {
        info!("[startup] 已经有一个 PowerRing 在跑了，本进程退出");
        return Ok(());
    }

    // Windows：给进程一个明确的 AppUserModelID，托盘通知才会归属到 PowerRing
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
        let _ = SetCurrentProcessExplicitAppUserModelID(windows::core::w!("PowerRing.PowerRing"));
    }

    // 声明 Per-Monitor DPI 感知：不做这一步，Windows 会把我们的窗口坐标按
    // 96 DPI 虚拟化，而托盘位置（Shell_NotifyIconGetRect）给的是物理像素 ——
    // 两套坐标一混，4K + 150% 缩放的机器上弹窗就飘到屏幕边上去了。
    // 副作用是系统不再帮我们拉伸界面，文字也会更清晰（代价是要自己按 DPI 放大字号）。
    // 必须放在创建任何窗口之前，晚了就不生效。
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        let ok = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        info!("[dpi] Per-Monitor V2 感知：{:?}", ok);
    }

    let cfg = Config::load();
    info!("[config] {:?}", cfg);

    let profiles = profile::load();
    info!("[profile] 已加载 {} 份设备声明", profiles.len());
    for p in &profiles {
        info!(
            "[profile] {} — {:04X}:{:04X}（{}，来源：{}）",
            p.name, p.vid, p.pid, p.verification, p.protocol_source
        );
    }

    let interval = cfg.poll_interval_secs.max(5);
    // 轮询线程每轮读 settings 里那个值，改完设置立刻生效，不用重启。
    // 阈值同理：环色和提醒判断都在运行时读它，改完立刻跟着变
    settings::set_interval(interval);
    settings::set_low(cfg.low_warn, cfg.low_critical);
    settings::set_notify(cfg.notify_enabled);
    settings::set_sync_url(cfg.remote_devices_url.clone());
    // 配色要在建窗口之前定下来：设置窗口和弹窗卡片都是按此刻的主题画的
    theme::set_mode(theme::Mode::from_str(&cfg.theme));

    // 开机自启自检：config 里说要自启、注册表里却没有，就补回去。
    // 会走到这一步的情况：exe 挪过位置、被清理软件删过启动项、config 是拷来的。
    // 放在单实例闸之后 —— 它改的是磁盘状态，一个进程做一次就够。
    // 反过来（注册表里有、config 说不要）不动：那多半是用户自己加的
    autostart::repair(cfg.autostart);

    // ---- 轮询线程：只管读设备，结果塞进 channel ----
    // hidapi 的设备句柄不能跨线程（Device 不是 Send），所以 HidApi 必须在线程内部创建，
    // 主线程从头到尾不碰 HID，只负责 UI 与消息泵。
    let (tx, rx) = mpsc::channel::<Vec<Status>>();
    // 低电量提醒走单独的通道：提醒是「事件」，不是「状态」，
    // 混进每轮都发的快照里，主线程无从判断该不该弹
    let (alert_tx, alert_rx) = mpsc::channel::<Vec<Alert>>();
    // 「立即刷新」用的唤醒通道：轮询线程平时睡在 recv_timeout 上，收到信号就提前醒。
    // 没有它，「刷新」菜单项就得干等下一轮轮询（60 秒），点了跟没点一样。
    let (trig_tx, trig_rx) = mpsc::channel::<()>();
    // 「导出设备信息」：菜单线程只发一个信号，真正生成报表的是轮询线程 ——
    // 报表要枚举 HID 并试探 HID++，而 HidApi 只能在轮询线程里用（Device 不是 Send）。
    // 生成完的文本走 report_tx 回主线程，由主线程写剪贴板（剪贴板要在有窗口的线程上开）
    let (export_tx, export_rx) = mpsc::channel::<()>();
    let (report_tx, report_rx) = mpsc::channel::<String>();
    // 「检查设备声明更新…」：网络请求最坏要等二十几秒，绝不能占着跑消息泵的那个线程
    // （占住了窗口就不响应，看起来像卡死）。所以另起一个线程去拉，结果送回来弹卡片。
    // reload 那条是给轮询线程的：声明落到磁盘上了，可它手里那份是启动时读的，
    // 不通知它重读一遍，用户得重启程序才认得出新设备
    let (sync_tx, sync_rx) = mpsc::channel::<Result<sync::Outcome, String>>();
    let (reload_tx, reload_rx) = mpsc::channel::<()>();
    thread::spawn(move || {
        struct Watched {
            dev: device::Device,
            percent: Option<u8>,
            charging: bool,
            /// 这个读数是刚读的、系统缓存的，还是这一轮没读到沿用的上次值
            source: ReadingSource,
            /// 两档提醒各自的弹药
            arm_critical: Arm,
            arm_warn: Arm,
            /// 连续读不到的次数。到上限就移出列表 ——
            /// 无线耳机一关机就读不到了，让它继续挂着上一次的电量是在骗人
            misses: u8,
            /// 联系不上但接收器还在：标成未连接留在列表里（见 Device::silence_is_ambiguous）。
            /// 它不是 misses 的另一种写法 —— misses 是「准备请出去」，这个是「留着等它回来」
            offline: bool,
            /// 承载它的硬件已经不在系统上了：接收器被拔、蓝牙设备从系统里消失。
            /// 这是硬证据，本轮直接移出，不必攒失败次数
            detached: bool,
            /// 第一次读数拿到了没有。与 percent 分开记 —— 见 ReadingState
            reading: ReadingState,
            /// 提醒的首次百分比是否已经用来装填弹药。
            /// 不能用 percent.is_none() 代替：那会让「充着电读到未知 → 拔线读到数字」
            /// 被当成又一次「刚开机」，把已经弹过的那档重新装弹
            alert_initialized: bool,
        }

        impl Watched {
            fn new(dev: device::Device) -> Self {
                Self {
                    dev,
                    percent: None,
                    charging: false,
                    source: ReadingSource::Live,
                    arm_critical: Arm::Armed,
                    arm_warn: Arm::Armed,
                    misses: 0,
                    offline: false,
                    detached: false,
                    reading: ReadingState::default(),
                    alert_initialized: false,
                }
            }

            /// 这台设备还要不要走 5 秒短周期
            fn needs_fast_retry(&self) -> bool {
                // 未连接的设备不算「还没读出来」—— 它读出来过，只是现在联系不上。
                // 算进去的话轮询会永久卡在 5 秒一轮，白费力气（还会一直叫醒它）
                !self.offline && self.reading.pending()
            }
        }

        /// 连续读不到几次就把设备移出列表。
        /// 不能取 1：GPW 偶尔单轮读失败，一次失败就删设备会让列表闪烁。
        /// 但从 3 降到 2 有代价更小的办法兜住误判 —— 见 REMOVED_RECHECK，
        /// 误删的设备 5 秒内就自己回来了。
        const MISS_LIMIT: u8 = 2;
        /// 有设备正在「读失败待确认」时，下一轮别等满一个周期。
        /// 关机后要连着确认两次才算数，如果两次间隔都是 30 秒，用户要等一分钟才看到它消失。
        /// 8 秒是折中：够让一次勉强的读取有第二次机会，又不会让确认拖太久。
        const CONFIRM_WAIT: u64 = 8;
        /// 刚移出设备后立刻回头看一眼：万一删错了（USB 重枚举、蓝牙闪断），
        /// 它在这个间隔内就会重新出现，用户只看到闪一下，而不是整分钟地缺一行。
        const REMOVED_RECHECK: u64 = 5;
        /// 设备被移出后，它「随时可能回来」的窗口长度，以及这段时间里重扫的间隔。
        ///
        /// 少了这个窗口，设备重新出现只能等那次 60 秒一次的定期重扫 ——
        /// 而轮询循环自己还要多久醒一次由 `wait` 决定，两者叠起来就是用户实测的 50 秒。
        /// 这段时间里把轮询和重扫都压密，它重新开机会在十几秒内出现。
        /// 到点自动恢复常规节奏：设备真的一晚上不开，也不该整夜每 10 秒扫一遍总线。
        const RETURN_WINDOW: Duration = Duration::from_secs(180);
        const RETURN_WAIT: u64 = 10;
        /// 空列表最多连续走几轮 5 秒短周期。
        ///
        /// 4 轮 ≈ 20 秒，覆盖得住「开机时接收器几十秒不响应」里最坏的那一段
        /// （每轮本身还要花时间在扫描上，实际墙钟更长）。之后退回正常周期：
        /// 一台设备都没有往往意味着本机就没有受支持的设备，这时候每 5 秒
        /// 枚举一遍总线、打开一遍 GATT、再发一遍探测报文，纯粹是白烧电 ——
        /// 而探测会把睡着的设备叫醒，等于替它耗电
        const EMPTY_SCAN_LIMIT: u32 = 4;
        /// 隔多久重新扫一遍总线。无线设备随时可能开机，
        /// 只在「一台都没扫到」时重扫是不够的 —— 已经有一台时，后开机的那台永远进不来
        const RESCAN_EVERY: Duration = Duration::from_secs(60);
        /// 隔多久再试一次「自动发现」—— 认那些没写进声明的设备。
        ///
        /// 只在**一台设备都没有**时才重试。探测要真的发报文，会把睡着的设备叫醒；
        /// 列表非空就说明已经有设备在用，这时候不该为了「万一还有一台」去打扰它们。
        /// 启动时那一次是例外（见下面的 force）：新买的设备插上就该被认出来。
        const DISCOVER_EVERY: Duration = Duration::from_secs(60);

        /// 判断这台设备是不是刚跌破阈值，并把弹药状态推进一轮。
        ///
        /// 只在**向下跨档**时提醒，且每档只响一次。回升 3 个百分点才重新装弹 ——
        /// 电压有 ±1% 抖动（M0.5 实测），不留缓冲会在 20/21 之间每 60 秒弹一次。
        fn check_alert(w: &mut Watched, fresh: bool) -> Option<Alert> {
            let percent = w.percent?;
            let critical = settings::low_critical();
            let warn = settings::low_warn();
            // 插上电就算满血：两档都装满，拔线放光后还能再提醒
            if w.charging {
                w.arm_critical = Arm::Armed;
                w.arm_warn = Arm::Armed;
                return None;
            }
            // 刚开机（或刚扫到的设备）：在阈值之上就装弹，在阈值之下算「已弹过」——
            // 开机时就是 8% 属于既有状态，不是刚跌破，不该一上来就弹
            if fresh {
                w.arm_critical = if percent > critical {
                    Arm::Armed
                } else {
                    Arm::Spent
                };
                w.arm_warn = if percent > warn {
                    Arm::Armed
                } else {
                    Arm::Spent
                };
            }

            let mut out = None;
            if percent <= critical && w.arm_critical == Arm::Armed {
                w.arm_critical = Arm::Spent;
                // 已经喊过「严重」，警告那档就别再开口
                w.arm_warn = Arm::Spent;
                out = Some(Alert {
                    name: w.dev.name.clone(),
                    percent,
                    critical: true,
                });
            } else if percent <= warn && w.arm_warn == Arm::Armed {
                w.arm_warn = Arm::Spent;
                out = Some(Alert {
                    name: w.dev.name.clone(),
                    percent,
                    critical: false,
                });
            }
            if out.is_none() {
                // 这一轮没弹，才轮到「回升了就重新装弹」
                if w.arm_critical == Arm::Spent && percent > critical.saturating_add(3) {
                    w.arm_critical = Arm::Armed;
                }
                if w.arm_warn == Arm::Spent && percent > warn.saturating_add(3) {
                    w.arm_warn = Arm::Armed;
                }
            }
            // 开关关着时状态机照跑，只是不出声：
            // 否则关掉期间电量一路跌到底，重新打开开关的瞬间会补弹一张旧的。
            // 跨档这件事还是记一笔 —— 用户问「怎么不提醒了」时，看日志就知道是开关关着
            if !settings::notify_enabled() {
                if let Some(_a) = &out {
                    info!(
                        "[alert] 已跨档但提醒开关关着，不弹：{} {}%",
                        w.dev.name, percent
                    );
                }
                return None;
            }
            out
        }
        /// 重新枚举一次总线。
        ///
        /// `HidApi` 只在构造时枚举一次，之后 `device_list()` 返回的**永远是构造那一刻的快照**。
        /// 所以不刷新的话，「设备中途插上」这件事在程序眼里根本不存在 ——
        /// 定期重扫看起来在干活，扫的却是一份过期列表，白扫。
        /// 拔接收器那种场景反而看不出来（设备本来就在列表里），
        /// 只有「先开程序、后插接收器」的人才会撞上，于是特别难被测试和反馈覆盖。
        fn refresh_bus(api: &mut HidApi) {
            // 失败了不算大问题：接下来那次扫描用的还是旧列表，
            // 也就是退回到修复前的行为，比直接不扫要好
            if let Err(e) = api.refresh_devices() {
                warn!("[hid] 重新枚举设备失败：{e}");
            }
        }

        /// 试一次自动发现：把「没写进声明、但能用公开标准读到电量」的设备认出来。
        ///
        /// 认出来的会写进用户目录变成一份真声明，下次启动直接命中，不必再探测。
        /// `force` 只用于启动第一轮 —— 那时还没任何设备，认一遍最值。
        fn run_discover(
            api: &HidApi,
            profiles: &mut Vec<Profile>,
            last: &mut Instant,
            force: bool,
        ) {
            if !force && last.elapsed() < DISCOVER_EVERY {
                return;
            }
            *last = Instant::now();
            let found = crate::discover::find(api, profiles);
            if found.is_empty() {
                return;
            }
            let saved = crate::profile::save_discovered(&found);
            info!(
                "[discover] 认出 {} 台没写进声明的设备，写下 {} 份声明",
                found.len(),
                saved
            );
            profiles.extend(found);
        }

        let mut profiles = profiles;
        let mut api = match HidApi::new() {
            Ok(a) => a,
            Err(e) => {
                warn!("[hid] 初始化失败：{e}");
                return;
            }
        };
        // 自动发现：启动第一轮无条件试一次。设备是插着开机的，
        // 这时候认出来，用户第一次打开就不是空壳
        let mut last_discover = Instant::now();
        run_discover(&api, &mut profiles, &mut last_discover, true);
        let devices = device::scan(&api, &profiles);
        for d in &devices {
            info!("[scan] 发现设备：{}", d.name);
        }
        warn!("[scan] 首轮匹配到 {} 台设备", devices.len());

        let mut watched: Vec<Watched> = devices.into_iter().map(Watched::new).collect();
        let mut last_scan = Instant::now();
        // 刚有设备消失时置为中短期的截止时刻，在这之前都当它「随时会回来」
        let mut return_until: Option<Instant> = None;
        // 连着多少轮一台设备都没有。
        //
        // 空列表要走 5 秒短周期重试（开机时接收器常有几十秒不响应），但**不能永远**：
        // 那个短周期每轮都会刷新总线枚举、对每台蓝牙设备打开 GATT、
        // 甚至重新发探测报文。本机就是没有受支持的设备时，这就成了拿全量扫描当心跳 ——
        // 白耗电，而且探测会叫醒睡着的设备。连续几轮都空就退回正常周期
        let mut empty_scans: u32 = 0;

        loop {
            let awaiting = return_until.map(|t| Instant::now() < t).unwrap_or(false);
            // 首轮没扫到不代表没有设备：实测上一进程被强杀后，接收器会有几秒不响应。
            // 所以只要列表是空的，每轮都重试一次，而不是一次扫不到就判死刑。
            if watched.is_empty() {
                // 一台都没有，先想想是不是压根没被任何声明覆盖：
                // 也许是新买的设备刚插上，也许是它开机晚了一步。
                // 再认一次（频率由 run_discover 自己把着，不会每轮都发报文）
                // 重扫之前先把枚举刷新一遍 —— 不然「刚插上的接收器」
                // 压根不在 device_list 里，再怎么扫都扫不到。
                // 自动发现也一样，它也是顺着这份列表找的
                refresh_bus(&mut api);
                run_discover(&api, &mut profiles, &mut last_discover, false);
                let fresh = device::scan(&api, &profiles);
                if !fresh.is_empty() {
                    for d in &fresh {
                        info!("[scan] 发现设备：{}", d.name);
                    }
                    return_until = None; // 回来了，恢复常规重扫节奏
                    watched = fresh.into_iter().map(Watched::new).collect();
                }
                last_scan = Instant::now();
            } else if last_scan.elapsed()
                >= if awaiting {
                    Duration::from_secs(RETURN_WAIT)
                } else {
                    RESCAN_EVERY
                }
            {
                // 定期重扫是为了接住「中途开机」的设备：接收器一直插着，
                // 但只有耳机开了才会回应，不重扫就永远发现不了它。
                // 刚有设备消失时这个周期会临时压到 RETURN_WAIT —— 用户实测
                // 「重开 ♪ 出现」要 50 秒，就是被 60 秒的周期卡住的
                last_scan = Instant::now();
                refresh_bus(&mut api);
                for d in device::scan(&api, &profiles) {
                    // 按名字找，而不是「在不在列表里」：未连接的设备也在列表里，
                    // 但它手里那个句柄可能是拔掉之前那一个 —— 接收器拔了又插回来时，
                    // 旧句柄指向的是一个已经不存在的实例，永远不会再答话，
                    // 于是它会永远挂着「未连接」。趁它现在答话把句柄换成新的，
                    // 设备就不必经历「移出列表 — 重新发现」这一趟。
                    // 先按物理身份认人；名字只用来显示
                    let found = watched.iter().position(|w| w.dev.id == d.id).or_else(|| {
                        watched.iter().position(|w| {
                            is_returning(
                                w.offline,
                                &w.dev.name,
                                &w.dev.profile,
                                &d.name,
                                &d.profile,
                            )
                        })
                    });
                    if let Some(i) = found {
                        let w = &mut watched[i];
                        // 声明变了没有？同步来一份新声明后，设备本身和句柄都还在，
                        // 只是「怎么读它」不一样了。以前这里只处理「离线又回来」，
                        // 于是界面说「更新了 1 份，现在就能用」，读数却还是按旧规则算，
                        // 得重启才生效 —— 而且新旧两套规则都自称正确，用户无从分辨
                        let action = refresh_of(w.offline, w.dev.profile != d.profile);
                        if action != Refresh::Keep {
                            match action {
                                Refresh::Reconnect => {
                                    info!("[scan] {} 重新连上了，换用新句柄", d.name)
                                }
                                Refresh::Redefined => {
                                    info!("[scan] {} 的声明变了，换用新的读取规则", d.name)
                                }
                                Refresh::Keep => unreachable!("上面已经排除了"),
                            }
                            w.dev = d;
                            w.offline = false;
                            w.misses = 0;
                            // 刚插回来的设备未必立刻答得上来，
                            // 给它重新享受几轮短周期，别一上来就干等 60 秒
                            w.reading = ReadingState::default();
                            if action == Refresh::Redefined {
                                // 旧读数是按旧规则算出来的：不能拿来平滑（那会把
                                // 新规则的第一个读数拽向旧值），也不能继续挂在界面上
                                // 冒充新规则的结果。清掉，下一轮重新读
                                w.percent = None;
                                w.charging = false;
                                w.source = ReadingSource::Live;
                                // 提醒的弹药不动：声明更新不是「重新开机」，
                                // 已经弹过的低电提醒不该因为改了个换算表再弹一次
                            }
                        }
                        continue;
                    }
                    info!("[scan] 发现新设备：{}", d.name);
                    // 它回来了，不用再按「可能有设备要回来」的密度扫总线
                    return_until = None;
                    watched.push(Watched::new(d));
                }
            }

            let mut alerts: Vec<Alert> = Vec::new();
            for w in watched.iter_mut() {
                // 记一次尝试：读不出来也要记，否则「一直读不出来的设备」
                // 会让短周期永远停不下来
                w.reading.begin_attempt();
                match w.dev.read() {
                    Some(b) => {
                        w.misses = 0;
                        w.offline = false;
                        w.detached = false;
                        // 读到设备就算成功，哪怕这一次没给出百分比
                        w.reading.record_success();
                        // 弹药的初始装填只做一次，用的是第一个**有效的**百分比。
                        // 未知读数（充着电、只报档位）不参与，也不该让它重置弹药
                        let fresh = !w.alert_initialized;
                        if b.percent.is_some() {
                            w.alert_initialized = true;
                        }
                        if w.percent != b.percent {
                            info!(
                                "[battery] {} → {} mV {:?}%（来源：{}）",
                                w.dev.name,
                                b.mv,
                                b.percent,
                                source_name(b.source)
                            );
                        }
                        // 只在读到了数字时做滞回；读不到就是读不到（None）
                        w.percent = b.percent.map(|p| smooth(w.percent, p));
                        w.charging = b.charging;
                        w.source = b.source;
                        if let Some(a) = check_alert(w, fresh) {
                            alerts.push(a);
                        }
                    }
                    // 读失败：保留上一次的值。绝不用 0% 表示「读不到」（PRD §2.3）
                    None => {
                        // 这一轮没读到：留下的那个数就是「上次读数」。
                        // 界面上要说清楚，否则用户会把旧账当成现在的状态
                        w.source = ReadingSource::Previous;
                        // 「读不到电量」不等于「设备没了」，先问一句再决定算不算数。
                        // 设备浅睡时（实测 GPW 静止一段时间后就是这样）探测照样答应，
                        // 只是不回答电量请求。这种情况让它留在列表里、保留上次读数 ——
                        // 用户回来动一下鼠标就恢复了，比从托盘消失再冒出来好得多。
                        if w.dev.present() {
                            w.misses = 0;
                            w.offline = false;
                            debug!(
                                "[battery] {} 没回答电量请求（设备仍在，保留上次读数）",
                                w.dev.name
                            );
                            continue;
                        }

                        // 硬件已经不在系统上了（接收器被拔、蓝牙设备从系统里消失）：
                        // 这是硬证据，不用攒失败次数，本轮就请出去。
                        if w.dev.gone() {
                            w.detached = true;
                            continue;
                        }

                        // 剩下的情况：硬件还在，但设备不答话。
                        if w.dev.silence_is_ambiguous() {
                            // HID 接收器设备分不清「休眠」和「关机」—— 两者都是不回答。
                            // 那就老实标成未连接留着，别把睡着的鼠标从托盘抹掉
                            // （2026-10-25 实测：鼠标睡一觉就被移出，托盘变成灰色
                            // 「未检测到设备」，用户一动鼠标它又冒出来，来回折腾）。
                            w.misses = 0;
                            if !w.offline {
                                warn!(
                                    "[battery] {} 联系不上了（接收器仍在），标记为未连接",
                                    w.dev.name
                                );
                            }
                            w.offline = true;
                            continue;
                        }

                        // 蓝牙：明确知道没连上，那就是关了，照旧攒次数请出去
                        w.misses = w.misses.saturating_add(1);
                        warn!(
                            "[battery] {} 读取失败（连续 {} 次），保留上次读数",
                            w.dev.name, w.misses
                        );
                    }
                }
            }
            // 连续读不到的设备移出列表：关机了的耳机不该一直占着一行旧电量
            let before = watched.len();
            watched.retain(|w| w.misses < MISS_LIMIT);
            if watched.len() != before {
                info!(
                    "[scan] {} 台设备连续读不到，已移出列表",
                    before - watched.len()
                );
            }

            // 「彻底没了」的设备立即移出，不必攒够次数。
            // 判据是硬件层面的：接收器被拔掉，或者蓝牙设备从系统里消失 ——
            // 都是硬证据，不需要那份耐心。
            // 而「接收器还在、设备不答话」不算：那可能是休眠，已经标成未连接留着了。
            let before_gone = watched.len();
            watched.retain(|w| !w.detached);
            if watched.len() != before_gone {
                info!(
                    "[scan] {} 台设备已从系统消失，立即移出列表",
                    before_gone - watched.len()
                );
            }

            // 两种移出都算「刚删过东西」：把重扫计时拨到期，并把下一轮压到 5 秒。
            // 这样万一是误判（USB 重枚举、蓝牙闪断），它马上就被扫回来，
            // 用户看到的是闪一下，而不是缺一行等一整分钟。
            let removed = watched.len() != before;
            if removed {
                last_scan = Instant::now() - RESCAN_EVERY;
                return_until = Some(Instant::now() + RETURN_WINDOW);
            }
            let snap = watched
                .iter()
                .map(|w| Status {
                    name: w.dev.name.clone(),
                    short: w.dev.short.clone(),
                    // 未连接的设备不报数：上次读数可能是一小时前的，
                    // 让它继续参与「取最低那台」会把过时的数字当真的用
                    percent: if w.offline { None } else { w.percent },
                    charging: w.charging && !w.offline,
                    offline: w.offline,
                    source: w.source,
                })
                .collect::<Vec<_>>();
            if tx.send(snap).is_err() {
                break; // 主线程已退出
            }
            // 通常是空数组，只有真跨档了才有内容
            if alert_tx.send(alerts).is_err() {
                break;
            }
            // 只要还有设备从没读出来过，就用 5 秒短间隔重试，而不是干等一整轮。
            // 实测：开机或上一进程被强杀后，接收器会有几十秒不响应，
            // 若按 60 秒轮询，面板会长时间挂着「--」，看起来像坏了。
            //
            // 但「没读出百分比」不等于「没读出来」：只报档位的设备每一轮都在答应，
            // 却永远给不出数字，把它算进来就等于永远 5 秒一轮（还会顺带叫醒别的设备）。
            // 所以看的是 ReadingState：读到过就结束，读不到也只快试有限几轮
            let still_pending = watched.is_empty() || watched.iter().any(|w| w.needs_fast_retry());
            // 空列表的短周期只连续用几轮：见 empty_scans 的说明。
            // 一旦检出设备就清零，下次真的空下来仍然享受那几轮快速重试
            if watched.is_empty() {
                empty_scans = empty_scans.saturating_add(1);
            } else {
                empty_scans = 0;
            }
            // 有设备刚读失败、还在等第二次确认：别等满一个周期，否则关机后
            // 两次确认之间又要白等 30 秒，用户会以为程序没反应
            let confirming = watched.iter().any(|w| w.misses > 0);
            // 刚有设备消失：多为「先关机，过会儿再开」，这段时间把节奏压密，
            // 让它重新出现在十几秒的量级，而不是等 60 秒的重扫周期
            let wait: u64 = if watched.is_empty() && empty_scans > EMPTY_SCAN_LIMIT {
                // 连着几轮都一台没有：本机大概就没有受支持的设备，
                // 退回正常周期，别再拿全量扫描当心跳
                settings::interval() as u64
            } else if still_pending || removed {
                REMOVED_RECHECK
            } else if confirming {
                CONFIRM_WAIT
            } else if awaiting {
                RETURN_WAIT
            } else {
                settings::interval() as u64
            };
            // 睡觉改成带唤醒的等待：有人点「立即刷新」就立刻进入下一轮
            match trig_rx.recv_timeout(Duration::from_secs(wait as u64)) {
                Ok(()) => {
                    // 连点几次会攒下多个信号，先排空，免得接下来几轮白转
                    while trig_rx.try_recv().is_ok() {}
                    info!("[poll] 收到刷新请求，立即读一次");
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }

            // 有人点了「导出设备信息」：报表必须在这一侧生成（HidApi 在这儿），
            // 生成完立刻交回主线程，由它写剪贴板。
            // 检查点放在**睡醒之后**而不是一轮的开头：点菜单时会顺带把这一侧叫醒，
            // 于是卡片几乎是立刻弹出来，不用再白跑一整轮读电量
            if export_rx.try_recv().is_ok() {
                // 连点会攒下多个信号，排空，免得连着生成好几份
                while export_rx.try_recv().is_ok() {}
                let known = watched
                    .iter()
                    .map(|w| crate::report::Known {
                        name: w.dev.name.clone(),
                        percent: if w.offline { None } else { w.percent },
                        charging: w.charging && !w.offline,
                    })
                    .collect::<Vec<_>>();
                info!("[report] 生成设备信息报表（当前 {} 台设备）", known.len());
                if report_tx.send(crate::report::build(&api, &known)).is_err() {
                    break;
                }
            }

            // 同步落下了新的声明文件：手里这份是启动时读的，得重读一遍。
            // 不重读的话，用户会看到卡片写着「新增了 3 份」，却一台新设备都没出现 ——
            // 他只能重启程序，而重启是他不该为「点了一下菜单」付的代价
            if reload_rx.try_recv().is_ok() {
                // 连点会攒下多个信号，排空，免得连着重读好几遍
                while reload_rx.try_recv().is_ok() {}
                let fresh = profile::load();
                info!(
                    "[poll] 重新加载设备声明：{} 份（原来 {} 份）",
                    fresh.len(),
                    profiles.len()
                );
                profiles = fresh;
                // 把重扫计时拨到期，下一轮立刻按新声明扫一遍，别让人等一分钟
                last_scan = Instant::now() - RESCAN_EVERY;
            }
        }
    });

    // ---- 托盘：必须在跑消息泵的那个线程上创建 ----
    // 菜单里只有「退出」是不够用的，实测用户左右键点下去都直奔退出：
    // tray-icon 默认左键也会弹菜单（menu_on_left_click 默认 true），
    // 于是左键的弹窗和系统菜单同时出现，而菜单里又只有退出一项 —— 左键等于被吃掉。
    // 修法：左键纯弹窗（关掉它的菜单），右键保留系统菜单并给足内容。
    let menu = Menu::new();
    let open_panel = MenuItem::new("显示电量面板", true, None);
    let refresh = MenuItem::new("立即刷新", true, None);
    let open_settings = MenuItem::new("设置…", true, None);
    // 认不出的设备只能靠用户报过来：这一步把「本机有什么」变成一段可直接粘贴的文本。
    // 放在菜单里而不是藏进设置窗口 —— 它只在「设备没被认出来」时才用得上，
    // 而那种时候用户不会想到去翻设置
    let export = MenuItem::new("导出设备信息…", true, None);
    // 导出到手之后还得有个地方交出去。多数人不愿为报个设备去注册 GitHub 账号，
    // 所以收集页是一个不用登录就能填的问卷；这一项就是替用户省掉「去哪找链接」
    let submit = MenuItem::new("提交设备信息…", true, None);
    // 别人适配好的新设备，不用等下一个 exe 版本也能认出来。
    // 它和「提交设备信息」是一对：一个往外报，一个往回取
    let sync_profiles = MenuItem::new("检查设备声明更新…", true, None);
    let quit = MenuItem::new("退出 PowerRing", true, None);
    menu.append(&open_panel)?;
    menu.append(&refresh)?;
    menu.append(&open_settings)?;
    menu.append(&export)?;
    menu.append(&submit)?;
    menu.append(&sync_profiles)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&quit)?;

    let quitting = Arc::new(AtomicBool::new(false));
    // 菜单线程拿不到 TrayIcon（它不是 Send），只能把意图传回主线程，由主线程真正去做
    let panel_requested = Arc::new(AtomicBool::new(false));
    let settings_requested = Arc::new(AtomicBool::new(false));
    // 「提交设备信息…」：打开浏览器这件事留在主线程做（和设置窗口同理），
    // 菜单线程只负责记下意图
    let submit_requested = Arc::new(AtomicBool::new(false));
    let sync_requested = Arc::new(AtomicBool::new(false));
    let panel_id = open_panel.id().clone();
    let refresh_id = refresh.id().clone();
    let settings_id = open_settings.id().clone();
    let export_id = export.id().clone();
    let submit_id = submit.id().clone();
    let sync_profiles_id = sync_profiles.id().clone();
    let quit_id = quit.id().clone();
    let flag = quitting.clone();
    let req = panel_requested.clone();
    let sreq = settings_requested.clone();
    let sbreq = submit_requested.clone();
    let syreq = sync_requested.clone();
    let trigger = trig_tx.clone();
    let exp = export_tx.clone();
    // MenuEvent 只有在消息泵跑起来之后才会产生，所以这里只是「接」，不是「驱动」
    thread::spawn(move || {
        for ev in MenuEvent::receiver().iter() {
            if ev.id() == &panel_id {
                req.store(true, Ordering::SeqCst);
            } else if ev.id() == &refresh_id {
                let _ = trigger.send(());
            } else if ev.id() == &settings_id {
                sreq.store(true, Ordering::SeqCst);
            } else if ev.id() == &export_id {
                let _ = exp.send(());
                // 顺手把轮询线程叫醒：它平时睡在 recv_timeout 上，最长一个周期（30 秒）。
                // 不叫醒的话，「点一下导出」要干等到下一轮才有卡片弹出来
                let _ = trigger.send(());
            } else if ev.id() == &submit_id {
                sbreq.store(true, Ordering::SeqCst);
            } else if ev.id() == &sync_profiles_id {
                syreq.store(true, Ordering::SeqCst);
            } else if ev.id() == &quit_id {
                info!("[tray] 收到退出命令");
                flag.store(true, Ordering::SeqCst);
            }
        }
    });

    let tray = TrayIconBuilder::new()
        .with_tooltip("PowerRing 启动中")
        .with_icon(ring_icon(None, false))
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .build()?;

    // 弹窗窗口必须和消息泵同线程，否则 WM_PAINT 永远到不了
    popup::create()?;
    settings::create()?;
    // 开发用开关：设了环境变量就一开机直接弹设置窗口，
    // 这样改界面尺寸能在日志里量出真实客户区，不必每次都麻烦用户手动点开
    if std::env::var("POWERRING_OPEN_SETTINGS").is_ok() {
        settings::open();
    }
    // 同类开关：设了就开机导出一次设备信息。右键菜单没法在无人值守时点，
    // 这个开关让「导出」这条链路能被自动验证，也能用来排查用户报上来的那台机器
    if std::env::var("POWERRING_EXPORT_REPORT").is_ok() {
        let _ = export_tx.send(());
        // 同上：把它叫醒，不然要等一整轮
        let _ = trig_tx.send(());
    }
    // 同类开关：设了就开机同步一次设备声明。右键菜单没法在无人值守时点，
    // 这个开关让同步这条链路能被自动验证，也能用来复现「某人那边拉不到」
    // 时到底卡在哪个地址上 —— 那种时候他只会说「点了没反应」
    if std::env::var("POWERRING_SYNC").is_ok() {
        SYNC_BUSY.store(true, Ordering::SeqCst);
        start_sync(sync_tx.clone());
    }

    info!(
        "[tray] 托盘已就绪，每 {} 秒刷新一次（消息泵已启动）",
        interval
    );

    // ---- 主线程：消息泵 + 渲染 ----
    let mut latest: Vec<Status> = Vec::new();
    // 盯着配色有没有变，变了要重画托盘图标（见主循环里的注释）
    let mut seen_dark = theme::is_dark();
    let rx: Receiver<Vec<Status>> = rx;
    let report_rx: Receiver<String> = report_rx;
    while !quitting.load(Ordering::SeqCst) {
        unsafe { pump_messages() };

        // 点击托盘。与 MenuEvent 不同，这里用 try_recv 而非独立线程：
        // 主循环本来就 20Hz 在转，顺手收一下即可，省一个线程。
        // 按键分工：左键 = 开关电量面板；右键 = 系统菜单（由 tray-icon 自己弹），
        // 我们只负责在菜单要冒出来之前把面板收掉，免得置顶面板挡住菜单。
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click {
                button,
                button_state,
                rect,
                ..
            } = ev
            {
                info!(
                    "[click] {:?} {:?} rect=({},{}) size=({},{}) 面板当前可见={}",
                    button,
                    button_state,
                    rect.position.x,
                    rect.position.y,
                    rect.size.width,
                    rect.size.height,
                    popup::is_visible()
                );
                match (button, button_state) {
                    (MouseButton::Left, MouseButtonState::Up) => {
                        if popup::is_visible() {
                            popup::hide();
                        } else {
                            popup::show(
                                popup_lines(&latest),
                                rect.position.x as i32,
                                rect.position.y as i32,
                                rect.size.width as i32,
                                rect.size.height as i32,
                            );
                            info!("[popup] 显示 {} 台设备", latest.len());
                        }
                    }
                    (MouseButton::Right, MouseButtonState::Down) => popup::hide(),
                    _ => {}
                }
            }
        }

        // 菜单里的「显示电量面板」：真正的弹窗动作必须留在这个线程（窗口和泵同线程），
        // 菜单线程只负责把「有人点了」这个事实递过来。
        if panel_requested.swap(false, Ordering::SeqCst) {
            if popup::is_visible() {
                popup::hide();
            } else {
                match icon_rect(&tray) {
                    Some((x, y, w, h)) => popup::show(popup_lines(&latest), x, y, w, h),
                    // 拿不到托盘位置（图标被折叠进 ^ 里了）：菜单是用户主动点的，
                    // 这会儿光标就在菜单上，跟着光标是对的
                    None => popup::show_near_cursor(popup_lines(&latest)),
                }
                info!("[popup] 菜单打开面板，共 {} 台设备", latest.len());
            }
        }

        // 「设置…」同理：窗口必须和消息泵同线程，打开动作也留在这里
        if settings_requested.swap(false, Ordering::SeqCst) {
            settings::open();
        }

        // 「导出设备信息…」：轮询线程生成好文本送回来，这里负责放进剪贴板。
        // 必须给反馈 —— 用户点了没动静，只会以为程序坏了
        if let Ok(text) = report_rx.try_recv() {
            let lines = match report::copy(&text) {
                Ok(()) => vec![
                    Line {
                        text: "设备信息已复制到剪贴板".to_string(),
                        // 颜色沿用环色那一套，不在这里另写一套配色
                        swatch: color_of(100, false),
                    },
                    Line {
                        text: "粘贴给维护者就能申请适配".to_string(),
                        swatch: (150, 150, 150),
                    },
                ],
                Err(e) => {
                    warn!("[report] 写剪贴板失败：{e}");
                    // 退路：剪贴板被别的程序占着是常有的事，至少把文件留下
                    let where_ = match report::write_file(&text) {
                        Ok(p) => format!("已存到 {}", p.display()),
                        Err(e2) => format!("文件也没存下：{e2}"),
                    };
                    vec![
                        Line {
                            text: "剪贴板用不了".to_string(),
                            swatch: color_of(5, false),
                        },
                        Line {
                            text: where_,
                            swatch: (150, 150, 150),
                        },
                    ]
                }
            };
            info!("[report] 已导出设备信息");
            popup::alert(lines, "导出设备信息", icon_rect(&tray));
        }

        // 「提交设备信息…」：打开收集页。
        // 浏览器打不开时不能就这么算了 —— 用户点了两下却什么都没发生，
        // 只会认为功能坏了。退路是把链接塞进剪贴板，他可以自己粘到地址栏
        if submit_requested.swap(false, Ordering::SeqCst) {
            if open_url(SUBMIT_URL) {
                info!("[report] 已打开收集页 {SUBMIT_URL}");
            } else {
                let second = match report::copy(SUBMIT_URL) {
                    Ok(()) => "链接已复制到剪贴板",
                    Err(_) => "请把链接手动敲进浏览器",
                };
                popup::alert(
                    vec![
                        Line {
                            text: "打不开浏览器".to_string(),
                            swatch: color_of(5, false),
                        },
                        Line {
                            text: second.to_string(),
                            swatch: (150, 150, 150),
                        },
                        Line {
                            text: SUBMIT_URL.to_string(),
                            swatch: (150, 150, 150),
                        },
                    ],
                    "提交设备信息",
                    icon_rect(&tray),
                );
            }
        }

        // 「检查设备声明更新…」：真正发请求的是另一个线程（网络会卡住消息泵），
        // 这里只负责把它起起来
        if sync_requested.swap(false, Ordering::SeqCst) {
            if SYNC_BUSY
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                info!("[sync] 上一次还没回来，这次不重复发");
            } else {
                start_sync(sync_tx.clone());
            }
        }

        // 同步的结果回来了：这次成不成都要给个说法。
        // 沉默最容易被当成「程序没反应」
        if let Ok(out) = sync_rx.try_recv() {
            SYNC_BUSY.store(false, Ordering::SeqCst);
            let (lines, changed) = match out {
                Ok(o) => {
                    let wrote = o.added + o.updated;
                    // 写盘失败要单独说：全失败时 added/updated 都是 0，
                    // 若只按那两个值生成文案，会对着「一份都没写进去」说「已经是最新了」。
                    // 校验失败同理 —— 那也是「远程有、本机没有」，不能让文案看起来一切正常
                    let head = if o.failed > 0 && wrote == 0 {
                        format!("{} 份都没能写进本机", o.failed)
                    } else if o.failed > 0 {
                        format!("写入 {} 份，另有 {} 份没写进去", wrote, o.failed)
                    } else if o.rejected > 0 && wrote == 0 {
                        format!("有效声明没有变化，另有 {} 份校验失败", o.rejected)
                    } else if o.added > 0 {
                        format!("新增了 {} 份设备声明", o.added)
                    } else if o.updated > 0 {
                        format!("更新了 {} 份设备声明", o.updated)
                    } else {
                        "已经是最新了".to_string()
                    };
                    let changed = wrote > 0;
                    if changed {
                        let _ = reload_tx.send(());
                        // 顺手把它叫醒：它平时睡一整轮，
                        // 不叫醒的话新声明要等一分钟才生效
                        let _ = trig_tx.send(());
                    }
                    let mut v = vec![Line {
                        text: head,
                        swatch: if o.failed > 0 || o.rejected > 0 {
                            theme::colors().warn
                        } else {
                            color_of(100, false)
                        },
                    }];
                    if o.rejected > 0 {
                        // 说清后果：这几份没能写进来，但本地已有的旧声明我们没删
                        v.push(Line {
                            text: format!("{} 份声明校验失败，本地旧声明已保留", o.rejected),
                            swatch: theme::colors().warn,
                        });
                    }
                    if changed {
                        v.push(Line {
                            text: "现在就能用，不用重启".to_string(),
                            swatch: (150, 150, 150),
                        });
                    }
                    v.push(Line {
                        text: format!("远程清单共 {} 份", o.total),
                        swatch: (150, 150, 150),
                    });
                    (v, changed)
                }
                // 拉不到是常态之一 —— 连不上 GitHub 的人永远拉不到。
                // 所以措辞是「这次没拉到」而不是「出错了」：已有的设备照常显示，
                // 这一项本来就是加分项，不是识别设备的唯一途径
                Err(e) => {
                    let mut why: String = e.chars().take(60).collect();
                    if e.chars().count() > 60 {
                        why.push('…');
                    }
                    (
                        vec![
                            Line {
                                text: "这次没拉到更新".to_string(),
                                swatch: color_of(20, false),
                            },
                            Line {
                                text: "已有的设备照常显示".to_string(),
                                swatch: (150, 150, 150),
                            },
                            Line {
                                text: why,
                                swatch: (150, 150, 150),
                            },
                        ],
                        false,
                    )
                }
            };
            info!("[sync] 结果已弹卡片（声明有变化={changed}）");
            popup::alert(lines, "检查设备声明更新", icon_rect(&tray));
        }

        // 设置窗口里的「试一次」：提醒本来只在跌破阈值时发生，鼠标不会说低就低，
        // 没有这个入口用户就没法确认卡片长什么样、弹在哪个位置
        if settings::take_test_request() {
            // 哪怕是「还没读到设备」也要弹一张：按钮点了却毫无动静，
            // 用户只会以为它坏了
            let lines = if latest.is_empty() {
                vec![Line {
                    text: "还没有读到任何设备".to_string(),
                    swatch: (150, 150, 150),
                }]
            } else {
                latest
                    .iter()
                    .map(|s| Line {
                        text: format!(
                            "{}  {}{}",
                            s.name,
                            s.percent.map(|p| format!("{}%", p)).unwrap_or("--".into()),
                            if s.charging { "  充电中" } else { "" }
                        ),
                        swatch: swatch_of(s),
                    })
                    .collect::<Vec<_>>()
            };
            info!("[alert] 试一次：弹出 {} 台设备", latest.len());
            // 走「强制」那条：面板开着也照弹。用户点这个按钮就是想看卡片长什么样，
            // 被「面板已开着」挡掉的话，屏幕上什么都不变，他只会以为按钮坏了
            popup::alert_forced(lines, "提醒卡片（试一次）", icon_rect(&tray));
        }

        // 低电量提醒：绝大多数轮次收到的是空数组，只有真跨档了才有内容
        while let Ok(alerts) = alert_rx.try_recv() {
            if alerts.is_empty() {
                continue;
            }
            let lines = alerts
                .iter()
                .map(|a| Line {
                    text: format!(
                        "{}  {}%{}",
                        a.name,
                        a.percent,
                        if a.critical { "  该充电了" } else { "" }
                    ),
                    swatch: color_of(a.percent, false),
                })
                .collect::<Vec<_>>();
            let title = if alerts.iter().any(|a| a.critical) {
                "电量严重不足"
            } else {
                "电量偏低"
            };
            // 锚定托盘图标，而不是光标 —— 提醒是程序主动冒出来的，
            // 用户这会儿多半在副屏上干别的，跟着光标就会弹到副屏去
            let anchor = icon_rect(&tray);
            info!(
                "[alert] {} 台设备跌破阈值：{} 锚点={}",
                alerts.len(),
                lines
                    .iter()
                    .map(|l| l.text.clone())
                    .collect::<Vec<_>>()
                    .join(" / "),
                match anchor {
                    Some((x, y, _, _)) => format!("托盘 ({},{})", x, y),
                    None => "主屏右下角".to_string(),
                }
            );
            popup::alert(lines, title, anchor);
        }

        popup::tick();

        // 配色变了就重画：托盘图标平时只在读数变化时更新，不盯着这里会一直保持旧配色。
        // 变化来源有两个 —— 设置窗口里切主题，以及用户在系统设置里换了浅色/深色
        let dark = theme::is_dark();
        if dark != seen_dark {
            seen_dark = dark;
            let (ring_pct, ring_charging) = tray_ring(&latest);
            if let Err(e) = tray.set_icon(Some(ring_icon(ring_pct, ring_charging))) {
                warn!("[tray] 换配色后重设图标失败：{e}");
            }
            popup::repaint();
            info!("[theme] 配色已切换，图标与卡片重绘");
        }

        let mut changed = false;
        while let Ok(snap) = rx.try_recv() {
            latest = snap;
            changed = true;
        }

        if changed {
            // tooltip 逐台列出；托盘图标取**电量最低**的那台（最需要被看到的那个）
            let tip = tooltip_of(&latest);
            let (ring_pct, ring_charging) = tray_ring(&latest);

            if let Err(e) = tray.set_icon(Some(ring_icon(ring_pct, ring_charging))) {
                warn!("[tray] set_icon 失败：{e}");
            }
            if let Err(e) = tray.set_tooltip(Some(&tip)) {
                warn!("[tray] set_tooltip 失败：{e}");
            }
            info!("[tray] tooltip = {:?}", tip);

            // 面板开着时，新读数直接刷到面板上（用户点开它就该看到最新值）
            if popup::is_visible() {
                popup::refresh(popup_lines(&latest));
            }
        }

        // 20Hz 轮询消息队列：既保证右键即时响应，空闲 CPU 也几乎为零
        thread::sleep(Duration::from_millis(50));
    }

    // 正常退出：让 TrayIcon 走到 drop，托盘图标才不会变成幽灵残影
    info!("[tray] 正在退出");
    drop(tray);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        is_returning, popup_lines, refresh_of, smooth, tier_of, tip_line, tooltip_of, tray_ring,
        ReadingState, Refresh, Status, Tier, FAST_RETRY_LIMIT, SUBMIT_URL, TIP_MAX_CHARS,
    };
    use crate::hidpp::ReadingSource;
    use crate::settings;

    /// 造一份只有 product_id 不同的声明，用来区分「同一台」和「另一台」
    fn profile_of(pid: &str) -> crate::profile::Profile {
        let text = r#"devices:
- name: 同名手柄
  protocol: ble
  match:
    vendor_id: '0x045E'
    product_id: 'PID'
  battery:
    kind: percent
"#
        .replace("PID", pid);
        crate::profile::parse_bundle(&text)
            .expect("测试声明应该能解析")
            .profiles
            .remove(0)
    }

    /// 接口路径变了（接收器拔插、蓝牙重连）之后，还得认得出是同一台设备
    /// —— 但只能认那些标着「未连接」的行
    #[test]
    fn 只有未连接那一行才算它回来了() {
        let a = profile_of("0x0B13");
        assert!(is_returning(true, "同名手柄", &a, "同名手柄", &a));
        // 在线的那行不能被抢：这正是第二只同型号设备被吞掉的老路
        assert!(
            !is_returning(false, "同名手柄", &a, "同名手柄", &a),
            "在线设备不该被同名的新设备接管"
        );
        // 名字一样、声明不一样：那是另一台设备
        let b = profile_of("0x0B14");
        assert!(!is_returning(true, "同名手柄", &a, "同名手柄", &b));
        // 名字不一样、声明一样：也不认
        assert!(!is_returning(true, "别的名字", &a, "同名手柄", &a));
    }

    /// 声明没变、设备也还在线：这一轮什么都不该做。
    ///
    /// 特别是**不换句柄** —— 每轮重扫都换，等于每 60 秒重新连一次设备，
    /// 蓝牙那边会明显变慢，HID 侧则多出一次不必要的打开
    #[test]
    fn 普通重扫不动在线设备() {
        assert_eq!(refresh_of(false, false), Refresh::Keep);
    }

    /// 离线又回来：换句柄（旧的那个指向已经不存在的实例），读数留着还能用
    #[test]
    fn 离线回来只换句柄() {
        assert_eq!(refresh_of(true, false), Refresh::Reconnect);
    }

    /// 声明变了：在线也要换读取规则，旧读数必须清掉。
    /// 否则界面说着「更新了 1 份」，读数还是按旧换算表算的，重启后才变
    #[test]
    fn 声明变了在线设备也换读取规则() {
        assert_eq!(refresh_of(false, true), Refresh::Redefined);
        // 离线且声明变了：以声明为准，同样要清读数
        assert_eq!(refresh_of(true, true), Refresh::Redefined);
    }

    /// 只报档位、永远给不出百分比的设备：读到一次就该结束短周期。
    ///
    /// 这是「读不到电量」和「读不到设备」被混为一谈的地方 —— 那一台会替所有
    /// 设备决定轮询节奏，表现是整晚每 5 秒刷一遍总线
    #[test]
    fn 未知百分比也算读到了() {
        let mut s = ReadingState::default();
        s.begin_attempt();
        assert!(s.pending(), "第一轮还没读到，应该继续快试");
        s.record_success();
        assert!(!s.pending(), "设备答应了但没给数字，也不该继续 5 秒一轮");
        // 之后再失败也不退回初始化状态
        s.begin_attempt();
        assert!(!s.pending());
    }

    /// 怎么都读不到的设备（声明写错了、设备根本没插）不能把轮询线永久钉在 5 秒
    #[test]
    fn 读不到的设备只快试有限轮() {
        let mut s = ReadingState::default();
        for _ in 0..FAST_RETRY_LIMIT - 1 {
            s.begin_attempt();
            assert!(s.pending(), "还没到上限，应该继续快试");
        }
        s.begin_attempt();
        assert!(!s.pending(), "到上限就该退回正常周期");
        // 再往后也不该自己复活
        s.begin_attempt();
        assert!(!s.pending());
    }

    /// 收集页的链接是写死在程序里的，改的时候容易手滑：
    /// 少个 s、多截一个字符，点下去就是 404，而这个错只有真去点才会发现。
    /// 所以让格式有个断言兜底
    #[test]
    fn 收集页链接是完整地址() {
        assert!(
            SUBMIT_URL.starts_with("https://"),
            "必须是 https：{SUBMIT_URL}"
        );
        assert!(
            SUBMIT_URL.contains("/s2/"),
            "看着不像问卷投放链接：{SUBMIT_URL}"
        );
        assert!(!SUBMIT_URL.ends_with('/'), "末尾多了斜杠：{SUBMIT_URL}");
    }

    fn s(name: &str, short: &str, pct: Option<u8>) -> Status {
        Status {
            name: name.to_string(),
            short: short.to_string(),
            percent: pct,
            charging: false,
            offline: false,
            source: ReadingSource::Live,
        }
    }

    /// 未连接的设备：给不出数，只能报状态
    fn offline(name: &str, short: &str) -> Status {
        Status {
            name: name.to_string(),
            short: short.to_string(),
            percent: None,
            charging: false,
            offline: true,
            source: ReadingSource::Live,
        }
    }

    #[test]
    fn 单设备用全名() {
        let t = tooltip_of(&[s("Logitech G PRO Wireless", "GPW", Some(18))]);
        assert_eq!(t, "Logitech G PRO Wireless 18%");
    }

    /// 数字背后是「刚读的」还是「系统缓存 / 上次读数」，界面必须说出来。
    /// 三种来源原本长得一模一样，用户没法判断这个 100% 值不值得信
    #[test]
    fn 非实时读数会标出来源() {
        let mut cached = s("Xbox 手柄", "手柄", Some(80));
        cached.source = ReadingSource::SystemCache;
        assert_eq!(tip_line(&cached, "Xbox 手柄"), "Xbox 手柄 80%（系统缓存）");

        let mut old = s("GPW", "GPW", Some(71));
        old.source = ReadingSource::Previous;
        assert_eq!(tip_line(&old, "GPW"), "GPW 71%（上次读数）");

        // 实时读数是常态，不标 —— 每个数字后面挂个「（实时）」只是噪音
        assert_eq!(tip_line(&s("GPW", "GPW", Some(71)), "GPW"), "GPW 71%");
    }

    /// 充着电但拿不到百分比：说「电量未知」，不能编 100%，也不能装作没读到
    #[test]
    fn 充电但读不到电量时不编数字() {
        let mut c = s("HyperX 天箭", "天箭", None);
        c.charging = true;
        assert_eq!(tip_line(&c, "天箭"), "天箭 充电中（电量未知）");
        assert_eq!(popup_lines(&[c])[0].text, "HyperX 天箭  充电中（电量未知）");
    }

    #[test]
    fn 多设备用短名且电量低的在前() {
        let t = tooltip_of(&[
            s("HyperX Cloud Flight", "Cloud Flight", Some(60)),
            s("Logitech G PRO Wireless", "GPW", Some(18)),
        ]);
        assert_eq!(t, "GPW 18% · Cloud Flight 60%");
    }

    #[test]
    fn 超过三台只列三台并说明总数() {
        let list: Vec<Status> = (0..5)
            .map(|i| s(&format!("设备{i}全名"), &format!("D{i}"), Some(10 + i)))
            .collect();
        let t = tooltip_of(&list);
        assert!(t.contains("等 5 台"), "实际：{t}");
        // 列出来的三台应该是电量最低的那三台
        assert!(t.starts_with("D0 10% · D1 11% · D2 12%"), "实际：{t}");
    }

    #[test]
    fn 读不到电量的排在最后() {
        let t = tooltip_of(&[
            s("未知设备", "未知", None),
            s("HyperX Cloud Flight", "Cloud Flight", Some(60)),
        ]);
        assert_eq!(t, "Cloud Flight 60% · 未知 --");
    }

    /// 守的是 2026-10-25 那次：鼠标睡一觉就被当成「设备没了」删掉，
    /// 托盘变成灰色「未检测到设备」。现在它该留在列表里标明未连接
    #[test]
    fn 未连接的设备留在列表里且说未连接() {
        let t = tooltip_of(&[offline("Pro Wireless Gaming Mouse", "GPW")]);
        assert_eq!(t, "Pro Wireless Gaming Mouse（未连接）");
    }

    /// 未连接排在「在线但读不到」之后：它是明确的离线，不该跟读不到混在一起
    #[test]
    fn 未连接排在最后() {
        let t = tooltip_of(&[
            offline("HyperX Cloud Flight", "天箭"),
            s("未知设备", "未知", None),
            s("Logitech G PRO Wireless", "GPW", Some(88)),
        ]);
        assert_eq!(t, "GPW 88% · 未知 -- · 天箭（未连接）");
    }

    /// 这条守的是「将来加了第三台、第四台设备」：名字再长也不会被系统截成半句话
    #[test]
    fn 再长也不会超过托盘文字上限() {
        let long = "超长设备名称".repeat(10);
        let list: Vec<Status> = (0..6).map(|i| s(&long, &long, Some(i))).collect();
        let t = tooltip_of(&list);
        assert!(
            t.chars().count() <= TIP_MAX_CHARS,
            "悬停文字 {} 字符，超过上限 {TIP_MAX_CHARS}",
            t.chars().count()
        );
    }

    /// 红环说的是「缺电」，插上线并不改变这个事实 —— 5% 在充电仍是严重档。
    /// 守的是 issue #11：以前充电优先，低电档一并变蓝，最该被看见的信号消失了
    #[test]
    fn 低电档在充电时仍然是严重档() {
        assert_eq!(tier_of(Some(5), true), Tier::Critical);
        assert_eq!(tier_of(Some(15), true), Tier::Warn);
        // 不缺电时充电才是一种状态
        assert_eq!(tier_of(Some(80), true), Tier::Charging);
        assert_eq!(tier_of(Some(80), false), Tier::Normal);
    }

    /// 托盘只有一圈，谁的颜色都不能借：以前是「任意一台在充电就整环变蓝」，
    /// 于是 5% 的鼠标被旁边充电的耳机染成蓝色（issue #11）
    #[test]
    fn 托盘环只跟随电量最低那台() {
        let mouse = s("鼠标", "鼠标", Some(5));
        let mut earphone = s("耳机", "耳机", Some(80));
        earphone.charging = true;
        assert_eq!(tray_ring(&[mouse, earphone]), (Some(5), false));

        // 最低的那台自己在充电，才画蓝环
        let mut mouse = s("鼠标", "鼠标", Some(80));
        mouse.charging = true;
        assert_eq!(tray_ring(&[mouse]), (Some(80), true));

        // 一台都给不出数：灰环，不是「0%」
        assert_eq!(tray_ring(&[offline("GPW", "GPW")]), (None, false));
    }

    /// 滞回是为了防抖，不该把「刚好跨过阈值」也吞掉（issue #9）：
    /// 21% → 20% 只差 1 个点，但 20% 正是默认警告线，压着不动就等于漏报
    #[test]
    fn 跨档时滞回让路() {
        let warn = settings::low_warn();
        assert_eq!(
            smooth(Some(warn.saturating_add(1)), warn),
            warn,
            "跨过警告线必须更新"
        );
        // 同一档内差 1 仍然压着，否则 27% 和 28% 会来回横跳
        assert_eq!(smooth(Some(50), 49), 50);
        // 差 2 个点照常更新
        assert_eq!(smooth(Some(50), 52), 52);
    }
}
