//! 环境变量模块：用户级变量的读取/编辑，以及自定义分组的启用与恢复。
//!
//! 状态所有权：
//! - 数据在 [`DipswitchStore`]（分组、组内变量、基线、流水）与真实环境里；视图只
//!   持有**快照**，每次操作后整体重载，避免两份状态互相漂移；
//! - 两个页签（变量 / 分组）共享同一份快照；
//! - 弹窗表单是独立实体（[`VariableForm`] / [`GroupForm`]），不在 `render` 里重建；
//! - 同名 key 谁生效、原值何时记录、停用后恢复什么，全部由 [`env_service`] 与
//!   [`env_model`] 决定，视图只负责把结果如实呈现。

use std::collections::BTreeMap;

use gpui_kit::{
    AnyElement, App, AppContext, ClipboardItem, Context, Div, Entity, FocusHandle, Focusable, Font,
    FontWeight, InteractiveElement, IntoElement, ParentElement, Pixels, Render, SharedString,
    StatefulInteractiveElement, Styled, WeakEntity, Window, actions,
    base::{InteractiveElementExt as _, StyledExt},
    component::{
        ActiveTheme, Icon, Selectable, Sizable, WindowExt,
        button::{Button, ButtonVariants},
        checkbox::Checkbox,
        dialog::{DialogAction, DialogClose, DialogFooter},
        input::{Input, InputState},
        menu::{DropdownMenu, PopupMenuItem},
        notification::NotificationType,
        resizable::{h_resizable, resizable_panel},
        switch::Switch,
    },
    div,
    prelude::FluentBuilder,
    px,
};

use crate::{
    env_model::{self, EnvGroup, GroupVariable, KeyOrigin, validate_key, validate_value},
    env_service::{self, ReconcileReport, SystemEnvBackend},
    icon::AppIcon,
    store::DipswitchStore,
    system_env::{self, SystemVariable, ValueKind},
};

pub const KEY_CONTEXT: &str = "Environment";

actions!(environment, [ReloadVariables, AddVariable, NewGroup]);

/// 模块内的两个目的地。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvironmentTab {
    Variables,
    Groups,
}

impl EnvironmentTab {
    const ALL: [Self; 2] = [Self::Variables, Self::Groups];

    fn label(self) -> &'static str {
        match self {
            Self::Variables => "Variables",
            Self::Groups => "Groups",
        }
    }
}

/// 变量弹窗正在编辑的对象。
#[derive(Debug, Clone, PartialEq, Eq)]
enum VariableTarget {
    /// 新建一条用户环境变量（不经过分组）。
    NewSystem,
    /// 编辑已有的用户环境变量。
    System(String),
    /// 在分组里新建变量。
    NewInGroup(i64),
    /// 编辑分组里的变量。
    InGroup { group_id: i64, id: i64, key: String },
}

/// 变量表单的 retained 状态。
pub struct VariableForm {
    target: VariableTarget,
    key: Entity<InputState>,
    value: Entity<InputState>,
    kind: ValueKind,
    title: SharedString,
    /// key 只在新建时可填；编辑已有变量时名称是身份，不允许改。
    is_key_editable: bool,
}

/// 分组表单的 retained 状态。
pub struct GroupForm {
    /// `None` 表示新建。
    id: Option<i64>,
    name: Entity<InputState>,
    note: Entity<InputState>,
    title: SharedString,
}

pub struct EnvironmentModule {
    tab: EnvironmentTab,
    search: Entity<InputState>,
    variables: Vec<SystemVariable>,
    groups: Vec<EnvGroup>,
    group_variables: Vec<GroupVariable>,
    origins: BTreeMap<String, KeyOrigin>,
    events: Vec<(i64, String)>,
    selected_group: Option<i64>,
    load_error: Option<String>,
    focus_handle: FocusHandle,
}

impl EnvironmentModule {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Filter variables…")
                .clean_on_escape()
        });

        let mut module = Self {
            tab: EnvironmentTab::Variables,
            search,
            variables: Vec::new(),
            groups: Vec::new(),
            group_variables: Vec::new(),
            origins: BTreeMap::new(),
            events: Vec::new(),
            selected_group: None,
            load_error: None,
            focus_handle: cx.focus_handle().tab_stop(true),
        };
        module.reload(cx);
        module
    }

    fn store(cx: &App) -> &DipswitchStore {
        cx.global::<DipswitchStore>()
    }

    /// 重新读取配置库与真实环境，刷新所有快照。
    fn reload(&mut self, cx: &mut Context<Self>) {
        let store = Self::store(cx);
        self.load_error = None;
        self.groups = store.groups().unwrap_or_else(|error| {
            self.load_error = Some(error.clone());
            Vec::new()
        });
        self.group_variables = store.group_variables().unwrap_or_default();
        self.events = store.recent_events(12).unwrap_or_default();

        match system_env::read_variables() {
            Ok(variables) => self.variables = variables,
            Err(error) => {
                self.load_error = Some(error.clone());
                self.variables = Vec::new();
            }
        }

        let layers = env_model::build_layers(&self.groups, &self.group_variables);
        let baselines = store.baselines().unwrap_or_default();
        self.origins = env_model::origins(&layers, &baselines);

        // 选中的分组可能刚被删掉，退回列表第一项。
        if let Some(id) = self.selected_group
            && !self.groups.iter().any(|group| group.id == id)
        {
            self.selected_group = None;
        }
        if self.selected_group.is_none() {
            self.selected_group = self.groups.first().map(|group| group.id);
        }
        cx.notify();
    }

    /// 把一次重新解析的结果如实报出来：失败要说清是哪一项，成功说明动了什么。
    fn report(&self, report: &ReconcileReport, reason: &str, window: &mut Window, cx: &mut App) {
        if !report.failures.is_empty() {
            let detail = report
                .failures
                .iter()
                .map(|(key, error)| format!("{key}：{error}"))
                .collect::<Vec<_>>()
                .join("；");
            window.push_notification(
                (
                    NotificationType::Warning,
                    SharedString::from(format!("{reason}未完全生效：{detail}")),
                ),
                cx,
            );
        } else if !report.is_quiet() {
            window.push_notification((NotificationType::Success, report.summary(reason)), cx);
        }
    }

    fn apply_and_reload(
        &mut self,
        result: Result<ReconcileReport, String>,
        reason: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(report) => self.report(&report, reason, window, cx),
            Err(error) => {
                window.push_notification((NotificationType::Warning, error), cx);
            }
        }
        self.reload(cx);
    }

    fn toggle_group(
        &mut self,
        group_id: i64,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = Self::store(cx);
        let mut backend = SystemEnvBackend;
        let result = env_service::set_group_enabled(store, &mut backend, group_id, enabled);
        self.apply_and_reload(
            result,
            if enabled {
                "启用分组"
            } else {
                "停用分组"
            },
            window,
            cx,
        );
    }

    fn delete_group(&mut self, group_id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let store = Self::store(cx);
        if let Err(error) = store.delete_group(group_id) {
            window.push_notification((NotificationType::Warning, error), cx);
            return;
        }
        let mut backend = SystemEnvBackend;
        let result = env_service::reconcile(store, &mut backend);
        self.apply_and_reload(result, "删除分组", window, cx);
    }

    fn remove_system_variable(&mut self, key: String, window: &mut Window, cx: &mut Context<Self>) {
        let store = Self::store(cx);
        let mut backend = SystemEnvBackend;
        let result = env_service::remove_system_variable(store, &mut backend, &key);
        match result {
            Ok(()) => {
                window.push_notification(
                    (
                        NotificationType::Success,
                        SharedString::from(format!("已删除 {key}")),
                    ),
                    cx,
                );
            }
            Err(error) => {
                window.push_notification((NotificationType::Warning, error), cx);
            }
        }
        self.reload(cx);
    }

    fn copy_value(&mut self, value: String, window: &mut Window, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(value));
        window.push_notification((NotificationType::Success, "Copied value"), cx);
    }

    /// 把一条当前变量复制进分组，走通「顺手把它做成一组」的路径。
    fn capture_into_group(
        &mut self,
        variable: SystemVariable,
        group_id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = Self::store(cx);
        let result = store.add_variable(group_id, variable.key(), variable.raw(), variable.kind());
        match result {
            Ok(_) => {
                let mut backend = SystemEnvBackend;
                let reconcile = env_service::reconcile(store, &mut backend);
                self.apply_and_reload(reconcile, "加入分组", window, cx);
            }
            Err(error) => {
                window.push_notification((NotificationType::Warning, error), cx);
                self.reload(cx);
            }
        }
    }

    fn delete_group_variable(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let store = Self::store(cx);
        if let Err(error) = store.delete_variable(id) {
            window.push_notification((NotificationType::Warning, error), cx);
            return;
        }
        let mut backend = SystemEnvBackend;
        let result = env_service::reconcile(store, &mut backend);
        self.apply_and_reload(result, "删除分组变量", window, cx);
    }

    /// 只清流水，不动分组与变量。
    fn clear_event_log(&mut self, cx: &mut Context<Self>) {
        let store = Self::store(cx);
        let _ = store.clear_events();
        self.reload(cx);
    }

    // ------------------------------------------------------------- 表单与弹窗

    fn open_variable_dialog(
        &mut self,
        target: VariableTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (key_value, value_value, kind, title, is_key_editable) = match &target {
            VariableTarget::NewSystem => (
                String::new(),
                String::new(),
                ValueKind::Plain,
                "New variable".to_string(),
                true,
            ),
            VariableTarget::System(key) => {
                let existing = self.variables.iter().find(|item| item.key() == key);
                (
                    key.clone(),
                    existing
                        .map(|item| item.raw().to_string())
                        .unwrap_or_default(),
                    existing
                        .map(SystemVariable::kind)
                        .unwrap_or(ValueKind::Plain),
                    "Edit variable".to_string(),
                    false,
                )
            }
            VariableTarget::NewInGroup(group_id) => (
                String::new(),
                String::new(),
                ValueKind::Plain,
                format!("New variable in “{}”", self.group_name(*group_id)),
                true,
            ),
            VariableTarget::InGroup { id, key, .. } => {
                let existing = self.group_variables.iter().find(|item| &item.id == id);
                (
                    key.clone(),
                    existing.map(|item| item.value.clone()).unwrap_or_default(),
                    existing.map(|item| item.kind).unwrap_or(ValueKind::Plain),
                    "Edit group variable".to_string(),
                    false,
                )
            }
        };
        let title = SharedString::from(title);

        let key_state = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("NAME")
                .default_value(key_value)
        });
        let value_state = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(value_value)
                .placeholder(r"C:\tools\bin 或 %USERPROFILE%\bin")
        });
        let form = cx.new(|_| VariableForm {
            target,
            key: key_state,
            value: value_state,
            kind,
            title,
            is_key_editable,
        });

        let module = cx.weak_entity();
        let content_form = form.clone();
        let dialog_title = form.read(cx).title.clone();
        let is_key_editable = form.read(cx).is_key_editable;
        window.open_dialog(cx, move |dialog, _, _| {
            let form = form.clone();
            let content_form = content_form.clone();
            dialog
                .title(dialog_title.clone())
                .w(px(460.))
                .content(move |content, window, cx| {
                    content.child(render_variable_form(
                        &content_form,
                        is_key_editable,
                        window,
                        cx,
                    ))
                })
                .footer(
                    DialogFooter::new()
                        .child(DialogClose::new().trigger(|button| button.label("Cancel")))
                        .child(
                            DialogAction::new()
                                .child(Button::new("save-variable").primary().label("Save")),
                        ),
                )
                .on_ok({
                    let module = module.clone();
                    move |_, window, cx| commit_variable(module.clone(), form.clone(), window, cx)
                })
        });
    }

    fn open_group_dialog(&mut self, id: Option<i64>, window: &mut Window, cx: &mut Context<Self>) {
        let (name, note, title) = match id {
            Some(id) => {
                let group = self.groups.iter().find(|group| group.id == id);
                (
                    group.map(|group| group.name.clone()).unwrap_or_default(),
                    group.map(|group| group.note.clone()).unwrap_or_default(),
                    "Rename group",
                )
            }
            None => (String::new(), String::new(), "New group"),
        };

        let name_state = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Rust toolchain")
                .default_value(name)
        });
        let note_state = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Optional note")
                .default_value(note)
        });
        let form = cx.new(|_| GroupForm {
            id,
            name: name_state,
            note: note_state,
            title: SharedString::from(title),
        });

        let module = cx.weak_entity();
        let content_form = form.clone();
        let dialog_title = form.read(cx).title.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let form = form.clone();
            let content_form = content_form.clone();
            let module = module.clone();
            dialog
                .title(dialog_title.clone())
                .w(px(420.))
                .content(move |content, window, cx| {
                    content.child(render_group_form(&content_form, window, cx))
                })
                .footer(
                    DialogFooter::new()
                        .child(DialogClose::new().trigger(|button| button.label("Cancel")))
                        .child(
                            DialogAction::new()
                                .child(Button::new("save-group").primary().label("Save")),
                        ),
                )
                .on_ok(move |_, window, cx| commit_group(module.clone(), form.clone(), window, cx))
        });
    }

    fn confirm_remove_variable(&self, key: String, window: &mut Window, cx: &mut Context<Self>) {
        let module = cx.weak_entity();
        let title = SharedString::from(format!("Delete “{key}”?"));
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let module = module.clone();
            let key = key.clone();
            dialog
                .confirm()
                .title(title.clone())
                .description(
                    "This removes the variable from the user environment. Group copies are not touched.",
                )
                .ok_text("Delete")
                .on_ok(move |_, window, cx| {
                    module
                        .update(cx, |this, cx| {
                            this.remove_system_variable(key.clone(), window, cx)
                        })
                        .is_ok()
                })
        });
    }

    fn select_group(&mut self, group_id: i64, cx: &mut Context<Self>) {
        if self.selected_group == Some(group_id) {
            return;
        }
        self.selected_group = Some(group_id);
        cx.notify();
    }

    fn set_tab(&mut self, tab: EnvironmentTab, cx: &mut Context<Self>) {
        if self.tab == tab {
            return;
        }
        self.tab = tab;
        cx.notify();
    }

    // ---------------------------------------------------------------- 渲染

    fn render_header(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let scope_note = if system_env::is_persistent() {
            SharedString::from(format!("User scope · {}", system_env::scope_label()))
        } else {
            "User scope · current process only".into()
        };

        div()
            .px_4()
            .pt_3()
            .pb_2()
            .v_flex()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Environment"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(scope_note),
                    )
                    .child(div().flex_1())
                    .child(div().h_flex().items_center().gap_1().children(
                        EnvironmentTab::ALL.into_iter().map(|tab| {
                            Button::new(format!("tab-{}", tab.label()))
                                .ghost()
                                .small()
                                .selected(self.tab == tab)
                                .label(tab.label())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.set_tab(tab, cx);
                                }))
                        }),
                    ))
                    .child(
                        Button::new("reload-environment")
                            .ghost()
                            .small()
                            .icon(AppIcon::Refresh)
                            .label("Reload")
                            .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                    ),
            )
    }

    fn render_actions(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_4()
            .py_2()
            .h_flex()
            .items_center()
            .gap_2()
            .when(self.tab == EnvironmentTab::Variables, |this| {
                this.child(
                    div()
                        .w_64()
                        .child(Input::new(&self.search).w_full().cleanable(true).prefix(Icon::new(AppIcon::Search).small())),
                )
                .child(
                    Button::new("add-variable")
                        .label("Add variable")
                        .icon(AppIcon::Add)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_variable_dialog(VariableTarget::NewSystem, window, cx);
                        })),
                )
            })
            .when(self.tab == EnvironmentTab::Groups, |this| {
                let enabled = self.groups.iter().filter(|group| group.is_enabled).count();
                let total = self.groups.len();
                this.child(
                    Button::new("new-group")
                        .label("New group")
                        .icon(AppIcon::NewGroup)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_group_dialog(None, window, cx);
                        })),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "{enabled} of {total} enabled · the last group enabled wins on a same-name key"
                        )),
                )
            })
            .child(div().flex_1())
    }

    fn render_variables(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(error) = self.load_error.clone() {
            return self.render_error(error, cx);
        }
        let variables = self.filtered_variables(cx);
        if variables.is_empty() {
            let filtering = !self.search.read(cx).value().is_empty();
            return self.render_empty(
                if filtering {
                    (
                        "No matching variables",
                        "Clear the filter to list user variables.",
                    )
                } else {
                    (
                        "No user variables",
                        "Add one here, or build a group and enable it.",
                    )
                },
                cx,
            );
        }

        let rows = variables
            .iter()
            .cloned()
            .map(|variable| self.render_variable_row(variable, cx))
            .collect::<Vec<_>>();

        div()
            .flex_1()
            .min_h_0()
            .id("environment-variables-scroll")
            .overflow_y_scroll()
            .child(
                div()
                    .w_full()
                    .v_flex()
                    .child(system_variables_header(cx))
                    .child(div().w_full().v_flex().children(rows)),
            )
            .into_any_element()
    }

    /// 一行系统变量。悬停整行高亮，双击 key / value 区域打开编辑弹窗；
    /// 操作列是兄弟元素，所以在按钮上双击不会误触发行编辑。
    fn render_variable_row(
        &mut self,
        variable: SystemVariable,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = variable.key().to_string();
        let display = variable.display_value();
        let origin: Option<KeyOrigin> = self.origins.get(&key).cloned();
        let manager = origin.clone().and_then(|origin| origin.winning_group);
        let shadowed = origin
            .map(|origin| origin.shadowed_groups)
            .unwrap_or_default();
        let editable = variable.is_editable();
        let mono = monospace(cx);

        let copy_value = display.clone();
        let row_edit_key = key.clone();
        let menu_edit_key = key.clone();
        let delete_key = key.clone();
        let capture_variable = variable.clone();
        let groups: Vec<(i64, String)> = self
            .groups
            .iter()
            .filter(|group| group.is_enabled)
            .map(|group| (group.id, group.name.clone()))
            .collect();
        let module = cx.weak_entity();

        div()
            .id(format!("variable-row-{key}"))
            .w_full()
            .h_flex()
            .items_center()
            .text_sm()
            .border_b_1()
            .border_color(cx.theme().table_row_border)
            .hover(|this| this.bg(cx.theme().table_hover))
            .child(
                div()
                    .id(format!("variable-row-main-{key}"))
                    .h_flex()
                    .items_center()
                    .flex_1()
                    .min_w_0()
                    .on_double_click(move |_, window, cx| {
                        let module = module.clone();
                        let row_edit_key = row_edit_key.clone();
                        module
                            .update(cx, |this, cx| this.edit_key(&row_edit_key, window, cx))
                            .ok();
                    })
                    .child(
                        cell(Some(px(220.))).child(
                            div()
                                .font(mono.clone())
                                .text_sm()
                                .truncate()
                                .child(key.clone()),
                        ),
                    )
                    .child(
                        cell(None)
                            .child(div().font(mono).text_sm().truncate().child(display.clone())),
                    )
                    .child(
                        cell(Some(px(120.))).child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(variable.kind().label()),
                        ),
                    )
                    .child(
                        cell(Some(px(150.))).child(
                            div()
                                .text_xs()
                                .truncate()
                                .text_color(if manager.is_some() {
                                    cx.theme().primary
                                } else {
                                    cx.theme().muted_foreground
                                })
                                .child(match &manager {
                                    Some(group) => format!("Group · {group}"),
                                    None if !shadowed.is_empty() => "Overridden".to_string(),
                                    None => "—".to_string(),
                                }),
                        ),
                    ),
            )
            .child(
                cell(Some(px(96.))).justify_end().child(
                    div()
                        .h_flex()
                        .items_center()
                        .justify_end()
                        .gap_1()
                        .child(
                            Button::new(format!("copy-variable-{key}"))
                                .ghost()
                                .xsmall()
                                .icon(AppIcon::Copy)
                                .tooltip("Copy value")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.copy_value(copy_value.clone(), window, cx);
                                })),
                        )
                        .child(
                            Button::new(format!("more-variable-{key}"))
                                .ghost()
                                .xsmall()
                                .icon(AppIcon::More)
                                .dropdown_menu({
                                    let weak = cx.weak_entity();
                                    let manager_label = manager.clone();
                                    move |menu, _, _cx| {
                                        let edit_weak = weak.clone();
                                        let delete_weak = weak.clone();
                                        let capture_weak = weak.clone();
                                        let edit_key = menu_edit_key.clone();
                                        let delete_key = delete_key.clone();
                                        let edit_label = match &manager_label {
                                            Some(group) => {
                                                format!("Edit the copy in “{group}”").into()
                                            }
                                            None => SharedString::from("Edit variable"),
                                        };
                                        let mut menu = menu
                                            .item(
                                                PopupMenuItem::new(edit_label)
                                                    .disabled(!editable)
                                                    .on_click(move |_, window, cx| {
                                                        let edit_key = edit_key.clone();
                                                        edit_weak
                                                            .update(cx, |this, cx| {
                                                                this.edit_key(&edit_key, window, cx)
                                                            })
                                                            .ok();
                                                    }),
                                            )
                                            .item(
                                                PopupMenuItem::new("Delete from user environment")
                                                    .disabled(manager_label.is_some() || !editable)
                                                    .on_click(move |_, window, cx| {
                                                        let delete_key = delete_key.clone();
                                                        delete_weak
                                                            .update(cx, |this, cx| {
                                                                this.confirm_remove_variable(
                                                                    delete_key, window, cx,
                                                                )
                                                            })
                                                            .ok();
                                                    }),
                                            )
                                            .separator();
                                        if groups.is_empty() {
                                            menu.item(
                                                PopupMenuItem::new("Capture into group")
                                                    .disabled(true),
                                            )
                                        } else {
                                            for (group_id, name) in groups.clone() {
                                                let capture = capture_variable.clone();
                                                let weak = capture_weak.clone();
                                                menu = menu.item(
                                                    PopupMenuItem::new(format!(
                                                        "Capture into {name}"
                                                    ))
                                                    .on_click(move |_, window, cx| {
                                                        let capture = capture.clone();
                                                        weak.update(cx, |this, cx| {
                                                            this.capture_into_group(
                                                                capture, group_id, window, cx,
                                                            )
                                                        })
                                                        .ok();
                                                    }),
                                                );
                                            }
                                            menu
                                        }
                                    }
                                }),
                        ),
                ),
            )
            .into_any_element()
    }

    /// 被分组接管的 key 打开的是分组里那条变量——直接改系统值会被分组立刻覆盖。
    fn edit_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let managed = self
            .origins
            .get(key)
            .and_then(|origin| origin.winning_group.clone())
            .and_then(|name| {
                let group = self.groups.iter().find(|group| group.name == name)?;
                let variable = self
                    .group_variables
                    .iter()
                    .find(|variable| variable.group_id == group.id && variable.key == key)?;
                Some(VariableTarget::InGroup {
                    group_id: group.id,
                    id: variable.id,
                    key: key.to_string(),
                })
            });

        match managed {
            Some(target) => self.open_variable_dialog(target, window, cx),
            None => self.open_variable_dialog(VariableTarget::System(key.to_string()), window, cx),
        }
    }

    fn filtered_variables(&self, cx: &App) -> Vec<SystemVariable> {
        let query = self.search.read(cx).value().to_lowercase();
        if query.is_empty() {
            return self.variables.clone();
        }
        self.variables
            .iter()
            .filter(|variable| {
                variable.key().to_lowercase().contains(&query)
                    || variable.display_value().to_lowercase().contains(&query)
            })
            .cloned()
            .collect()
    }

    /// 分组列表所需的数据：分组、变量数与启用优先级（1 = 最早启用，0 = 未启用）。
    /// 一次算好再建元素，避免在渲染闭包里重复扫快照。
    fn group_rows(&self) -> Vec<(EnvGroup, usize, usize)> {
        let mut ranks: Vec<(i64, i64)> = self
            .groups
            .iter()
            .filter(|group| group.is_enabled)
            .filter_map(|group| group.enabled_seq.map(|seq| (group.id, seq)))
            .collect();
        ranks.sort_by_key(|(_, seq)| *seq);

        self.groups
            .iter()
            .map(|group| {
                let count = self
                    .group_variables
                    .iter()
                    .filter(|variable| variable.group_id == group.id)
                    .count();
                let rank = ranks
                    .iter()
                    .position(|(id, _)| *id == group.id)
                    .map(|ix| ix + 1)
                    .unwrap_or(0);
                (group.clone(), count, rank)
            })
            .collect()
    }

    fn render_groups(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(error) = self.load_error.clone() {
            return self.render_error(error, cx);
        }
        if self.groups.is_empty() {
            return self.render_empty(
                (
                    "No groups yet",
                    "Create a group, add its variables, then enable it.",
                ),
                cx,
            );
        }

        let selected = self.selected_group;
        // 先建详情再建列表：列表行只借 `&self` / `&Context`，而详情要注册监听器，
        // 顺序反过来会让两段借用同时活着。
        let detail = match selected
            .and_then(|id| self.groups.iter().find(|group| group.id == id).cloned())
        {
            Some(group) => self.render_group_detail(group, cx),
            None => self.render_empty(("Select a group", "Pick a group to edit it."), cx),
        };

        let rows = self.group_rows();
        let context: &Context<Self> = cx;
        let list = div()
            .size_full()
            .min_h_0()
            .id("environment-groups-scroll")
            .overflow_y_scroll()
            .child(div().p_2().v_flex().gap_1().children(rows.into_iter().map(
                |(group, count, rank)| {
                    self.render_group_row(&group, count, rank, selected == Some(group.id), context)
                },
            )));

        div()
            .flex_1()
            .min_h_0()
            .child(
                h_resizable("environment-groups")
                    .child(
                        resizable_panel()
                            .size(px(260.))
                            .size_range(px(200.)..px(420.))
                            .child(list),
                    )
                    .child(resizable_panel().min_w_0().child(detail)),
            )
            .into_any_element()
    }

    /// 分组列表里的一行：名称、变量数、启用优先级与启用开关。
    fn render_group_row(
        &self,
        group: &EnvGroup,
        variable_count: usize,
        rank: usize,
        is_selected: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let group_id = group.id;
        let is_enabled = group.is_enabled;
        let status = if is_enabled {
            format!("{variable_count} variables · priority {rank}")
        } else {
            format!("{variable_count} variables · off")
        };
        let secondary = if is_selected {
            cx.theme().accent_foreground.opacity(0.8)
        } else {
            cx.theme().muted_foreground
        };
        let select_module = cx.weak_entity();
        let switch_module = select_module.clone();

        div()
            .id(format!("group-row-{group_id}"))
            .h_flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .rounded(cx.theme().radius)
            .hover(|this| this.bg(cx.theme().accent.opacity(0.5)))
            .when(is_selected, |this| {
                this.bg(cx.theme().accent)
                    .text_color(cx.theme().accent_foreground)
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .truncate()
                            .child(group.name.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(secondary)
                            .truncate()
                            .child(status),
                    ),
            )
            .child(
                Switch::new(format!("toggle-group-{group_id}"))
                    .small()
                    .checked(is_enabled)
                    .tooltip(if is_enabled {
                        "Disable this group and restore what it overrode"
                    } else {
                        "Enable this group"
                    })
                    .on_change(move |checked, window, cx| {
                        let checked = *checked;
                        let _ = switch_module.update(cx, |this, cx| {
                            this.toggle_group(group_id, checked, window, cx)
                        });
                    }),
            )
            .on_click(move |_, _, cx| {
                let _ = select_module.update(cx, |this, cx| this.select_group(group_id, cx));
            })
            .into_any_element()
    }

    fn render_group_detail(&mut self, group: EnvGroup, cx: &mut Context<Self>) -> AnyElement {
        let variables: Vec<GroupVariable> = self
            .group_variables
            .iter()
            .filter(|variable| variable.group_id == group.id)
            .cloned()
            .collect();
        let conflicts = variables
            .iter()
            .filter(|variable| {
                self.origins
                    .get(&variable.key)
                    .map(|origin| !origin.shadowed_groups.is_empty())
                    .unwrap_or(false)
            })
            .count();

        let header = div()
            .px_4()
            .py_3()
            .v_flex()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(group.name.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .truncate()
                                    .child(group_status(&group, &self.groups)),
                            ),
                    )
                    .child(
                        Button::new("add-group-variable")
                            .label("Add variable")
                            .icon(AppIcon::Add)
                            .small()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_variable_dialog(
                                    VariableTarget::NewInGroup(group.id),
                                    window,
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Button::new("rename-group")
                            .ghost()
                            .small()
                            .icon(AppIcon::Edit)
                            .tooltip("Rename group")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_group_dialog(Some(group.id), window, cx);
                            })),
                    )
                    .child(
                        Button::new("delete-group")
                            .ghost()
                            .small()
                            .icon(AppIcon::Delete)
                            .tooltip("Delete group")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let name = this.group_name(group.id);
                                let module = cx.weak_entity();
                                let title = SharedString::from(format!("Delete group “{name}”?"));
                                window.open_alert_dialog(cx, move |dialog, _, _| {
                                    let module = module.clone();
                                    dialog
                                        .confirm()
                                        .title(title.clone())
                                        .description("Its variables leave with the group. Anything this group wrote to the user environment is restored first.")
                                        .ok_text("Delete")
                                        .on_ok(move |_, window, cx| {
                                            module
                                                .update(cx, |this, cx| {
                                                    this.delete_group(group.id, window, cx)
                                                })
                                                .is_ok()
                                        })
                                });
                            })),
                    ),
            )
            .when(!group.note.is_empty(), |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(group.note.clone()),
                )
            })
            .when(conflicts > 0, |this| {
                this.child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_2()
                        .text_xs()
                        .text_color(cx.theme().warning)
                        .child(Icon::new(AppIcon::Warning).small())
                        .child(format!(
                            "{conflicts} variables are overridden by a group enabled later"
                        )),
                )
            });

        let body = if variables.is_empty() {
            self.render_empty(
                (
                    "This group is empty",
                    "Add a variable to make it worth enabling.",
                ),
                cx,
            )
        } else {
            let rows = variables
                .iter()
                .cloned()
                .map(|variable| self.render_group_variable_row(variable, cx))
                .collect::<Vec<_>>();
            div()
                .flex_1()
                .min_h_0()
                .id("group-variables-scroll")
                .overflow_y_scroll()
                .child(
                    div()
                        .w_full()
                        .v_flex()
                        .child(group_variables_header(cx))
                        .child(div().w_full().v_flex().children(rows)),
                )
                .into_any_element()
        };

        let footer = div()
            .px_4()
            .py_2()
            .v_flex()
            .gap_1()
            .border_t_1()
            .border_color(cx.theme().border)
            .when(!self.events.is_empty(), |this| {
                this.child(
                    div()
                        .h_flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(cx.theme().muted_foreground)
                                .child("RECENT CHANGES"),
                        )
                        .child(
                            Button::new("clear-events")
                                .ghost()
                                .xsmall()
                                .label("Clear")
                                .tooltip("Clear the change log only, not the groups")
                                .on_click(cx.listener(|this, _, _, cx| this.clear_event_log(cx))),
                        ),
                )
                .children(self.events.iter().take(4).map(|(_, detail)| {
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .truncate()
                        .child(detail.clone())
                }))
            });

        div()
            .size_full()
            .min_w_0()
            .v_flex()
            .child(header)
            .child(body)
            .child(footer)
            .into_any_element()
    }

    fn render_group_variable_row(
        &mut self,
        variable: GroupVariable,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = variable.key.clone();
        let value = variable.value.clone();
        let id = variable.id;
        let edit_target = VariableTarget::InGroup {
            group_id: variable.group_id,
            id,
            key: key.clone(),
        };
        let mono = monospace(cx);
        let overridden = self
            .origins
            .get(&key)
            .map(|origin| !origin.shadowed_groups.is_empty())
            .unwrap_or(false);
        let module = cx.weak_entity();
        let row_target = edit_target.clone();

        div()
            .id(format!("group-variable-row-{id}"))
            .w_full()
            .h_flex()
            .items_center()
            .text_sm()
            .border_b_1()
            .border_color(cx.theme().table_row_border)
            .hover(|this| this.bg(cx.theme().table_hover))
            .child(
                div()
                    .id(format!("group-variable-row-main-{id}"))
                    .h_flex()
                    .items_center()
                    .flex_1()
                    .min_w_0()
                    .on_double_click(move |_, window, cx| {
                        let module = module.clone();
                        let target = row_target.clone();
                        module
                            .update(cx, |this, cx| this.open_variable_dialog(target, window, cx))
                            .ok();
                    })
                    .child(
                        cell(Some(px(200.))).child(
                            div()
                                .font(mono.clone())
                                .text_sm()
                                .truncate()
                                .child(key.clone()),
                        ),
                    )
                    .child(
                        cell(None).child(
                            div()
                                .font(mono.clone())
                                .text_sm()
                                .truncate()
                                .child(value.clone()),
                        ),
                    )
                    .child(
                        cell(Some(px(120.))).child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(variable.kind.label()),
                        ),
                    ),
            )
            .child(
                cell(Some(px(110.))).justify_end().child(
                    div()
                        .h_flex()
                        .items_center()
                        .justify_end()
                        .gap_1()
                        .when(overridden, |this| {
                            this.child(
                                div()
                                    .flex_1()
                                    .text_xs()
                                    .text_color(cx.theme().warning)
                                    .child("Overridden by a later group"),
                            )
                        })
                        .child(
                            Button::new(format!("copy-group-variable-{key}"))
                                .ghost()
                                .xsmall()
                                .icon(AppIcon::Copy)
                                .tooltip("Copy value")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.copy_value(value.clone(), window, cx);
                                })),
                        )
                        .child(
                            Button::new(format!("edit-group-variable-{key}"))
                                .ghost()
                                .xsmall()
                                .icon(AppIcon::Edit)
                                .tooltip("Edit in this group")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_variable_dialog(edit_target.clone(), window, cx);
                                })),
                        )
                        .child(
                            Button::new(format!("delete-group-variable-{id}"))
                                .ghost()
                                .xsmall()
                                .icon(AppIcon::Delete)
                                .tooltip("Remove from group")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.delete_group_variable(id, window, cx);
                                })),
                        ),
                ),
            )
            .into_any_element()
    }

    fn render_empty(&self, copy: (&'static str, &'static str), cx: &App) -> AnyElement {
        div()
            .flex_1()
            .min_h_0()
            .v_flex()
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                Icon::new(AppIcon::Empty)
                    .size_8()
                    .text_color(cx.theme().muted_foreground.opacity(0.6)),
            )
            .child(div().text_sm().child(copy.0))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(copy.1),
            )
            .into_any_element()
    }

    fn render_error(&mut self, message: String, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex_1()
            .min_h_0()
            .v_flex()
            .items_center()
            .justify_center()
            .gap_3()
            .p_8()
            .child(
                Icon::new(AppIcon::Error)
                    .size_8()
                    .text_color(cx.theme().danger),
            )
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Couldn’t read the user environment"),
            )
            .child(
                div()
                    .max_w_96()
                    .text_center()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(message),
            )
            .child(
                Button::new("retry-environment")
                    .label("Try again")
                    .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
            )
            .into_any_element()
    }

    fn on_reload(&mut self, _: &ReloadVariables, _: &mut Window, cx: &mut Context<Self>) {
        self.reload(cx);
    }

    fn on_add_variable(&mut self, _: &AddVariable, window: &mut Window, cx: &mut Context<Self>) {
        self.open_variable_dialog(VariableTarget::NewSystem, window, cx);
    }

    fn on_new_group(&mut self, _: &NewGroup, window: &mut Window, cx: &mut Context<Self>) {
        self.open_group_dialog(None, window, cx);
    }

    fn group_name(&self, id: i64) -> String {
        self.groups
            .iter()
            .find(|group| group.id == id)
            .map(|group| group.name.clone())
            .unwrap_or_else(|| "group".to_string())
    }

    /// 状态栏文本，由工作台统一绘制。
    pub fn status_text(&self) -> String {
        let enabled = self.groups.iter().filter(|group| group.is_enabled).count();
        format!(
            "{} variables · {} groups · {} enabled",
            self.variables.len(),
            self.groups.len(),
            enabled
        )
    }

    /// 状态栏右侧的键盘提示，由工作台统一绘制。
    pub fn status_hint(&self) -> String {
        if cfg!(target_os = "macos") {
            "⌥R reload · ⌥N new group · ⌥⇧A add variable".to_string()
        } else {
            "Alt+R reload · Alt+N new group · Alt+Shift+A add variable".to_string()
        }
    }

    /// 分组接管的 key 数量，状态栏用它说明「有多少值随时可能被还原」。
    pub fn managed_count(&self, _cx: &App) -> usize {
        self.origins
            .iter()
            .filter(|(_, origin)| origin.winning_group.is_some())
            .count()
    }
}

impl Render for EnvironmentModule {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let managed = self.managed_count(cx);
        let content = match self.tab {
            EnvironmentTab::Variables => self.render_variables(cx),
            EnvironmentTab::Groups => self.render_groups(cx),
        };

        div()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_reload))
            .on_action(cx.listener(Self::on_add_variable))
            .on_action(cx.listener(Self::on_new_group))
            .size_full()
            .min_w_0()
            .v_flex()
            .child(self.render_header(cx))
            .child(self.render_actions(cx))
            .when(managed > 0, move |this| {
                this.child(
                    div()
                        .px_4()
                        .py_1()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "{managed} variables are held by an enabled group; disabling that group restores their previous value"
                        )),
                )
            })
            .child(content)
    }
}

impl Focusable for EnvironmentModule {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// 分组详情里的状态行：未启用直说；启用则给出「第几层 / 共几层」。
fn group_status(group: &EnvGroup, groups: &[EnvGroup]) -> String {
    if !group.is_enabled {
        return "Disabled".to_string();
    }
    let mut ranks: Vec<(i64, i64)> = groups
        .iter()
        .filter(|other| other.is_enabled)
        .filter_map(|other| other.enabled_seq.map(|seq| (other.id, seq)))
        .collect();
    ranks.sort_by_key(|(_, seq)| *seq);
    let rank = ranks
        .iter()
        .position(|(id, _)| *id == group.id)
        .map(|ix| ix + 1)
        .unwrap_or(1);
    format!("Enabled · priority {rank} of {}", ranks.len())
}

fn monospace(cx: &App) -> Font {
    Font {
        family: cx.theme().mono_font_family.clone(),
        ..Default::default()
    }
}

/// 表头外框。列宽由调用方的 [`cell`] 决定，Value 列吃掉剩余宽度。
fn table_header(cx: &App) -> Div {
    div()
        .w_full()
        .h_flex()
        .items_center()
        .text_sm()
        .bg(cx.theme().tokens.table_head)
        .text_color(cx.theme().table_head_foreground)
        .border_b_1()
        .border_color(cx.theme().table_row_border)
}

fn system_variables_header(cx: &App) -> AnyElement {
    table_header(cx)
        .child(cell(Some(px(220.))).child("Variable"))
        .child(cell(None).child("Value"))
        .child(cell(Some(px(120.))).child("Type"))
        .child(cell(Some(px(150.))).child("Source"))
        .child(cell(Some(px(96.))).justify_end().child("Actions"))
        .into_any_element()
}

fn group_variables_header(cx: &App) -> AnyElement {
    table_header(cx)
        .child(cell(Some(px(200.))).child("Variable"))
        .child(cell(None).child("Value"))
        .child(cell(Some(px(120.))).child("Type"))
        .child(cell(Some(px(110.))).justify_end().child("Actions"))
        .into_any_element()
}

/// 单元格：固定宽度的列不参与伸缩，`None` 的列独占剩余空间。
fn cell(width: Option<Pixels>) -> Div {
    let cell = div().h_flex().items_center().px(px(6.)).py(px(3.));
    match width {
        Some(width) => cell.w(width).flex_shrink_0(),
        None => cell.flex_1().min_w_0(),
    }
}

fn render_variable_form(
    form: &Entity<VariableForm>,
    is_key_editable: bool,
    _window: &mut Window,
    cx: &mut App,
) -> impl IntoElement {
    let form_read = form.read(cx);
    let is_expandable = form_read.kind == ValueKind::Expandable;
    let kind_form = form.clone();

    div()
        .v_flex()
        .gap_3()
        .p_4()
        .child(
            div()
                .v_flex()
                .gap_1()
                .child(div().text_sm().child("Name"))
                .child(
                    Input::new(&form_read.key)
                        .w_full()
                        .readonly(!is_key_editable),
                ),
        )
        .child(
            div()
                .v_flex()
                .gap_1()
                .child(div().text_sm().child("Value"))
                .child(Input::new(&form_read.value).w_full()),
        )
        .child(
            Checkbox::new("kind-expandable")
                .checked(is_expandable)
                .label("Expandable (%VAR%)")
                .on_click(move |checked, _, cx| {
                    let checked = *checked;
                    kind_form.update(cx, |form, cx| {
                        form.kind = if checked {
                            ValueKind::Expandable
                        } else {
                            ValueKind::Plain
                        };
                        cx.notify();
                    });
                }),
        )
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(if system_env::is_persistent() {
                    format!(
                        "Writes to {}. Newly started processes pick it up.",
                        system_env::scope_label()
                    )
                } else {
                    "This platform can only change the current process’s environment.".to_string()
                }),
        )
}

fn render_group_form(form: &Entity<GroupForm>, _: &mut Window, cx: &mut App) -> impl IntoElement {
    let form_read = form.read(cx);
    div()
        .v_flex()
        .gap_3()
        .p_4()
        .child(
            div()
                .v_flex()
                .gap_1()
                .child(div().text_sm().child("Name"))
                .child(Input::new(&form_read.name).w_full()),
        )
        .child(
            div()
                .v_flex()
                .gap_1()
                .child(div().text_sm().child("Note"))
                .child(Input::new(&form_read.note).w_full()),
        )
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("Enabling a group applies its variables; the last group enabled wins on a same-name key."),
        )
}

/// 弹窗里的 Save：校验后落库/落环境；返回 false 时保持弹窗打开，输入不丢。
fn commit_variable(
    module: WeakEntity<EnvironmentModule>,
    form: Entity<VariableForm>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let (target, key, value, kind) = {
        let form_read = form.read(cx);
        (
            form_read.target.clone(),
            form_read.key.read(cx).value().to_string(),
            form_read.value.read(cx).value().to_string(),
            form_read.kind,
        )
    };

    let invalid = [validate_key(&key), validate_value(&value)]
        .into_iter()
        .find_map(Result::err);
    if let Some(message) = invalid {
        window.push_notification((NotificationType::Warning, message), cx);
        return false;
    }

    let store = cx.global::<DipswitchStore>();
    let result: Result<(), String> = match &target {
        VariableTarget::NewSystem | VariableTarget::System(_) => {
            let mut backend = SystemEnvBackend;
            env_service::write_system_variable(
                store,
                &mut backend,
                &SystemVariable::new(key.clone(), value.clone(), kind),
            )
        }
        VariableTarget::NewInGroup(group_id) => store
            .add_variable(*group_id, &key, &value, kind)
            .map(|_| ()),
        VariableTarget::InGroup { id, .. } => store.update_variable(*id, &key, &value, kind),
    };

    match result {
        Ok(()) => {
            let touched_group = matches!(
                target,
                VariableTarget::NewInGroup(_) | VariableTarget::InGroup { .. }
            );
            let _ = module.update(cx, |this, cx| {
                if touched_group {
                    let store = cx.global::<DipswitchStore>();
                    let mut backend = SystemEnvBackend;
                    if let Err(error) = env_service::reconcile(store, &mut backend) {
                        window.push_notification((NotificationType::Warning, error), cx);
                    }
                }
                this.reload(cx);
            });
            true
        }
        Err(message) => {
            window.push_notification((NotificationType::Warning, message), cx);
            false
        }
    }
}

fn commit_group(
    module: WeakEntity<EnvironmentModule>,
    form: Entity<GroupForm>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let (id, name, note) = {
        let form_read = form.read(cx);
        (
            form_read.id,
            form_read.name.read(cx).value().to_string(),
            form_read.note.read(cx).value().to_string(),
        )
    };

    if name.trim().is_empty() {
        window.push_notification((NotificationType::Warning, "Group name is required"), cx);
        return false;
    }

    let store = cx.global::<DipswitchStore>();
    let saved_id = match id {
        Some(id) => store.update_group(id, &name, &note).map(|()| id),
        None => store.create_group(&name, &note),
    };

    match saved_id {
        Ok(saved_id) => {
            let _ = module.update(cx, |this, cx| {
                if id.is_none() {
                    this.selected_group = Some(saved_id);
                }
                this.tab = EnvironmentTab::Groups;
                this.reload(cx);
            });
            true
        }
        Err(message) => {
            window.push_notification((NotificationType::Warning, message), cx);
            false
        }
    }
}
