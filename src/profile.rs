//! 设备协议声明（PRD §6）
//!
//! 设计目标：**加一台设备 = 加一份 YAML，不改代码**。
//!
//! 声明只描述「事实」（VID/PID、feature 号、报文类型、取值方式），不含可执行逻辑；
//! 协议实现仍住在 src/hidpp.rs。`protocol_source` 是许可证红线字段：
//! 协议常量属事实信息可移植，但禁止复制 Solaar / HeadsetControl 等 GPL 项目源码。
//!
//! 原则（对齐 F11.3）：声明文件解析失败时跳过并告警，绝不因此让整个程序崩溃。

use crate::{info, warn};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

use crate::hidpp::BatteryKind;

/// `raw` 声明里 `reply_len` 的上限。
///
/// 不是防谁，是防手误：raw.rs 每一轮轮询都按这个值分配缓冲，
/// 多打几个 0 就是每轮试图分配几个 GB，而 Rust 分配失败是 abort ——
/// 一份写坏的声明能把程序带走，这不能接受。1024 远超任何真实 HID 报文
/// （USB HID 中断端点单包最大 64 字节，厂商自定义接口也在这个量级）
const MAX_REPLY_LEN: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// 罗技 HID++ 2.0
    Hidpp20,
    /// 蓝牙标准 Battery Service（0x180F）。
    /// 与上面两类最大的区别：**不需要逆向** —— UUID 是蓝牙 SIG 定的公开标准，
    /// 声明里连报文都不用写，只要有 VID/PID 就能读
    Ble,
    /// 原始报文：请求/响应都是裸字节，格式全写在声明里。
    /// 为非 HID++ 的设备（HyperX、雷蛇等）准备 —— 它们的报文各家自定义，
    /// 与其每加一家就写一份 Rust，不如把「发什么、读哪几个字节、怎么换算」
    /// 都变成声明里的字段
    Raw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// 经接收器：需要按 slot 寻址
    Receiver,
    /// 设备直连（USB 线或自带接收器已配对为一台设备）
    Direct,
}

/// 编译后的声明：十六进制已解析成数值，可直接参与比较
///
/// 派生 `PartialEq` 只为单测服务：`to_yaml` 必须做到「写出去再读回来一模一样」，
/// 判等交给编译器生成，比手写一串逐字段断言可靠 —— 以后加了字段，漏写进
/// `to_yaml` 会当场红，而不是等用户重启后发现那台设备认不出来了
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub name: String,
    /// 托盘悬停文字用的短名。同时挂好几台设备时全名拼起来会很长，
    /// 而这个位置只有一两秒的阅读时间，短名才扫得完。
    /// 不填就用全名 —— 完整名字始终在左键面板里，不会丢信息
    pub short_name: Option<String>,
    pub protocol: Protocol,
    pub transport: Transport,
    pub vid: u16,
    pub pid: u16,
    pub usage_page: Option<u16>,
    pub usage: Option<u16>,
    pub battery_feature: Option<u16>,
    pub battery_kind: BatteryKind,
    /// protocol: raw 时的报文规格
    pub raw: Option<RawSpec>,
    /// Receiver 模式下要探测的 slot 列表
    pub slots: Vec<u8>,
    /// 设备名 feature（用于识别与显示）
    pub name_feature: Option<u16>,
    /// 若声明了该串，读到的设备名必须包含它（不区分大小写）才算命中
    pub identify_contains: Option<String>,
    pub protocol_source: String,
    pub verification: String,
}

// ---------- YAML 形状 ----------

#[derive(Debug, Deserialize)]
struct Raw {
    name: String,
    #[serde(default)]
    short: Option<String>,
    #[serde(default)]
    protocol: String,
    #[serde(default)]
    transport: String,
    #[serde(rename = "match")]
    matches: RawMatch,
    battery: RawBattery,
    #[serde(default)]
    identify: Option<RawIdentify>,
    #[serde(default)]
    slots: Option<Vec<u8>>,
    #[serde(default)]
    protocol_source: String,
    #[serde(default)]
    verification: String,
}

#[derive(Debug, Deserialize)]
struct RawMatch {
    vendor_id: String,
    product_id: String,
    #[serde(default)]
    usage_page: Option<String>,
    #[serde(default)]
    usage: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawBattery {
    #[serde(default)]
    feature: Option<String>,
    #[serde(default)]
    kind: String,
    // 以下仅 protocol: raw 用到
    #[serde(default)]
    request: Option<String>,
    #[serde(default)]
    reply_len: Option<usize>,
    #[serde(default)]
    state_index: Option<usize>,
    #[serde(default)]
    value_index: Option<usize>,
    #[serde(default)]
    charging_states: Option<Vec<String>>,
    /// 档位 → 分段表。key 是档位字节（如 "0x0E"），value 是 [[上界, 百分比], ...]
    #[serde(default)]
    table: Option<std::collections::HashMap<String, Vec<(u8, u8)>>>,
}

impl RawBattery {
    fn compile_spec(&self) -> Result<RawSpec, String> {
        let request = self
            .request
            .as_deref()
            .and_then(hex_bytes)
            .ok_or("raw 协议缺少 battery.request（如 \"21 FF 05\"）")?;
        let value_index = self
            .value_index
            .ok_or("raw 协议缺少 battery.value_index（数值字节的下标）")?;
        let mut table: Vec<(u8, Vec<(u8, u8)>)> = Vec::new();
        if let Some(t) = &self.table {
            for (k, rows) in t {
                let state = hex_byte(k).ok_or(format!("table 的档位键 {k} 不是合法十六进制"))?;
                if rows.is_empty() {
                    return Err(format!("table[{k}] 是空的"));
                }
                table.push((state, rows.clone()));
            }
            // 查表是顺序扫描，排序后可以让声明里的书写顺序不影响结果
            table.sort_by_key(|(s, _)| *s);
        }
        let reply_len = self.reply_len.unwrap_or(20).max(request.len());
        // 上限见 MAX_REPLY_LEN 的注释：这里的值直接决定每轮分配多大一块内存
        if reply_len > MAX_REPLY_LEN {
            return Err(format!(
                "reply_len {reply_len} 太大（上限 {MAX_REPLY_LEN}）"
            ));
        }
        let state_index = self.state_index.unwrap_or(value_index.saturating_sub(1));
        // 下标越界不会崩，只会让这台设备**永远静默读不到** —— 「声明明明写了却认不出一台」
        // 是这里最难查的一类问题，所以在编译期就拒掉，日志里留一句明确的
        if value_index >= reply_len || state_index >= reply_len {
            return Err(format!(
                "字节下标越界：value_index={value_index}、state_index={state_index}，但 reply_len 只有 {reply_len}"
            ));
        }
        Ok(RawSpec {
            reply_len,
            request,
            state_index,
            value_index,
            charging_states: self
                .charging_states
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .map(|s| hex_byte(s).ok_or(format!("charging_states 里的 {s} 不是合法十六进制")))
                .collect::<Result<Vec<u8>, String>>()?,
            table,
        })
    }
}

#[derive(Debug, Deserialize)]
struct RawIdentify {
    #[serde(default)]
    name_feature: Option<String>,
    #[serde(default)]
    contains: Option<String>,
}

/// protocol: raw 的报文规格。字段全部来自声明，代码里不出现任何厂商常量
#[derive(Debug, Clone, PartialEq)]
pub struct RawSpec {
    /// 请求报文（首字节是 report id），会补 0 到 reply_len 的长度
    pub request: Vec<u8>,
    /// 期望的响应长度。发请求时报文也按这个长度补齐 —— 很多设备对
    /// 长度敏感，短了干脆不回（罗技那边踩过同样的坑）
    pub reply_len: usize,
    /// 响应里「档位」字节的下标。有些设备分成几档，每档的数值含义不同
    pub state_index: usize,
    /// 响应里「数值」字节的下标
    pub value_index: usize,
    /// 档位字节取这些值时表示正在充电
    pub charging_states: Vec<u8>,
    /// 分段映射表：每个档位一串 (数值上界, 百分比)，
    /// 数值落在第一个「不超过上界」的区间里就取那个百分比。
    /// 用分段而不是公式 —— 实测这些设备的数值与百分比不是线性关系
    pub table: Vec<(u8, Vec<(u8, u8)>)>,
}

/// 解析 "0x046D" / "046D" 这类写法
fn hex(s: &str) -> Option<u16> {
    let t = s.trim().to_ascii_lowercase();
    let t = t.strip_prefix("0x").unwrap_or(&t);
    u16::from_str_radix(t, 16).ok()
}

/// 解析单个字节，一律按十六进制：`"0x0E"` / `"0E"`。
/// 注意**不支持十进制** —— `"231"` 会被当成 0x231，超出 u8 于是报「不是合法十六进制」。
/// 声明里的字节值全部照报文写，十六进制是唯一口径，混着来只会让换算表对不上号
fn hex_byte(s: &str) -> Option<u8> {
    let t = s.trim().to_ascii_lowercase();
    let t = t.strip_prefix("0x").unwrap_or(&t);
    u8::from_str_radix(t, 16).ok()
}

/// 解析 "21 FF 05" 这样的空格分隔字节序列
fn hex_bytes(s: &str) -> Option<Vec<u8>> {
    let out: Option<Vec<u8>> = s.split_whitespace().map(hex_byte).collect();
    out.filter(|v| !v.is_empty())
}

impl Raw {
    fn compile(self) -> Result<Profile, String> {
        let protocol = match self.protocol.trim().to_ascii_lowercase().as_str() {
            "" | "hid++2.0" | "hidpp20" | "hidpp" => Protocol::Hidpp20,
            "raw" | "raw_report" => Protocol::Raw,
            "ble" | "bluetooth" | "ble-gatt" => Protocol::Ble,
            other => return Err(format!("不支持的协议 {other}")),
        };
        // raw 协议的报文规格是必填的：没有请求字节就无从下手，
        // 与其让它带着空表跑起来再读出 0%，不如在加载时就说清楚
        let raw = if protocol == Protocol::Raw {
            Some(self.battery.compile_spec()?)
        } else {
            None
        };
        // 蓝牙设备自身就是终点，没有「一个接收器挂多台」这回事，所以不认 receiver。
        // 声明里就算写了 receiver 也按 direct 处理 —— 写错不该变成一个空列表
        let transport = match protocol {
            Protocol::Ble => Transport::Direct,
            _ => match self.transport.trim().to_ascii_lowercase().as_str() {
                "" | "receiver" => Transport::Receiver,
                "direct" => Transport::Direct,
                other => return Err(format!("不支持的连接方式 {other}")),
            },
        };
        let battery_kind = match self.battery.kind.trim().to_ascii_lowercase().as_str() {
            "" | "voltage" | "voltage-mv" | "mv" => BatteryKind::Voltage,
            "percent" | "percentage" => BatteryKind::Percent,
            other => return Err(format!("不支持的电量取值方式 {other}")),
        };
        let vid = hex(&self.matches.vendor_id).ok_or("vendor_id 不是合法十六进制")?;
        let pid = hex(&self.matches.product_id).ok_or("product_id 不是合法十六进制")?;

        let slots = match (transport, self.slots.clone()) {
            (Transport::Receiver, Some(s)) if !s.is_empty() => s,
            (Transport::Receiver, _) => (1..=6u8).collect(),
            (Transport::Direct, _) => vec![1],
        };

        let ident = self.identify.unwrap_or(RawIdentify {
            name_feature: None,
            contains: None,
        });
        let name_feature = match ident.name_feature.as_deref() {
            Some(f) => Some(hex(f).ok_or("identify.name_feature 不是合法十六进制")?),
            None => None,
        };

        // 短名留空就用全名，但去掉首尾空白：YAML 里顺手写的空格不该进到界面上
        let short_name = self
            .short
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        Ok(Profile {
            name: self.name,
            short_name,
            protocol,
            transport,
            vid,
            pid,
            usage_page: self.matches.usage_page.as_deref().and_then(hex),
            usage: self.matches.usage.as_deref().and_then(hex),
            battery_feature: self.battery.feature.as_deref().and_then(hex),
            battery_kind,
            raw,
            slots,
            name_feature,
            identify_contains: ident.contains,
            protocol_source: self.protocol_source,
            verification: self.verification,
        })
    }
}

/// 按 exe 位置找到 devices 目录并加载全部声明。解析失败的会打印原因并跳过。
///
/// 为什么不能直接用相对路径「devices」：它依赖的是**当前工作目录**，
/// 而开发态和发布态的布局完全不同 ——
///   · cargo 运行时：工作目录是项目根，exe 在 target/release 下
///   · 双击 exe 时：工作目录是任意位置（资源管理器里是 target/release）
/// 只认一边，另一边必然失效，表现是「托盘起来了但一台设备都扫不到」。
/// 所以按 exe 所在目录往上找，找不着再退回工作目录。
pub fn load() -> Vec<Profile> {
    let mut all: Vec<Loaded> = Vec::new();
    let mut any_dir = false;
    for base in candidate_bases() {
        let d = base.join("devices");
        if d.is_dir() {
            any_dir = true;
            info!("[profile] 设备声明目录：{:?}", d);
            all.extend(load_from(&d));
        }
    }
    if !any_dir {
        warn!("[profile] 没找到 devices 目录，程序将检测不到任何设备");
        return Vec::new();
    }
    merge(all)
}

/// 装载进来的一份声明，连同「它有多可信」。
struct Loaded {
    p: Profile,
    /// 可信度分层，数字越小越优先。取值见 `rank_of`
    rank: u8,
    /// 来自哪个文件。只在日志里用，但必须留着 ——
    /// 出问题时用户问的是「到底用了哪一份」，没有文件名就只能让他挨个删着试
    file: String,
}

/// 一份声明的可信度。**数字小 = 更可信**。
///
/// 排序依据是「谁最可能知道这台设备的真相」：
///   0. 用户自己写的（用户目录里 non-synced / non-auto 的）—— 这台机器的主人的意志
///   1. 远程同步来的 `synced-*` —— 维护者维护、且是最新的，所以压过内置
///   2. 内置（exe 自带）—— 跟着版本走，可能已经旧了
///   3. 自动发现写的 `auto-*` —— 程序自己猜的，名字和参数都可能是将就
///
/// **`synced-*` 必须压过内置**，否则「检查设备声明更新」这个功能对已支持的设备
/// 完全无效：用户在磁盘上看到文件被更新了、界面报「更新了 1 份」、重启，
/// 内存里用的却还是 exe 里那份旧的。这一条是 2026-10-27 审查揪出来的。
fn rank_of(dir: &Path, name: &str) -> u8 {
    if user_devices_dir().as_deref() != Some(dir) {
        return 2;
    }
    if name.starts_with("synced-") {
        1
    } else if name.starts_with("auto-") {
        3
    } else {
        0
    }
}

/// 把多个目录、多个来源的声明合并成一份不重复的列表。
///
/// 拆分出来是为了能离线单测：真实目录结构没法在测试里摆出来，
/// 而「谁盖住谁」恰恰是那种写反了也不报错、只在用户那儿表现为「同步没反应」的逻辑。
fn merge(mut all: Vec<Loaded>) -> Vec<Profile> {
    // 先按可信度、再按文件名排序。
    //
    // 只按 rank 排是不够的：同 rank 的输入顺序来自 read_dir，而 NTFS 上它不保证有序 ——
    // 同一个目录里放了两份描述同一设备的声明时，重启两次可能用的是不同那份，
    // 日志里的「用了 X 跳过 Y」也就没法复现。拿文件名当第二关键字，行为才真的确定
    all.sort_by(|a, b| (a.rank, &a.file).cmp(&(b.rank, &b.file)));
    let mut out: Vec<(Profile, String)> = Vec::new();
    for l in all {
        match out.iter().position(|(q, _)| same_target(q, &l.p)) {
            Some(i) => info!(
                "[profile] {} 有不止一份声明：用 {}，跳过 {}",
                l.p.name, out[i].1, l.file
            ),
            None => out.push((l.p, l.file)),
        }
    }
    out.into_iter().map(|(p, _)| p).collect()
}

/// 两份声明是不是在描述**同一台**设备。
///
/// 不能只比 VID/PID + 协议。两个真实场景会被那样的粗键误判：
/// · 一个接收器上挂着两台（鼠标 + 键盘），VID/PID 是接收器的，完全相同
/// · 同一型号有两种电量读法（0x1001 电压 / 0x1004 百分比），协议也相同
/// 误判的后果是后一份整份被丢掉 —— 表现出来就是「明明声明了这台，面板里没有」。
/// 所以把「定位到设备」的每个维度都比一遍：接口（usage_page/usage）
/// 和电量 feature 都是区分同一外壳下不同设备的依据。
fn same_target(a: &Profile, b: &Profile) -> bool {
    a.vid == b.vid
        && a.pid == b.pid
        && a.protocol == b.protocol
        && usage_compatible(a.usage_page, b.usage_page)
        && usage_compatible(a.usage, b.usage)
        && a.battery_feature == b.battery_feature
}

/// 两个接口定位是不是同一处。
///
/// **任一方没写就算同一个**，而不是要求严格相等：老声明里常常没写
/// `usage_page/usage`（省略时会匹配该 VID/PID 的所有接口），而远程修正版可能补上了。
/// 严格相等会把「同一条声明的修正版」判成另一台设备，两份都进列表 ——
/// 后果是面板上同一台鼠标出现两行。
///
/// 刻意**不**把 `identify.contains` 纳入比较：它决定的是「这个 slot 上是不是这台」，
/// 不是「这条声明在说哪台设备」。拿它参与去重，会让「给老声明补一个识别关键字」
/// 这种修正被判成新设备，正好抵消掉同步功能存在的意义。
fn usage_compatible(a: Option<u16>, b: Option<u16>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x == y,
        _ => true,
    }
}

/// 一份解析过的远程清单。
///
/// `rejected` 不是装饰：只要清单里有一份没通过校验，这份清单就**不再代表远程的
/// 完整状态** —— 缺的那台并不等于「远程把它撤回了」。清理本地旧声明时必须看这个数，
/// 否则「远程某台写错了」会表现为「用户那台设备从面板上凭空消失」。
pub struct Bundle {
    pub profiles: Vec<Profile>,
    /// 校验失败、被跳过的份数
    pub rejected: usize,
}

/// 解析远程聚合清单：一个文件里装着多份声明。
///
/// 为「检查设备声明更新」准备 —— 一次请求拿全部，而不是一台一个请求。
/// 清单是**不可信输入**：整份解析不出来就报错，单份有问题只跳过那一份 ——
/// 不能因为远程某一台写错了，就让所有人一台都同步不到。
///
/// 解析刻意分两段：先只做 YAML 语法解析，再逐份取值。若直接一次性
/// `from_str::<Bundle>`，「某一份的字段类型写错」（vendor_id 忘了加引号变成整数、
/// reply_len 写成字符串、table 里的数值超过 u8……）会让整份清单解析失败，
/// 于是所有人一台都同步不到 —— 正好与这个函数要保证的东西相反。
pub fn parse_bundle(text: &str) -> Result<Bundle, String> {
    let doc: serde_yml::Value =
        serde_yml::from_str(text).map_err(|e| format!("清单不是合法 YAML：{e}"))?;
    let devices = doc
        .get("devices")
        .and_then(|d| d.as_sequence())
        .ok_or_else(|| "清单里没有 devices 列表".to_string())?;
    if devices.is_empty() {
        return Err("清单里一份声明都没有".to_string());
    }
    let mut out = Vec::new();
    let mut rejected = 0;
    for v in devices {
        let raw: Raw = match serde_yml::from_value(v.clone()) {
            Ok(r) => r,
            Err(e) => {
                rejected += 1;
                warn!("[profile] 远程清单里有一份声明字段不对，跳过：{e}");
                continue;
            }
        };
        match raw.compile() {
            Ok(p) => out.push(p),
            Err(e) => {
                rejected += 1;
                warn!("[profile] 远程清单里有一份声明不对，跳过：{e}");
            }
        }
    }
    if out.is_empty() {
        return Err("清单里的声明全都用不了".to_string());
    }
    Ok(Bundle {
        profiles: out,
        rejected,
    })
}

/// 自动发现到的设备落到用户目录，下次启动直接命中，不必再探测一遍。
///
/// 写在 `%APPDATA%\PowerRing\devices` 而不是 exe 旁边：程序可能装在 Program Files，
/// 那边不一定有写权限；而这份东西本来就是「这台机器上认出来的」，跟着用户走更合适。
///
/// 文件名带 VID/PID，一台一个文件，方便用户整份贴给我 —— 贴过来就是能直接合并的声明。
/// 已存在的同名文件**不覆盖**：用户可能改过名字、调过字段，自动发现不该把他的改动冲掉。
pub fn save_discovered(list: &[Profile]) -> usize {
    let Some(dir) = user_devices_dir() else {
        return 0;
    };
    if let Err(e) = fs::create_dir_all(&dir) {
        warn!("[profile] 建不了用户声明目录 {:?}：{e}", dir);
        return 0;
    }
    let mut n = 0;
    for p in list {
        let path = dir.join(format!("auto-{}.yaml", slug(p)));
        if path.exists() {
            continue;
        }
        match fs::write(&path, to_yaml(p, NOTE_DISCOVERED)) {
            Ok(()) => {
                info!("[profile] 记下新设备 {:?}", path);
                n += 1;
            }
            Err(e) => warn!("[profile] 写不进 {:?}：{e}", path),
        }
    }
    n
}

/// 一份声明对应的文件名主体：`{VID}-{PID}` 或 `{VID}-{PID}-{电量 feature}`。
///
/// feature 后缀不能省：同一个接收器上可能挂着用不同 feature 的两台设备，
/// 那就得有两份声明。不带后缀的话第二份会被「已存在」挡掉，等于白认一台。
fn slug(p: &Profile) -> String {
    match p.battery_feature {
        Some(f) => format!("{:04X}-{:04X}-{f:04X}", p.vid, p.pid),
        None => format!("{:04X}-{:04X}", p.vid, p.pid),
    }
}

/// 远程同步来的声明落盘：`%APPDATA%\PowerRing\devices\synced-*.yaml`。
///
/// 和自动发现那份（auto-*.yaml）最大的区别是**远程清单是权威的**：
/// - 内容变了就覆盖，不学 auto-* 那样「已存在就跳过」—— 远程改对了，
///   本地必须跟着改，否则等于没同步
/// - 本次清单里没有的 synced-* 一律删掉。远程撤掉一份错声明，
///   本地还留着就是继续用错的
///
/// 返回（新增的份数，更新的份数，没写成功的份数）
///
/// 第三个值不能省：目录建不出来、磁盘满、`%APPDATA%` 只读时，前两个都是 0，
/// 而「全都已经是最新」也是 0 —— 界面若只看前两个，就会在「一份都没写进去」时
/// 报「已经是最新了」，用户重启后发现设备还是缺的，却不知道该怀疑什么。
pub fn save_synced(bundle: &Bundle, prune: bool) -> (usize, usize, usize) {
    let Some(dir) = user_devices_dir() else {
        warn!("[profile] 找不到用户声明目录（APPDATA 为空？）");
        return (0, 0, bundle.profiles.len());
    };
    save_synced_to(&dir, bundle, prune)
}

/// 落到指定目录。拆出来只为测试能用一个临时目录，不碰真实的 `%APPDATA%`。
fn save_synced_to(dir: &Path, bundle: &Bundle, prune: bool) -> (usize, usize, usize) {
    let list = &bundle.profiles;
    if let Err(e) = fs::create_dir_all(dir) {
        warn!("[profile] 建不了用户声明目录 {:?}：{e}", dir);
        return (0, 0, list.len());
    }
    let mut added = 0;
    let mut updated = 0;
    let mut failed = 0;
    let mut keep: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    for p in list {
        // 两份不同的声明可能算出同一个文件名：slug 只看 VID/PID/feature，而同一台设备
        // 可能有两种寻址接口、两套报文。以前后写的会静默覆盖先写的，界面上还报
        // 「新增 1 更新 1」，磁盘上却只剩一个文件 —— 重启后少一台，日志里看不出是谁。
        // 撞名就加序号，两份都留下
        let stem = slug(p);
        let mut file = format!("synced-{stem}.yaml");
        let mut n = 1;
        while used.contains(&file) {
            n += 1;
            if n > 9 {
                break;
            }
            file = format!("synced-{stem}-{n}.yaml");
        }
        if n > 9 {
            warn!("[profile] 文件名反复撞名，跳过这一份：{}", p.name);
            failed += 1;
            continue;
        }
        used.insert(file.clone());
        keep.insert(file.clone());
        let path = dir.join(&file);
        let text = to_yaml(p, NOTE_SYNCED);
        // 先读旧内容判断这是「新增」还是「更新」，但**计数要等写成功之后**。
        // 以前在写之前就加，写失败只打一行 warn，界面照样报「新增了 3 份」——
        // 用户看到的是成功，磁盘上是空的，重启还是老样子。
        // 内容没变就不写：省一次磁盘写入，也让文件时间戳老实待着 ——
        // 用户可能正盯着这个目录排查问题，每次同步都跳一遍时间戳会误导他
        let is_new = match fs::read_to_string(&path) {
            Ok(old) if old == text => continue,
            Ok(_) => false,
            Err(_) => true,
        };
        match fs::write(&path, text) {
            Ok(()) => {
                if is_new {
                    added += 1;
                } else {
                    updated += 1;
                }
                info!("[profile] 同步声明 {file}");
            }
            Err(e) => {
                warn!("[profile] 写不进 {:?}：{e}", path);
                failed += 1;
            }
        }
    }
    // 清理：本次清单里没有的 synced-* 全部删掉。
    //
    // `prune` 只在拉的是项目官方清单时为真。用户自己填了一个只含几台设备的镜像时
    // 不能清理 —— 那份清单不完整是他的自由，但按它去删别人的 synced-* 就是
    // 拿一份「不完整的权威」去覆盖「完整的权威」，结果是设备从面板上凭空消失。
    //
    // 还有两种情况同样算「这份清单不代表远程的全部」：
    //   - 清单里有一份没通过校验（rejected > 0）：缺的那台是写错了，不是被撤回
    //   - 本次有文件没写进去（failed > 0）：磁盘满、被杀毒锁住时，连自己的
    //     keep 清单都不完整，此时还去删别的文件，等于用一次失败的同步搞坏本地
    // 2026-10-28 外部审查指出：以前只看 `prune`，这两种情况都会误删。
    if prune && bundle.rejected == 0 && failed == 0 {
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with("synced-") && name.ends_with(".yaml") && !keep.contains(&name) {
                    match fs::remove_file(e.path()) {
                        Ok(()) => info!("[profile] 远程清单里没有了，删掉旧的 {name}"),
                        Err(err) => warn!("[profile] 删不掉 {name}：{err}"),
                    }
                }
            }
        }
    }
    (added, updated, failed)
}

/// 用户目录：%APPDATA%\PowerRing\devices
fn user_devices_dir() -> Option<PathBuf> {
    let app = std::env::var("APPDATA").ok()?;
    if app.trim().is_empty() {
        return None;
    }
    Some(PathBuf::from(app).join("PowerRing").join("devices"))
}

/// 自动发现生成的文件头：说清它是怎么来的、能不能删
const NOTE_DISCOVERED: &str = "\
# 由 PowerRing 自动发现生成：这台设备不在内置名单里，
# 程序用公开标准探测到它能报电量，于是记在这里。
# 这个文件可以随便删，删了下次启动会重新探测一遍。
# 想让更多人用上它，把这个文件的内容贴到项目的 Issue 里就行。
";

/// 远程同步生成的文件头。措辞必须和上面那份不一样：
/// 同步来的会被下次同步覆盖，写「可以随便改」是害人 —— 改了也白改，
/// 而他真正该知道的「怎么才能改得住」又没写进去
const NOTE_SYNCED: &str = "\
# 由 PowerRing 从远程清单同步：项目里已经适配好、验证过的设备声明。
# 这个文件由托盘菜单里的「检查设备声明更新…」维护，**手改会被下次同步覆盖**。
# 想改就另存一个别的名字放在同一个目录下（比如 my-xxx.yaml），那份不会被覆盖。
# 不需要它就整个删掉；只要它还在远程清单里，下次同步还会再拉回来。
";

/// 把一份声明写成 YAML 文本。
///
/// 手写而不走 serde 是有意的：文件里要带上「这是怎么来的、能不能改」的说明，
/// 序列化出来的东西没法夹带这些话，而恰恰是这些话决定了用户敢不敢动手改。
/// `note` 就是那段说明，两种来源的说法不一样（见上面两个常量）。
///
/// **必须做到「写出去再读回来一模一样」**。这不只是整洁问题 ——
/// 远程同步下来的声明就是先 `to_yaml` 落到磁盘、下次启动再由 `load_from` 读回来的。
/// 少写一个字段，那份声明落盘即残废：文件还在、看起来没问题，程序却认不出那台设备，
/// 排查时会往网络、往设备上找，很难想到是这一层丢的。
///
/// 2026-10-27 修过一次：原版只写 9 个字段，raw 协议的 `request` / `reply_len` /
/// `state_index` / `value_index` / `charging_states` / `table` 全丢，
/// 于是「同步一台新的 raw 设备」这条路根本不通 —— 写下去的就是一份读不出来的残文件。
/// 现在由单测 `raw声明写出去再读回来不丢字段` 守着：以后加字段忘了写进这里，立刻红。
fn to_yaml(p: &Profile, note: &str) -> String {
    let name = yaml_string(&p.name);
    let protocol = match p.protocol {
        Protocol::Hidpp20 => "hid++2.0",
        Protocol::Ble => "ble",
        Protocol::Raw => "raw",
    };
    let transport = match p.transport {
        Transport::Receiver => "receiver",
        Transport::Direct => "direct",
    };
    let kind = match p.battery_kind {
        BatteryKind::Voltage => "voltage",
        BatteryKind::Percent => "percent",
    };
    let mut s = String::new();
    s.push_str(note);
    s.push('\n');
    s.push_str(&format!("name: {name}\n"));
    if let Some(short) = &p.short_name {
        s.push_str(&format!("short: {}\n", yaml_string(short)));
    }
    s.push_str(&format!("protocol: {protocol}\n"));
    s.push_str(&format!("transport: {transport}\n"));
    s.push_str("match:\n");
    s.push_str(&format!("  vendor_id: \"0x{:04X}\"\n", p.vid));
    s.push_str(&format!("  product_id: \"0x{:04X}\"\n", p.pid));
    // usage 定的是「往哪个接口发包」，漏了它设备可能根本收不到 ——
    // 罗技接收器三个厂商接口里只有一个认 HID++，挑错了就是一路没回应
    if let Some(up) = p.usage_page {
        s.push_str(&format!("  usage_page: \"0x{up:04X}\"\n"));
    }
    if let Some(u) = p.usage {
        s.push_str(&format!("  usage: \"0x{u:04X}\"\n"));
    }
    // 只有「经接收器」才需要 slot 列表；直连那份写了也会被解析端忽略。
    // 这里的判据只看 transport，不看协议 —— raw 协议同样可以经接收器寻址
    // （compile 那边是允许 raw + receiver 的）。以前多写了 `protocol == Hidpp20`
    // 这个条件，于是 raw+receiver 的声明写出去就丢了 slots 整行，读回来变成默认 1..6，
    // 往返悄悄改了值，而仓库里那 4 份内置声明里没有这种组合，测试也就测不到
    if p.transport == Transport::Receiver {
        let slots = p
            .slots
            .iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        s.push_str(&format!("slots: [{slots}]\n"));
    }
    s.push_str("battery:\n");
    if let Some(f) = p.battery_feature {
        s.push_str(&format!("  feature: \"0x{f:04X}\"\n"));
    }
    if let Some(raw) = &p.raw {
        s.push_str(&format!("  request: \"{}\"\n", hex_str(&raw.request)));
        s.push_str(&format!("  reply_len: {}\n", raw.reply_len));
        s.push_str(&format!("  state_index: {}\n", raw.state_index));
        s.push_str(&format!("  value_index: {}\n", raw.value_index));
        let charging = raw
            .charging_states
            .iter()
            .map(|b| format!("\"0x{b:02X}\""))
            .collect::<Vec<_>>()
            .join(", ");
        s.push_str(&format!("  charging_states: [{charging}]\n"));
        if !raw.table.is_empty() {
            s.push_str("  table:\n");
            // 编译时已按档位排过序，写出去也保持这个顺序：
            // 每次同步生成的文件才会是同一份字节，而不是顺序随机跳。
            // （sync 那边靠「内容没变就不写」省磁盘写入，字节不稳会让它每次都写）
            for (state, rows) in &raw.table {
                s.push_str(&format!("    \"0x{state:02X}\":\n"));
                for (upper, percent) in rows {
                    s.push_str(&format!("      - [{upper}, {percent}]\n"));
                }
            }
        }
    }
    s.push_str(&format!("  kind: {kind}\n"));
    // 这两个字段的值已经是「带引号的字符串」，别再套一层引号 ——
    // 套两层会让 YAML 解析出一个以引号开头的怪值，而且解析失败是静默跳过的，
    // 用户只会看到这台设备不见了，很难联想到是这里多写了一对引号
    s.push_str(&format!(
        "protocol_source: {}\n",
        yaml_string(&p.protocol_source)
    ));
    s.push_str(&format!("verification: {}\n", yaml_string(&p.verification)));
    if p.name_feature.is_some() || p.identify_contains.is_some() {
        s.push_str("identify:\n");
        if let Some(nf) = p.name_feature {
            s.push_str(&format!("  name_feature: \"0x{nf:04X}\"\n"));
        }
        // `contains` 是「读到名字必须包含这串才算命中」。丢了它，
        // 同一个接收器上的其它设备就会被误认成这一台
        if let Some(c) = &p.identify_contains {
            s.push_str(&format!("  contains: {}\n", yaml_string(c)));
        }
    }
    s
}

/// 字节序列 → `"21 FF 05"`（`hex_bytes` 的逆操作）
fn hex_str(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// YAML 双引号字符串：引号、反斜杠和控制字符都得处理，
/// 否则设备名里一个引号就能让整份声明解析失败 —— 而声明解析失败是静默跳过的，
/// 用户只会看到「这台设备没了」，很难联想到是自己名字里的引号惹的祸。
///
/// 换行和制表符走**转义**而不是替换成空格：有些说明字段本来就是多行写的
/// （`protocol_source` 里贴一段来源说明很常见），换成空格等于悄悄改了内容，
/// 同步落盘再读出来就和远程那份不一样了。双引号里的 `\n` 是合法转义，读回来是原字符。
fn yaml_string(s: &str) -> String {
    let esc: String = s
        .chars()
        .map(|c| match c {
            '"' => "\\\"".to_string(),
            '\\' => "\\\\".to_string(),
            '\n' => "\\n".to_string(),
            '\r' => "\\r".to_string(),
            '\t' => "\\t".to_string(),
            // 其余控制字符 YAML 双引号里也能写成 \xNN，但这些字符在设备声明里
            // 没有任何意义，删掉比转义更干净 —— 与换行不同，删了不影响可读性
            c if c.is_control() => String::new(),
            c => c.to_string(),
        })
        .collect();
    format!("\"{esc}\"")
}

/// 从 exe 所在目录开始，逐级向上找 devices；最后是工作目录与 %APPDATA%
fn candidate_bases() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // 发布布局：exe 与 devices 平级（PowerRing/devices + PowerRing/powerring.exe）
            let mut cur = dir.to_path_buf();
            out.push(cur.clone());
            // 开发布局：exe 在 target/release 里，devices 在项目根，所以往上探几级
            for _ in 0..3 {
                match cur.parent() {
                    Some(p) => {
                        cur = p.to_path_buf();
                        out.push(cur.clone());
                    }
                    None => break,
                }
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        out.push(cwd);
    }
    // 以后装到系统里，声明会放在 %APPDATA%\PowerRing\devices
    if let Ok(app) = std::env::var("APPDATA") {
        out.push(PathBuf::from(app).join("PowerRing"));
    }
    // 同一个目录可能从两条路被推进来（从 exe 往上找会经过它，当前工作目录也可能正好是它）。
    // 不去重的话同一份声明会被读两遍，后面的靠去重挡住，日志里平白多出几行
    // 「已在前面的目录里声明过」，看起来像用户多了几份重复声明。
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

fn load_from(dir: &Path) -> Vec<Loaded> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else {
        warn!("[profile] 设备声明目录不可用：{:?}", dir);
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("");
        if ext != "yaml" && ext != "yml" {
            continue;
        }
        let text = match fs::read_to_string(&p) {
            Ok(t) => t,
            Err(err) => {
                warn!("[profile] 读取失败，跳过 {:?}：{err}", p);
                continue;
            }
        };
        let name = e.file_name().to_string_lossy().to_string();
        match serde_yml::from_str::<Raw>(&text) {
            Ok(raw) => match raw.compile() {
                Ok(prof) => out.push(Loaded {
                    rank: rank_of(dir, &name),
                    file: p.display().to_string(),
                    p: prof,
                }),
                Err(err) => warn!("[profile] 声明有误，跳过 {:?}：{err}", p),
            },
            Err(err) => warn!("[profile] YAML 解析失败，跳过 {:?}：{err}", p),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 从一段清单文本里取出第一份声明
    fn first(text: &str) -> Profile {
        parse_bundle(text)
            .expect("这一份应该能用")
            .profiles
            .remove(0)
    }

    fn ble() -> Profile {
        first(
            "devices:\n  - name: \"测试设备\"\n    protocol: \"ble\"\n    match:\n      vendor_id: \"0x045E\"\n      product_id: \"0x0B13\"\n    battery:\n      kind: \"percent\"\n",
        )
    }

    /// 只给本模块用的临时目录。
    ///
    /// 落盘相关的测试不能碰真实的 `%APPDATA%\PowerRing\devices` ——
    /// 测试里那些「旧文件该被留下 / 该被删掉」的断言，在用户目录上执行一遍
    /// 就等于替用户做了决定。
    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("powerring-sync-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// 把几份声明包成一份带 `devices:` 的清单文本
    fn bundle_text(profiles: &[Profile]) -> String {
        let values: Vec<serde_yml::Value> = profiles
            .iter()
            .map(|p| serde_yml::from_str(&to_yaml(p, NOTE_SYNCED)).unwrap())
            .collect();
        let mut map = serde_yml::Mapping::new();
        map.insert(
            serde_yml::Value::String("devices".into()),
            serde_yml::Value::Sequence(values),
        );
        serde_yml::to_string(&map).unwrap()
    }

    /// 清单里有一份写错时，好条目照常更新，但**不许**拿这份不完整的清单去删旧文件。
    ///
    /// 这是「远程某台写错了」与「远程把那台撤回了」在两个方向上长得一模一样的地方：
    /// 只看「这次的清单里没有它」，就会把用户本机那份有效的旧声明删掉，
    /// 那台设备随即从面板上消失 —— 而用户什么也没做错。
    #[test]
    fn 部分清单校验失败时更新好条目并保留旧文件() {
        // 两种坏法各测一遍：YAML 值类型能读出来但协议不认识，以及整段读不出来
        for bad in ["bad-protocol", "type-error"] {
            let dir = TestDir::new();
            let a = ble();
            let mut b = ble();
            b.pid += 1;
            let initial = parse_bundle(&bundle_text(&[a.clone(), b.clone()])).unwrap();
            assert_eq!(save_synced_to(&dir.0, &initial, true), (2, 0, 0));
            let old_b = dir.0.join(format!("synced-{}.yaml", slug(&b)));
            let before = fs::read_to_string(&old_b).unwrap();

            let mut updated = a;
            updated.name = "更新后的设备".into();
            let bad_entry = if bad == "bad-protocol" {
                "- name: 坏条目\n  protocol: nope\n"
            } else {
                // vendor_id 不加引号会变成整数，取值阶段才炸
                "- name: 坏条目\n  protocol: ble\n  match:\n    vendor_id: 1126\n    product_id: 2835\n"
            };
            let text = format!(
                "{}{}",
                bundle_text(std::slice::from_ref(&updated)),
                bad_entry
            );
            let bundle = parse_bundle(&text).unwrap_or_else(|e| panic!("{e}\n---\n{text}"));
            assert_eq!(bundle.rejected, 1, "{bad}：坏的那一份应该被计数");
            // 好条目照常写进去
            assert_eq!(save_synced_to(&dir.0, &bundle, true), (0, 1, 0));
            // 不在清单里的 b 必须原样留着
            assert_eq!(fs::read_to_string(&old_b).unwrap(), before);
            let new_a = dir.0.join(format!("synced-{}.yaml", slug(&updated)));
            assert_eq!(
                fs::read_to_string(new_a).unwrap(),
                to_yaml(&updated, NOTE_SYNCED)
            );
        }
    }

    /// 只有「清单完整」这一种情况才清理撤回条目，而且永远不碰用户自己的声明
    #[test]
    fn 完整清单才清理撤回条目且不碰用户声明() {
        let dir = TestDir::new();
        let a = ble();
        let mut b = ble();
        b.pid += 1;
        let initial = parse_bundle(&bundle_text(&[a.clone(), b.clone()])).unwrap();
        save_synced_to(&dir.0, &initial, true);
        let user = dir.0.join("my-device.yaml");
        let auto = dir.0.join("auto-device.yaml");
        fs::write(&user, "user content").unwrap();
        fs::write(&auto, "auto content").unwrap();
        let current = parse_bundle(&bundle_text(&[a])).unwrap();
        let old_b = dir.0.join(format!("synced-{}.yaml", slug(&b)));
        // 自定义源（prune=false）即使解析完整也不能清理官方来源的声明
        save_synced_to(&dir.0, &current, false);
        assert!(old_b.exists());
        // 官方源 + 完整清单：这次可以清理
        save_synced_to(&dir.0, &current, true);
        assert!(!old_b.exists());
        assert_eq!(fs::read_to_string(user).unwrap(), "user content");
        assert_eq!(fs::read_to_string(auto).unwrap(), "auto content");
    }

    /// 写盘失败时也不能清理：一次失败的同步不该顺手删掉本机还能用的声明
    #[test]
    fn 同步写入失败也不能清理旧声明() {
        let dir = TestDir::new();
        let a = ble();
        let mut b = ble();
        b.pid += 1;
        let initial = parse_bundle(&bundle_text(std::slice::from_ref(&b))).unwrap();
        save_synced_to(&dir.0, &initial, true);
        // 用目录占住目标文件名：写文件必然失败，且不依赖权限设置
        fs::create_dir(dir.0.join(format!("synced-{}.yaml", slug(&a)))).unwrap();
        let current = parse_bundle(&bundle_text(&[a])).unwrap();
        assert_eq!(save_synced_to(&dir.0, &current, true), (0, 0, 1));
        assert!(dir.0.join(format!("synced-{}.yaml", slug(&b))).exists());
    }

    /// 造一份最小可用的声明，只用来测「合并时谁盖住谁」
    fn stub(name: &str, feature: u16) -> Profile {
        Profile {
            name: name.to_string(),
            short_name: None,
            protocol: Protocol::Hidpp20,
            transport: Transport::Receiver,
            vid: 0x046D,
            pid: 0xC539,
            usage_page: Some(0xFF00),
            usage: Some(0x0002),
            battery_feature: Some(feature),
            battery_kind: BatteryKind::Voltage,
            raw: None,
            slots: (1..=6).collect(),
            name_feature: None,
            identify_contains: None,
            protocol_source: "测试".into(),
            verification: "测试".into(),
        }
    }

    fn at(p: Profile, rank: u8, file: &str) -> Loaded {
        Loaded {
            p,
            rank,
            file: file.to_string(),
        }
    }

    /// 这条是整个声明同步功能的地基。
    ///
    /// 如果内置那份永远赢，那么「检查设备声明更新」对**已支持**的设备就完全无效：
    /// 文件被覆盖了、界面报「更新了 1 份」、重启 —— 用的还是 exe 里那份旧的。
    /// 写反了不会报错，只会安静地什么都不发生，用户只会觉得这功能没用。
    #[test]
    fn 同步来的声明盖过内置的同名设备() {
        let list = merge(vec![
            at(stub("内置的旧写法", 0x1001), 2, "内置/logitech_gpw.yaml"),
            at(
                stub("远程修正过的", 0x1001),
                1,
                "用户/synced-046D-C539-1001.yaml",
            ),
        ]);
        assert_eq!(list.len(), 1, "同一台设备只该留一份");
        assert_eq!(list[0].name, "远程修正过的");
    }

    #[test]
    fn 用户手写的赢过同步下来的() {
        let list = merge(vec![
            at(stub("远程的", 0x1001), 1, "用户/synced-046D-C539-1001.yaml"),
            at(stub("我自己改的", 0x1001), 0, "用户/my-gpw.yaml"),
        ]);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "我自己改的");
    }

    /// 同一条链路上的两种电量读法（0x1001 电压 / 0x1004 百分比）必须都留下。
    /// 旧的去重键只比 VID/PID + 协议，会把后一份整份丢掉 ——
    /// 表现出来就是「声明明明写了，面板里却没有这台设备」。
    #[test]
    fn 同一个接收器上两种电量方式各留一份() {
        let list = merge(vec![
            at(stub("电压式", 0x1001), 2, "a.yaml"),
            at(stub("百分比式", 0x1004), 2, "b.yaml"),
        ]);
        assert_eq!(list.len(), 2);
    }

    /// 分层顺序：用户手写 < 同步 < 内置 < 自动发现。
    /// 这里比的是大小关系而不是具体数值 —— 以后插入新层只要不破坏这个顺序就不算改坏。
    #[test]
    fn 可信度顺序是用户手写_同步_内置_自动发现() {
        let user = user_devices_dir().expect("测试环境应该有 APPDATA");
        let builtin = Path::new(r"C:\nowhere\devices");
        let r = rank_of;
        assert!(
            r(&user, "my-gpw.yaml") < r(&user, "synced-046D-C539-1001.yaml"),
            "用户手写的应该压过同步来的"
        );
        assert!(
            r(&user, "synced-046D-C539-1001.yaml") < r(builtin, "logitech_gpw.yaml"),
            "同步来的应该压过内置的"
        );
        assert!(
            r(builtin, "logitech_gpw.yaml") < r(&user, "auto-046D-C539-1001.yaml"),
            "内置的应该压过自动发现猜的"
        );
    }

    /// 这条测的不是代码，是**生成脚本和解析端对不对得上**：
    /// tools/build_remote.py 改了缩进或结构，这里第一时间红，
    /// 总好过等真有人点了「检查更新」才发现拉下来解析不了
    #[test]
    fn 仓库里的远程清单能被解析() {
        let text = std::fs::read_to_string("remote/devices.yaml")
            .expect("读不到 remote/devices.yaml，先跑一次 tools/build_remote.py");
        let list = parse_bundle(&text)
            .expect("仓库里那份清单解析不了")
            .profiles;
        assert!(
            list.len() >= 4,
            "清单里至少要有 4 份声明，实际 {}",
            list.len()
        );
        for p in &list {
            assert!(!p.name.is_empty(), "有一份声明没写名字");
            assert!(!p.protocol_source.is_empty(), "{} 没写协议来源", p.name);
        }
    }

    /// 一条声明写出去、再读回来，必须**一模一样**。
    ///
    /// 这是 `to_yaml` 的最高验收标准，而且是拿 `devices/` 下的真文件跑的 ——
    /// 构造数据只能验证「我想到的字段」，真文件验证的是「实际用到的每个字段」。
    ///
    /// 2026-10-27 的事故正是这里漏的：raw 的 `request` / `table` 全丢，
    /// 同步下来的声明落盘即残废，而当时的测试只验证了「文件生成了、内存里份数对了」。
    /// 遍历目录而不是写死文件名，是为了以后加设备自动纳入，不用记得回来补测试。
    #[test]
    fn 内置声明写出去再读回来不丢字段() {
        let mut checked = 0;
        for e in std::fs::read_dir("devices")
            .expect("读不到 devices 目录")
            .flatten()
        {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("yaml") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("读不了声明文件");
            let orig = serde_yml::from_str::<Raw>(&text)
                .unwrap_or_else(|e| panic!("{:?} 解析不了：{e}", path))
                .compile()
                .unwrap_or_else(|e| panic!("{:?} 编译不了：{e}", path));
            let written = to_yaml(&orig, "");
            let back = serde_yml::from_str::<Raw>(&written)
                .unwrap_or_else(|e| panic!("{:?} 写出去之后解析不了：{e}\n{written}", path))
                .compile()
                .unwrap_or_else(|e| panic!("{:?} 写出去之后编译不了：{e}\n{written}", path));
            assert_eq!(
                orig, back,
                "{:?} 往返之后字段变了，to_yaml 有字段没写\n--- 写出来的 ---\n{written}",
                path
            );
            checked += 1;
        }
        assert!(checked >= 4, "只验了 {checked} 份，devices 目录不对？");
    }

    #[test]
    fn raw_经接收器与多行说明都能往返() {
        // 两种以前会静默改值的写法，仓库里那 4 份内置声明都没有，所以以前测不到：
        // ① raw + receiver + 自定义 slots —— to_yaml 曾按 `protocol == Hidpp20` 判断，
        //    raw 的 slots 整行不写，读回来变成默认 1..6
        // ② 说明字段里带换行 —— 曾被替换成空格，一段多行来源说明被压成一行
        let text = "name: \"某耳机\"\
            \nprotocol: \"raw\"\
            \ntransport: \"receiver\"\
            \nmatch:\
            \n  vendor_id: \"0x0951\"\
            \n  product_id: \"0x1723\"\
            \nslots: [2, 3]\
            \nbattery:\
            \n  request: \"21 FF 05\"\
            \n  reply_len: 20\
            \n  state_index: 3\
            \n  value_index: 4\
            \nprotocol_source: \"第一行\\n第二行\"\
            \nverification: \"inferred\"\n";
        let orig = serde_yml::from_str::<Raw>(text)
            .expect("解析不了")
            .compile()
            .expect("编译不了");
        assert_eq!(orig.slots, vec![2, 3], "解析阶段就该保留 slots");

        let written = to_yaml(&orig, "");
        assert!(
            written.contains("slots: [2, 3]"),
            "写出去丢了 slots：\n{written}"
        );
        let back = serde_yml::from_str::<Raw>(&written)
            .expect("写出去的解析不了")
            .compile()
            .expect("写出去的编译不了");
        assert_eq!(orig, back, "往返之后字段变了：\n{written}");
    }

    /// `identify.contains` 是「读到名字要含这串才算命中」，丢了会把同一接收器上的
    /// 别的设备认成这一台。内置声明里没用到这个字段，往返测试覆盖不到，
    /// 所以单独构造一份。
    #[test]
    fn 识别关键字也要往返保留() {
        let text = "devices:\
            \n  - name: \"带关键字的设备\"\
            \n    protocol: \"hid++2.0\"\
            \n    transport: \"receiver\"\
            \n    match:\
            \n      vendor_id: \"0x046D\"\
            \n      product_id: \"0xC539\"\
            \n      usage_page: \"0xFF00\"\
            \n      usage: \"0x0002\"\
            \n    slots: [1, 2, 3, 4, 5, 6]\
            \n    battery:\
            \n      feature: \"0x1001\"\
            \n      kind: \"voltage\"\
            \n    identify:\
            \n      name_feature: \"0x0005\"\
            \n      contains: \"Pro Wireless\"\
            \n    protocol_source: \"公开常量\"\
            \n    verification: \"verified\"\n";
        let orig = first(text);
        // to_yaml 产出的是单份声明，不是 `devices:` 包裹的清单，所以不能走 parse_bundle
        let back = serde_yml::from_str::<Raw>(&to_yaml(&orig, ""))
            .expect("写出来的解析不了")
            .compile()
            .expect("写出来的编译不了");
        assert_eq!(orig, back);
        assert_eq!(back.identify_contains.as_deref(), Some("Pro Wireless"));
        assert_eq!(back.usage, Some(0x0002));
        assert_eq!(back.usage_page, Some(0xFF00));
    }

    #[test]
    fn 清单里有一份坏的不影响其它() {
        let text = "devices:\
            \n  - name: \"坏设备\"\
            \n    protocol: \"瞎写的协议\"\
            \n    match:\
            \n      vendor_id: \"0x046D\"\
            \n      product_id: \"0xC539\"\
            \n    battery:\
            \n      kind: \"voltage\"\
            \n  - name: \"好设备\"\
            \n    protocol: \"ble\"\
            \n    match:\
            \n      vendor_id: \"0x045E\"\
            \n      product_id: \"0x0B13\"\
            \n    battery:\
            \n      kind: \"percent\"\n";
        let list = parse_bundle(text).expect("还剩一份能用的，不该整个失败");
        assert_eq!(list.rejected, 1);
        assert_eq!(list.profiles.len(), 1);
        assert_eq!(list.profiles[0].name, "好设备");
    }

    #[test]
    fn 字段类型写错也只跳过那一份() {
        // reply_len 写成带引号的字符串（字段要整数）。这类**类型**错误以前会让
        // 整份清单解析失败，于是所有人一台都同步不到 —— 和「坏一份跳一份」的承诺
        // 正好相反。parse_bundle 因此改成分两段解析，这条测试守着它
        let text = "devices:\
            \n  - name: \"字段类型错的\"\
            \n    protocol: \"raw\"\
            \n    transport: \"direct\"\
            \n    match:\
            \n      vendor_id: \"0x0951\"\
            \n      product_id: \"0x1723\"\
            \n    battery:\
            \n      request: \"21 FF 05\"\
            \n      reply_len: \"20\"\
            \n      value_index: 4\
            \n  - name: \"好设备\"\
            \n    protocol: \"ble\"\
            \n    match:\
            \n      vendor_id: \"0x045E\"\
            \n      product_id: \"0x0B13\"\
            \n    battery:\
            \n      kind: \"percent\"\n";
        let list = parse_bundle(text).expect("类型错的那一份不该拖垮整份清单");
        assert_eq!(list.rejected, 1);
        assert_eq!(list.profiles.len(), 1);
        assert_eq!(list.profiles[0].name, "好设备");
    }

    #[test]
    fn 报文长度过大或下标越界会被拒() {
        // raw.rs 每轮都按 reply_len 分配缓冲，多打几个 0 就是每轮分配几个 GB；
        // 而 Rust 分配失败是 abort —— 一份写坏的声明不该能把程序带走
        let big = RawBattery {
            feature: None,
            kind: "percent".into(),
            request: Some("21 FF 05".into()),
            reply_len: Some(4_000_000_000),
            state_index: None,
            value_index: Some(4),
            charging_states: None,
            table: None,
        };
        assert!(big.compile_spec().is_err());

        // 下标超出响应长度不会崩，只会让设备永远静默读不到 —— 比崩更难查，
        // 所以在编译期就拒掉
        let oob = RawBattery {
            feature: None,
            kind: "percent".into(),
            request: Some("21 FF 05".into()),
            reply_len: Some(20),
            state_index: None,
            value_index: Some(30),
            charging_states: None,
            table: None,
        };
        assert!(oob.compile_spec().is_err());

        // 正常的一份不能受影响
        let ok = RawBattery {
            feature: None,
            kind: "percent".into(),
            request: Some("21 FF 05".into()),
            reply_len: Some(20),
            state_index: Some(3),
            value_index: Some(4),
            charging_states: None,
            table: None,
        };
        let spec = ok.compile_spec().expect("20 字节、下标 3/4 是合法的");
        assert_eq!(
            (spec.reply_len, spec.state_index, spec.value_index),
            (20, 3, 4)
        );
    }

    #[test]
    fn 空清单要报错而不是当成零台设备() {
        assert!(parse_bundle("devices: []").is_err());
        // devices 后面什么都没有 —— 生成脚本万一写出这种东西，也得是错的
        assert!(parse_bundle("devices:").is_err());
    }

    #[test]
    fn 整个文件不是清单时要报错() {
        assert!(parse_bundle("这根本不是 YAML：[[[").is_err());
        // 单份声明文件（没有 devices: 那一层）也不能被当成空清单蒙混过关
        assert!(parse_bundle("name: \"x\"\n").is_err());
    }

    #[test]
    fn 文件名主体带电量feature() {
        let mut p = ble();
        p.vid = 0x046D;
        p.pid = 0xC539;
        p.battery_feature = Some(0x1001);
        assert_eq!(slug(&p), "046D-C539-1001");
        // 没有 feature 的设备（比如蓝牙自报百分比的）不能多个尾巴
        p.battery_feature = None;
        assert_eq!(slug(&p), "046D-C539");
    }
}
