//! 应用偏好：主题、字体预览参数、收藏/最近、窗口尺寸与导航状态。
//!
//! 存储已经迁到 SQLite 的 `app_setting` 键值表（一个 key 一行，便于单独读取与
//! 排障）。首次启动时如果旧的 `font-manager/settings.json` 还在，会一次性导入，
//! 并在库里记下来源；旧文件保留不动。

use gpui_kit::{App, Global};
use serde::{Deserialize, Serialize};

use crate::store::DipswitchStore;

pub const LATIN_SAMPLE: &str = "The quick brown fox jumps over the lazy dog 0123456789";
pub const CJK_SAMPLE: &str = "天地玄黄，宇宙洪荒。日月盈昃，辰宿列张。";
pub const CODE_SAMPLE: &str = "fn main() { println!(\"Hello, 世界\"); }";
pub const NUMBERS_SAMPLE: &str = "0123456789 !@#$%^&*()[]{} <> /\\| ~";

/// 偏好所在库的 key，按「区域.名字」组织，避免各模块互相覆盖。
pub const IS_DARK: &str = "appearance.is_dark";
pub const PREVIEW_SIZE: &str = "fonts.preview_size";
pub const PREVIEW_BACKGROUND: &str = "fonts.preview_background";
pub const SAMPLE_PRESET: &str = "fonts.sample_preset";
pub const SAMPLE_TEXT: &str = "fonts.sample_text";
pub const FAVORITES: &str = "fonts.favorites";
pub const RECENT: &str = "fonts.recent";
pub const LAST_SELECTED: &str = "fonts.last_selected";
pub const WINDOW_WIDTH: &str = "window.width";
pub const WINDOW_HEIGHT: &str = "window.height";
pub const WINDOW_MAXIMIZED: &str = "window.maximized";
pub const ACTIVE_MODULE: &str = "dipswitch.active_module";
pub const NAV_COLLAPSED: &str = "dipswitch.nav_collapsed";
pub const SETTINGS_IMPORTED: &str = "dipswitch.settings_imported";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PreviewBackground {
    #[default]
    Theme,
    Light,
    Dark,
}

impl PreviewBackground {
    pub fn label(self) -> &'static str {
        match self {
            Self::Theme => "Match theme",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    /// 存库与旧 JSON 用的稳定标识（等于变体名，不随展示文案变化）。
    pub fn key(self) -> &'static str {
        match self {
            Self::Theme => "Theme",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    pub fn parse(text: &str) -> Self {
        match text {
            "Light" => Self::Light,
            "Dark" => Self::Dark,
            _ => Self::Theme,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SamplePreset {
    #[default]
    Latin,
    Cjk,
    Code,
    Numbers,
    Custom,
}

impl SamplePreset {
    pub const PRESETS: [Self; 4] = [Self::Latin, Self::Cjk, Self::Code, Self::Numbers];

    pub fn label(self) -> &'static str {
        match self {
            Self::Latin => "Latin",
            Self::Cjk => "中文",
            Self::Code => "Code",
            Self::Numbers => "Numbers",
            Self::Custom => "Custom",
        }
    }

    pub fn text(self) -> &'static str {
        match self {
            Self::Latin => LATIN_SAMPLE,
            Self::Cjk => CJK_SAMPLE,
            Self::Code => CODE_SAMPLE,
            Self::Numbers => NUMBERS_SAMPLE,
            Self::Custom => LATIN_SAMPLE,
        }
    }

    /// 存库用的稳定标识（`label()` 里含中文，不适合做持久化键）。
    pub fn key(self) -> &'static str {
        match self {
            Self::Latin => "Latin",
            Self::Cjk => "Cjk",
            Self::Code => "Code",
            Self::Numbers => "Numbers",
            Self::Custom => "Custom",
        }
    }

    pub fn parse(text: &str) -> Self {
        match text {
            "Cjk" => Self::Cjk,
            "Code" => Self::Code,
            "Numbers" => Self::Numbers,
            "Custom" => Self::Custom,
            _ => Self::Latin,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub is_dark: bool,
    pub preview_size: f32,
    pub preview_background: PreviewBackground,
    pub sample_preset: SamplePreset,
    pub sample_text: String,
    pub favorites: Vec<String>,
    pub recent: Vec<String>,
    pub last_selected: Option<String>,
    pub window_width: Option<f32>,
    pub window_height: Option<f32>,
    pub window_maximized: Option<bool>,
    pub active_module: String,
    pub nav_collapsed: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            is_dark: false,
            preview_size: 32.0,
            preview_background: PreviewBackground::Theme,
            sample_preset: SamplePreset::Latin,
            sample_text: LATIN_SAMPLE.to_string(),
            favorites: Vec::new(),
            recent: Vec::new(),
            last_selected: None,
            window_width: Some(1180.0),
            window_height: Some(760.0),
            window_maximized: Some(false),
            active_module: crate::shell::FONTS_MODULE.to_string(),
            nav_collapsed: false,
        }
    }
}

impl Global for Settings {}

impl Settings {
    /// 从配置库读取偏好。旧 JSON 的导入由 [`import_legacy_once`] 在启动时单独做，
    /// 这里保持纯读，测试才不会因为机器上恰好有旧文件而得到不同结果。
    pub fn load(store: &DipswitchStore) -> Self {
        Self {
            is_dark: store.bool_setting(IS_DARK, false),
            preview_size: store
                .float_setting(PREVIEW_SIZE)
                .unwrap_or(Self::default().preview_size),
            preview_background: store
                .string_setting(PREVIEW_BACKGROUND)
                .map(|text| PreviewBackground::parse(&text))
                .unwrap_or_default(),
            sample_preset: store
                .string_setting(SAMPLE_PRESET)
                .map(|text| SamplePreset::parse(&text))
                .unwrap_or_default(),
            sample_text: store
                .string_setting(SAMPLE_TEXT)
                .filter(|text| !text.trim().is_empty())
                .unwrap_or_else(|| LATIN_SAMPLE.to_string()),
            favorites: store.json_setting(FAVORITES).unwrap_or_default(),
            recent: store.json_setting(RECENT).unwrap_or_default(),
            last_selected: store.string_setting(LAST_SELECTED),
            window_width: store.float_setting(WINDOW_WIDTH),
            window_height: store.float_setting(WINDOW_HEIGHT),
            window_maximized: store.setting(WINDOW_MAXIMIZED).map(|v| v == "true"),
            active_module: store
                .string_setting(ACTIVE_MODULE)
                .unwrap_or_else(|| Self::default().active_module),
            nav_collapsed: store.bool_setting(NAV_COLLAPSED, false),
        }
    }

    /// 全量写回配置库。偏好项很少，整组写入比逐字段脏标记更不容易漏；
    /// 整组只开一个事务，避免一次交互触发十几次单独的提交。
    pub fn save(&self, store: &DipswitchStore) {
        let mut entries: Vec<(&str, String)> = Vec::with_capacity(14);
        entries.push((IS_DARK, self.is_dark.to_string()));
        entries.push((PREVIEW_SIZE, self.preview_size.to_string()));
        entries.push((
            PREVIEW_BACKGROUND,
            self.preview_background.key().to_string(),
        ));
        entries.push((SAMPLE_PRESET, self.sample_preset.key().to_string()));
        entries.push((SAMPLE_TEXT, self.sample_text.clone()));
        entries.push((FAVORITES, json_text(&self.favorites)));
        entries.push((RECENT, json_text(&self.recent)));
        entries.push((
            LAST_SELECTED,
            self.last_selected.clone().unwrap_or_default(),
        ));
        if let Some(width) = self.window_width {
            entries.push((WINDOW_WIDTH, width.to_string()));
        }
        if let Some(height) = self.window_height {
            entries.push((WINDOW_HEIGHT, height.to_string()));
        }
        if let Some(maximized) = self.window_maximized {
            entries.push((WINDOW_MAXIMIZED, maximized.to_string()));
        }
        entries.push((ACTIVE_MODULE, self.active_module.clone()));
        entries.push((NAV_COLLAPSED, self.nav_collapsed.to_string()));
        let _ = store.set_settings(&entries);
    }
}

/// 把内存里的偏好落盘：视图改完 `Settings` Global 后调用它。
pub fn persist(cx: &App) {
    let settings = cx.global::<Settings>().clone();
    settings.save(cx.global::<DipswitchStore>());
}

/// 列表型偏好的存储形态。序列化失败时退回空列表，不拖累其余偏好。
fn json_text<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "[]".to_string())
}

/// 旧 `font-manager/settings.json` 的位置（升级为工作台之前的持久化文件）。
fn legacy_json_path() -> Option<std::path::PathBuf> {
    dirs::config_local_dir()
        .map(|dir| dir.join("font-manager").join("settings.json"))
        .filter(|path| path.exists())
}

/// 每个库只做一次导入：导入过就记标记，之后不再读旧文件。
/// 找不到旧文件也记标记，避免每次启动都做一次磁盘探测。
pub fn import_legacy_once(store: &DipswitchStore) {
    if store.string_setting(SETTINGS_IMPORTED).is_some() {
        return;
    }
    match legacy_json_path() {
        Some(path) => {
            let marker = if import_from_legacy(store, &path) {
                path.display().to_string()
            } else {
                "unreadable-legacy-file".to_string()
            };
            let _ = store.set_setting(SETTINGS_IMPORTED, &marker);
        }
        None => {
            let _ = store.set_setting(SETTINGS_IMPORTED, "no-legacy-file");
        }
    }
}

/// 把一个旧 JSON 导入配置库；成功返回 true。旧文件保留不动。
pub fn import_from_legacy(store: &DipswitchStore, path: &std::path::Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(legacy) = serde_json::from_str::<LegacySettings>(&text) else {
        return false;
    };

    Settings {
        is_dark: legacy.is_dark,
        preview_size: legacy.preview_size,
        preview_background: legacy.preview_background,
        sample_preset: legacy.sample_preset,
        sample_text: legacy.sample_text,
        favorites: legacy.favorites,
        recent: legacy.recent,
        last_selected: legacy.last_selected,
        window_width: legacy.window_width,
        window_height: legacy.window_height,
        window_maximized: legacy.window_maximized,
        // 工作台新增的偏好从默认值开始，旧文件里没有对应字段。
        active_module: Settings::default().active_module,
        nav_collapsed: false,
    }
    .save(store);
    true
}

/// 旧 `font-manager/settings.json` 的字段形状，只用于一次性导入。
#[derive(Debug, Deserialize)]
#[serde(default)]
struct LegacySettings {
    is_dark: bool,
    preview_size: f32,
    preview_background: PreviewBackground,
    sample_preset: SamplePreset,
    sample_text: String,
    favorites: Vec<String>,
    recent: Vec<String>,
    last_selected: Option<String>,
    window_width: Option<f32>,
    window_height: Option<f32>,
    window_maximized: Option<bool>,
}

impl Default for LegacySettings {
    fn default() -> Self {
        let settings = Settings::default();
        Self {
            is_dark: settings.is_dark,
            preview_size: settings.preview_size,
            preview_background: settings.preview_background,
            sample_preset: settings.sample_preset,
            sample_text: settings.sample_text,
            favorites: settings.favorites,
            recent: settings.recent,
            last_selected: settings.last_selected,
            window_width: settings.window_width,
            window_height: settings.window_height,
            window_maximized: settings.window_maximized,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_store() -> DipswitchStore {
        DipswitchStore::with_connection(rusqlite::Connection::open_in_memory().unwrap()).unwrap()
    }

    #[test]
    fn settings_survive_a_round_trip_through_the_database() {
        let store = memory_store();
        let settings = Settings {
            is_dark: true,
            preview_size: 72.0,
            preview_background: PreviewBackground::Dark,
            sample_preset: SamplePreset::Code,
            sample_text: CODE_SAMPLE.to_string(),
            favorites: vec!["Arial".to_string()],
            recent: vec!["Calibri".to_string(), "Arial".to_string()],
            last_selected: Some("Arial".to_string()),
            window_width: Some(1200.0),
            window_height: Some(800.0),
            window_maximized: Some(true),
            active_module: crate::shell::ENVIRONMENT_MODULE.to_string(),
            nav_collapsed: true,
        };
        settings.save(&store);

        let loaded = Settings::load(&store);
        assert!(loaded.is_dark);
        assert_eq!(loaded.preview_size, 72.0);
        assert_eq!(loaded.preview_background, PreviewBackground::Dark);
        assert_eq!(loaded.sample_preset, SamplePreset::Code);
        assert_eq!(loaded.sample_text, CODE_SAMPLE);
        assert_eq!(loaded.favorites, vec!["Arial".to_string()]);
        assert_eq!(
            loaded.recent,
            vec!["Calibri".to_string(), "Arial".to_string()]
        );
        assert_eq!(loaded.last_selected.as_deref(), Some("Arial"));
        assert_eq!(loaded.window_width, Some(1200.0));
        assert_eq!(loaded.window_maximized, Some(true));
        assert_eq!(loaded.active_module, crate::shell::ENVIRONMENT_MODULE);
        assert!(loaded.nav_collapsed);
    }

    #[test]
    fn empty_database_falls_back_to_defaults() {
        let store = memory_store();
        let settings = Settings::load(&store);
        assert_eq!(settings.preview_size, 32.0);
        assert_eq!(settings.preview_background, PreviewBackground::Theme);
        assert_eq!(settings.sample_text, LATIN_SAMPLE);
        assert!(settings.favorites.is_empty());
        assert_eq!(settings.active_module, crate::shell::FONTS_MODULE);
    }

    #[test]
    fn legacy_json_imports_into_the_database() {
        let store = memory_store();
        let dir = std::env::temp_dir().join(format!("dipswitch-legacy-{}", store_marker()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            r#"{
                "is_dark": true,
                "preview_size": 72.0,
                "preview_background": "Dark",
                "sample_preset": "Cjk",
                "sample_text": "天地玄黄",
                "favorites": ["Arial"],
                "recent": ["Arial", "Calibri"],
                "last_selected": "Calibri",
                "window_width": 1024.0,
                "window_height": 700.0,
                "window_maximized": false
            }"#,
        )
        .unwrap();

        assert!(import_from_legacy(&store, &path));
        let loaded = Settings::load(&store);
        assert!(loaded.is_dark);
        assert_eq!(loaded.preview_size, 72.0);
        assert_eq!(loaded.preview_background, PreviewBackground::Dark);
        assert_eq!(loaded.sample_preset, SamplePreset::Cjk);
        assert_eq!(loaded.sample_text, "天地玄黄");
        assert_eq!(loaded.favorites, vec!["Arial".to_string()]);
        assert_eq!(loaded.last_selected.as_deref(), Some("Calibri"));
        assert_eq!(loaded.window_width, Some(1024.0));
        // 旧文件没有的字段回到工作台默认值。
        assert_eq!(loaded.active_module, crate::shell::FONTS_MODULE);
        assert!(!loaded.nav_collapsed);

        // 坏 JSON 不导入也不 panic；库里保持空偏好。
        let broken = dir.join("broken.json");
        std::fs::write(&broken, "{ not json").unwrap();
        assert!(!import_from_legacy(&memory_store(), &broken));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 让每次测试用不同的临时目录，避免并行跑测试时互相覆盖。
    fn store_marker() -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::time::Instant::now()
            .elapsed()
            .as_nanos()
            .hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn preview_background_and_sample_preset_keep_their_storage_keys() {
        for background in [
            PreviewBackground::Theme,
            PreviewBackground::Light,
            PreviewBackground::Dark,
        ] {
            assert_eq!(PreviewBackground::parse(background.key()), background);
        }
        for preset in [
            SamplePreset::Latin,
            SamplePreset::Cjk,
            SamplePreset::Code,
            SamplePreset::Numbers,
            SamplePreset::Custom,
        ] {
            assert_eq!(SamplePreset::parse(preset.key()), preset);
        }
    }
}
