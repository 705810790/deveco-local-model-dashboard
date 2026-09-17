//! 编译脚本：读取项目根目录的 `VERSION` 文件，注入为编译期环境变量
//! `APP_VERSION`，供源码通过 `env!("APP_VERSION")` 读取。
//!
//! 这样更新版本号只需修改 `VERSION` 文件，无需改动任何源码。
//!
//! 同时用 `cargo:rerun-if-changed=VERSION` 确保 VERSION 文件变更时触发重新编译。

use std::fs;
use std::path::PathBuf;

fn main() {
    let version_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("VERSION");

    let version = fs::read_to_string(&version_path)
        .expect("无法读取 VERSION 文件")
        .trim()
        .to_string();

    assert!(
        !version.is_empty(),
        "VERSION 文件内容为空，请填写版本号（如 2.0.2）"
    );

    println!("cargo:rustc-env=APP_VERSION={}", version);
    // 覆盖 CARGO_PKG_VERSION，使 Cargo.toml 的 version 字段也跟随 VERSION 文件。
    println!("cargo:rustc-env=CARGO_PKG_VERSION={}", version);
    println!("cargo:rerun-if-changed=VERSION");
}
