//! Windows 下把应用图标与版本信息编进 exe 资源，让 Explorer、任务栏、Alt-Tab 与
//! 文件属性对话框显示应用自己的信息。
//!
//! 做法照搬 Zed 的 `crates/windows_resources`：build 期在 `OUT_DIR` 里**生成** `.rc`
//! （不提交手写 `.rc`），再用 `embed-resource` 编译。选它而不是自己找 `rc.exe` 的
//! 理由很实际：它已经实现了 `KitsRoot10` 注册表 → `vswhom` → `PATH` 的发现链，
//! 支持 GNU 工具链走 `windres`，并且认 `RC` / `RC_<target>` 环境变量做覆盖。
//!
//! 两处刻意与 Zed 不同，都写在下面对应位置：
//! 1. **不写 manifest 行**——`gpui-pre` 自己的 build.rs 已经嵌了 `1 RT_MANIFEST`
//!    （实测产物里有 `dpiAware`），再来一份同 ID 同类型的资源就是自相冲突；
//! 2. **失败不 panic**——Zed 用 `.unwrap()`，因为图标对它是发布要求；本项目把图标
//!    当观感项，缺 SDK 时应该照常构建出可用的 exe，所以降级成 `cargo:warning`。
//!
//! 边界：本脚本只**消费**已生成的 `assets/app.ico`，不参与资源生成。生成是维护者的
//! 显式步骤（`python tools/make-icon.py`），否则构建就要依赖 Python 加一个 SVG
//! 光栅器，与「离线构建」的验收项冲突。改了 `assets/brand.svg` 必须重跑该脚本。

use std::path::PathBuf;

const ICO_SOURCE: &str = "assets/app.ico";

fn main() {
    println!("cargo:rerun-if-changed={ICO_SOURCE}");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    match embed() {
        Ok(()) => {}
        Err(message) => println!("cargo:warning=未能嵌入 exe 资源（图标与版本信息）：{message}"),
    }
}

fn embed() -> Result<(), String> {
    let manifest = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").map_err(|_| "缺 CARGO_MANIFEST_DIR".to_string())?,
    );
    let icon = manifest.join(ICO_SOURCE);
    if !icon.is_file() {
        return Err(format!("缺少 {ICO_SOURCE}，先跑 python tools/make-icon.py"));
    }
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").map_err(|_| "缺 OUT_DIR".to_string())?);

    // rc.exe 默认按 ANSI 代码页解码 .rc；仓库路径含非 ASCII 字符时它会找不到被引用的
    // 文件。声明 UTF-8 代码页，与 Zed 同一处理。
    let rc_path = out_dir.join("dipswitch.rc");
    std::fs::write(&rc_path, rc_file(&icon)).map_err(|error| error.to_string())?;

    embed_resource::compile(&rc_path, embed_resource::NONE)
        .manifest_optional()
        .map_err(|failure| format!("{failure:?}"))
}

/// 生成 `.rc` 内容。路径里的反斜杠要双写，否则在 rc 的字符串字面量里成了转义。
fn rc_file(icon: &std::path::Path) -> String {
    let icon_escaped = icon.to_string_lossy().replace('\\', "\\\\");
    let version = pkg_version();
    let numeric = file_version(&version);
    format!(
        r#"#pragma code_page(65001)
1 ICON "{icon_escaped}"

1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "FileDescription", "dipswitch\0"
            VALUE "FileVersion", "{version}\0"
            VALUE "ProductName", "dipswitch\0"
            VALUE "ProductVersion", "{version}\0"
            VALUE "LegalCopyright", "Copyright 2026 Slothtron\0"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x0409, 1200
    END
END
"#
    )
}

fn pkg_version() -> String {
    std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string())
}

/// `0.3.0` → `0,3,0,0`：VERSIONINFO 要四段 16 位整数，缺的补零。
fn file_version(version: &str) -> String {
    let parts: Vec<u16> = version
        .split('.')
        .take(4)
        .map(|part| part.parse::<u16>().unwrap_or(0))
        .chain(std::iter::repeat(0))
        .take(4)
        .collect();
    parts
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(",")
}
