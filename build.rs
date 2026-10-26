//! Windows 下把应用图标与版本信息编进 exe 资源，供 Explorer、任务栏、Alt-Tab 与
//! 文件属性对话框使用。
//!
//! 做法照搬 Zed 的 `crates/windows_resources`：构建期在 `OUT_DIR` 里生成 `.rc`，
//! 交给 `embed-resource` 编译。选它而不是自己找 `rc.exe`，因为它已经实现了
//! `KitsRoot10` 注册表 → `vswhom` → `PATH` 的发现链、支持 GNU 工具链走 `windres`，
//! 并认 `RC` / `RC_<target>` 覆盖。
//!
//! 两处刻意与 Zed 不同：
//! 1. **不写 manifest 行**——`gpui-pre` 自己的构建脚本已经嵌了 `1 RT_MANIFEST`
//!    （产物里能搜到 `dpiAware`），再来一份同 ID 同类型的资源就是冲突；
//! 2. **失败不 panic**——图标是观感项，缺 SDK 时也该构建出可用的 exe，所以降级成
//!    `cargo:warning`。
//!
//! 边界：本脚本只**消费**已提交的 `assets/app.ico`。静态资源是提前生成好、随仓库
//! 一起同步的产物，构建不依赖任何生成脚本。

fn main() {
    println!("cargo:rerun-if-changed=assets/app.ico");
    println!("cargo:rerun-if-changed=build.rs");

    // 编译期门控：`embed-resource` 只作为 Windows 主机上的 build-dependency 存在，
    // 少了这层 cfg，本文件在非 Windows 主机上根本编译不过。
    #[cfg(windows)]
    {
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
            && let Err(message) = embed()
        {
            println!("cargo:warning=未嵌入 exe 资源（图标与版本信息）：{message}");
        }
    }
}

/// 生成 `.rc` 并编译进二进制目标。
#[cfg(windows)]
fn embed() -> Result<(), String> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").map_err(|error| error.to_string())?;
    let icon = std::path::Path::new(&manifest).join("assets/app.ico");
    if !icon.is_file() {
        return Err(format!("缺少 {}", icon.display()));
    }
    let out_dir = std::env::var("OUT_DIR").map_err(|error| error.to_string())?;
    let rc_path = std::path::Path::new(&out_dir).join("dipswitch.rc");
    std::fs::write(&rc_path, rc_file(&icon)).map_err(|error| error.to_string())?;

    embed_resource::compile(&rc_path, embed_resource::NONE)
        .manifest_optional()
        .map_err(|failure| format!("{failure:?}"))
}

/// `.rc` 内容。
///
/// `#pragma code_page(65001)`：`rc.exe` 默认按 ANSI 代码页解码 `.rc`，仓库路径含
/// 非 ASCII 字符时它会找不到被引用的文件——与 Zed 同一处理。路径里的反斜杠要双写，
/// 否则在 rc 的字符串字面量中成了转义。
#[cfg(windows)]
fn rc_file(icon: &std::path::Path) -> String {
    let icon = icon.to_string_lossy().replace('\\', "\\\\");
    let name = env!("CARGO_PKG_NAME");
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let numeric = file_version(&version);
    format!(
        r#"#pragma code_page(65001)
1 ICON "{icon}"

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
            VALUE "FileDescription", "{name}\0"
            VALUE "FileVersion", "{version}\0"
            VALUE "ProductName", "{name}\0"
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

/// `0.3.0` → `0,3,0,0`：VERSIONINFO 要四段 16 位整数。预发布后缀（`0.3.0-rc.1`）
/// 先剥掉再解析，缺位或解析不出的一律补零，不能让 `rc` 拿到非数字。
#[cfg(windows)]
fn file_version(version: &str) -> String {
    (0..4)
        .map(|index| {
            version
                .split('.')
                .nth(index)
                .and_then(|part| part.split('-').next())
                .and_then(|part| part.parse::<u16>().ok())
                .unwrap_or(0)
        })
        .map(|part| part.to_string())
        .collect::<Vec<_>>()
        .join(",")
}
