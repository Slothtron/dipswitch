//! 环境变量分组的分层模型与解析（纯逻辑，不依赖 GPUI 与系统调用）。
//!
//! 语义：
//!
//! - 每个分组是一层「期望值」；启用的分组按**启用先后**排优先级，最后启用的
//!   一层覆盖前一层里的同名 key。
//! - 某个 key 第一次被分组接管时，把当时的用户环境变量值记入**基线**
//!   （包括「原本不存在」这一状态）。
//! - 停用分组后重新解析：还有别的分组接管就生效那层的值；没有任何分组接管
//!   就恢复基线，基线是「原本不存在」就删除该变量。
//!
//! [`plan`] 是这个规则的唯一实现：输入「启用层 + 基线 + 当前系统变量」，输出
//! 一份可执行的写入计划与需要新记录的基线，视图与存储层都只负责喂数据和落盘。

use std::collections::BTreeMap;

use crate::system_env::{SystemVariable, ValueKind};

/// 一个环境变量分组。记录由 `DipswitchStore` 直接读写，不走 serde。
#[derive(Debug, Clone)]
pub struct EnvGroup {
    pub id: i64,
    pub name: String,
    pub note: String,
    pub is_enabled: bool,
    /// 启用序号，越大优先级越高；停用时清空。
    pub enabled_seq: Option<i64>,
}

/// 分组内的一条变量。
#[derive(Debug, Clone)]
pub struct GroupVariable {
    pub id: i64,
    pub group_id: i64,
    pub key: String,
    pub value: String,
    pub kind: ValueKind,
}

/// 分组接管某个 key 之前，该 key 在用户环境变量里的原始状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Baseline {
    pub key: String,
    /// `None` 表示接管时该变量并不存在，恢复动作因此是删除。
    pub value: Option<String>,
    pub kind: ValueKind,
}

/// 一层已启用的分组及其变量。
#[derive(Debug, Clone)]
pub struct Layer {
    pub group_id: i64,
    pub group_name: String,
    pub enabled_seq: i64,
    pub variables: Vec<GroupVariable>,
}

/// 把分组与其变量组装成按启用顺序排列的层。
pub fn build_layers(groups: &[EnvGroup], variables: &[GroupVariable]) -> Vec<Layer> {
    let mut layers = groups
        .iter()
        .filter(|group| group.is_enabled)
        .filter_map(|group| {
            let enabled_seq = group.enabled_seq?;
            Some(Layer {
                group_id: group.id,
                group_name: group.name.clone(),
                enabled_seq,
                variables: variables
                    .iter()
                    .filter(|variable| variable.group_id == group.id)
                    .cloned()
                    .collect(),
            })
        })
        .collect::<Vec<_>>();
    layers.sort_by_key(Layer::priority);
    layers
}

impl Layer {
    fn priority(&self) -> i64 {
        self.enabled_seq
    }

    /// 该层里某个 key 的变量（同层重复 key 由存储层的唯一约束保证不会出现）。
    pub fn variable(&self, key: &str) -> Option<&GroupVariable> {
        self.variables.iter().find(|variable| variable.key == key)
    }
}

/// 同名 key 上「谁赢、谁被压住」的诊断信息，供界面标注来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyOrigin {
    /// 最终生效的分组名；`None` 表示没有分组接管。
    pub winning_group: Option<String>,
    /// 被更高层压住（因此不会生效）的分组名，按优先级从低到高。
    pub shadowed_groups: Vec<String>,
    /// 基线仍在保护这个 key（即存在需要恢复的原值记录）。
    pub has_baseline: bool,
}

/// 计划里的一步动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepAction {
    /// 写入（新增或覆盖）为用户环境里的这个值。
    Set { value: String, kind: ValueKind },
    /// 从用户环境里删除。
    Delete,
    /// 系统里已经是目标值，无需写入。
    Keep,
}

/// 计划里的一步。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub key: String,
    pub action: StepAction,
    /// 这一步是为哪个分组执行的；`None` 表示恢复基线。
    pub for_group: Option<(i64, String)>,
}

/// 需要新记录的基线：接管前该 key 的原值（`None` = 原本不存在）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineCapture {
    pub key: String,
    pub value: Option<String>,
    pub kind: ValueKind,
}

/// 一次解析的完整结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub steps: Vec<Step>,
    pub captures: Vec<BaselineCapture>,
    /// 已消费完（分组不再接管、值也已恢复）的基线 key，应当删除。
    pub released: Vec<String>,
}

/// 某个 key 最终由哪一层决定。
fn winner<'a>(layers: &'a [Layer], key: &str) -> Option<&'a Layer> {
    // layers 已按优先级升序，反向找到第一个定义该 key 的层即最高优先级。
    layers
        .iter()
        .rev()
        .find(|layer| layer.variable(key).is_some())
}

/// 按分层规则算出要写什么、要记哪些基线、可以释放哪些基线。
///
/// `current` 是当前用户环境变量，`baselines` 是已记录的基线。
pub fn plan(
    layers: &[Layer],
    baselines: &BTreeMap<String, Baseline>,
    current: &BTreeMap<String, SystemVariable>,
) -> Plan {
    let mut result = Plan::default();
    let mut managed: BTreeMap<&str, &GroupVariable> = BTreeMap::new();

    for layer in layers {
        for variable in &layer.variables {
            managed.insert(variable.key.as_str(), variable);
        }
    }

    for (key, variable) in &managed {
        let winning = winner(layers, key).expect("managed key has a winning layer");
        let desired_kind = variable.kind;
        let desired_value = variable.value.clone();
        let current_variable = current.get(*key);

        // 第一次接管：先记下原值，否则停用分组后无法恢复。
        if !baselines.contains_key(*key) {
            result.captures.push(BaselineCapture {
                key: (*key).to_string(),
                value: current_variable
                    .map(SystemVariable::raw)
                    .map(str::to_string),
                kind: current_variable
                    .map(SystemVariable::kind)
                    .unwrap_or(desired_kind),
            });
        }

        result.steps.push(Step {
            key: (*key).to_string(),
            action: match current_variable {
                Some(existing)
                    if existing.raw() == desired_value && existing.kind() == desired_kind =>
                {
                    StepAction::Keep
                }
                _ => StepAction::Set {
                    value: desired_value,
                    kind: desired_kind,
                },
            },
            for_group: Some((winning.group_id, winning.group_name.clone())),
        });
    }

    // 基线里有、但已经没有任何启用分组接管的 key：恢复原值并释放基线。
    for (key, baseline) in baselines {
        if managed.contains_key(key.as_str()) {
            continue;
        }
        let current_variable = current.get(key.as_str());
        result.steps.push(Step {
            key: key.clone(),
            action: match (&baseline.value, current_variable) {
                (None, None) => StepAction::Keep,
                (None, Some(_)) => StepAction::Delete,
                (Some(value), Some(existing))
                    if existing.raw() == value && existing.kind() == baseline.kind =>
                {
                    StepAction::Keep
                }
                (Some(value), _) => StepAction::Set {
                    value: value.clone(),
                    kind: baseline.kind,
                },
            },
            for_group: None,
        });
        result.released.push(key.clone());
    }

    result.steps.sort_by(|a, b| a.key.cmp(&b.key));
    result
}

/// 汇总每个被分组接管的 key 的来源与被压住情况。
pub fn origins(
    layers: &[Layer],
    baselines: &BTreeMap<String, Baseline>,
) -> BTreeMap<String, KeyOrigin> {
    let mut keys: Vec<&str> = layers
        .iter()
        .flat_map(|layer| layer.variables.iter().map(|variable| variable.key.as_str()))
        .collect();
    keys.sort_unstable();
    keys.dedup();

    keys.into_iter()
        .map(|key| {
            let winning = winner(layers, key);
            let shadowed_groups = layers
                .iter()
                .filter(|layer| layer.variable(key).is_some())
                .filter(|layer| Some(layer.group_id) != winning.map(|layer| layer.group_id))
                .map(|layer| layer.group_name.clone())
                .collect();
            (
                key.to_string(),
                KeyOrigin {
                    winning_group: winning.map(|layer| layer.group_name.clone()),
                    shadowed_groups,
                    has_baseline: baselines.contains_key(key),
                },
            )
        })
        .collect()
}

/// key 名称校验：拒绝空值、含 `=` 或控制字符的名字。
pub fn validate_key(key: &str) -> Result<(), &'static str> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return Err("变量名不能为空");
    }
    if trimmed != key {
        return Err("变量名首尾不能有空格");
    }
    if trimmed.contains('=') {
        return Err("变量名不能包含 =");
    }
    if trimmed
        .chars()
        .any(|ch| ch == '\0' || ch.is_control() || ch.is_whitespace())
    {
        return Err("变量名不能包含空白或控制字符");
    }
    Ok(())
}

/// 值校验：只拒绝无法写回注册表的字符。
pub fn validate_value(value: &str) -> Result<(), &'static str> {
    if value.chars().any(|ch| ch == '\0') {
        return Err("值不能包含空字符");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(id: i64, name: &str, enabled: bool, enabled_seq: Option<i64>) -> EnvGroup {
        EnvGroup {
            id,
            name: name.to_string(),
            note: String::new(),
            is_enabled: enabled,
            enabled_seq,
        }
    }

    fn variable(group_id: i64, key: &str, value: &str) -> GroupVariable {
        GroupVariable {
            id: group_id * 100 + key.len() as i64,
            group_id,
            key: key.to_string(),
            value: value.to_string(),
            kind: ValueKind::Plain,
        }
    }

    fn system(key: &str, value: &str) -> SystemVariable {
        SystemVariable::new(key, value, ValueKind::Plain)
    }

    fn map(variables: Vec<SystemVariable>) -> BTreeMap<String, SystemVariable> {
        variables
            .into_iter()
            .map(|variable| (variable.key().to_string(), variable))
            .collect()
    }

    #[test]
    fn last_enabled_layer_wins_on_same_key() {
        let layers = build_layers(
            &[
                group(1, "Go", true, Some(1)),
                group(2, "Rust", true, Some(2)),
            ],
            &[
                variable(1, "RUST_LOG", "warn"),
                variable(2, "RUST_LOG", "debug"),
            ],
        );

        let plan = plan(
            &layers,
            &BTreeMap::new(),
            &map(vec![system("RUST_LOG", "warn")]),
        );
        assert_eq!(
            plan.steps,
            vec![Step {
                key: "RUST_LOG".to_string(),
                action: StepAction::Set {
                    value: "debug".to_string(),
                    kind: ValueKind::Plain,
                },
                for_group: Some((2, "Rust".to_string())),
            }]
        );
        // 第一次接管必须记录原值，停用后才能还原。
        assert_eq!(plan.captures.len(), 1);
        assert_eq!(plan.captures[0].value.as_deref(), Some("warn"));
    }

    #[test]
    fn disabling_the_winner_falls_back_to_the_layer_below() {
        // Rust 停用后只剩 Go 这一层。
        let layers = build_layers(
            &[group(1, "Go", true, Some(1)), group(2, "Rust", false, None)],
            &[
                variable(1, "RUST_LOG", "warn"),
                variable(2, "RUST_LOG", "debug"),
            ],
        );
        let baselines = BTreeMap::from([(
            "RUST_LOG".to_string(),
            Baseline {
                key: "RUST_LOG".to_string(),
                value: Some("info".to_string()),
                kind: ValueKind::Plain,
            },
        )]);

        let plan = plan(&layers, &baselines, &map(vec![system("RUST_LOG", "debug")]));
        assert_eq!(plan.captures, vec![]);
        assert_eq!(
            plan.steps[0].action,
            StepAction::Set {
                value: "warn".to_string(),
                kind: ValueKind::Plain
            }
        );
    }

    #[test]
    fn last_layer_off_restores_the_baseline_value() {
        let layers = build_layers(
            &[group(1, "Go", false, None)],
            &[variable(1, "GOFLAGS", "-mod=vendor")],
        );
        let baselines = BTreeMap::from([(
            "GOFLAGS".to_string(),
            Baseline {
                key: "GOFLAGS".to_string(),
                value: Some("-mod=mod".to_string()),
                kind: ValueKind::Plain,
            },
        )]);

        let plan = plan(
            &layers,
            &baselines,
            &map(vec![system("GOFLAGS", "-mod=vendor")]),
        );
        assert_eq!(
            plan.steps,
            vec![Step {
                key: "GOFLAGS".to_string(),
                action: StepAction::Set {
                    value: "-mod=mod".to_string(),
                    kind: ValueKind::Plain,
                },
                for_group: None,
            }]
        );
        assert_eq!(plan.released, vec!["GOFLAGS".to_string()]);
    }

    #[test]
    fn baseline_of_absent_key_restores_by_deleting_it() {
        let layers = build_layers(&[], &[]);
        let baselines = BTreeMap::from([(
            "TEMP_KEY".to_string(),
            Baseline {
                key: "TEMP_KEY".to_string(),
                value: None,
                kind: ValueKind::Plain,
            },
        )]);

        let activated = plan(
            &layers,
            &baselines,
            &map(vec![system("TEMP_KEY", "grouped")]),
        );
        assert_eq!(activated.steps[0].action, StepAction::Delete);

        // 系统里本来也没有（例如用户手工删过），不必再写。
        let idle = plan(&layers, &baselines, &map(vec![]));
        assert_eq!(idle.steps[0].action, StepAction::Keep);
    }

    #[test]
    fn already_matching_value_does_not_rewrite() {
        let layers = build_layers(
            &[group(1, "Go", true, Some(1))],
            &[variable(1, "GO111MODULE", "on")],
        );
        let baselines = BTreeMap::from([(
            "GO111MODULE".to_string(),
            Baseline {
                key: "GO111MODULE".to_string(),
                value: Some("auto".to_string()),
                kind: ValueKind::Plain,
            },
        )]);

        let plan = plan(&layers, &baselines, &map(vec![system("GO111MODULE", "on")]));
        assert_eq!(plan.steps[0].action, StepAction::Keep);
        assert!(plan.captures.is_empty());
        assert!(plan.released.is_empty());
    }

    #[test]
    fn expandable_kind_is_written_back_with_its_type() {
        let layers = build_layers(
            &[group(1, "Path", true, Some(1))],
            &[GroupVariable {
                id: 1,
                group_id: 1,
                key: "Path_Add".to_string(),
                value: "%USERPROFILE%\\bin".to_string(),
                kind: ValueKind::Expandable,
            }],
        );

        let plan = plan(&layers, &BTreeMap::new(), &map(vec![]));
        assert_eq!(
            plan.steps[0].action,
            StepAction::Set {
                value: "%USERPROFILE%\\bin".to_string(),
                kind: ValueKind::Expandable
            }
        );
        assert_eq!(plan.captures[0].value, None);
    }

    #[test]
    fn origins_report_the_shadowed_chain_and_baseline() {
        let layers = build_layers(
            &[
                group(1, "Go", true, Some(1)),
                group(2, "Rust", true, Some(2)),
                group(3, "Node", true, Some(3)),
            ],
            &[
                variable(1, "RUST_LOG", "warn"),
                variable(2, "RUST_LOG", "debug"),
                variable(3, "NODE_ENV", "dev"),
            ],
        );
        let baselines = BTreeMap::from([(
            "RUST_LOG".to_string(),
            Baseline {
                key: "RUST_LOG".to_string(),
                value: Some("info".to_string()),
                kind: ValueKind::Plain,
            },
        )]);

        let origins = origins(&layers, &baselines);
        let rust_log = &origins["RUST_LOG"];
        assert_eq!(rust_log.winning_group.as_deref(), Some("Rust"));
        assert_eq!(rust_log.shadowed_groups, vec!["Go".to_string()]);
        assert!(rust_log.has_baseline);
        assert!(origins["NODE_ENV"].shadowed_groups.is_empty());
    }

    #[test]
    fn key_validation_rejects_names_that_cannot_round_trip() {
        assert!(validate_key("PATH").is_ok());
        assert_eq!(validate_key("  "), Err("变量名不能为空"));
        assert_eq!(validate_key("A B"), Err("变量名不能包含空白或控制字符"));
        assert_eq!(validate_key("A=B"), Err("变量名不能包含 ="));
        assert_eq!(validate_key(" A"), Err("变量名首尾不能有空格"));
        assert_eq!(validate_value("a\0b"), Err("值不能包含空字符"));
    }
}
