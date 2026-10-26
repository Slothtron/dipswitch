//! 本地配置库：一个 SQLite 文件保存工作台的全部用户配置。
//!
//! 作为 GPUI Global 安装一次，视图通过它读写；连接放在 `Mutex` 里以满足
//! `Global: Send + Sync`，实际访问都发生在主线程。
//!
//! 表：
//!
//! - `app_setting`：键值型偏好（主题、字体预览参数、窗口尺寸、上次停留的模块）。
//! - `env_group` / `env_variable`：环境变量分组与组内变量。
//! - `env_baseline`：分组首次接管某个 key 之前的原值，是「停用后恢复」的依据。
//! - `env_event`：启用/停用/覆盖/恢复的流水，用于回溯一次分组切换动了什么。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use gpui_kit::Global;
use rusqlite::{Connection, params};

use crate::env_model::{Baseline, EnvGroup, GroupVariable};
use crate::system_env::ValueKind;

const SCHEMA_VERSION: i64 = 1;

/// 全局安装的配置库；视图一律通过它读写数据。
pub struct DipswitchStore {
    connection: Mutex<Connection>,
}

impl Global for DipswitchStore {}

impl DipswitchStore {
    /// 打开（必要时创建）配置文件里的数据库并建表。
    pub fn open(path: PathBuf) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("无法创建配置目录：{error}"))?;
        }
        let connection =
            Connection::open(&path).map_err(|error| format!("无法打开配置库：{error}"))?;
        Self::build(connection)
    }

    /// 用现成的连接建库（内存库测试走这条路）。
    #[cfg(test)]
    pub fn with_connection(connection: Connection) -> Result<Self, String> {
        Self::build(connection)
    }

    /// 打开默认位置的配置库。
    pub fn open_or_default() -> Result<Self, String> {
        let path =
            database_path().ok_or_else(|| "找不到配置目录，本次会话的设置不会保存".to_string())?;
        migrate_legacy_database(&path);
        Self::open(path)
    }

    /// 内存库：配置库打不开时的降级。工作台仍然可用，但必须在界面上说明
    /// 「设置不会保存」，不能让用户以为已经写进去了。
    pub fn memory() -> Self {
        let connection = Connection::open_in_memory().expect("内存配置库应当总能打开");
        Self::build(connection).expect("内存配置库建表失败")
    }

    fn build(connection: Connection) -> Result<Self, String> {
        connection
            .busy_timeout(std::time::Duration::from_millis(2000))
            .map_err(|error| error.to_string())?;
        // WAL + NORMAL：写入不再每次交互都 fsync，界面不会因为落盘卡顿。
        // 配置库不是账本，掉电最多丢最后一条偏好，不会损坏库。
        // 某些文件系统不支持 WAL，装上失败就退回默认模式，不影响可用性。
        let _ = connection.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;");
        let store = Self {
            connection: Mutex::new(connection),
        };
        store.migrate()?;
        Ok(store)
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>, String> {
        self.connection
            .lock()
            .map_err(|_| "配置库被占用，请重试".to_string())
    }

    fn migrate(&self) -> Result<(), String> {
        let connection = self.connection()?;
        connection
            .execute_batch(
                r#"
                PRAGMA foreign_keys = ON;

                CREATE TABLE IF NOT EXISTS app_setting (
                    key   TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS env_group (
                    id          INTEGER PRIMARY KEY,
                    name        TEXT NOT NULL UNIQUE,
                    note        TEXT NOT NULL DEFAULT '',
                    is_enabled  INTEGER NOT NULL DEFAULT 0,
                    enabled_seq INTEGER,
                    created_at  INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS env_variable (
                    id       INTEGER PRIMARY KEY,
                    group_id INTEGER NOT NULL REFERENCES env_group(id) ON DELETE CASCADE,
                    key      TEXT NOT NULL,
                    value    TEXT NOT NULL,
                    kind     TEXT NOT NULL DEFAULT 'REG_SZ',
                    UNIQUE (group_id, key)
                );

                CREATE TABLE IF NOT EXISTS env_baseline (
                    key         TEXT PRIMARY KEY,
                    value       TEXT,
                    kind        TEXT NOT NULL DEFAULT 'REG_SZ',
                    captured_at INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS env_event (
                    id     INTEGER PRIMARY KEY,
                    at     INTEGER NOT NULL,
                    detail TEXT NOT NULL
                );
                "#,
            )
            .map_err(|error| format!("配置库建表失败：{error}"))?;

        // 改名为 dipswitch 之前，外壳偏好叫 `workbench.*`。就地改前缀，
        // 否则上次停留的模块、导航折叠状态会静默回到默认值。幂等：跑过一次就不再匹配。
        connection
            .execute(
                "UPDATE app_setting
                    SET key = 'dipswitch.' || substr(key, length('workbench.') + 1)
                  WHERE key LIKE 'workbench.%'",
                [],
            )
            .map_err(|error| format!("偏好改名失败：{error}"))?;

        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap_or(0);
        if version < SCHEMA_VERSION {
            connection
                .execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- 偏好

    pub fn setting(&self, key: &str) -> Option<String> {
        let connection = self.connection().ok()?;
        connection
            .query_row(
                "SELECT value FROM app_setting WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .ok()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), String> {
        let connection = self.connection()?;
        connection
            .execute(
                "INSERT INTO app_setting (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// 整组偏好走一个事务：交互路径上只提交一次，而不是每写一个键提交一次。
    pub fn set_settings(&self, entries: &[(&str, String)]) -> Result<(), String> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        for (key, value) in entries {
            transaction
                .execute(
                    "INSERT INTO app_setting (key, value) VALUES (?1, ?2)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![key, value],
                )
                .map_err(|error| error.to_string())?;
        }
        transaction.commit().map_err(|error| error.to_string())
    }

    pub fn bool_setting(&self, key: &str, default: bool) -> bool {
        match self.setting(key).as_deref() {
            Some("true") => true,
            Some("false") => false,
            _ => default,
        }
    }

    pub fn float_setting(&self, key: &str) -> Option<f32> {
        self.setting(key).and_then(|value| value.parse().ok())
    }

    pub fn string_setting(&self, key: &str) -> Option<String> {
        self.setting(key)
    }

    /// 结构化偏好按 JSON 文本存放（收藏、最近使用这类列表）。
    pub fn json_setting<T: serde::de::DeserializeOwned>(&self, key: &str) -> Option<T> {
        self.setting(key)
            .and_then(|text| serde_json::from_str(&text).ok())
    }

    // ---------------------------------------------------------------- 分组

    pub fn groups(&self) -> Result<Vec<EnvGroup>, String> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT id, name, note, is_enabled, enabled_seq
                 FROM env_group ORDER BY name COLLATE NOCASE",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], row_to_group)
            .map_err(|error| error.to_string())?;
        collect(rows)
    }

    pub fn create_group(&self, name: &str, note: &str) -> Result<i64, String> {
        let connection = self.connection()?;
        connection
            .execute(
                "INSERT INTO env_group (name, note, is_enabled, enabled_seq, created_at)
                 VALUES (?1, ?2, 0, NULL, ?3)",
                params![name, note, now()],
            )
            .map_err(|error| {
                if error
                    .to_string()
                    .contains("UNIQUE constraint failed: env_group.name")
                {
                    format!("已存在同名分组「{name}」")
                } else {
                    error.to_string()
                }
            })?;
        Ok(connection.last_insert_rowid())
    }

    pub fn update_group(&self, id: i64, name: &str, note: &str) -> Result<(), String> {
        let connection = self.connection()?;
        connection
            .execute(
                "UPDATE env_group SET name = ?2, note = ?3 WHERE id = ?1",
                params![id, name, note],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// 删除分组。组内变量随外键级联删除；该分组接管的 key 由调用方重新解析后恢复。
    pub fn delete_group(&self, id: i64) -> Result<(), String> {
        let connection = self.connection()?;
        connection
            .execute("DELETE FROM env_group WHERE id = ?1", params![id])
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// 启用＝排到最后（最后启用者覆盖同名 key）；停用＝让出优先级。
    pub fn set_group_enabled(&self, id: i64, enabled: bool) -> Result<(), String> {
        let connection = self.connection()?;
        if enabled {
            connection
                .execute(
                    "UPDATE env_group
                     SET is_enabled = 1,
                         enabled_seq = (SELECT COALESCE(MAX(enabled_seq), 0) + 1 FROM env_group)
                     WHERE id = ?1",
                    params![id],
                )
                .map_err(|error| error.to_string())?;
        } else {
            connection
                .execute(
                    "UPDATE env_group SET is_enabled = 0, enabled_seq = NULL WHERE id = ?1",
                    params![id],
                )
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    // -------------------------------------------------------------- 组内变量

    pub fn group_variables(&self) -> Result<Vec<GroupVariable>, String> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT id, group_id, key, value, kind
                 FROM env_variable ORDER BY group_id, key COLLATE NOCASE",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], row_to_variable)
            .map_err(|error| error.to_string())?;
        collect(rows)
    }

    pub fn add_variable(
        &self,
        group_id: i64,
        key: &str,
        value: &str,
        kind: ValueKind,
    ) -> Result<i64, String> {
        let connection = self.connection()?;
        connection
            .execute(
                "INSERT INTO env_variable (group_id, key, value, kind) VALUES (?1, ?2, ?3, ?4)",
                params![group_id, key, value, kind.label()],
            )
            .map_err(|error| {
                if error
                    .to_string()
                    .contains("UNIQUE constraint failed: env_variable.group_id, env_variable.key")
                {
                    format!("该分组里已经有 {key}")
                } else {
                    error.to_string()
                }
            })?;
        Ok(connection.last_insert_rowid())
    }

    pub fn update_variable(
        &self,
        id: i64,
        key: &str,
        value: &str,
        kind: ValueKind,
    ) -> Result<(), String> {
        let connection = self.connection()?;
        connection
            .execute(
                "UPDATE env_variable SET key = ?2, value = ?3, kind = ?4 WHERE id = ?1",
                params![id, key, value, kind.label()],
            )
            .map_err(|error| {
                if error
                    .to_string()
                    .contains("UNIQUE constraint failed: env_variable.group_id, env_variable.key")
                {
                    format!("该分组里已经有 {key}")
                } else {
                    error.to_string()
                }
            })?;
        Ok(())
    }

    pub fn delete_variable(&self, id: i64) -> Result<(), String> {
        let connection = self.connection()?;
        connection
            .execute("DELETE FROM env_variable WHERE id = ?1", params![id])
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    // ------------------------------------------------------------ 基线与流水

    pub fn baselines(&self) -> Result<BTreeMap<String, Baseline>, String> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT key, value, kind FROM env_baseline")
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                let key: String = row.get(0)?;
                Ok(Baseline {
                    kind: ValueKind::parse(&row.get::<_, String>(2)?),
                    value: row.get::<_, Option<String>>(1)?,
                    key: key.clone(),
                })
            })
            .map_err(|error| error.to_string())?;
        let mut map = BTreeMap::new();
        for row in rows {
            let baseline = row.map_err(|error| error.to_string())?;
            map.insert(baseline.key.clone(), baseline);
        }
        Ok(map)
    }

    pub fn record_baseline(
        &self,
        key: &str,
        value: Option<&str>,
        kind: ValueKind,
    ) -> Result<(), String> {
        let connection = self.connection()?;
        connection
            .execute(
                "INSERT INTO env_baseline (key, value, kind, captured_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value, kind = excluded.kind",
                params![key, value, kind.label(), now()],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// 恢复完成后释放基线，避免把已经不属于任何分组的值当作被接管状态。
    pub fn release_baselines(&self, keys: &[String]) -> Result<(), String> {
        if keys.is_empty() {
            return Ok(());
        }
        let connection = self.connection()?;
        for key in keys {
            connection
                .execute("DELETE FROM env_baseline WHERE key = ?1", params![key])
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub fn log_event(&self, detail: &str) -> Result<(), String> {
        let connection = self.connection()?;
        connection
            .execute(
                "INSERT INTO env_event (at, detail) VALUES (?1, ?2)",
                params![now(), detail],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn recent_events(&self, limit: usize) -> Result<Vec<(i64, String)>, String> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT at, detail FROM env_event ORDER BY id DESC LIMIT ?1")
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map(params![limit as i64], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| error.to_string())?;
        collect(rows)
    }

    pub fn clear_events(&self) -> Result<(), String> {
        let connection = self.connection()?;
        connection
            .execute("DELETE FROM env_event", [])
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

/// 配置库路径：`%LOCALAPPDATA%\dipswitch\dipswitch.db`（Linux 为 `~/.config/…`）。
pub fn database_path() -> Option<PathBuf> {
    dirs::config_local_dir().map(|dir| dir.join("dipswitch").join("dipswitch.db"))
}

/// 改名前的库路径：`%LOCALAPPDATA%\workbench\workbench.db`。
fn legacy_database_path() -> Option<PathBuf> {
    dirs::config_local_dir()
        .map(|dir| dir.join("workbench").join("workbench.db"))
        .filter(|path| path.exists())
}

/// 第一次以新名字启动时，把旧库整份搬到新位置（含 WAL 边文件）。
///
/// 这一步是必须的，不是怀旧兼容：`env_baseline` 记着分组接管各变量之前的原值，
/// 库一旦「凭空消失」，正在生效的分组就再也回不去了。旧库保留不删，出问题时
/// 仍可手工还原。
fn migrate_legacy_database(new_path: &Path) {
    let Some(old_path) = legacy_database_path() else {
        return;
    };
    if new_path.exists() {
        return;
    }
    let Some(parent) = new_path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    for suffix in ["", "-wal", "-shm"] {
        let from = format!("{}{}", old_path.display(), suffix);
        let to = format!("{}{}", new_path.display(), suffix);
        let _ = std::fs::copy(from, to);
    }
}

fn row_to_group(row: &rusqlite::Row<'_>) -> rusqlite::Result<EnvGroup> {
    Ok(EnvGroup {
        id: row.get(0)?,
        name: row.get(1)?,
        note: row.get(2)?,
        is_enabled: row.get::<_, i64>(3)? != 0,
        enabled_seq: row.get(4)?,
    })
}

fn row_to_variable(row: &rusqlite::Row<'_>) -> rusqlite::Result<GroupVariable> {
    Ok(GroupVariable {
        id: row.get(0)?,
        group_id: row.get(1)?,
        key: row.get(2)?,
        value: row.get(3)?,
        kind: ValueKind::parse(&row.get::<_, String>(4)?),
    })
}

fn collect<T, I>(rows: I) -> Result<Vec<T>, String>
where
    I: Iterator<Item = rusqlite::Result<T>>,
{
    rows.map(|row| row.map_err(|error| error.to_string()))
        .collect()
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_store() -> DipswitchStore {
        DipswitchStore::with_connection(rusqlite::Connection::open_in_memory().unwrap()).unwrap()
    }

    /// 改名前的库里存的是 `workbench.*`，打开时必须就地换成 `dipswitch.*`，
    /// 否则上次停留的模块与导航折叠状态会静默回到默认值。
    #[test]
    fn legacy_setting_keys_are_renamed_on_open() {
        let store = memory_store();
        {
            let connection = store.connection().unwrap();
            connection
                .execute(
                    "INSERT INTO app_setting (key, value) VALUES ('workbench.active_module', 'environment')",
                    [],
                )
                .unwrap();
        }

        store.migrate().unwrap();
        assert_eq!(
            store.setting("dipswitch.active_module").as_deref(),
            Some("environment")
        );
        assert!(store.setting("workbench.active_module").is_none());

        // 幂等：再打开一次不应该动任何键。
        store.migrate().unwrap();
        assert_eq!(
            store.setting("dipswitch.active_module").as_deref(),
            Some("environment")
        );
    }

    #[test]
    fn group_names_are_unique() {
        let store = memory_store();
        store.create_group("Rust", "本地开发").unwrap();
        let error = store
            .create_group("Rust", "")
            .expect_err("同名分组应当被拒绝");
        assert!(error.contains("已存在同名分组"));
    }

    #[test]
    fn enabling_appends_priority_and_disabling_clears_it() {
        let store = memory_store();
        let first = store.create_group("Go", "").unwrap();
        let second = store.create_group("Rust", "").unwrap();

        store.set_group_enabled(first, true).unwrap();
        store.set_group_enabled(second, true).unwrap();
        let groups = store.groups().unwrap();
        let priority = |name: &str| {
            groups
                .iter()
                .find(|group| group.name == name)
                .and_then(|group| group.enabled_seq)
        };
        assert!(priority("Rust") > priority("Go"), "后启用的优先级更高");

        store.set_group_enabled(second, false).unwrap();
        let groups = store.groups().unwrap();
        let rust = groups.iter().find(|group| group.name == "Rust").unwrap();
        assert!(!rust.is_enabled);
        assert_eq!(rust.enabled_seq, None);
    }

    #[test]
    fn duplicate_keys_within_a_group_are_rejected() {
        let store = memory_store();
        let group_id = store.create_group("Rust", "").unwrap();
        store
            .add_variable(group_id, "RUST_LOG", "debug", ValueKind::Plain)
            .unwrap();
        let error = store
            .add_variable(group_id, "RUST_LOG", "info", ValueKind::Plain)
            .expect_err("同组内 key 必须唯一");
        assert!(error.contains("该分组里已经有"));

        // 不同分组可以定义同一个 key——覆盖语义正是由此而来。
        let other = store.create_group("Go", "").unwrap();
        store
            .add_variable(other, "RUST_LOG", "warn", ValueKind::Plain)
            .unwrap();
        assert_eq!(store.group_variables().unwrap().len(), 2);
    }

    #[test]
    fn deleting_a_group_cascades_to_its_variables() {
        let store = memory_store();
        let keep = store.create_group("Keep", "").unwrap();
        let drop = store.create_group("Drop", "").unwrap();
        store
            .add_variable(keep, "A", "1", ValueKind::Plain)
            .unwrap();
        store
            .add_variable(drop, "B", "2", ValueKind::Plain)
            .unwrap();

        store.delete_group(drop).unwrap();
        let remaining = store.group_variables().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].key, "A");
    }

    #[test]
    fn variable_kind_is_stored_by_its_registry_name() {
        let store = memory_store();
        let group_id = store.create_group("Path", "").unwrap();
        store
            .add_variable(
                group_id,
                "Path_Add",
                r"%USERPROFILE%\bin",
                ValueKind::Expandable,
            )
            .unwrap();
        let variables = store.group_variables().unwrap();
        assert_eq!(variables[0].kind, ValueKind::Expandable);
    }

    #[test]
    fn baselines_record_absent_keys_and_release_by_name() {
        let store = memory_store();
        store
            .record_baseline("GONE", None, ValueKind::Plain)
            .unwrap();
        store
            .record_baseline("BACK", Some("original"), ValueKind::Expandable)
            .unwrap();

        let baselines = store.baselines().unwrap();
        assert_eq!(baselines["GONE"].value, None);
        assert_eq!(baselines["BACK"].value.as_deref(), Some("original"));
        assert_eq!(baselines["BACK"].kind, ValueKind::Expandable);

        store.release_baselines(&["GONE".to_string()]).unwrap();
        let baselines = store.baselines().unwrap();
        assert!(!baselines.contains_key("GONE"));
        assert!(baselines.contains_key("BACK"));
    }

    #[test]
    fn events_are_newest_first_and_can_be_cleared() {
        let store = memory_store();
        store.log_event("启用「Rust」· 写入 2 项").unwrap();
        store.log_event("停用「Rust」· 删除 2 项").unwrap();
        let events = store.recent_events(10).unwrap();
        assert_eq!(events.len(), 2);
        assert!(events[0].1.contains("停用"), "最新的流水在前");

        store.clear_events().unwrap();
        assert!(store.recent_events(10).unwrap().is_empty());
    }
}
