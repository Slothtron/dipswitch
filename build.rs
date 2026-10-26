//! Windows 下把 `assets/app.ico` 编进 exe 资源，让 Explorer、任务栏与 Alt-Tab
//! 显示应用自己的图标。
//!
//! 边界：本脚本**只消费**已生成的 `app.ico`，不参与任何静态资源的生成——生成是
//! 维护者的显式步骤（`python tools/make-icon.py`），不进构建，否则构建就会依赖
//! Python 与一个 SVG 光栅器，与「离线构建」的验收项冲突。
//!
//! 为什么走 `rc.exe` 而不是 `embed-resource`：多引一个构建依赖就要多赌一次 crates
//! 缓存；而 `rc.exe` 与 MSVC 工具链同属 Windows SDK（SQLite 的 `bundled` 已经依赖
//! 这套）。
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
        println!(
            "cargo:warning=找不到 Windows SDK 的 rc.exe，跳过 exe 图标。\
             设 DIPSWITCH_RC 指向 rc.exe，或安装 Windows 10/11 SDK"
        );
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

/// 定位 `rc.exe`，不写死任何机器专属路径。顺序：
/// 1. `DIPSWITCH_RC` —— 显式指定，CI 与非标准安装用；
/// 2. `WindowsSdkDir` —— 只在 Developer Command Prompt 里存在；
/// 3. 注册表 `KitsRoot10` —— SDK 安装时写入，普通 shell 下唯一可靠的来源；
/// 4. `PATH`。
fn find_rc() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("DIPSWITCH_RC") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
    }
    for root in sdk_roots() {
        if let Some(found) = newest_rc_under(&root.join("bin")) {
            return Some(found);
        }
    }
    Some(PathBuf::from("rc.exe"))
}

fn sdk_roots() -> impl Iterator<Item = PathBuf> {
    let from_env = std::env::var_os("WindowsSdkDir").map(PathBuf::from);
    let from_registry = registry_kits_root();
    from_env.into_iter().chain(from_registry)
}

/// `HKLM\...\Windows Kits\Installed Roots` 的 `KitsRoot10`。64 位系统上 SDK 键
/// 落在 WOW6432Node 视图下，两个都试。
fn registry_kits_root() -> Option<PathBuf> {
    const KEYS: [&str; 2] = [
        r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows Kits\Installed Roots",
        r"HKLM\SOFTWARE\Microsoft\Windows Kits\Installed Roots",
    ];
    for key in KEYS {
        let Ok(output) = Command::new("reg")
            .args(["query", key, "/v", "KitsRoot10"])
            .output()
        else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        // 输出是 `KitsRoot10    REG_SZ    C:\...\10\`，按 REG_SZ 切分取尾部；
        // 这样不受控制台代码页影响（值本身是 ASCII 路径）。
        let text = String::from_utf8_lossy(&output.stdout);
        if let Some(root) = text
            .lines()
            .find_map(|line| line.split_once("REG_SZ"))
            .map(|(_, value)| value.trim().trim_end_matches(['\\', '"']))
            .filter(|value| !value.is_empty())
        {
            let path = PathBuf::from(root);
            if path.is_dir() {
                return Some(path);
            }
        }
    }
    None
}

/// SDK 的 `bin` 下会并存多个版本目录，取最高的那个里的 `x64\rc.exe`。
fn newest_rc_under(bin: &Path) -> Option<PathBuf> {
    let Ok(entries) = std::fs::read_dir(bin) else {
        return None;
    };
    let mut versions: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".0"))
        .collect();
    // 版本号是点分数，按段做数值比较，避免 10.0.99 排在 10.0.22621 之后。
    versions.sort_by_key(|version| {
        version
            .split('.')
            .map(|part| part.parse::<u32>().unwrap_or(0))
            .collect::<Vec<_>>()
    });
    versions
        .iter()
        .rev()
        .map(|version| bin.join(version).join("x64").join("rc.exe"))
        .find(|candidate| candidate.is_file())
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
