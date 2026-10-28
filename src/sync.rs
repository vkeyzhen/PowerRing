//! 远程设备声明同步（托盘菜单里的「检查设备声明更新…」）
//!
//! 这是整个程序**唯一**会联网的地方，而且只有用户点了菜单才会跑。
//! 改这个模块时别破下面三条：
//!
//! 1. **失败必须无害。** 拉不到就是这次没更新，已经认出的设备照常显示。
//!    不弹错误框、不卡启动、不碰自动发现写下的那份声明。
//!    理由是可达性根本不由我们决定：同一台机器上一分钟通、下一分钟不通都常见
//!    （实测过：连 rust-lang 的 raw 都会从 200 变成超时），把网络波动做成
//!    「报错」只会让人以为程序坏了。
//! 2. **拉到的东西是不可信输入。** 逐份走声明编译的校验，坏一份跳一份，
//!    绝不让一份写坏的声明把整次同步拖垮 —— 那等于远程写错一次，
//!    所有人的「认新设备」功能一起失效。
//! 3. **不引 HTTP 依赖。** 用系统自带的 WinHTTP。为一个一年点不了几次的功能
//!    往里塞一个 HTTP 客户端（连带一整套 TLS 栈）不划算。

use crate::{info, warn};
use std::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_OPEN_REQUEST_FLAGS,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
};

/// 主源：仓库里的聚合清单（由 tools/build_remote.py 生成）
const PRIMARY: &str =
    "https://raw.githubusercontent.com/vkeyzhen/PowerRing/main/remote/devices.yaml";
/// 备源：jsdelivr 对 GitHub 仓库做的 CDN 镜像，国内可达性通常更好。
///
/// 用 `@main` 而不是某个 tag 是刻意的：jsdelivr 对 tag 是**永久缓存**，
/// 打了 tag 之后再加的设备永远拉不到，同步就白做了。分支有约 12 小时缓存，
/// 对「加一台设备」这种低频事完全够用。
const MIRROR: &str = "https://cdn.jsdelivr.net/gh/vkeyzhen/PowerRing@main/remote/devices.yaml";

/// 每个阶段都设同一个超时。连不上的地址能卡几十秒，
/// 而在用户看来「点了没反应」和「程序卡死」是一回事
const TIMEOUT_MS: i32 = 8_000;
/// 清单大小上限。不是防攻击，是防「拉下来的其实是个 HTML 错误页」——
/// 一份设备声明清单不可能有 1 MB
const MAX_BYTES: usize = 1024 * 1024;

/// 一次同步的结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// 远程清单里一共多少份
    pub total: usize,
    /// 校验没过、被跳过的份数。
    /// 非 0 意味着这份清单不完整：本机没拿到的那几台是远程写错了，
    /// 不是远程把它们撤回了 —— 所以本次一律不清理本地旧声明
    pub rejected: usize,
    /// 新写进去的份数
    pub added: usize,
    /// 内容发生了变化的份数
    pub updated: usize,
    /// 拉到了、也解析了，但没能写进本机磁盘的份数。
    /// 单独留一格是为了让界面能说实话：全失败时 added/updated 都是 0，
    /// 若没有这一格，「一份都没写进去」和「全都已经是最新」长得一模一样
    pub failed: usize,
    /// 最终用哪个地址拉到的。日志和提示里都要说清，否则排查时无从下手
    pub source: String,
}

/// 拉一次远程清单并落盘。
///
/// `custom` 是用户自己在设置里填的地址，填了就排在最前面（见 sources 的注释）。
/// 返回 Err 表示**一个源都没成**，原因放在字符串里 —— 调用方只管把它显示出来，
/// 不要据此认为程序坏了：拉不到是常态之一。
pub fn run(custom: Option<&str>) -> Result<Outcome, String> {
    let mut last = String::from("没有可用的地址");
    for url in sources(custom) {
        info!("[sync] 尝试 {url}");
        match fetch(&url).and_then(|body| apply(&body, &url)) {
            Ok(out) => {
                info!(
                    "[sync] 成功：{url} 共 {} 份，新增 {} 更新 {} 写不进 {}",
                    out.total, out.added, out.updated, out.failed
                );
                return Ok(out);
            }
            Err(e) => {
                warn!("[sync] {url} 不行：{e}");
                last = e;
            }
        }
    }
    Err(last)
}

/// 尝试顺序：自定义地址 → 主源 → 备源。
///
/// 自定义排第一是因为填它的人**多半正是主源不通的那批**（国内连不上 GitHub），
/// 让他先干等一轮公共地址的超时再去试能通的那个，纯属浪费时间。
fn sources(custom: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(c) = custom {
        let t = c.trim();
        if !t.is_empty() {
            out.push(t.to_string());
        }
    }
    out.push(PRIMARY.to_string());
    out.push(MIRROR.to_string());
    out
}

fn apply(body: &str, source: &str) -> Result<Outcome, String> {
    let bundle = crate::profile::parse_bundle(body)?;
    // 校验失败的那几份也算进总数：界面上说「远程 6 份，写入 5 份」才对得上账，
    // 只报 5 会让用户以为远程本来就只有 5 台
    let total = bundle.profiles.len() + bundle.rejected;
    // 只有拉的是项目官方清单时才按它清理本地已有的那份。
    // 用户自填的镜像往往只含他自己那几台 —— 拿它去清理，会把官方清单同步来的
    // 其它设备声明一起删掉，那些设备于是从面板上消失
    let prune = source == PRIMARY || source == MIRROR;
    let (added, updated, failed) = crate::profile::save_synced(&bundle, prune);
    Ok(Outcome {
        total,
        rejected: bundle.rejected,
        added,
        updated,
        failed,
        source: source.to_string(),
    })
}

/// 拆 URL 成（是否 https，主机，端口，路径）。只认 http/https，
/// 不打算支持别的 —— 拉个文本文件而已
fn split_url(url: &str) -> Result<(bool, &str, u16, &str), String> {
    let (secure, rest) = url
        .strip_prefix("https://")
        .map(|r| (true, r))
        .or_else(|| url.strip_prefix("http://").map(|r| (false, r)))
        .ok_or_else(|| format!("地址要以 https:// 开头：{url}"))?;
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    // 允许 host:port 的写法，镜像地址不一定跑在标准端口上
    let (host, port) = match host.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().map_err(|_| format!("端口号不对：{p}"))?),
        None => (host, if secure { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err(format!("地址里没有主机名：{url}"));
    }
    Ok((secure, host, port, path))
}

fn fetch(url: &str) -> Result<String, String> {
    let (secure, host, port, path) = split_url(url)?;
    unsafe {
        let agent = wide("PowerRing");
        // AUTOMATIC_PROXY：装了代理软件的人，他们的系统代理会被用上。
        // 这批人恰恰是最可能连得上 GitHub 的，不认他们的代理等于白搭一个源
        let session = WinHttpOpen(
            PCWSTR(agent.as_ptr()),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        );
        if session.is_null() {
            return Err("打不开 WinHTTP 会话".to_string());
        }
        let host_w = wide(host);
        let conn = WinHttpConnect(session, PCWSTR(host_w.as_ptr()), port, 0);
        if conn.is_null() {
            let _ = WinHttpCloseHandle(session);
            return Err(format!("连不上 {host}"));
        }
        let verb = wide("GET");
        let path_w = wide(path);
        let flags = if secure {
            WINHTTP_FLAG_SECURE
        } else {
            WINHTTP_OPEN_REQUEST_FLAGS(0)
        };
        let req = WinHttpOpenRequest(
            conn,
            PCWSTR(verb.as_ptr()),
            PCWSTR(path_w.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            flags,
        );
        if req.is_null() {
            let _ = WinHttpCloseHandle(conn);
            let _ = WinHttpCloseHandle(session);
            return Err("建不了请求".to_string());
        }
        let _ = WinHttpSetTimeouts(req, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS);
        // 三个句柄都要关，所以把正文读出来这一步单独装进函数，
        // 不论它成功失败，关句柄那几行都跑得到
        let out = read_body(req);
        let _ = WinHttpCloseHandle(req);
        let _ = WinHttpCloseHandle(conn);
        let _ = WinHttpCloseHandle(session);
        out
    }
}

unsafe fn read_body(req: *mut c_void) -> Result<String, String> {
    WinHttpSendRequest(req, None, None, 0, 0, 0).map_err(|e| format!("发不出请求：{e}"))?;
    WinHttpReceiveResponse(req, std::ptr::null_mut()).map_err(|e| format!("收不到响应：{e}"))?;

    let mut status: u32 = 0;
    let mut len = std::mem::size_of::<u32>() as u32;
    let mut index: u32 = 0;
    // 必须带 FLAG_NUMBER：不带的话 WinHTTP 给回来的是字符串 "200" 而不是数值，
    // 拿它当 u32 读会读出一个天文数字
    WinHttpQueryHeaders(
        req,
        WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
        PCWSTR::null(),
        Some(&mut status as *mut u32 as *mut c_void),
        &mut len,
        &mut index,
    )
    .map_err(|e| format!("读不到状态码：{e}"))?;
    if status != 200 {
        return Err(format!("服务器返回 {status}"));
    }

    let mut body: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let mut read: u32 = 0;
        WinHttpReadData(
            req,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            &mut read,
        )
        .map_err(|e| format!("读响应失败：{e}"))?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&buf[..read as usize]);
        if body.len() > MAX_BYTES {
            return Err("拉下来的东西太大，不像设备清单".to_string());
        }
    }
    String::from_utf8(body).map_err(|_| "拉下来的不是 UTF-8 文本".to_string())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 拆出主机与路径() {
        let (secure, host, port, path) =
            split_url("https://raw.githubusercontent.com/a/b/devices.yaml").unwrap();
        assert!(secure);
        assert_eq!(host, "raw.githubusercontent.com");
        assert_eq!(port, 443);
        assert_eq!(path, "/a/b/devices.yaml");
    }

    #[test]
    fn 带端口的地址能拆出来() {
        let (secure, host, port, path) = split_url("http://127.0.0.1:8080/x.yaml").unwrap();
        assert!(!secure);
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 8080);
        assert_eq!(path, "/x.yaml");
    }

    #[test]
    fn 根目录也算合法路径() {
        let (_, _, _, path) = split_url("https://example.com").unwrap();
        assert_eq!(path, "/");
    }

    #[test]
    fn 非http地址一律不认() {
        assert!(split_url("ftp://example.com/a").is_err());
        assert!(split_url("example.com/a").is_err());
    }

    #[test]
    fn 自定义地址排在最前() {
        let s = sources(Some("https://my.mirror/devices.yaml"));
        assert_eq!(s.len(), 3);
        assert_eq!(s[0], "https://my.mirror/devices.yaml");
        assert_eq!(s[1], PRIMARY);
        assert_eq!(s[2], MIRROR);
    }

    #[test]
    fn 没填自定义就只有两个公共源() {
        let s = sources(None);
        assert_eq!(s, vec![PRIMARY.to_string(), MIRROR.to_string()]);
        // 空白也算没填：用户留了个空格不该变成一个会超时的地址
        assert_eq!(sources(Some("   ")), s);
    }

    #[test]
    fn 两个公共源都必须是https() {
        for u in [PRIMARY, MIRROR] {
            let (secure, _, _, _) = split_url(u).unwrap();
            assert!(secure, "{u} 不是 https");
        }
    }
}
