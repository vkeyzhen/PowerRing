//! 托盘上方弹出的卡片。目前有两张：点击托盘的「设备面板」和电量跌破阈值的「低电量提醒」。
//!
//! 刻意不引入 GUI 框架 —— 卡片是一个 WS_POPUP 窗口，内容在 WM_PAINT 里用
//! 几行 TextOutW 画出来。理由同 §8.4 ADR-001：本项目最大的成本是反馈循环速度
//! （AI 看不到屏幕，UI 只能靠用户截图往返），为一个几行文本的面板拖进 winit/wry，
//! 会让每次增量编译从 5 秒变成几十秒。等真需要复杂 UI 时再评估。
//!
//! 设计取舍：
//!   · 不抢焦点（SW_SHOWNA）—— 用户可能在打游戏或打字，抢焦点会打断。
//!     代价是无法用「失焦自动关闭」，所以关闭靠：再点一次托盘、点卡片内部、超时。
//!   · WS_EX_TOOLWINDOW —— 不在任务栏留按钮，也不出现在 Alt+Tab 里。
//!
//! 两张卡片共用一个窗口类和一个 wndproc：wndproc 拿不到闭包上下文，
//! 卡片索引只能靠比对 HWND 反查（只有两张，比存 GWLP_USERDATA 简单）。

use crate::{info, warn};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::ui::{self, create_font, rgb};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateCompatibleDC, CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, Ellipse,
    EndPaint, FillRect, GetStockObject, GetTextExtentPoint32W, InvalidateRect, SelectObject,
    SetBkMode, SetTextColor, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HDC, NULL_PEN, PAINTSTRUCT,
    TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetClientRect, GetCursorPos, IsWindowVisible, RegisterClassW,
    SetWindowPos, ShowWindow, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SW_HIDE,
    SW_SHOWNA, WM_LBUTTONDOWN, WM_PAINT, WM_SETTINGCHANGE, WNDCLASSW, WS_BORDER, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};

/// 卡片槽位。索引就是它们在下面各个静态数组里的下标
const PANEL: usize = 0;
const ALERT: usize = 1;
const N: usize = 2;

/// 卡片里的一行
pub struct Line {
    pub text: String,
    /// 左侧小色块的颜色，用来一眼区分电量档位
    pub swatch: (u8, u8, u8),
}

/// 卡片几何：96 DPI 下的基准尺寸，实际绘制时按系统 DPI 放大。
/// 进程声明了 DPI 感知，系统不再替我们拉伸，不自己放大就只有一小条。
const BASE_WIDTH: f32 = 300.0;
const BASE_ROW_H: f32 = 28.0;
const BASE_PAD: f32 = 12.0;
const BASE_TOP: f32 = 30.0;
/// 没人管它就自己消失，避免一直挂在屏幕上。
/// 提醒卡片短一点：它是主动冒出来的，不该长时间占着屏幕。
///
/// 面板从 10 秒改成 30 秒：日志里看到用户连着七次每 11 秒点一次托盘 ——
/// 面板刚打开就又收了，等于一直在跟它抢。30 秒跟默认轮询周期一致，
/// 用户看一眼数字的工夫够了，也不至于忘在那儿一整天。
const AUTO_HIDE: [Duration; N] = [Duration::from_secs(30), Duration::from_secs(8)];

fn width() -> i32 {
    ui::px(BASE_WIDTH)
}
fn row_h() -> i32 {
    ui::px(BASE_ROW_H)
}
fn pad() -> i32 {
    ui::px(BASE_PAD)
}
fn top() -> i32 {
    ui::px(BASE_TOP)
}

/// 量一段文字在卡片字号下的物理宽度。拿不到设备上下文就返回 0，
/// 调用方会退化成最小宽度 —— 宁可窄了再夹，也不要因为测量失败画出个巨宽的框
fn text_width(hdc: HDC, s: &str) -> i32 {
    let w = wide(s);
    let mut sz = SIZE::default();
    if unsafe { GetTextExtentPoint32W(hdc, &w, &mut sz) }.as_bool() {
        sz.cx
    } else {
        0
    }
}

/// 卡片尺寸：宽度按最长的一行实测，高度按行数。
///
/// 早先是固定 300 宽（96 DPI）。问题在两台设备以下会剩一大片空白，
/// 名字长的设备又可能被 `DrawTextW` 裁掉。实际量一遍再夹在 230..460 之间，
/// 既不会空得难看，也不至于长成一个横条。
fn layout(title: &str, texts: &[String]) -> (i32, i32) {
    // 点直径 10 + 点与文字的间隙 6
    let lead = ui::px(16.0);
    let trail = ui::px(12.0);
    let mut widest = 0;
    unsafe {
        let dc = CreateCompatibleDC(None);
        if !dc.is_invalid() {
            let font = create_font(ui::font_px(9.0));
            let old = SelectObject(dc, font.into());
            widest = text_width(dc, title);
            for t in texts {
                widest = widest.max(text_width(dc, t));
            }
            SelectObject(dc, old);
            let _ = DeleteObject(font.into());
            let _ = DeleteDC(dc);
        }
    }
    let w = (pad() * 2 + lead + widest + trail).clamp(ui::px(230.0), ui::px(460.0));
    (w, top() + pad() + texts.len() as i32 * row_h())
}

// 全局状态：wndproc 必须是 extern "system" 的自由函数，拿不到闭包上下文，
// 所以状态只能放静态。HWND 是裸指针包装、不保证 Sync，用整型存地址。
static HWS: [AtomicIsize; N] = [AtomicIsize::new(0), AtomicIsize::new(0)];
/// 卡片里的一袋行：每行 = (文字, 色块 RGB)。静态数组多处引用，起个名字省得反复写这一长串
type Rows = Vec<(String, (u8, u8, u8))>;
static LINES: [Mutex<Rows>; N] = [Mutex::new(Vec::new()), Mutex::new(Vec::new())];
static TITLES: [Mutex<String>; N] = [Mutex::new(String::new()), Mutex::new(String::new())];
static VISIBLE: [AtomicBool; N] = [AtomicBool::new(false), AtomicBool::new(false)];
static SHOWN_AT: [Mutex<Option<Instant>>; N] = [Mutex::new(None), Mutex::new(None)];
/// 锚点矩形：(x, y, 宽, 高)。托盘图标用它，光标位置也用它（宽高给 0）
type Anchor = (i32, i32, i32, i32);

/// 这张卡片是按哪个锚点放出来的。
///
/// 内容变化后要重新摆一次位置 —— 行数变多、最长那行变长，窗口会往右下角长，
/// 而屏幕边缘不会跟着动。只改尺寸不重算坐标的话，托盘在右下角时
/// 右边一截和底下一行会被切在屏幕外（真的发生过：插上一台名字更长的设备就露馅）。
static ANCHOR: [Mutex<Option<Anchor>>; N] = [Mutex::new(None), Mutex::new(None)];

fn hwnd(idx: usize) -> HWND {
    HWND(HWS[idx].load(Ordering::SeqCst) as *mut core::ffi::c_void)
}

/// 反查这个窗口是第几张卡片。只有两张，直接比指针
fn idx_of(hwnd: HWND) -> usize {
    if hwnd.0 == HWS[PANEL].load(Ordering::SeqCst) as *mut core::ffi::c_void {
        PANEL
    } else {
        ALERT
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// 注册窗口类并创建两张初始隐藏的窗口。
/// 必须在跑消息泵的那个线程调用（窗口和泵必须同线程，否则消息永远到不了）。
pub fn create() -> Result<(), windows::core::Error> {
    let class = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        lpszClassName: w!("PowerRingPopup"),
        ..Default::default()
    };
    unsafe {
        RegisterClassW(&class);
        for slot in &HWS {
            let hw = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                w!("PowerRingPopup"),
                w!("PowerRing"),
                WS_POPUP | WS_BORDER,
                0,
                0,
                width(),
                top() + pad() * 2,
                None,
                None,
                None,
                None,
            )?;
            slot.store(hw.0 as isize, Ordering::SeqCst);
        }
    }
    Ok(())
}

/// 在托盘图标正上方显示设备面板；放不下就翻到下方。
pub fn show(lines: Vec<Line>, icon_x: i32, icon_y: i32, icon_w: i32, icon_h: i32) {
    if let Ok(mut g) = TITLES[PANEL].lock() {
        *g = "PowerRing — 设备电量".to_string();
    }
    place(PANEL, lines, icon_x, icon_y, icon_w, icon_h)
}

/// 不知道托盘图标在哪时用这个：直接以当前光标位置为锚点。
/// 走右键菜单打开面板时就用它 —— 菜单项被点下的那一瞬间，光标就在菜单上、也就在图标旁边。
pub fn show_near_cursor(lines: Vec<Line>) {
    let mut pt = POINT { x: 0, y: 0 };
    let (cx, cy) = match unsafe { GetCursorPos(&mut pt) } {
        Ok(()) => (pt.x, pt.y),
        Err(_) => (0, 0),
    };
    show(lines, cx, cy, 0, 0);
}

/// 低电量提醒卡片。面板已经开着就不弹 —— 那时用户正看着设备列表，
/// 橙/红色块已经把信息给出来了，再盖一张上去纯属添乱。
///
/// anchor 是托盘图标的位置。给不出来时用主屏右下角 —— 刻意**不用光标**：
/// 提醒是程序主动冒出来的，而用户这会儿多半在副屏上干别的事，
/// 跟着光标就会弹到副屏去，托盘却在另一块屏上，等于提醒跟主体脱节。
pub fn alert(lines: Vec<Line>, title: &str, anchor: Option<Anchor>) {
    alert_card(lines, title, anchor, false);
}

/// 设置窗口里那个「试一次」走这条路。
///
/// 它要的就是看见卡片长什么样，所以面板开着也照弹 —— 早退条件对自动提醒是对的，
/// 但把「试一次」也挡掉就变成：用户点了按钮，屏幕上一动不动，
/// 只有日志里一句「面板已开着，跳过提醒」，他只会以为按钮坏了。
pub fn alert_forced(lines: Vec<Line>, title: &str, anchor: Option<Anchor>) {
    alert_card(lines, title, anchor, true);
}

fn alert_card(lines: Vec<Line>, title: &str, anchor: Option<Anchor>, force: bool) {
    if VISIBLE[PANEL].load(Ordering::SeqCst) {
        if !force {
            info!("[alert] 面板已开着，跳过提醒");
            return;
        }
        // 让开位置：两张卡片叠在一起，提醒就被面板盖住了，等于没弹
        hide_card(PANEL);
    }
    if let Ok(mut g) = TITLES[ALERT].lock() {
        *g = title.to_string();
    }
    let (x, y, w, h) = match anchor {
        Some(r) => r,
        None => {
            let (bx, by) = ui::primary_work_bottom_right();
            info!("[alert] 拿不到托盘位置，回退主屏右下角 ({},{})", bx, by);
            (bx, by, 0, 0)
        }
    };
    place(ALERT, lines, x, y, w, h)
}

/// 按锚点算卡片的最终位置（已夹进可见区域）。
///
/// 抽出来是因为有两处要用：`place()` 第一次摆，`refresh_card()` 内容变大后重摆。
/// 以前只有前者算过这道夹取，后者只改尺寸不动坐标 —— 于是面板开着的时候插上
/// 一台名字更长的设备，右边一截和最下面一行就被切在屏幕外了
fn clamp_pos(w: i32, h: i32, icon_x: i32, icon_y: i32, icon_w: i32, icon_h: i32) -> (i32, i32) {
    let (vx, vy, vw, vh) = ui::virtual_screen();
    let min_x = vx + 8;
    let min_y = vy + 8;
    // 托盘通常在屏幕右下角，默认往上弹；上方真的放不下才翻到下方
    let x = icon_x + icon_w / 2 - w / 2;
    let mut y = icon_y - h - 6;
    if y < min_y {
        y = icon_y + icon_h + 6;
    }
    // 夹在可见区域内：托盘贴着边时宁可整个面板往里挪，也不要被裁掉一块。
    // 夹取范围用虚拟桌面而不是主屏 —— 用户是双屏，托盘可能在副屏。
    let max_x = (vx + vw - w - 8).max(min_x);
    let max_y = (vy + vh - h - 8).max(min_y);
    (x.clamp(min_x, max_x), y.clamp(min_y, max_y))
}

fn place(idx: usize, lines: Vec<Line>, icon_x: i32, icon_y: i32, icon_w: i32, icon_h: i32) {
    let title = TITLES[idx].lock().map(|g| g.clone()).unwrap_or_default();
    let texts: Vec<String> = lines.iter().map(|l| l.text.clone()).collect();
    let (w, height) = layout(&title, &texts);
    if let Ok(mut g) = LINES[idx].lock() {
        *g = lines
            .into_iter()
            .map(|l| (l.text, l.swatch))
            .collect::<Vec<_>>();
    }
    // 记下锚点：内容变化后要按同一个锚点重算位置（见 refresh_card）
    if let Ok(mut g) = ANCHOR[idx].lock() {
        *g = Some((icon_x, icon_y, icon_w, icon_h));
    }
    let hw = hwnd(idx);
    let (vx, vy, vw, vh) = ui::virtual_screen();
    let (x, y) = clamp_pos(w, height, icon_x, icon_y, icon_w, icon_h);
    // 位置日志是主要诊断手段 —— AI 看不到屏幕，只能靠数字对
    info!(
        "[{}] 锚点=({},{}) 尺寸 {}x{} → 实际=({},{}) 桌面=({},{},{},{})",
        if idx == PANEL { "popup" } else { "alert" },
        icon_x,
        icon_y,
        w,
        height,
        x,
        y,
        vx,
        vy,
        vw,
        vh
    );
    unsafe {
        if let Err(e) = SetWindowPos(hw, Some(HWND_TOPMOST), x, y, w, height, SWP_NOACTIVATE) {
            warn!("[popup] 摆位置失败，这张卡片这次不显示：{e}");
            return;
        }
        let _ = ShowWindow(hw, SW_SHOWNA);
        // 尺寸变了，强制重绘一次，否则会残留上一帧
        let _ = InvalidateRect(Some(hw), None, true);
        // 真的显示出来了才算「可见」。以前是无条件置位：万一没显示成，
        // is_visible() 报真，下一次点托盘走的是 hide() 分支 —— 用户要点两次才有反应，
        // 低电量提醒也会因为「面板正开着」被静默跳过
        if !IsWindowVisible(hw).as_bool() {
            warn!("[popup] 窗口没能显示出来（ShowWindow 之后仍不可见）");
            return;
        }
    }
    VISIBLE[idx].store(true, Ordering::SeqCst);
    if let Ok(mut g) = SHOWN_AT[idx].lock() {
        *g = Some(Instant::now());
    }
}

pub fn hide() {
    hide_card(PANEL)
}

fn hide_card(idx: usize) {
    if !VISIBLE[idx].load(Ordering::SeqCst) {
        return;
    }
    unsafe {
        let _ = ShowWindow(hwnd(idx), SW_HIDE);
    }
    VISIBLE[idx].store(false, Ordering::SeqCst);
    if let Ok(mut g) = SHOWN_AT[idx].lock() {
        *g = None;
    }
}

pub fn is_visible() -> bool {
    VISIBLE[PANEL].load(Ordering::SeqCst)
}

/// 让两张卡片按当前主题重画一遍。切换深色/浅色时用：
/// 卡片可能正挂在屏幕上，不重绘就会一半深一半浅
pub fn repaint() {
    for idx in 0..N {
        unsafe {
            let _ = InvalidateRect(Some(hwnd(idx)), None, true);
        }
    }
}

/// 卡片开着时来了新读数：换掉内容并重绘。
/// 不重置超时计时 —— 面板只是被动刷新，不该因为它一直有人看着就永不消失。
pub fn refresh(lines: Vec<Line>) {
    refresh_card(PANEL, lines)
}

fn refresh_card(idx: usize, lines: Vec<Line>) {
    if !VISIBLE[idx].load(Ordering::SeqCst) {
        return;
    }
    let title = TITLES[idx].lock().map(|g| g.clone()).unwrap_or_default();
    let texts: Vec<String> = lines.iter().map(|l| l.text.clone()).collect();
    let (w, h) = layout(&title, &texts);
    if let Ok(mut g) = LINES[idx].lock() {
        *g = lines
            .into_iter()
            .map(|l| (l.text, l.swatch))
            .collect::<Vec<_>>();
    }
    // 行数可能变了（插拔设备），窗口尺寸得跟着走：
    // 只重绘不改尺寸的话，少一台会留一片空行，多一台会把最后一行裁掉。
    // 位置也必须按当初那个锚点重算 —— 贴着屏幕右下角的卡片只会越长越往右下长，
    // 光改尺寸不挪位置，多出来的那部分就在屏幕外面
    let pos = ANCHOR[idx]
        .lock()
        .ok()
        .and_then(|g| *g)
        .map(|(ax, ay, aw, ah)| clamp_pos(w, h, ax, ay, aw, ah));
    unsafe {
        let hw = hwnd(idx);
        // 没有锚点（理论上不会）就退回老行为：只改尺寸
        let (x, y, flags) = match pos {
            Some((x, y)) => (x, y, SWP_NOZORDER | SWP_NOACTIVATE),
            None => (0, 0, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE),
        };
        let _ = SetWindowPos(hw, None, x, y, w, h, flags);
        let _ = InvalidateRect(Some(hw), None, true);
    }
}

/// 主循环每轮调用：到点自动收起，省得用户忘了关
pub fn tick() {
    for idx in 0..N {
        let expired = match SHOWN_AT[idx].lock() {
            Ok(g) => g.map(|t| t.elapsed() >= AUTO_HIDE[idx]).unwrap_or(false),
            Err(_) => false,
        };
        if expired {
            hide_card(idx);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let idx = idx_of(hwnd);
            paint(hwnd, idx);
            LRESULT(0)
        }
        // 点卡片内部即关闭：不抢焦点就没法靠失焦关闭，得给个明确出口
        WM_LBUTTONDOWN => {
            hide_card(idx_of(hwnd));
            LRESULT(0)
        }
        // 用户在系统设置里换了浅色/深色：重算配色并重绘。
        // 这张卡片可能正开着，不重绘的话会一半深一半浅
        WM_SETTINGCHANGE => {
            crate::theme::reload();
            repaint();
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn paint(hwnd: HWND, idx: usize) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    if hdc.is_invalid() {
        return;
    }

    let mut rc = RECT::default();
    if !GetClientRect(hwnd, &mut rc).is_ok() {
        let _ = EndPaint(hwnd, &ps);
        return;
    }

    // 背景取自当前主题：这张卡片就贴在托盘旁边，托盘所在的任务栏可能是深色的
    let c = crate::theme::colors();
    let bg = CreateSolidBrush(rgb(c.card_bg.0, c.card_bg.1, c.card_bg.2));
    let _ = FillRect(hdc, &rc, bg);
    let _ = DeleteObject(bg.into());

    let _ = SetBkMode(hdc, TRANSPARENT);
    // 9 磅是 Windows 界面字号（96 DPI 下 12px）。按 DPI 换算成物理像素，
    // 4K + 150% 上就是 18px —— 和别的应用里的正文一样大。
    let font = create_font(ui::font_px(9.0));
    let old = SelectObject(hdc, font.into());

    // 文字一律用 DrawTextW 的 DT_VCENTER 定位，而不是 TextOutW 的「文字左上角」：
    // 后者要自己猜基线偏移，换个字号或换台 DPI 不同的机器就会偏高偏低。
    // DT_VCENTER 让 GDI 按字体度量自己算，跨字号稳定。
    // 文字左边距 = 点直径 10 + 间隙 6。留 20 的话间隙等于一个点直径，
    // 收缩后点才像是「贴在文字左边」而不是各自飘着
    let text_left = pad() + ui::px(16.0);

    // 标题
    let _ = SetTextColor(hdc, rgb(c.card_title.0, c.card_title.1, c.card_title.2));
    let mut title = match TITLES[idx].lock() {
        Ok(g) => wide(&g),
        Err(_) => wide(""),
    };
    let mut trc = RECT {
        left: pad(),
        top: 0,
        right: rc.right - pad(),
        bottom: top(),
    };
    // DT_NOPREFIX 不能省：DrawTextW 默认把 & 当快捷键前缀吃掉，
    // 设备名里真有 & 时会少显示一个字。TextOutW 没有这个问题，
    // 换成 DrawTextW 就得自己把这个行为关掉
    let flags = DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX;
    let _ = DrawTextW(hdc, &mut title, &mut trc, flags);

    if let Ok(lines) = LINES[idx].lock() {
        for (i, (text, (r, g, b))) in lines.iter().enumerate() {
            let y = top() + i as i32 * row_h();

            // 左侧状态点：颜色对应档位，比读数字快。
            // 画圆而不是方块 —— 实心方块看着像没画完的占位符，圆点才是「状态灯」的语汇。
            // 直径取 10（96 DPI）跟着 DPI 缩放；必须把画笔换成 NULL_PEN，
            // 否则 DC 默认的 1px 黑边会给圆点描一圈深色轮廓
            let d = ui::px(10.0);
            let cx = pad() + d / 2;
            let cy = y + row_h() / 2;
            let dot = CreateSolidBrush(rgb(*r, *g, *b));
            let old_brush = SelectObject(hdc, dot.into());
            let old_pen = SelectObject(hdc, GetStockObject(NULL_PEN));
            let _ = Ellipse(hdc, cx - d / 2, cy - d / 2, cx + d / 2, cy + d / 2);
            SelectObject(hdc, old_pen);
            SelectObject(hdc, old_brush);
            let _ = DeleteObject(dot.into());

            let _ = SetTextColor(hdc, rgb(c.card_text.0, c.card_text.1, c.card_text.2));
            let mut w = wide(text);
            let mut lrc = RECT {
                left: text_left,
                top: y,
                right: rc.right - pad(),
                bottom: y + row_h(),
            };
            let _ = DrawTextW(hdc, &mut w, &mut lrc, flags);
        }
    }

    SelectObject(hdc, old);
    let _ = DeleteObject(font.into());
    let _ = EndPaint(hwnd, &ps);
}
