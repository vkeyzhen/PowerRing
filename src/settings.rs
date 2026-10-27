//! F11 设置窗口：刷新间隔 + 两档低电量阈值 + 提醒开关 + 开机自启，保存后立即生效
//!
//! 控件用的是 Win32 标准子控件（STATIC / EDIT / BUTTON），不是自绘。
//! 理由和托盘面板一样（§8.4 ADR-001）：不引 GUI 框架，编译速度优先。
//! 标准控件的代价是外观朴素（系统经典样式），换来的是零依赖、零自绘代码。
//!
//! M1 只做「改了就能立刻用」的三项；M2 补上开机自启（要动注册表）。
//! 自启这一项的显示状态每次都重新问注册表（autostart::is_enabled），
//! 不认 config —— 用户可能在任务管理器里禁掉过它，那份开关 config 看不到。

use crate::config::Config;
use crate::info;
use crate::theme::{self, Colors};
use crate::ui::{create_font, font_px as ui_font_px, px, rgb};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU8, Ordering};
use std::sync::Mutex;

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, FillRect, FrameRect, InvalidateRect,
    LineTo, MoveToEx, SelectObject, SetBkColor, SetBkMode, SetTextColor, DRAW_TEXT_FORMAT, HBRUSH,
    HDC, HGDIOBJ, PEN_STYLE, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, GetClientRect, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, IsChild, IsDialogMessageW, IsWindowVisible,
    RegisterClassW, SendMessageW, SetWindowPos, SetWindowTextW, ShowWindow, HMENU, HWND_TOP, MSG,
    SW_HIDE, SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_CTLCOLOREDIT,
    WM_CTLCOLORSTATIC, WM_DRAWITEM, WM_ERASEBKGND, WM_KEYDOWN, WM_SETFONT, WM_SETTINGCHANGE,
    WNDCLASSW, WS_BORDER, WS_CAPTION, WS_CHILD, WS_POPUP, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};

// 按钮和复选框都改成自绘（BS_OWNERDRAW）。
// 原因：标准按钮在启用视觉样式时根本不理 WM_CTLCOLORBTN 返回的颜色 ——
// 深色窗口上摆一排浅灰按钮，比不做深色还难看。自绘是唯一能真正控住颜色的路子，
// 顺带浅色下也比系统经典控件更贴合弹窗的观感。
// BS_* 常量在 windows crate 里是 i32，跟着它走
// 按钮样式的**低 4 位是互斥的类型枚举**：BS_PUSHBUTTON=0 / BS_CHECKBOX=2 /
// BS_AUTOCHECKBOX=3 / BS_OWNERDRAW=0xB ……它们不是可以叠加的标志位。
// 写过 `BS_AUTOCHECKBOX | BS_OWNERDRAW`，结果 3|0xB 还是 0xB —— 控件只是个自绘按钮，
// 复选框语义被吃掉，BM_GETCHECK / BM_SETCHECK 全部失效（表现就是「怎么点都勾不上」）。
// 自绘复选框只能自己维护勾选状态，见 NOTIFY_CHECK / AUTO_CHECK。
const BS_OWNERDRAW: i32 = 0x000B;
// 自绘状态位：ODS_SELECTED = 按钮正被按下。
// 注意 ODS_CHECKED 只对菜单项有效，别拿它判断按钮的勾选 —— 那是另一个坑
const ODS_SELECTED: u32 = 0x0001;
// DrawTextW 的排版方式。DT_NOPREFIX 不能省：否则文字里的 & 会被当成快捷键下划线
const DT_CENTER: u32 = 0x0001;
const DT_VCENTER: u32 = 0x0004;
const DT_SINGLELINE: u32 = 0x0020;
const DT_NOPREFIX: u32 = 0x0800;

/// WM_DRAWITEM 带过来的绘制描述。
/// windows crate 把它放在 Win32_UI_Controls 里，为这一个结构去开那个大 feature 不值；
/// 它是 Win32 公开的 ABI 结构，自己按同样布局声明一份是安全的
// 名字沿用 Win32 的原名，方便对着官方文档查字段；缩写全大写是 Windows 的写法，
// 不是本项目的命名风格
#[allow(clippy::upper_case_acronyms)]
#[repr(C)]
struct DRAWITEMSTRUCT {
    _ctl_type: u32,
    ctl_id: u32,
    _item_id: u32,
    _item_action: u32,
    item_state: u32,
    hwnd_item: HWND,
    hdc: HDC,
    rc_item: RECT,
    _item_data: usize,
}

// 控件 ID：子窗口 ID 是塞在 CreateWindowExW 的 hMenu 位里的，必须自己编号
const ID_INTERVAL: isize = 1001;
const ID_WARN: isize = 1002;
const ID_CRITICAL: isize = 1003;
const ID_SAVE: isize = 1010;
const ID_CANCEL: isize = 1011;
const ID_STATUS: isize = 1020;
const ID_NOTIFY: isize = 1030;
const ID_TEST: isize = 1040;
const ID_AUTO: isize = 1050;
const ID_THEME: isize = 1060;
const ID_SYNC_URL: isize = 1070;
/// 对话框管理器（`IsDialogMessageW`）转发过来的两个固定 ID：Enter → IDOK、
/// Tab 到带默认按钮时也一样。它们和上面那套自定的控件 ID 不是一回事，得单独认
const ID_DLG_OK: isize = 1;
const ID_DLG_CANCEL: isize = 2;
/// Esc 的虚拟键码。同样的道理：为一两个数去开 `Win32_UI_Input_KeyboardAndMouse`
/// 那套 feature 不值（本项目一贯的做法，见 ui.rs 里那几个 SM_* 常量）
const VK_ESCAPE: u32 = 0x1B;

// 96 DPI 下的**客户区**布局，实际按 DPI 放大。
// 注意这是客户区（放控件的那块），不是窗口外框 —— 外框还要再加标题栏和边框，
// 由 outer_size() 负责换算，别直接拿它去 CreateWindowExW。
const BASE_W: f32 = 340.0;
const BASE_H: f32 = 370.0;
const MARGIN: f32 = 16.0;
const ROW_TOP: f32 = 20.0;
const ROW_PITCH: f32 = 36.0;
const LABEL_W: f32 = 178.0;
const EDIT_X: f32 = 200.0;
const EDIT_W: f32 = 124.0;
const EDIT_H: f32 = 24.0;
/// 第 4 行是「设备声明更新地址」：标签单独占一行，输入框整行宽。
/// 不做成「标签在左、输入框在右」是因为 URL 很长，
/// 124px 那点宽度连一个地址的一半都显示不下
const URL_LABEL_Y: f32 = 128.0;
const URL_EDIT_Y: f32 = 150.0;
/// 复选框紧跟地址那一行
const CHECK_Y: f32 = 186.0;
const CHECK_H: f32 = 24.0;
/// 复选框给它右边的「试一次」按钮让出位置
const CHECK_W: f32 = BASE_W - MARGIN * 2.0 - 68.0;
const TEST_W: f32 = 60.0;
const TEST_X: f32 = MARGIN + CHECK_W + 8.0;
/// 开机自启紧跟提醒那一行
const CHECK2_Y: f32 = CHECK_Y + 30.0;
/// 主题切换按钮占整行，挨着自启
const THEME_Y: f32 = CHECK2_Y + 30.0;
const STATUS_Y: f32 = 288.0;
const BTN_Y: f32 = 320.0;
const BTN_W: f32 = 84.0;
const BTN_H: f32 = 30.0;
const BTN_GAP: f32 = 16.0;

static HW: AtomicIsize = AtomicIsize::new(0);
static HW_INTERVAL: AtomicIsize = AtomicIsize::new(0);
static HW_WARN: AtomicIsize = AtomicIsize::new(0);
static HW_CRITICAL: AtomicIsize = AtomicIsize::new(0);
static HW_STATUS: AtomicIsize = AtomicIsize::new(0);
static HW_NOTIFY: AtomicIsize = AtomicIsize::new(0);
static HW_AUTO: AtomicIsize = AtomicIsize::new(0);
/// 两个复选框的勾选状态。**我们自己存**，不问控件 ——
/// 自绘之后它们不再是复选框（见 BS_OWNERDRAW 那条注释），BM_GETCHECK 是读不到东西的。
/// 打开窗口时从配置/注册表灌进来，点击时翻转，保存时读走。
static NOTIFY_CHECK: AtomicBool = AtomicBool::new(true);
static AUTO_CHECK: AtomicBool = AtomicBool::new(false);
static HW_THEME: AtomicIsize = AtomicIsize::new(0);
/// 打开窗口那一刻，配置里记着的主题。
///
/// 主题是唯一「点了就立刻落到运行时、但不经过保存」的项。点两下看了看效果再点取消，
/// 界面已经变了而配置没变，重启后又自己变回去 —— 用户会以为「设置没存住」。
/// 所以关窗时按这个值退回去。保存路径不走关窗逻辑，正常保存不受影响
static THEME_ON_OPEN: Mutex<Option<theme::Mode>> = Mutex::new(None);
static HW_SYNC_URL: AtomicIsize = AtomicIsize::new(0);
/// 「检查设备声明更新…」用的地址。留空表示用程序内置的那两个。
/// 用锁而不是 Atomic 是因为它是个字符串；用 String 而不是 &'static str
/// 是因为它每次保存都可能变
static SYNC_URL: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
// WM_CTLCOLOR 要返回一个画刷，而系统在返回之后还要用它 —— 不能每次现造现删。
// 所以缓存一个，颜色变了才重建（切换主题时 key 变化，旧的被删掉）
static BRUSH_WIN: AtomicIsize = AtomicIsize::new(0);
static BRUSH_WIN_KEY: AtomicU32 = AtomicU32::new(0);
static BRUSH_EDIT: AtomicIsize = AtomicIsize::new(0);
static BRUSH_EDIT_KEY: AtomicU32 = AtomicU32::new(0);
/// 当前生效的刷新间隔。轮询线程每轮直接读它，改完设置不用重启程序
static INTERVAL: AtomicU32 = AtomicU32::new(60);
/// 当前生效的两档低电量阈值。电量环配色和提醒判断都读它，
/// 所以改完设置颜色立刻跟着变，不用重启
static LOW_WARN: AtomicU8 = AtomicU8::new(20);
static LOW_CRITICAL: AtomicU8 = AtomicU8::new(10);
/// 低电量提醒开关。关掉只是不弹卡片，阈值仍驱动电量环配色
static NOTIFY: AtomicBool = AtomicBool::new(true);
/// 设置窗口里点了「试一次」
static TEST_REQUESTED: AtomicBool = AtomicBool::new(false);

/// 声明更新的来源地址。留空时 sync 模块会用内置的两个源
pub fn sync_url() -> String {
    SYNC_URL.lock().map(|s| s.clone()).unwrap_or_default()
}

pub fn set_sync_url(v: String) {
    if let Ok(mut s) = SYNC_URL.lock() {
        *s = v;
    }
}

fn hwnd() -> HWND {
    HWND(HW.load(Ordering::SeqCst) as *mut core::ffi::c_void)
}
fn ctl(a: &AtomicIsize) -> HWND {
    HWND(a.load(Ordering::SeqCst) as *mut core::ffi::c_void)
}

pub fn interval() -> u32 {
    INTERVAL.load(Ordering::SeqCst)
}

pub fn set_interval(v: u32) {
    INTERVAL.store(v, Ordering::SeqCst);
}

pub fn low_warn() -> u8 {
    LOW_WARN.load(Ordering::SeqCst)
}

pub fn low_critical() -> u8 {
    LOW_CRITICAL.load(Ordering::SeqCst)
}

pub fn set_low(warn: u8, critical: u8) {
    LOW_WARN.store(warn, Ordering::SeqCst);
    LOW_CRITICAL.store(critical, Ordering::SeqCst);
}

pub fn notify_enabled() -> bool {
    NOTIFY.load(Ordering::SeqCst)
}

pub fn set_notify(on: bool) {
    NOTIFY.store(on, Ordering::SeqCst);
}

/// 主循环每轮问一次：设置窗口里的「试一次」被点过吗。
/// 取走即清空，连点只会弹一张
pub fn take_test_request() -> bool {
    TEST_REQUESTED.swap(false, Ordering::SeqCst)
}

/// 普通窗口样式，**不带 WS_EX_TOOLWINDOW**。
///
/// 工具窗口样式是给浮动工具栏/调色板用的：它的标题栏是矮的，
/// 但系统给它预留的非客户区仍是标准标题栏的高度（144 DPI 下 45px），
/// 矮标题栏摆在 45px 里，右上角 X 的上方就空出一条。
/// 设置窗口是有标题的对话框，用标准样式才对 —— 代价是任务栏会多一个按钮，
/// 换来的是窗口被别的窗口盖住时还能在任务栏找回来。
fn window_style() -> WINDOW_STYLE {
    WS_POPUP | WS_CAPTION | WS_SYSMENU
}
/// 客户区尺寸 → 窗口外框尺寸。中间差的是标题栏和边框，
/// 这一步交给 AdjustWindowRectEx 算，别自己加常数：
/// 标题栏高度随 DPI 变，硬编码的常数换了台显示器就错。
fn outer_size() -> (i32, i32) {
    let mut r = RECT {
        left: 0,
        top: 0,
        right: px(BASE_W),
        bottom: px(BASE_H),
    };
    unsafe {
        if AdjustWindowRectEx(&mut r, window_style(), false, WINDOW_EX_STYLE(0)).is_err() {
            // 算不出来就退回加点余量，总比把按钮切掉强
            return (px(BASE_W) + 8, px(BASE_H) + 40);
        }
    }
    (r.right - r.left, r.bottom - r.top)
}

/// 把客户区撑到设计尺寸。
///
/// AdjustWindowRectEx 用的是**系统 DPI** 下的非客户区尺寸，而我们的窗口是
/// Per-Monitor 感知的：它一旦落在 4K + 150% 那块屏上，标题栏会比系统 DPI 算出来的
/// 高出一截，客户区就被啃掉 —— 表现就是底部的按钮被切掉一截。
/// 这里不猜，直接实测客户区，差多少补多少。
fn fit_client() {
    unsafe {
        let mut c = RECT::default();
        if GetClientRect(hwnd(), &mut c).is_err() {
            return;
        }
        let need_w = px(BASE_W);
        let need_h = px(BASE_H);
        let dw = (need_w - (c.right - c.left)).max(0);
        let dh = (need_h - (c.bottom - c.top)).max(0);
        info!(
            "[settings] 客户区实测 {}x{}，需要 {}x{}，补 {}x{}",
            c.right - c.left,
            c.bottom - c.top,
            need_w,
            need_h,
            dw,
            dh
        );
        if dw == 0 && dh == 0 {
            return;
        }
        let mut r = RECT::default();
        if GetWindowRect(hwnd(), &mut r).is_err() {
            return;
        }
        let _ = SetWindowPos(
            hwnd(),
            Some(HWND_TOP),
            r.left,
            r.top,
            (r.right - r.left) + dw,
            (r.bottom - r.top) + dh,
            Default::default(),
        );
    }
}

/// 转成 UTF-16 并补 NUL：Win32 的字符串参数都以 NUL 结尾，不补会读到后面的内存
fn wide_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 注册窗口类、创建窗口和全部控件。必须和消息泵同线程。
pub fn create() -> Result<(), windows::core::Error> {
    // 背景刷子**不在这里给**：窗口类上的刷子是死的，切主题时换不掉。
    // 改由 WM_ERASEBKGND 每次按当前主题填背景。
    let class = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        lpszClassName: w!("PowerRingSettings"),
        ..Default::default()
    };
    unsafe {
        RegisterClassW(&class);

        let (ow, oh) = outer_size();
        let wnd = CreateWindowExW(
            WINDOW_EX_STYLE(0), // 不用 WS_EX_TOOLWINDOW：矮标题栏会留空档，见 window_style 注释
            w!("PowerRingSettings"),
            w!("PowerRing 设置"),
            window_style(),
            0,
            0,
            ow,
            oh,
            None,
            None,
            None,
            None,
        )?;
        HW.store(wnd.0 as isize, Ordering::SeqCst);

        // 9 磅 = Windows 标准界面字号
        let font = create_font(ui_font_px(9.0));
        // 字号和缩放写进日志：换台显示器说「字太小」时，看这一行就能知道算成了多少
        info!(
            "[settings] 缩放 {:.2}x，9 磅 = {}px，客户区 {}x{}",
            crate::ui::scale(),
            ui_font_px(9.0),
            px(BASE_W),
            px(BASE_H)
        );
        let rows = [
            (ID_INTERVAL, "刷新间隔（秒）"),
            (ID_WARN, "低电量警告阈值（%）"),
            (ID_CRITICAL, "低电量严重阈值（%）"),
        ];
        for (i, (id, label)) in rows.iter().enumerate() {
            let y = px(ROW_TOP) + i as i32 * px(ROW_PITCH);
            let lb = make(
                w!("STATIC"),
                label,
                WS_CHILD | WS_VISIBLE,
                px(MARGIN),
                y + px(4.0),
                px(LABEL_W),
                px(20.0),
                wnd,
                0,
            );
            let ed = make(
                w!("EDIT"),
                "",
                WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP,
                px(EDIT_X),
                y,
                px(EDIT_W),
                px(EDIT_H),
                wnd,
                *id,
            );
            set_font(lb, font);
            set_font(ed, font);
            match *id {
                ID_INTERVAL => HW_INTERVAL.store(ed.0 as isize, Ordering::SeqCst),
                ID_WARN => HW_WARN.store(ed.0 as isize, Ordering::SeqCst),
                _ => HW_CRITICAL.store(ed.0 as isize, Ordering::SeqCst),
            }
        }

        // BS_AUTOCHECKBOX：勾选状态由控件自己切换，我们只负责读
        let notify = make(
            w!("BUTTON"),
            "低电量时提醒我",
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
            px(MARGIN),
            px(CHECK_Y),
            px(CHECK_W),
            px(CHECK_H),
            wnd,
            ID_NOTIFY,
        );
        set_font(notify, font);
        HW_NOTIFY.store(notify.0 as isize, Ordering::SeqCst);

        // 提醒是「跌破阈值时才发生」的事件，鼠标不会说低就低，
        // 光靠设置开关用户根本没法验证它长什么样、弹在哪。
        // 给个手动入口，比让他等鼠标真的放电实在
        let test = make(
            w!("BUTTON"),
            "试一次",
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
            px(TEST_X),
            px(CHECK_Y),
            px(TEST_W),
            px(CHECK_H),
            wnd,
            ID_TEST,
        );
        set_font(test, font);

        // 开机自启整行占满，它右边没有配套按钮
        let auto = make(
            w!("BUTTON"),
            "开机时自动启动",
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
            px(MARGIN),
            px(CHECK2_Y),
            px(BASE_W - MARGIN * 2.0),
            px(CHECK_H),
            wnd,
            ID_AUTO,
        );
        set_font(auto, font);
        HW_AUTO.store(auto.0 as isize, Ordering::SeqCst);

        // 主题三态（跟随系统/浅色/深色）用一个按钮循环切换，而不是下拉框：
        // 标准 COMBOBOX 在深色下要自绘整个列表，代价远超它带来的价值
        let theme_btn = make(
            w!("BUTTON"),
            &theme::current_mode().label(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
            px(MARGIN),
            px(THEME_Y),
            px(BASE_W - MARGIN * 2.0),
            px(BTN_H),
            wnd,
            ID_THEME,
        );
        set_font(theme_btn, font);
        HW_THEME.store(theme_btn.0 as isize, Ordering::SeqCst);

        // 「设备声明更新地址」：默认留空（用内置的两个源），
        // 只有连不上 GitHub 的人才需要填。把它摆在设置窗口里而不是藏得更深，
        // 是因为要填它的人恰恰是那批最着急的 —— 让他们翻三层菜单是折磨
        let url_label = make(
            w!("STATIC"),
            "设备声明更新地址（留空则用默认）",
            WS_CHILD | WS_VISIBLE,
            px(MARGIN),
            px(URL_LABEL_Y),
            px(BASE_W - MARGIN * 2.0),
            px(20.0),
            wnd,
            0,
        );
        let url_edit = make(
            w!("EDIT"),
            "",
            WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP,
            px(MARGIN),
            px(URL_EDIT_Y),
            px(BASE_W - MARGIN * 2.0),
            px(EDIT_H),
            wnd,
            ID_SYNC_URL,
        );
        set_font(url_label, font);
        set_font(url_edit, font);
        HW_SYNC_URL.store(url_edit.0 as isize, Ordering::SeqCst);

        let status = make(
            w!("STATIC"),
            "",
            WS_CHILD | WS_VISIBLE,
            px(MARGIN),
            px(STATUS_Y),
            px(BASE_W - MARGIN * 2.0),
            px(20.0),
            wnd,
            ID_STATUS,
        );
        set_font(status, font);
        HW_STATUS.store(status.0 as isize, Ordering::SeqCst);

        // 两个按钮右对齐到边距，取消在右
        let bx = px(BASE_W - MARGIN - BTN_W - BTN_GAP - BTN_W);
        let save = make(
            w!("BUTTON"),
            "保存",
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
            bx,
            px(BTN_Y),
            px(BTN_W),
            px(BTN_H),
            wnd,
            ID_SAVE,
        );
        let cancel = make(
            w!("BUTTON"),
            "取消",
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
            bx + px(BTN_W + BTN_GAP),
            px(BTN_Y),
            px(BTN_W),
            px(BTN_H),
            wnd,
            ID_CANCEL,
        );
        set_font(save, font);
        set_font(cancel, font);
    }
    Ok(())
}

/// 打开窗口并填入当前配置
pub fn open() {
    let cfg = Config::load();
    // 记下此刻**配置里**的主题（不是运行时那个），关窗时用它回滚
    if let Ok(mut g) = THEME_ON_OPEN.lock() {
        *g = Some(theme::Mode::from_str(&cfg.theme));
    }
    unsafe {
        set_text(ctl(&HW_INTERVAL), &cfg.poll_interval_secs.to_string());
        set_text(ctl(&HW_WARN), &cfg.low_warn.to_string());
        set_text(ctl(&HW_CRITICAL), &cfg.low_critical.to_string());
        set_text(ctl(&HW_STATUS), "");
        set_text(ctl(&HW_SYNC_URL), &cfg.remote_devices_url);
        NOTIFY_CHECK.store(cfg.notify_enabled, Ordering::SeqCst);

        // 自启这一格的勾选状态问注册表，不问 config —— 用户可能在设置之外改过它
        // （任务管理器的「启动」页、或者清理软件删过启动项），config 不知道那些事
        let auto = crate::autostart::is_enabled();
        AUTO_CHECK.store(auto, Ordering::SeqCst);
        // 这两格的状态现在是我们自己存的，打出来便于核对「显示的勾」和「实际值」是否一致
        info!(
            "[settings] 打开：提醒={}（config）自启={}（注册表）",
            cfg.notify_enabled, auto
        );
        // 主题按钮的标签里写着「当前深色/浅色」，每次打开都要按此刻的状态重算
        set_text(ctl(&HW_THEME), &theme::current_mode().label());
        if cfg.autostart != auto {
            info!(
                "[settings] 自启意图（config）={} 但注册表实际={}，按注册表显示",
                cfg.autostart, auto
            );
        }

        // 居中于虚拟桌面，双屏时才不会跑到某一块的角落
        let (vx, vy, vw, vh) = crate::ui::virtual_screen();
        let (w, h) = outer_size();
        let x = vx + (vw - w) / 2;
        let y = vy + (vh - h) / 2;
        let _ = SetWindowPos(hwnd(), Some(HWND_TOP), x, y, w, h, Default::default());
        // 标题栏的颜色归 DWM 管，不归我们画 —— 深色模式下不单独说一声，
        // 窗口顶上会留一条白标题栏，比不做深色还扎眼
        apply_titlebar_theme();
        let _ = ShowWindow(hwnd(), SW_SHOW);
        // 先显示再校准：客户区只有窗口真实存在后才量得准
        fit_client();
        info!(
            "[settings] 打开设置窗口，位置=({},{}) 外框={}x{}",
            x, y, w, h
        );
    }
}

fn hide() {
    // 主题是唯一「点了就落到运行时、但没保存」的项：点两下看了看再点取消，
    // 界面样式已经变了而配置没变，重启后又自己变回去 —— 用户会以为没存住。
    // 关窗时退回打开那一刻配置里的值。保存路径不经过这里，正常保存不受影响
    if let Some(was) = THEME_ON_OPEN.lock().ok().and_then(|g| *g) {
        if theme::current_mode() != was {
            theme::set_mode(was);
            // 卡片可能正挂在屏幕上，不重绘就会一半深一半浅
            crate::popup::repaint();
        }
    }
    unsafe {
        let _ = ShowWindow(hwnd(), SW_HIDE);
    }
}

/// 在消息泵里替设置窗口补上「对话框的肌肉记忆」：Esc 关闭、Tab 换焦点、Enter 保存。
///
/// 这是一张自建的顶层窗口，不是对话框，而里面的控件全带着 `WS_TABSTOP`。没有对话框
/// 管理器的话，Tab 不会移动焦点、Enter 什么都不触发、Esc 按了没反应 —— 一组控件
/// 只能用鼠标点，习惯用键盘的人连「保存」都够不着。
///
/// Esc 由这里自己判，不交给 `IsDialogMessageW`：那个函数只在窗口上真有 Cancel 按钮时
/// 才把 Esc 翻成 `IDCANCEL`，而我们的按钮 ID 是自己定的，对不上。
/// 而且只在焦点落在这张窗口（或它的子控件）上时才吃，不该吞掉别处的 Esc。
pub fn handle_dialog_keys(msg: &MSG) -> bool {
    let h = hwnd();
    if h.0.is_null() || !unsafe { IsWindowVisible(h).as_bool() } {
        return false;
    }
    if msg.message == WM_KEYDOWN
        && msg.wParam.0 as u32 == VK_ESCAPE
        && (msg.hwnd == h || unsafe { IsChild(h, msg.hwnd) }.as_bool())
    {
        hide();
        return true;
    }
    // 剩下的交给系统的对话框管理器：Tab / Shift+Tab、Enter 转成 IDOK、
    // 方向键在同类控件间移动。它只处理自己认识的消息，其余的返回 FALSE 由我们继续派发
    unsafe { IsDialogMessageW(h, msg).as_bool() }
}

/// 保存：三样都校验，不合法就地提示，绝不让脏值写进配置
fn on_save() {
    let interval = text_of(ctl(&HW_INTERVAL)).trim().parse::<u32>().ok();
    let warn = text_of(ctl(&HW_WARN)).trim().parse::<u8>().ok();
    let critical = text_of(ctl(&HW_CRITICAL)).trim().parse::<u8>().ok();

    let (interval, warn, critical) = match (interval, warn, critical) {
        (Some(i), Some(w), Some(c)) => (i, w, c),
        _ => {
            set_status("三个值都必须是整数");
            return;
        }
    };

    if !(5..=3600).contains(&interval) {
        set_status("刷新间隔要在 5 ~ 3600 秒之间");
        return;
    }
    if warn == 0 || warn > 100 || critical == 0 || critical >= warn {
        set_status("严重阈值要低于警告阈值，且都在 1 ~ 100 之间");
        return;
    }

    // 更新地址留空是合法的（那就用内置的两个源），填了就得以 http(s):// 开头。
    // 在这里挡一道，而不是等人点了「检查更新」才发现地址写错了 ——
    // 那要白等好几轮超时才等得到一句报错
    let url = text_of(ctl(&HW_SYNC_URL)).trim().to_string();
    if !url.is_empty() && !url.starts_with("https://") && !url.starts_with("http://") {
        set_status("更新地址要以 https:// 开头；不需要就留空");
        return;
    }

    let mut cfg = Config::load();
    // 两格的勾选状态读我们自己那份（控件已经不认 BM_GETCHECK 了）
    let notify = NOTIFY_CHECK.load(Ordering::SeqCst);
    // 打开窗口时只记了一次注册表，这里重新问一遍：
    // 用户在窗口开着的那几十秒里完全可能从任务管理器里改了启动项
    let auto = AUTO_CHECK.load(Ordering::SeqCst);
    let auto_was = crate::autostart::is_enabled();

    cfg.poll_interval_secs = interval;
    cfg.low_warn = warn;
    cfg.low_critical = critical;
    cfg.notify_enabled = notify;
    cfg.autostart = auto;
    // 主题是点了就生效的，保存时把此刻的模式记下来就行
    cfg.theme = theme::current_mode().as_str().to_string();
    cfg.remote_devices_url = url.clone();
    // 存不下是要说出来的。磁盘满、%APPDATA% 被策略锁成只读时，写盘会失败，
    // 而运行中的值仍然按新的生效 —— 若照样显示「已保存」，用户要到下次重启
    // 才发现白改了，还会怀疑是别的原因。所以把失败留到提示里说
    let save_err = cfg.save().err();
    set_interval(interval);
    set_low(warn, critical);
    set_notify(notify);
    set_sync_url(url.clone());

    // 这句提示有实际用处：勾回开关的当下不会补弹一张旧提醒（那次跨档早就过去了），
    // 不说清楚的话，用户点了保存却发现啥也没弹，只会以为开关没生效
    let mut tip: String = if notify {
        "已保存；电量再次跌破阈值的那一刻才会提醒".into()
    } else {
        "已保存；提醒已关闭，环色仍按阈值变化".into()
    };

    // 只在勾选状态真的变了才动注册表。没变还写一遍的话，
    // 会把「用户自己在外面加的、指向别的路径的启动项」覆盖掉 —— 那是他的设置，
    // 不该被我们这边的保存按钮抹掉
    if auto != auto_was {
        match crate::autostart::set(auto) {
            Ok(()) => {
                tip = if auto {
                    "已保存；已加入开机启动，下次登录时自动运行".into()
                } else {
                    "已保存；已从开机启动里移除".into()
                };
            }
            // 写失败（公司电脑的常见情况：注册表被策略挡住）值得盖掉默认提示 ——
            // 用户点的是「勾上」，界面却什么都不说，他只会以为已经生效
            Err(e) => {
                tip = format!("开机自启{}失败：{}", if auto { "开启" } else { "关闭" }, e);
            }
        }
    }
    // 写盘失败的提示要盖过上面所有文案。运行中的值已经按新的生效了，
    // 只有磁盘上还是旧的 —— 不说清楚，用户重启后才莫名其妙地回到旧设置
    if let Some(e) = &save_err {
        tip = format!("已生效，但没能写入配置文件（{e}）—— 重启后会恢复成原来的设置");
    }
    set_status(&tip);
    info!(
        "[settings] 保存配置：间隔={}s 警告={}% 严重={}% 提醒={} 自启={}（原 {}）主题={} 更新地址={:?}",
        interval, warn, critical, notify, auto, auto_was, cfg.theme, url
    );
}

/// 让标题栏跟随深色/浅色。
///
/// 这条属性（DWMWA_USE_IMMERSIVE_DRAW_MODE 那一族里的第 20 号）在 Win10 上属于
/// 「能用但没写进文档」的那类：设置了没效果也不会报错，最坏的结果就是标题栏不变色。
/// 所以它失败也无所谓，返回值直接丢掉。
unsafe fn apply_titlebar_theme() {
    use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
    let v: i32 = if theme::is_dark() { 1 } else { 0 };
    let _ = DwmSetWindowAttribute(
        hwnd(),
        DWMWA_USE_IMMERSIVE_DARK_MODE,
        &v as *const i32 as *const core::ffi::c_void,
        std::mem::size_of::<i32>() as u32,
    );
}

/// 取（并缓存）一个纯色画刷。
///
/// WM_CTLCOLOR 和自绘都要把画刷交回给系统，系统在返回之后还会用它 ——
/// 现造现删会让控件画不出来（或者更糟：用到已释放的对象）。
/// 所以缓存住，只在颜色真的变了的时候重建，把旧的删掉。
fn brush(cell: &AtomicIsize, key: &AtomicU32, c: (u8, u8, u8)) -> HBRUSH {
    let k = ((c.0 as u32) << 16) | ((c.1 as u32) << 8) | c.2 as u32;
    let cur = cell.load(Ordering::SeqCst);
    if cur != 0 && key.load(Ordering::SeqCst) == k {
        return HBRUSH(cur as *mut core::ffi::c_void);
    }
    let hb = unsafe { CreateSolidBrush(rgb(c.0, c.1, c.2)) };
    let old = cell.swap(hb.0 as isize, Ordering::SeqCst);
    if old != 0 {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(HBRUSH(old as *mut core::ffi::c_void).0));
        }
    }
    key.store(k, Ordering::SeqCst);
    hb
}

/// 在方框里画一个勾。用两条线段而不是字符「√」：
/// 后者要赌字体里有这个字形，画线段永远不会缺字
unsafe fn draw_check(hdc: HDC, rc: &RECT, c: &Colors) {
    let pen = CreatePen(
        PEN_STYLE(0), // PS_SOLID
        px(2.0).max(1),
        rgb(c.win_fg.0, c.win_fg.1, c.win_fg.2),
    );
    let old = SelectObject(hdc, HGDIOBJ(pen.0));
    let _ = MoveToEx(
        hdc,
        rc.left + px(3.0),
        rc.top + (rc.bottom - rc.top) / 2,
        None,
    );
    let _ = LineTo(hdc, rc.left + px(6.0), rc.bottom - px(4.0));
    let _ = LineTo(hdc, rc.right - px(4.0), rc.top + px(3.0));
    SelectObject(hdc, old);
    let _ = DeleteObject(HGDIOBJ(pen.0));
}

/// 自绘一个控件：按钮（含主题切换）或复选框
unsafe fn draw_item(dis: &DRAWITEMSTRUCT) {
    let c = theme::colors();
    let hdc = dis.hdc;
    let pressed = dis.item_state & ODS_SELECTED != 0;
    let id = dis.ctl_id as isize;
    let is_check = id == ID_NOTIFY || id == ID_AUTO;
    // 勾选状态从我们自己那份变量读。
    // 不能问控件（它不是复选框了），也不能看 itemState（ODS_CHECKED 只对菜单项有效）
    let checked = if id == ID_NOTIFY {
        NOTIFY_CHECK.load(Ordering::SeqCst)
    } else {
        AUTO_CHECK.load(Ordering::SeqCst)
    };

    // 先把整块填成窗口底色：自绘控件不会自己擦背景，
    // 少了这一步，切换主题或窗口重绘后会留着上一次的痕迹
    let win_brush = brush(&BRUSH_WIN, &BRUSH_WIN_KEY, c.win_bg);
    let _ = FillRect(hdc, &dis.rc_item, win_brush);

    let font = create_font(ui_font_px(9.0));
    let old = SelectObject(hdc, HGDIOBJ(font.0));
    let _ = SetBkMode(hdc, TRANSPARENT);
    let _ = SetTextColor(hdc, rgb(c.win_fg.0, c.win_fg.1, c.win_fg.2));

    if is_check {
        let s = px(14.0);
        let top = dis.rc_item.top + (dis.rc_item.bottom - dis.rc_item.top - s) / 2;
        let left = dis.rc_item.left + px(1.0);
        let rc_box = RECT {
            left,
            top,
            right: left + s,
            bottom: top + s,
        };
        let box_brush = brush(&BRUSH_EDIT, &BRUSH_EDIT_KEY, c.edit_bg);
        let _ = FillRect(hdc, &rc_box, box_brush);
        let bd = CreateSolidBrush(rgb(c.btn_border.0, c.btn_border.1, c.btn_border.2));
        let _ = FrameRect(hdc, &rc_box, bd);
        let _ = DeleteObject(HGDIOBJ(bd.0));
        if checked {
            draw_check(hdc, &rc_box, c);
        }
        let mut rc_text = RECT {
            left: rc_box.right + px(8.0),
            top: dis.rc_item.top,
            right: dis.rc_item.right,
            bottom: dis.rc_item.bottom,
        };
        let mut buf: Vec<u16> = text_of(dis.hwnd_item).encode_utf16().collect();
        let _ = DrawTextW(
            hdc,
            &mut buf,
            &mut rc_text,
            DRAW_TEXT_FORMAT(DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX),
        );
    } else {
        let fill = if pressed { c.btn_pressed } else { c.btn_bg };
        let fb = CreateSolidBrush(rgb(fill.0, fill.1, fill.2));
        let _ = FillRect(hdc, &dis.rc_item, fb);
        let _ = DeleteObject(HGDIOBJ(fb.0));
        // 「保存」是主按钮：自绘之后 BS_DEFPUSHBUTTON 的语义没了（那也是个类型位），
        // 用一圈更醒目的边框把它标出来，别让用户分不清哪个是确定
        let edge = if id == ID_SAVE {
            c.win_fg
        } else {
            c.btn_border
        };
        let bd = CreateSolidBrush(rgb(edge.0, edge.1, edge.2));
        let _ = FrameRect(hdc, &dis.rc_item, bd);
        let _ = DeleteObject(HGDIOBJ(bd.0));
        let mut rc = dis.rc_item;
        let mut buf: Vec<u16> = text_of(dis.hwnd_item).encode_utf16().collect();
        let _ = DrawTextW(
            hdc,
            &mut buf,
            &mut rc,
            DRAW_TEXT_FORMAT(DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX),
        );
    }

    SelectObject(hdc, old);
    let _ = DeleteObject(HGDIOBJ(font.0));
}

/// 主题按钮被点：在三态之间循环，并立刻生效。
/// 立刻生效是刻意的 —— 配色这种东西，点一下没反应就会以为按钮坏了
fn cycle_theme() {
    let next = theme::current_mode().next();
    theme::set_mode(next);
    set_text(ctl(&HW_THEME), &next.label());
    unsafe {
        apply_titlebar_theme();
        // 整个窗口重绘：背景、静态文字、自绘控件都得跟着换色
        let _ = InvalidateRect(Some(hwnd()), None, true);
    }
    crate::popup::repaint();
    info!(
        "[settings] 主题切到 {}（深色={}）",
        next.as_str(),
        theme::is_dark()
    );
}

fn set_status(text: &str) {
    let t = wide_nul(text);
    unsafe {
        let _ = SetWindowTextW(ctl(&HW_STATUS), windows::core::PCWSTR(t.as_ptr()));
    }
}

fn text_of(hw: HWND) -> String {
    // 长度按控件里的实际内容取，不写死栈缓冲。
    // 这里踩过两次：先是 32 长度把 URL 截到 31 个字符，改成 512 之后又只是把阈值
    // 挪到了 511 —— 粘贴一个带签名的长地址，存下去仍是半截，而输入框里看着完整，
    // 用户根本想不到问题出在读取这一步。GetWindowTextW 超长时是**静默截断**，
    // 所以只能先问长度再分配。
    let n = unsafe { GetWindowTextLengthW(hw) };
    if n <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; n as usize + 1];
    let got = unsafe { GetWindowTextW(hw, &mut buf) };
    if got <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..got as usize])
}

fn set_text(hw: HWND, text: &str) {
    let t = wide_nul(text);
    unsafe {
        let _ = SetWindowTextW(hw, windows::core::PCWSTR(t.as_ptr()));
    }
}

fn set_font(hw: HWND, font: windows::Win32::Graphics::Gdi::HFONT) {
    unsafe {
        let _ = SendMessageW(
            hw,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
    }
}

/// 建一个子控件。id 为 0 表示不需要接收消息（纯标签）
// CreateWindowExW 本身就摊着九个必填参数，这是 Win32 的调用约定，不是设计失误；
// 硬拆成结构体只会让调用点更绕
#[allow(clippy::too_many_arguments)]
fn make(
    class: windows::core::PCWSTR,
    text: &str,
    style: windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    parent: HWND,
    id: isize,
) -> HWND {
    // 窗口名要 NUL 结尾，w! 出来的字面量自带，这里自己拼的文本得补上
    let name = wide_nul(text);
    unsafe {
        CreateWindowExW(
            Default::default(),
            class,
            windows::core::PCWSTR(name.as_ptr()),
            style,
            x,
            y,
            w,
            h,
            Some(parent),
            Some(HMENU(id as *mut core::ffi::c_void)),
            None,
            None,
        )
        .unwrap_or(HWND(std::ptr::null_mut::<core::ffi::c_void>()))
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            // 子控件通知：wparam 低 16 位是控件 ID，高 16 位是通知码
            let id = (wparam.0 & 0xFFFF) as isize;
            match id {
                ID_SAVE => on_save(),
                ID_CANCEL => hide(),
                // Enter 保存、Esc 关闭：来自 handle_dialog_keys 那条路
                ID_DLG_OK => on_save(),
                ID_DLG_CANCEL => hide(),
                // 真正的弹卡片动作得留给主线程（窗口和消息泵同线程），
                // 这里只把「有人点了」这个事实递过去
                ID_TEST => TEST_REQUESTED.store(true, Ordering::SeqCst),
                ID_THEME => cycle_theme(),
                // 自绘复选框的状态得自己翻，翻完还要让控件重画 ——
                // 系统不会再替我们做这两件事（原因见 BS_OWNERDRAW 那条注释）
                ID_NOTIFY | ID_AUTO => {
                    let is_notify = id == ID_NOTIFY;
                    let cell: &AtomicBool = if is_notify {
                        &NOTIFY_CHECK
                    } else {
                        &AUTO_CHECK
                    };
                    let on = !cell.load(Ordering::SeqCst);
                    cell.store(on, Ordering::SeqCst);
                    let _ = InvalidateRect(
                        Some(ctl(if is_notify { &HW_NOTIFY } else { &HW_AUTO })),
                        None,
                        true,
                    );
                    info!(
                        "[settings] 复选框「{}」现在={}",
                        if is_notify {
                            "低电量时提醒我"
                        } else {
                            "开机时自动启动"
                        },
                        on
                    );
                }
                _ => {}
            }
            LRESULT(0)
        }
        // 窗口背景按当前主题填。窗口类上不再挂死刷子 —— 挂了就切不了主题
        WM_ERASEBKGND => {
            let hdc = HDC(wparam.0 as *mut core::ffi::c_void);
            let mut rc = RECT::default();
            if GetClientRect(hwnd, &mut rc).is_ok() {
                let c = theme::colors();
                let b = brush(&BRUSH_WIN, &BRUSH_WIN_KEY, c.win_bg);
                let _ = FillRect(hdc, &rc, b);
            }
            LRESULT(1)
        }
        // 静态文字和输入框只认这两个消息返回的颜色，所以它们不用自绘就能跟着主题走
        WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => {
            let is_edit = msg == WM_CTLCOLOREDIT;
            let hdc = HDC(wparam.0 as *mut core::ffi::c_void);
            let c = theme::colors();
            let bg = if is_edit { c.edit_bg } else { c.win_bg };
            let _ = SetTextColor(hdc, rgb(c.win_fg.0, c.win_fg.1, c.win_fg.2));
            let _ = SetBkColor(hdc, rgb(bg.0, bg.1, bg.2));
            let b = if is_edit {
                brush(&BRUSH_EDIT, &BRUSH_EDIT_KEY, bg)
            } else {
                brush(&BRUSH_WIN, &BRUSH_WIN_KEY, bg)
            };
            LRESULT(b.0 as isize)
        }
        WM_DRAWITEM => {
            let dis = &*(lparam.0 as *const DRAWITEMSTRUCT);
            draw_item(dis);
            LRESULT(1)
        }
        // 系统设置里换了浅色/深色：Windows 会广播这条消息，我们跟着换，
        // 连「跟随系统」这一档也才真的跟得上
        WM_SETTINGCHANGE => {
            theme::reload();
            set_text(ctl(&HW_THEME), &theme::current_mode().label());
            apply_titlebar_theme();
            let _ = InvalidateRect(Some(hwnd), None, true);
            crate::popup::repaint();
            LRESULT(0)
        }
        // 标题栏的 ×：隐藏而不是销毁，控件留着下次接着用
        WM_CLOSE => {
            hide();
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
