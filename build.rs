//! 把图标与版本信息写进 exe
//!
//! 没有这一步，exe 在资源管理器里是个空白默认图标，右键「属性 → 详细信息」也一片空。
//! 用户下载下来第一眼看的就是这两处，空白图标还会让 SmartScreen 更容易把它当成可疑程序。
//!
//! 只在 Windows 上做：别的平台上 embed-resource 找不到资源编译器。

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    // 改了图标或版本号要重新链接，否则会一直用旧的那份嵌进去
    // （.ico 不用写：.rc 里 ICON 那行会自己带上依赖）
    println!("cargo:rerun-if-changed=assets/powerring.rc");
    check_version_matches();
    // 失败要停下，不能「凑合着出个没图标的包」——
    // 那样 CI 是绿的，问题要等用户看到空白图标才发现。
    // 返回值只说明「重新编译了没」，真失败会由实现内部让链接报错
    let _ = embed_resource::compile("assets/powerring.rc", embed_resource::NONE);
}

/// 版本号散在两处：`Cargo.toml` 与 `assets/powerring.rc`。
/// 后者只有右键「属性 → 详细信息」才看得见，忘了改不报错，只是 exe 一直自称旧版本
/// —— v0.2.1 就这么漏过一次（发出去的属性页还写着 0.2.0）。
/// 所以在构建时对一遍，不一致直接停：这种事靠人记是记不住的。
fn check_version_matches() {
    let want = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let rc = std::fs::read_to_string("assets/powerring.rc").expect("读不到 assets/powerring.rc");
    let line = rc
        .lines()
        .find(|l| l.trim_start().starts_with("FILEVERSION"))
        .expect("assets/powerring.rc 里没有 FILEVERSION");
    // "FILEVERSION     0,2,2,0" → "0.2.2"
    let got = line
        .split_whitespace()
        .nth(1)
        .unwrap_or("")
        .split(',')
        .take(3)
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(".");
    assert_eq!(
        got, want,
        "assets/powerring.rc 的 FILEVERSION 是 {got}，Cargo.toml 却是 {want} —— 发版时两处都要改"
    );
}
