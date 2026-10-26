//! Windows 下把 `assets/app.ico` 编进 exe 资源，让 Explorer、任务栏与 Alt-Tab
//! 显示应用自己的图标。
//!
//! 为什么走 `rc.exe` 而不是 `embed-resource`：本项目把**离线构建**当验收项，
//! 多引一个构建依赖就要多赌一次 crates 缓存；而 Windows SDK 的 `rc.exe` 与
//! MSVC 工具链本来就装好了（SQLite 的 `bundled` 已经依赖这套）。
//!
//! 图标是观感项，不该让构建失败：找不到 `rc.exe` 或缺 `.ico` 时跳过并留一条
//! warning，产物仍然可用，只是没有自定义图标。

use std::path::{Path, PathBuf};
use std::process::Command;

const RC_SOURCE: &str = "assets/windows.rc";
const ICO_SOURCE: &str = "assets/app.ico";

fn main() {
    println!("cargo:rerun-if-changed={RC_SOURCE}");
    println!("cargo:rerun-if-changed={ICO_SOURCE}");
    println!("cargo:rerun-if-changed=assets/brand.svg");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("由 cargo 提供"));
    let rc = manifest.join(RC_SOURCE);
    if !rc.is_file() || !manifest.join(ICO_SOURCE).is_file() {
        println!(
            "cargo:warning=缺少 {ICO_SOURCE}，跳过 exe 图标（先跑 python tools/make-icon.py）"
        );
        return;
    }
    let Some(compiler) = find_rc() else {
        println!("cargo:warning=找不到 Windows SDK 的 rc.exe，跳过 exe 图标");
        return;
    };
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("由 cargo 提供"));
    let resource = out_dir.join("app.res");
    match compile_icon(&compiler, &rc, &resource) {
        Ok(()) => {
            // link.exe 接受 .res 作为输入，把它交给二进制目标即可。
            println!("cargo:rustc-link-arg-bins={}", resource.display());
        }
        Err(message) => println!("cargo:warning=编译图标资源失败：{message}"),
    }
}

/// 在 Windows SDK 与 PATH 里找 `rc.exe`。SDK 下可能并存多个版本目录，取最高的。
fn find_rc() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(sdk) = std::env::var_os("WindowsSdkDir") {
        let bin = Path::new(&sdk).join("bin");
        candidates.extend(from_bin_root(&bin));
        if let Ok(version) = std::env::var("WindowsSdkVersion") {
            candidates.push(bin.join(&version).join("x64").join("rc.exe"));
        }
    }
    for root in [
        r"C:\Program Files (x86)\Windows Kits\10\bin",
        r"C:\Program Files\Windows Kits\10\bin",
    ] {
        candidates.extend(from_bin_root(Path::new(root)));
    }
    candidates.push(PathBuf::from("rc.exe"));
    candidates.into_iter().find(|path| path.is_file())
}

fn from_bin_root(bin: &Path) -> impl Iterator<Item = PathBuf> {
    let Ok(entries) = std::fs::read_dir(bin) else {
        return Vec::new().into_iter();
    };
    let mut versions: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .ends_with(".0")
                .then(|| entry.file_name().to_string_lossy().into_owned())
        })
        .collect();
    // 版本号是点分数，按段做数值比较，避免 10.0.99 排在 10.0.100 之后。
    versions.sort_by_key(|version| {
        version
            .split('.')
            .map(|part| part.parse::<u32>().unwrap_or(0))
            .collect::<Vec<_>>()
    });
    versions
        .into_iter()
        .rev()
        .map(|version| bin.join(version).join("x64").join("rc.exe"))
        .collect::<Vec<_>>()
        .into_iter()
}

fn compile_icon(compiler: &Path, rc: &Path, resource: &Path) -> Result<(), String> {
    let output = Command::new(compiler)
        .arg("/nologo")
        .arg("/fo")
        .arg(resource)
        .arg(rc)
        .output()
        .map_err(|error| format!("无法启动 {}: {error}", compiler.display()))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).into_owned())
    }
}
