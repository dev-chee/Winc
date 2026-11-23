//! 产品版本号。
//!
//! `winc` 和 `winc-version` 从根 `Cargo.toml` 的 `[workspace.package]` 继承版本号。
//! 发布脚本 `release/common/version.mjs` 读取同一个字段。

#[cfg(test)]
mod tests;

/// 产品版本号，例如 `0.1.592`。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
