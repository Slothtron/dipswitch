//! 分组启用/停用的编排：把 [`env_model::plan`] 的解析结果落到真实环境与数据库上。
//!
//! 环境读写收在 [`EnvBackend`] 这个窄缝里：生产走 [`SystemEnvBackend`]（注册表 /
//! 进程环境），测试走内存假实现，因此「启用 → 覆盖 → 停用 → 恢复」这条主链路
//! 可以端到端验证，而不需要碰真实注册表。

use std::collections::BTreeMap;

use crate::env_model::{self, Baseline, StepAction, build_layers};
use crate::store::DipswitchStore;
use crate::system_env::{self, SystemVariable, ValueKind};

/// 环境读写的能力缝。
pub trait EnvBackend {
    fn read(&self) -> Result<BTreeMap<String, SystemVariable>, String>;
    fn write(&mut self, key: &str, raw: &str, kind: ValueKind) -> Result<(), String>;
    fn remove(&mut self, key: &str) -> Result<(), String>;
}

/// 真实环境：Windows 用户级注册表，其他平台为当前进程环境。
pub struct SystemEnvBackend;

impl EnvBackend for SystemEnvBackend {
    fn read(&self) -> Result<BTreeMap<String, SystemVariable>, String> {
        Ok(system_env::read_variables()?
            .into_iter()
            .map(|variable| (variable.key().to_string(), variable))
            .collect())
    }

    fn write(&mut self, key: &str, raw: &str, kind: ValueKind) -> Result<(), String> {
        system_env::write_variable(&SystemVariable::new(key, raw, kind))
    }

    fn remove(&mut self, key: &str) -> Result<(), String> {
        system_env::remove_variable(key)
    }
}

/// 一次成功落地的改动。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedChange {
    pub key: String,
    pub action: ChangeAction,
    /// 由哪个分组决定；`None` 表示恢复基线。
    pub source_group: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeAction {
    Written { value: String },
    Deleted,
}

/// 一次重新解析的结果汇总，界面据此给出如实的反馈。
#[derive(Debug, Clone, Default)]
pub struct ReconcileReport {
    pub applied: Vec<AppliedChange>,
    /// (key, 错误) —— 失败的 key 不会被释放基线，下次操作仍会重试。
    pub failures: Vec<(String, String)>,
    pub captured: usize,
    pub released: usize,
}

impl ReconcileReport {
    pub fn is_quiet(&self) -> bool {
        self.applied.is_empty() && self.failures.is_empty()
    }

    /// 一句话说明这次动了什么，用于通知与流水。
    pub fn summary(&self, reason: &str) -> String {
        let written = self
            .applied
            .iter()
            .filter(|change| matches!(change.action, ChangeAction::Written { .. }))
            .count();
        let deleted = self
            .applied
            .iter()
            .filter(|change| matches!(change.action, ChangeAction::Deleted))
            .count();
        let mut parts = vec![reason.to_string()];
        if written > 0 {
            parts.push(format!("写入 {written} 项"));
        }
        if deleted > 0 {
            parts.push(format!("删除 {deleted} 项"));
        }
        if self.captured > 0 {
            parts.push(format!("记录原值 {} 项", self.captured));
        }
        if self.released > 0 {
            parts.push(format!("释放原值 {} 项", self.released));
        }
        if !self.failures.is_empty() {
            parts.push(format!("失败 {} 项", self.failures.len()));
        }
        parts.join(" · ")
    }
}

/// 按当前分组状态重新解析并落地。任何时候调用都得到一致的结果。
pub fn reconcile(
    store: &DipswitchStore,
    backend: &mut dyn EnvBackend,
) -> Result<ReconcileReport, String> {
    reconcile_reasoned(store, backend, "重新解析")
}

fn reconcile_reasoned(
    store: &DipswitchStore,
    backend: &mut dyn EnvBackend,
    reason: &str,
) -> Result<ReconcileReport, String> {
    let groups = store.groups()?;
    let variables = store.group_variables()?;
    let layers = build_layers(&groups, &variables);
    let mut baselines = store.baselines()?;
    let current = backend.read()?;

    let plan = env_model::plan(&layers, &baselines, &current);
    let mut report = ReconcileReport::default();

    for capture in &plan.captures {
        store.record_baseline(&capture.key, capture.value.as_deref(), capture.kind)?;
        baselines.insert(
            capture.key.clone(),
            Baseline {
                key: capture.key.clone(),
                value: capture.value.clone(),
                kind: capture.kind,
            },
        );
        report.captured += 1;
    }

    let mut released = Vec::new();
    for step in &plan.steps {
        let restores = step.for_group.is_none();
        match step.action {
            StepAction::Keep => {
                if restores && plan.released.contains(&step.key) {
                    released.push(step.key.clone());
                }
            }
            StepAction::Set { ref value, kind } => {
                if let Err(error) = backend.write(&step.key, value, kind) {
                    report.failures.push((step.key.clone(), error));
                    continue;
                }
                report.applied.push(AppliedChange {
                    key: step.key.clone(),
                    action: ChangeAction::Written {
                        value: value.clone(),
                    },
                    source_group: source_group(&step.for_group, restores),
                });
                if restores && plan.released.contains(&step.key) {
                    released.push(step.key.clone());
                }
            }
            StepAction::Delete => {
                if let Err(error) = backend.remove(&step.key) {
                    report.failures.push((step.key.clone(), error));
                    continue;
                }
                report.applied.push(AppliedChange {
                    key: step.key.clone(),
                    action: ChangeAction::Deleted,
                    source_group: Some("原值".to_string()),
                });
                if restores && plan.released.contains(&step.key) {
                    released.push(step.key.clone());
                }
            }
        }
    }

    // 写入失败的 key 保留基线，下一次操作仍会尝试恢复它。
    store.release_baselines(&released)?;
    report.released = released.len();

    if !report.applied.is_empty() || !report.failures.is_empty() {
        let _ = store.log_event(&report.summary(reason));
    }
    Ok(report)
}

fn source_group(for_group: &Option<(i64, String)>, restores: bool) -> Option<String> {
    match (for_group, restores) {
        (Some((_, name)), _) => Some(name.clone()),
        (None, true) => Some("原值".to_string()),
        (None, false) => None,
    }
}

/// 启用或停用整个分组，随后重新解析。
pub fn set_group_enabled(
    store: &DipswitchStore,
    backend: &mut dyn EnvBackend,
    group_id: i64,
    enabled: bool,
) -> Result<ReconcileReport, String> {
    let group = store
        .groups()?
        .into_iter()
        .find(|group| group.id == group_id)
        .ok_or_else(|| "分组已不存在".to_string())?;
    store.set_group_enabled(group_id, enabled)?;
    let reason = if enabled {
        format!("启用「{}」", group.name)
    } else {
        format!("停用「{}」", group.name)
    };
    reconcile_reasoned(store, backend, &reason)
}

/// 某个 key 当前是否被启用分组接管；接管者不允许在系统层面直接编辑。
pub fn managing_group(store: &DipswitchStore, key: &str) -> Option<env_model::KeyOrigin> {
    let groups = store.groups().ok()?;
    let variables = store.group_variables().ok()?;
    let layers = build_layers(&groups, &variables);
    let baselines = store.baselines().ok()?;
    env_model::origins(&layers, &baselines).get(key).cloned()
}

/// 直接编辑一条系统变量。被启用分组接管的 key 会被拒绝，避免写出一个
/// 立刻被分组覆盖掉、却看起来已经保存的值。
pub fn write_system_variable(
    store: &DipswitchStore,
    backend: &mut dyn EnvBackend,
    variable: &SystemVariable,
) -> Result<(), String> {
    if let Some(origin) =
        managing_group(store, variable.key()).filter(|origin| origin.winning_group.is_some())
    {
        return Err(format!(
            "{} 由分组「{}」接管，请编辑分组里的那一条",
            variable.key(),
            origin.winning_group.unwrap_or_default()
        ));
    }
    backend.write(variable.key(), variable.raw(), variable.kind())
}

/// 删除一条系统变量；被分组接管时同样拒绝，否则会留下无法解释的「复活」。
pub fn remove_system_variable(
    store: &DipswitchStore,
    backend: &mut dyn EnvBackend,
    key: &str,
) -> Result<(), String> {
    if let Some(origin) = managing_group(store, key).filter(|origin| origin.winning_group.is_some())
    {
        return Err(format!(
            "{key} 由分组「{}」接管，请先停用分组或删除分组里的这个变量",
            origin.winning_group.unwrap_or_default()
        ));
    }
    backend.remove(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 内存环境，用来验证主链路而不动真实注册表。
    struct FakeEnv {
        variables: BTreeMap<String, SystemVariable>,
    }

    impl FakeEnv {
        fn new(entries: &[(&str, &str)]) -> Self {
            Self {
                variables: entries
                    .iter()
                    .map(|(key, value)| {
                        (
                            (*key).to_string(),
                            SystemVariable::new(*key, *value, ValueKind::Plain),
                        )
                    })
                    .collect(),
            }
        }

        fn value_of(&self, key: &str) -> Option<String> {
            self.variables.get(key).map(|v| v.raw().to_string())
        }

        fn contains(&self, key: &str) -> bool {
            self.variables.contains_key(key)
        }
    }

    impl EnvBackend for FakeEnv {
        fn read(&self) -> Result<BTreeMap<String, SystemVariable>, String> {
            Ok(self.variables.clone())
        }

        fn write(&mut self, key: &str, raw: &str, kind: ValueKind) -> Result<(), String> {
            self.variables
                .insert(key.to_string(), SystemVariable::new(key, raw, kind));
            Ok(())
        }

        fn remove(&mut self, key: &str) -> Result<(), String> {
            self.variables.remove(key);
            Ok(())
        }
    }

    fn memory_store() -> DipswitchStore {
        DipswitchStore::with_connection(rusqlite::Connection::open_in_memory().unwrap())
            .expect("内存库初始化")
    }

    #[test]
    fn enabling_a_group_applies_its_variables() {
        let store = memory_store();
        let group_id = store.create_group("Rust", "").unwrap();
        store
            .add_variable(group_id, "RUST_LOG", "debug", ValueKind::Plain)
            .unwrap();
        let mut env = FakeEnv::new(&[("RUST_LOG", "warn"), ("KEEP", "same")]);

        let report = set_group_enabled(&store, &mut env, group_id, true).unwrap();

        assert_eq!(env.value_of("RUST_LOG").as_deref(), Some("debug"));
        assert_eq!(env.value_of("KEEP").as_deref(), Some("same"));
        assert_eq!(report.captured, 1);
        assert_eq!(
            store.baselines().unwrap()["RUST_LOG"].value.as_deref(),
            Some("warn")
        );
    }

    #[test]
    fn last_enabled_group_overrides_and_first_one_is_restored_on_disable() {
        let store = memory_store();
        let go = store.create_group("Go", "").unwrap();
        let rust = store.create_group("Rust", "").unwrap();
        store
            .add_variable(go, "SHARED", "from-go", ValueKind::Plain)
            .unwrap();
        store
            .add_variable(rust, "SHARED", "from-rust", ValueKind::Plain)
            .unwrap();
        let mut env = FakeEnv::new(&[("SHARED", "original")]);

        set_group_enabled(&store, &mut env, go, true).unwrap();
        assert_eq!(env.value_of("SHARED").as_deref(), Some("from-go"));

        // 后启用的 Rust 覆盖同名 key，但基线仍是第一次记录的 original。
        set_group_enabled(&store, &mut env, rust, true).unwrap();
        assert_eq!(env.value_of("SHARED").as_deref(), Some("from-rust"));
        assert_eq!(
            store.baselines().unwrap()["SHARED"].value.as_deref(),
            Some("original")
        );

        // 停用最高层退回下一层，而不是直接跳回原值。
        set_group_enabled(&store, &mut env, rust, false).unwrap();
        assert_eq!(env.value_of("SHARED").as_deref(), Some("from-go"));
        assert!(store.baselines().unwrap().contains_key("SHARED"));

        set_group_enabled(&store, &mut env, go, false).unwrap();
        assert_eq!(env.value_of("SHARED").as_deref(), Some("original"));
        assert!(store.baselines().unwrap().is_empty());
    }

    #[test]
    fn keys_the_group_created_are_deleted_when_it_turns_off() {
        let store = memory_store();
        let group_id = store.create_group("Fresh", "").unwrap();
        store
            .add_variable(group_id, "FRESH_KEY", "1", ValueKind::Plain)
            .unwrap();
        let mut env = FakeEnv::new(&[]);

        set_group_enabled(&store, &mut env, group_id, true).unwrap();
        assert_eq!(env.value_of("FRESH_KEY").as_deref(), Some("1"));

        set_group_enabled(&store, &mut env, group_id, false).unwrap();
        assert!(!env.contains("FRESH_KEY"));
        assert!(store.baselines().unwrap().is_empty());
    }

    #[test]
    fn deleting_an_enabled_group_restores_what_it_took_over() {
        let store = memory_store();
        let group_id = store.create_group("Temp", "").unwrap();
        store
            .add_variable(group_id, "TEMP_VAR", "grouped", ValueKind::Plain)
            .unwrap();
        let mut env = FakeEnv::new(&[("TEMP_VAR", "original")]);

        set_group_enabled(&store, &mut env, group_id, true).unwrap();
        assert_eq!(env.value_of("TEMP_VAR").as_deref(), Some("grouped"));

        store.delete_group(group_id).unwrap();
        let report = reconcile(&store, &mut env).unwrap();
        assert_eq!(env.value_of("TEMP_VAR").as_deref(), Some("original"));
        assert_eq!(report.released, 1);
    }

    #[test]
    fn editing_a_managed_key_is_refused() {
        let store = memory_store();
        let group_id = store.create_group("Rust", "").unwrap();
        store
            .add_variable(group_id, "RUST_LOG", "debug", ValueKind::Plain)
            .unwrap();
        let mut env = FakeEnv::new(&[("RUST_LOG", "warn"), ("OWN", "mine")]);
        set_group_enabled(&store, &mut env, group_id, true).unwrap();

        let error = write_system_variable(
            &store,
            &mut env,
            &SystemVariable::new("RUST_LOG", "trace", ValueKind::Plain),
        )
        .expect_err("被分组接管的 key 不允许直接编辑");
        assert!(error.contains("Rust"));
        assert_eq!(env.value_of("RUST_LOG").as_deref(), Some("debug"));

        write_system_variable(
            &store,
            &mut env,
            &SystemVariable::new("OWN", "ours", ValueKind::Plain),
        )
        .unwrap();
        assert_eq!(env.value_of("OWN").as_deref(), Some("ours"));

        assert!(remove_system_variable(&store, &mut env, "RUST_LOG").is_err());
        remove_system_variable(&store, &mut env, "OWN").unwrap();
        assert!(!env.contains("OWN"));
    }

    #[test]
    fn failed_writes_keep_the_baseline_for_a_retry() {
        struct FailOnWrite;
        impl EnvBackend for FailOnWrite {
            fn read(&self) -> Result<BTreeMap<String, SystemVariable>, String> {
                Ok(BTreeMap::from([(
                    "KEEP".to_string(),
                    SystemVariable::new("KEEP", "original", ValueKind::Plain),
                )]))
            }
            fn write(&mut self, _: &str, _: &str, _: ValueKind) -> Result<(), String> {
                Err("写入被系统拒绝".to_string())
            }
            fn remove(&mut self, _: &str) -> Result<(), String> {
                Ok(())
            }
        }

        let store = memory_store();
        let group_id = store.create_group("Rust", "").unwrap();
        store
            .add_variable(group_id, "KEEP", "debug", ValueKind::Plain)
            .unwrap();

        let report = set_group_enabled(&store, &mut FailOnWrite, group_id, true).unwrap();
        assert_eq!(report.failures.len(), 1);
        // 基线还在：下一次停用仍能恢复。
        assert_eq!(
            store.baselines().unwrap()["KEEP"].value.as_deref(),
            Some("original")
        );
    }

    #[test]
    fn reconcile_twice_is_idempotent() {
        let store = memory_store();
        let group_id = store.create_group("Rust", "").unwrap();
        store
            .add_variable(group_id, "RUST_LOG", "debug", ValueKind::Plain)
            .unwrap();
        let mut env = FakeEnv::new(&[("RUST_LOG", "warn")]);

        set_group_enabled(&store, &mut env, group_id, true).unwrap();
        let second = reconcile(&store, &mut env).unwrap();
        assert!(
            second.is_quiet(),
            "第二次解析不应再改动任何东西: {second:?}"
        );
    }
}
