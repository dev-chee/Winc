//! 构建脚本：把应用图标嵌入 Windows 的 `winc.exe`，文件管理器、文件关联和任务栏都从这里取图标。

use std::env::var;
use std::path::Path;

/// 由 `cargo xtask icons` 生成并提交的图标资源，路径相对于根包。
const ICON_RESOURCE: &str = "crates/winc-app/assets/winc.res";

fn main() {
    println!("cargo:rerun-if-changed={ICON_RESOURCE}");

    let msvc = var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows")
        && var("CARGO_CFG_TARGET_ENV").is_ok_and(|env| env == "msvc");
    if msvc {
        let resource = Path::new(env!("CARGO_MANIFEST_DIR")).join(ICON_RESOURCE);
        println!("cargo:rustc-link-arg-bin=winc={}", resource.display());
    }
}
