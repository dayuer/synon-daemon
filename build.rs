//! build.rs — 编译时注入 BUILD_VERSION 环境变量
//!
//! 版本号格式: YYYYMMDD.HHmmss（如 20260405.223000）
//! 用途: 每次编译自动递增，控制 self_updater 新旧二进制替换判定。
//!
//! 通过 `env!("BUILD_VERSION")` 在 Rust 代码中读取。

use std::process::Command;

fn main() {
    // 用 date 命令（跨平台兼容 Linux/macOS）生成时间戳版本号
    let build_version = Command::new("date")
        .arg("+%Y%m%d.%H%M%S")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "00000000.000000".to_string());

    println!("cargo:rustc-env=BUILD_VERSION={build_version}");

    // 仅在源码或 Cargo.toml 变化时重新运行（避免每次 cargo check 都重跑）
    // 注意：这里不设 rerun-if-changed，确保每次 cargo build 都更新版本号
    // 如果想只在代码变更时更新，可取消注释下面这行：
    // println!("cargo:rerun-if-changed=src/");
}
