//! 配置：%APPDATA%\PowerRing\config.json
//! 原则（PRD F11.3）：缺字段或字段非法时回退默认值并继续运行，绝不因配置问题崩溃。
//!
//! 两条边界写在这里，因为都踩过：
//!
//! 1. **解析失败不许直接覆盖原文件。** 这份 json 是留给用户手改的，手改打错一个逗号
//!    （或断电留下半截文件）时，若「回退默认值 + 立刻回写」，等于把写坏的那份证据当场
//!    抹掉，之后谁也说不清原来写的是什么。所以先改名成 `config.bad.json` 再写新的。
//! 2. **范围校验不能只做在保存路径上。** 设置窗口保存时会夹取，但手改文件绕过了那一道，
//!    所以读进来之后再夹一遍 —— 否则 `poll_interval_secs: 4294967295` 会让轮询线程睡上
//!    一百多年，而界面上完全看不出异常。

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// 轮询间隔的下界。与 `main.rs` 里那道 `.max()` 保持一致，两处都拦一道
pub const MIN_INTERVAL_SECS: u32 = 5;
/// 轮询间隔的上界。再长就不是「省电」而是「不工作了」——一小时已经远超任何合理用途
pub const MAX_INTERVAL_SECS: u32 = 3600;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 轮询间隔（秒）
    pub poll_interval_secs: u32,
    /// 低电量警告阈值
    pub low_warn: u8,
    /// 低电量严重阈值
    pub low_critical: u8,
    /// 低电量时弹卡片提醒。关掉后阈值仍驱动电量环配色，只是不再主动弹
    pub notify_enabled: bool,
    pub language: String,
    pub theme: String,
    pub autostart: bool,
    /// 「检查设备声明更新…」从哪里拉清单。留空就用程序内置的两个地址
    /// （GitHub raw，外加 jsdelivr 备源）。
    /// 只有连不上 GitHub 的人才需要填 —— 填一个自己那边能通的镜像。
    /// 这个字段存在的理由：可达性不由我们决定，得给走投无路的人留一条自救的路
    pub remote_devices_url: String,
    pub update_channel: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            poll_interval_secs: 60,
            low_warn: 20,
            low_critical: 10,
            notify_enabled: true,
            language: "zh-CN".into(),
            theme: "system".into(),
            autostart: false,
            remote_devices_url: String::new(),
            update_channel: "stable".into(),
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        let base = std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        base.join("PowerRing").join("config.json")
    }

    pub fn load() -> Self {
        let p = Self::path();
        let text = match fs::read_to_string(&p) {
            Ok(t) => t,
            // 第一次运行没有这个文件是正常状态，不必 warn
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let cfg = Self::default();
                let _ = cfg.save();
                return cfg;
            }
            // 读不出来（权限、占用）也照常运行，只是这次不入库
            Err(e) => {
                crate::warn!("[config] 读不出来，这次用默认值：{e}");
                return Self::default();
            }
        };

        match serde_json::from_str::<Config>(&text) {
            Ok(cfg) => {
                // 手改留下的越界值在这里夹回去，夹过的值随下面的回写落盘
                let cfg = cfg.sanitized();
                // 回写一次完整配置，用户照着手改也能用（缺字段自动补成默认值）
                let _ = cfg.save();
                cfg
            }
            Err(e) => {
                // 先留一份证据再退回默认值，绝不静默覆盖用户手写的内容
                let bad = p.with_extension("bad.json");
                let kept = fs::rename(&p, &bad).is_ok() || fs::copy(&p, &bad).is_ok();
                if kept {
                    crate::warn!(
                        "[config] 解析失败（{e}），原文件已留一份在 {}，这次用默认值",
                        bad.display()
                    );
                } else {
                    crate::warn!("[config] 解析失败（{e}），备份也没成功，这次用默认值");
                }
                let cfg = Self::default();
                let _ = cfg.save();
                cfg
            }
        }
    }

    /// 把越界或自相矛盾的值夹回可用范围。
    ///
    /// 严重阈值不许高于警告阈值，否则「严重」这一档永远不会触发
    /// （环色按严重档判断在前，数值乱了会让配色跳档）。
    fn sanitized(mut self) -> Self {
        self.poll_interval_secs = self
            .poll_interval_secs
            .clamp(MIN_INTERVAL_SECS, MAX_INTERVAL_SECS);
        self.low_warn = self.low_warn.clamp(1, 100);
        self.low_critical = self.low_critical.clamp(1, self.low_warn);
        self
    }

    /// 写回磁盘。
    ///
    /// 返回 `Err` 是为了让设置窗口能如实告诉用户「这次没存上」——
    /// 磁盘满、`%APPDATA%` 只读（组策略/漫游配置）时，旧行为是照样显示「已保存」，
    /// 于是用户以为改好了，重启后全部回到旧值。
    /// 启动路径（`load` 里的回写）不关心结果，忽略即可。
    pub fn save(&self) -> Result<(), String> {
        let p = Self::path();
        if let Some(dir) = p.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let s = serde_json::to_string_pretty(self).map_err(|e| format!("序列化失败：{e}"))?;
        fs::write(&p, s).map_err(|e| format!("写入 {} 失败：{e}", p.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_intervals(secs: u32, warn: u8, crit: u8) -> Config {
        Config {
            poll_interval_secs: secs,
            low_warn: warn,
            low_critical: crit,
            ..Config::default()
        }
    }

    #[test]
    fn 间隔被夹进合法范围() {
        assert_eq!(with_intervals(0, 20, 10).sanitized().poll_interval_secs, 5);
        assert_eq!(
            with_intervals(u32::MAX, 20, 10)
                .sanitized()
                .poll_interval_secs,
            3600
        );
        // 区间内的值原样保留
        assert_eq!(
            with_intervals(120, 20, 10).sanitized().poll_interval_secs,
            120
        );
    }

    #[test]
    fn 严重阈值不许高于警告阈值() {
        let c = with_intervals(60, 10, 90).sanitized();
        assert_eq!((c.low_warn, c.low_critical), (10, 10));
        // 正常配置不动
        let c = with_intervals(60, 20, 10).sanitized();
        assert_eq!((c.low_warn, c.low_critical), (20, 10));
    }

    #[test]
    fn 阈值零会被抬到一() {
        let c = with_intervals(60, 0, 0).sanitized();
        assert_eq!((c.low_warn, c.low_critical), (1, 1));
    }
}
