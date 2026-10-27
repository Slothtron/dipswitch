//! 抓包日志模块：导入 HAR / Charles 会话，按请求浏览头部与正文。
//!
//! 状态所有权：
//! - 解析后的 [`CaptureLog`] 是唯一数据源，导入后整份替换；
//! - `filtered` 只存索引，搜索不复制请求；
//! - 详情浮层默认关闭，双击行才打开；页签、正文视图与 JSON 树展开状态是本地 UI 状态；
//! - 列宽只在本次会话有效，不进 `Settings`；
//! - 抓包内容只活在本次会话里：不落库，也不进 `Settings`，避免把整份报文写进配置库。

use std::{collections::HashSet, path::PathBuf, rc::Rc};

use chrono::{DateTime, Local, TimeZone};
use gpui_kit::{
    AnyElement, App, AppContext, ClickEvent, ClipboardItem, Context, Div, Entity, ExternalPaths,
    FocusHandle, Focusable, FontWeight, HighlightStyle, Hsla, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement, Pixels, Render, Size,
    StatefulInteractiveElement, Styled, StyledText, Subscription, Task, Window, actions,
    base::{StyledExt, VirtualListScrollHandle, v_virtual_list},
    component::{
        ActiveTheme, Disableable, Icon, Selectable, Sizable, WindowExt,
        button::{Button, ButtonVariants},
        input::{Input, InputEvent, InputState},
        notification::NotificationType,
    },
    div,
    prelude::FluentBuilder,
    px,
};

#[cfg(not(target_os = "windows"))]
use gpui_kit::{PathPromptOptions, SharedString};

use crate::{
    capture::{self, Body, CaptureLog, Entry, Header, StatusClass, status_class},
    highlight::{self, Token},
    icon::AppIcon,
    ui::{
        self, TRAFFIC_METHOD_COLUMN, TRAFFIC_PATH_COLUMN, TRAFFIC_SIZE_COLUMN,
        TRAFFIC_STATUS_COLUMN, TRAFFIC_TIME_COLUMN,
    },
};

pub const KEY_CONTEXT: &str = "Traffic";

/// 表格行高：虚拟列表按这个值排布，所有行必须同高。
const ROW_HEIGHT: gpui_kit::Pixels = px(30.);

/// 详情是盖在列表右侧的浮层：宽度固定，不再挤压列表列宽。
/// 560 是「键列 184 + 值列 + 行内复制列」能读下常见头名的最小值。
const DETAIL_WIDTH: Pixels = px(560.);

/// JSON 树一次最多铺多少行：再多的结构用 Raw / Pretty 看更合适。
const MAX_TREE_ROWS: usize = 1_000;

/// 文件选择器里的默认过滤项；内容嗅探才是准绳，这里只帮用户少翻几页。
const CAPTURE_EXTENSIONS: &[&str] = &["har", "chls", "chlsj", "xml"];

actions!(traffic, [ImportCapture, FocusFilter]);

/// 详情区的三个页签。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanelTab {
    Overview,
    Request,
    Response,
}

impl PanelTab {
    const ALL: [Self; 3] = [Self::Overview, Self::Request, Self::Response];

    fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Request => "Request",
            Self::Response => "Response",
        }
    }
}

/// 请求体与响应体的两套独立视图状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodySide {
    Request,
    Response,
}

impl BodySide {
    fn key(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Response => "response",
        }
    }
}

/// 正文的展示方式。可用项由内容决定，不把所有格式硬塞给每种正文。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyMode {
    Pretty,
    Tree,
    Form,
    Raw,
}

impl BodyMode {
    fn label(self) -> &'static str {
        match self {
            Self::Pretty => "Pretty",
            Self::Tree => "Tree",
            Self::Form => "Form",
            Self::Raw => "Raw",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Pretty => "pretty",
            Self::Tree => "tree",
            Self::Form => "form",
            Self::Raw => "raw",
        }
    }
}

/// 可拖宽度的请求表列。Path 也在这张表里，但它同时是「吃掉多余宽度」的列。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrafficColumn {
    Status,
    Method,
    Path,
    Size,
    Time,
}

impl TrafficColumn {
    const ALL: [Self; 5] = [
        Self::Status,
        Self::Method,
        Self::Path,
        Self::Size,
        Self::Time,
    ];

    fn index(self) -> usize {
        match self {
            Self::Status => 0,
            Self::Method => 1,
            Self::Path => 2,
            Self::Size => 3,
            Self::Time => 4,
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Method => "method",
            Self::Path => "path",
            Self::Size => "size",
            Self::Time => "time",
        }
    }

    fn default_width(self) -> Pixels {
        match self {
            Self::Status => TRAFFIC_STATUS_COLUMN,
            Self::Method => TRAFFIC_METHOD_COLUMN,
            Self::Path => TRAFFIC_PATH_COLUMN,
            Self::Size => TRAFFIC_SIZE_COLUMN,
            Self::Time => TRAFFIC_TIME_COLUMN,
        }
    }

    fn width_range(self) -> std::ops::RangeInclusive<Pixels> {
        match self {
            Self::Status => px(40.)..=px(160.),
            Self::Method => px(44.)..=px(160.),
            Self::Path => px(80.)..=px(900.),
            Self::Size => px(48.)..=px(160.),
            Self::Time => px(56.)..=px(160.),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ColumnDrag {
    column: TrafficColumn,
    start_x: Pixels,
    start_width: Pixels,
}

#[derive(Debug, Clone)]
enum LoadState {
    Idle,
    Loading,
    Failed(String),
}

pub struct TrafficModule {
    search: Entity<InputState>,
    capture: Option<CaptureLog>,
    /// 过滤结果存的是 `capture.entries` 的下标。
    filtered: Vec<usize>,
    /// 当前高亮的行。单击只改高亮，不打开详情。
    selected: Option<usize>,
    /// 详情浮层显示的请求；`None` 表示详情关闭（默认状态）。
    detail: Option<usize>,
    /// 各列宽度只在本次会话有效，不落库。
    column_widths: [Pixels; 5],
    column_drag: Option<ColumnDrag>,
    tab: PanelTab,
    request_mode: Option<BodyMode>,
    response_mode: Option<BodyMode>,
    /// JSON 树里已展开节点的路径；换一条请求就清空。
    expanded_nodes: HashSet<String>,
    /// `v_virtual_list` 每次渲染都会新建滚动句柄；这里留一份持久的，
    /// 否则滚一格、下一帧就弹回顶部。
    list_scroll: VirtualListScrollHandle,
    state: LoadState,
    focus_handle: FocusHandle,
    _import_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl TrafficModule {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Filter by URL, method or status…")
                .clean_on_escape()
        });
        let search_sub = cx.subscribe_in(&search, window, |this, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change) {
                this.apply_filter(cx);
                cx.notify();
            }
        });

        Self {
            search,
            capture: None,
            filtered: Vec::new(),
            selected: None,
            detail: None,
            column_widths: TrafficColumn::ALL.map(TrafficColumn::default_width),
            column_drag: None,
            tab: PanelTab::Overview,
            request_mode: None,
            response_mode: None,
            expanded_nodes: HashSet::new(),
            list_scroll: VirtualListScrollHandle::new(),
            state: LoadState::Idle,
            focus_handle: cx.focus_handle().tab_stop(true),
            _import_task: None,
            _subscriptions: vec![search_sub],
        }
    }

    /// 状态栏左侧文本，由工作台统一绘制。
    pub fn status_text(&self) -> String {
        let Some(capture) = self.capture.as_ref() else {
            return "No capture imported".to_string();
        };
        let failed = capture
            .entries
            .iter()
            .filter(|entry| {
                matches!(
                    status_class(entry.status),
                    StatusClass::ClientError | StatusClass::ServerError
                ) || entry.note.is_some()
            })
            .count();
        format!(
            "{} · {failed} failed · {}",
            ui::counted(capture.entries.len(), "request", "requests"),
            capture.format.label()
        )
    }

    /// 状态栏右侧的键盘提示，由工作台统一绘制。
    pub fn status_hint(&self) -> String {
        if cfg!(target_os = "macos") {
            "⌘O import · ⌘F filter".to_string()
        } else {
            "Ctrl+O import · Ctrl+F filter".to_string()
        }
    }

    fn detail_entry(&self) -> Option<(usize, &Entry)> {
        let capture = self.capture.as_ref()?;
        let index = self.detail?;
        capture.entries.get(index).map(|entry| (index, entry))
    }

    fn apply_filter(&mut self, cx: &App) {
        let query = self.search.read(cx).value().to_lowercase();
        self.filtered = self
            .capture
            .as_ref()
            .map(|capture| {
                capture
                    .entries
                    .iter()
                    .enumerate()
                    .filter(|(_, entry)| {
                        query.is_empty()
                            || entry.url.to_lowercase().contains(&query)
                            || entry.method.to_lowercase().contains(&query)
                            || entry
                                .status
                                .map(|status| status.to_string().contains(&query))
                                .unwrap_or(false)
                            || entry.host.to_lowercase().contains(&query)
                            || entry
                                .note
                                .as_deref()
                                .is_some_and(|note| note.to_lowercase().contains(&query))
                    })
                    .map(|(index, _)| index)
                    .collect()
            })
            .unwrap_or_default();
        if let Some(detail) = self.detail
            && !self.filtered.contains(&detail)
        {
            self.detail = None;
            self.reset_detail_state();
        }
        if let Some(selected) = self.selected
            && !self.filtered.contains(&selected)
        {
            self.selected = self.filtered.first().copied();
        }
        if self.selected.is_none() {
            self.selected = self.filtered.first().copied();
        }
        // 列表换了内容（新文件或新过滤词）就回到顶部，别停在旧位置。
        self.list_scroll.set_offset(Default::default());
    }

    /// 单击只改高亮；详情已经打开时，单击别的行直接切换内容。
    /// `open` 来自双击：详情关着时双击才会打开。
    fn select_entry(&mut self, index: usize, open: bool, cx: &mut Context<Self>) {
        let changed = self.selected != Some(index);
        self.selected = Some(index);
        if self.detail.is_some() {
            if changed {
                self.detail = Some(index);
                self.reset_detail_state();
            }
        } else if open {
            self.detail = Some(index);
            self.reset_detail_state();
        }
        if changed || (open && self.detail == Some(index)) {
            cx.notify();
        }
    }

    fn close_detail(&mut self, cx: &mut Context<Self>) {
        if self.detail.take().is_some() {
            self.reset_detail_state();
            cx.notify();
        }
    }

    fn begin_column_drag(&mut self, column: TrafficColumn, start_x: Pixels) {
        self.column_drag = Some(ColumnDrag {
            column,
            start_x,
            start_width: self.column_widths[column.index()],
        });
    }

    fn drag_column(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.column_drag else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.column_drag = None;
            return;
        }
        let range = drag.column.width_range();
        let width = (drag.start_width + (event.position.x - drag.start_x))
            .clamp(*range.start(), *range.end());
        if self.column_widths[drag.column.index()] != width {
            self.column_widths[drag.column.index()] = width;
            cx.notify();
        }
    }

    fn end_column_drag(&mut self) {
        self.column_drag = None;
    }

    fn reset_detail_state(&mut self) {
        self.request_mode = None;
        self.response_mode = None;
        self.expanded_nodes.clear();
    }

    fn import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.state, LoadState::Loading) {
            return;
        }
        #[cfg(not(target_os = "windows"))]
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(SharedString::from("Import a HAR or Charles session")),
        });
        self.state = LoadState::Loading;
        cx.notify();
        let task = cx.spawn_in(window, async move |this, cx| {
            #[cfg(target_os = "windows")]
            let picked = rfd::AsyncFileDialog::new()
                .set_title("Import a capture log")
                .add_filter("Capture logs", CAPTURE_EXTENSIONS)
                .add_filter("HAR", &["har"])
                .add_filter("Charles session", &["chls", "chlsj", "xml"])
                .pick_file()
                .await
                .map(|file| file.path().to_path_buf());
            #[cfg(not(target_os = "windows"))]
            let picked = match receiver.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Ok(None)) | Err(_) => None,
                Ok(Err(error)) => {
                    let message = format!("Could not open the file picker: {error}");
                    this.update_in(cx, |this, window, cx| this.fail(message, window, cx))
                        .ok();
                    return;
                }
            };
            let Some(path) = picked else {
                this.update_in(cx, |this, _, cx| this.cancel_import(cx))
                    .ok();
                return;
            };
            let result = cx
                .background_spawn(async move { capture::load(&path) })
                .await;
            this.update_in(cx, |this, window, cx| this.apply_import(result, window, cx))
                .ok();
        });
        self._import_task = Some(task);
    }

    /// 从命令行或拖拽直接打开一份会话（`dipswitch session.har`）。
    pub fn open_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.state, LoadState::Loading) {
            return;
        }
        self.state = LoadState::Loading;
        cx.notify();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { capture::load(&path) })
                .await;
            this.update_in(cx, |this, window, cx| this.apply_import(result, window, cx))
                .ok();
        });
        self._import_task = Some(task);
    }

    fn cancel_import(&mut self, cx: &mut Context<Self>) {
        self.state = LoadState::Idle;
        cx.notify();
    }

    #[cfg(not(target_os = "windows"))]
    fn fail(&mut self, message: String, window: &mut Window, cx: &mut Context<Self>) {
        window.push_notification((NotificationType::Error, "Import failed"), cx);
        self.state = LoadState::Failed(message);
        cx.notify();
    }

    fn apply_import(
        &mut self,
        result: Result<CaptureLog, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(capture) => {
                let count = capture.entries.len();
                self.capture = Some(capture);
                self.state = LoadState::Idle;
                self.tab = PanelTab::Overview;
                self.selected = None;
                self.detail = None;
                self.reset_detail_state();
                self.apply_filter(cx);
                // 导入后只高亮第一条，详情保持关闭——打开详情是用户的双击动作。
                self.selected = self.filtered.first().copied();
                window.push_notification(
                    (
                        NotificationType::Success,
                        format!("Imported {count} requests"),
                    ),
                    cx,
                );
            }
            Err(message) => {
                self.state = LoadState::Failed(message);
                window.push_notification((NotificationType::Error, "Import failed"), cx);
            }
        }
        cx.notify();
    }

    fn close_capture(&mut self, cx: &mut Context<Self>) {
        self.capture = None;
        self.filtered.clear();
        self.selected = None;
        self.detail = None;
        self.tab = PanelTab::Overview;
        self.reset_detail_state();
        self.state = LoadState::Idle;
        cx.notify();
    }

    fn copy_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        window.push_notification((NotificationType::Success, "Copied"), cx);
    }

    fn on_import(&mut self, _: &ImportCapture, window: &mut Window, cx: &mut Context<Self>) {
        self.import(window, cx);
    }

    fn on_focus_filter(&mut self, _: &FocusFilter, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |input, cx| input.focus(window, cx));
    }

    fn on_close_detail_key(
        &mut self,
        event: &KeyDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key == "escape" && self.detail.is_some() {
            self.close_detail(cx);
            cx.stop_propagation();
        }
    }

    fn mode_for(&self, body: &Body, side: BodySide) -> BodyMode {
        let available = available_modes(body);
        let stored = match side {
            BodySide::Request => self.request_mode,
            BodySide::Response => self.response_mode,
        };
        stored
            .filter(|mode| available.contains(mode))
            .unwrap_or(available[0])
    }

    fn set_mode(&mut self, side: BodySide, mode: BodyMode) {
        match side {
            BodySide::Request => self.request_mode = Some(mode),
            BodySide::Response => self.response_mode = Some(mode),
        }
        self.expanded_nodes.clear();
    }

    // ---------------------------------------------------------------- 渲染

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let source = match self.capture.as_ref() {
            Some(capture) => format!("{} · {}", capture.format.label(), capture.file_name),
            None => "HAR · Charles JSON · Charles XML · Charles binary".to_string(),
        };
        let loading = matches!(self.state, LoadState::Loading);

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
                            .child("Traffic"),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(source),
                    )
                    .child(div().flex_1())
                    .when(self.capture.is_some(), |this| {
                        this.child(
                            ui::icon_button("close-capture", AppIcon::Remove, cx)
                                .tooltip("Close this capture")
                                .on_click(cx.listener(|this, _, _, cx| this.close_capture(cx))),
                        )
                    })
                    .child(
                        Button::new("import-capture")
                            .label("Import…")
                            .icon(AppIcon::Import)
                            .loading(loading)
                            .disabled(loading)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.import(window, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div().w_96().child(
                            Input::new(&self.search)
                                .w_full()
                                .cleanable(true)
                                .prefix(Icon::new(AppIcon::Search).small()),
                        ),
                    )
                    .child(div().flex_1())
                    .when(self.capture.is_some(), |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!(
                                    "{} of {}",
                                    self.filtered.len(),
                                    self.capture.as_ref().map_or(0, |c| c.entries.len())
                                )),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let content = if let Some(capture) = self.capture.as_ref() {
            self.render_capture(capture, cx)
        } else {
            match &self.state {
                LoadState::Loading => self.render_notice(
                    AppIcon::Import,
                    "Reading capture…",
                    "Large sessions are parsed on a background thread.",
                    None,
                    cx,
                ),
                LoadState::Failed(message) => self.render_notice(
                    AppIcon::Error,
                    "Could not read that file",
                    message,
                    Some(
                        Button::new("failed-import")
                            .label("Choose another file…")
                            .icon(AppIcon::Import)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.import(window, cx);
                            }))
                            .into_any_element(),
                    ),
                    cx,
                ),
                LoadState::Idle => self.render_notice(
                    AppIcon::Import,
                    "No capture imported",
                    "Drop a HAR or Charles session anywhere on this page, or pick one below.",
                    Some(
                        Button::new("empty-import")
                            .label("Choose a file…")
                            .icon(AppIcon::Import)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.import(window, cx);
                            }))
                            .into_any_element(),
                    ),
                    cx,
                ),
            }
        };

        // 拖拽区必须是 flex 容器：里面的内容靠 `flex_1` 吃剩余高度，
        // 普通 block 包裹层会把这条链掐断，列表与详情就都量成 0 高。
        div()
            .flex_1()
            .min_h_0()
            .v_flex()
            .on_drop::<ExternalPaths>(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                if let Some(path) = paths.paths().first().cloned() {
                    this.open_path(path, window, cx);
                }
            }))
            .drag_over::<ExternalPaths>(|style, _, _, cx| style.bg(cx.theme().accent.opacity(0.12)))
            .child(content)
            .into_any_element()
    }

    fn render_notice(
        &self,
        icon: AppIcon,
        title: &str,
        detail: &str,
        action: Option<AnyElement>,
        cx: &App,
    ) -> AnyElement {
        div()
            .flex_1()
            .min_h_0()
            .v_flex()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                Icon::new(icon)
                    .size_8()
                    .text_color(cx.theme().muted_foreground.opacity(0.6)),
            )
            .child(div().text_sm().child(title.to_string()))
            .child(
                div()
                    .max_w(px(520.))
                    .text_center()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(detail.to_string()),
            )
            .when_some(action, |this, action| this.child(action))
            .into_any_element()
    }

    fn render_capture(&self, capture: &CaptureLog, cx: &mut Context<Self>) -> AnyElement {
        if capture.entries.is_empty() {
            return self.render_notice(
                AppIcon::Empty,
                "No requests in this file",
                "The session parsed, but it holds no HTTP transactions.",
                None,
                cx,
            );
        }
        let warning = capture.warnings.first().cloned();
        let error = match &self.state {
            LoadState::Failed(message) => Some(message.clone()),
            _ => None,
        };
        let detail = if self.detail.is_some() {
            Some(self.render_detail(cx))
        } else {
            None
        };
        // 详情是浮层，不参与列表的宽度分配：打开/关闭都不会让列宽跳动。
        let mut content = div()
            .flex_1()
            .min_h_0()
            .relative()
            .child(self.render_list(cx));
        if let Some(detail) = detail {
            content = content.child(
                div()
                    // 浮层遮住的部分不再参与命中：滚轮不会穿透到底下的列表，
                    // 空白处点击也不会选中被盖住的行。
                    .occlude()
                    .absolute()
                    .top_0()
                    .right_0()
                    .bottom_0()
                    .w(DETAIL_WIDTH)
                    .border_l_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().background)
                    .shadow_lg()
                    .child(detail),
            );
        }
        let content = content.into_any_element();

        div()
            .flex_1()
            .min_h_0()
            .v_flex()
            .when_some(error, |this, message| {
                this.child(self.banner(AppIcon::Error, message.as_str(), cx, true))
            })
            .when_some(warning, |this, message| {
                this.child(self.banner(AppIcon::Warning, message.as_str(), cx, false))
            })
            .child(content)
            .into_any_element()
    }

    fn banner(&self, icon: AppIcon, message: &str, cx: &App, danger: bool) -> AnyElement {
        div()
            .px_4()
            .py_2()
            .h_flex()
            .items_center()
            .gap_2()
            .text_xs()
            .text_color(if danger {
                cx.theme().danger
            } else {
                cx.theme().warning
            })
            .child(Icon::new(icon).small())
            .child(message.to_string())
            .into_any_element()
    }

    fn render_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let entity = cx.entity();
        let sizes = Rc::new(
            (0..self.filtered.len())
                .map(|_| Size {
                    width: px(0.),
                    height: ROW_HEIGHT,
                })
                .collect::<Vec<_>>(),
        );
        let list = v_virtual_list(
            entity,
            "traffic-entries",
            sizes,
            move |this, range, _, cx| {
                range
                    .filter_map(|row_index| {
                        let entry_index = *this.filtered.get(row_index)?;
                        Some(this.render_row(row_index, entry_index, cx))
                    })
                    .collect::<Vec<_>>()
            },
        )
        .track_scroll(&self.list_scroll)
        .flex_1()
        .min_h_0();

        div()
            .w_full()
            .h_full()
            .min_h_0()
            .min_w_0()
            .v_flex()
            .overflow_hidden()
            .child(self.render_table_header(cx))
            .child(list)
            .into_any_element()
    }

    fn render_table_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut header = ui::table_header(cx);
        for column in TrafficColumn::ALL {
            let label = match column {
                TrafficColumn::Status => "Status",
                TrafficColumn::Method => "Method",
                TrafficColumn::Path => "Path",
                TrafficColumn::Size => "Size",
                TrafficColumn::Time => "Time",
            };
            let mut cell = if column == TrafficColumn::Path {
                ui::shrinkable_cell(self.column_widths[column.index()])
            } else {
                ui::cell(Some(self.column_widths[column.index()]))
            };
            if matches!(column, TrafficColumn::Size | TrafficColumn::Time) {
                cell = cell.justify_end();
            }
            header = header.child(
                cell.relative().child(label.to_string()).child(
                    div()
                        .id(format!("traffic-col-{}", column.key()))
                        // 边界压在相邻列上：occlude 保证点在下半格也能拿到拖拽。
                        .occlude()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .right(px(-4.))
                        .w(px(8.))
                        .border_l_1()
                        .border_color(cx.theme().table_row_border)
                        .cursor_col_resize()
                        .hover(|this| this.bg(cx.theme().secondary_active))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                this.begin_column_drag(column, event.position.x);
                                cx.notify();
                            }),
                        ),
                ),
            );
        }
        header.child(div().flex_1()).into_any_element()
    }

    fn render_row(
        &self,
        row_index: usize,
        entry_index: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(entry) = self
            .capture
            .as_ref()
            .and_then(|capture| capture.entries.get(entry_index))
        else {
            return div().into_any_element();
        };
        let selected = self.selected == Some(entry_index);
        let status_color = status_color(entry, cx);
        let time = entry
            .started
            .as_deref()
            .map(started_label)
            .unwrap_or_else(|| "—".to_string());
        let size = entry
            .size
            .map(format_bytes)
            .unwrap_or_else(|| "—".to_string());

        div()
            .id(format!("traffic-row-{row_index}"))
            .w_full()
            .h(ROW_HEIGHT)
            .h_flex()
            .items_center()
            .text_sm()
            .border_b_1()
            .border_color(cx.theme().table_row_border)
            .when(selected, |this| this.bg(cx.theme().accent))
            .when(!selected, |this| {
                this.hover(|this| this.bg(cx.theme().table_hover))
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                let open = matches!(
                    event,
                    ClickEvent::Mouse(click) if click.down.click_count >= 2
                );
                // 让焦点落回模块根节点，Esc / 快捷键才有 Traffic 上下文。
                window.focus(&this.focus_handle, cx);
                this.select_entry(entry_index, open, cx);
            }))
            .child(
                ui::cell(Some(self.column_widths[TrafficColumn::Status.index()])).child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(status_color)
                        .child(status_text(entry)),
                ),
            )
            .child(
                ui::cell(Some(self.column_widths[TrafficColumn::Method.index()])).child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(cx.theme().muted_foreground)
                        .child(entry.method.clone()),
                ),
            )
            .child(
                ui::shrinkable_cell(self.column_widths[TrafficColumn::Path.index()]).child(
                    div()
                        .font(ui::mono_font(cx))
                        .text_xs()
                        .truncate()
                        .child(entry.path.clone()),
                ),
            )
            .child(
                ui::cell(Some(self.column_widths[TrafficColumn::Size.index()]))
                    .justify_end()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(size),
                    ),
            )
            .child(
                ui::cell(Some(self.column_widths[TrafficColumn::Time.index()]))
                    .justify_end()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(time),
                    ),
            )
            // 列宽可以被拖大，剩余空间由这个尾巴吃掉；否则 Time 会跟着宽度漂移。
            .child(div().flex_1())
            .into_any_element()
    }

    fn render_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some((entry_index, entry)) = self.detail_entry() else {
            return self.render_notice(
                AppIcon::Empty,
                "Select a request",
                "Pick a row to see its headers and bodies.",
                None,
                cx,
            );
        };

        let copy_url = entry.url.clone();
        let header = div()
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
                            .px_2()
                            .py_1()
                            .rounded(cx.theme().radius)
                            .bg(cx.theme().accent)
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(entry.method.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(status_color(entry, cx))
                            .child(status_text(entry)),
                    )
                    .child(meta_text(
                        &entry
                            .duration_ms
                            .map(format_duration)
                            .unwrap_or_else(|| "—".to_string()),
                        cx,
                    ))
                    .child(meta_text(
                        &entry
                            .size
                            .map(format_bytes)
                            .unwrap_or_else(|| "—".to_string()),
                        cx,
                    ))
                    .child(div().flex_1())
                    .child(
                        ui::icon_button("copy-url", AppIcon::Copy, cx)
                            .tooltip("Copy URL")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.copy_text(copy_url.clone(), window, cx);
                            })),
                    )
                    .child(
                        ui::icon_button("close-detail", AppIcon::Remove, cx)
                            .tooltip("Close details")
                            .on_click(cx.listener(|this, _, _, cx| this.close_detail(cx))),
                    ),
            )
            .child(
                // 长 URL 不能靠内容撑宽详情：`min_w_0` 让截断真正生效，
                // 面板才能保持用户/默认给的宽度。
                div()
                    .w_full()
                    .min_w_0()
                    .font(ui::mono_font(cx))
                    .text_xs()
                    .truncate()
                    .child(entry.url.clone()),
            );

        let tabs = div()
            .px_4()
            .py_2()
            .h_flex()
            .items_center()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .children(PanelTab::ALL.into_iter().map(|tab| {
                Button::new(format!("traffic-tab-{}", tab.label()))
                    .ghost()
                    .small()
                    .selected(self.tab == tab)
                    .label(tab.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tab = tab;
                        cx.notify();
                    }))
            }))
            .child(div().flex_1())
            .child(meta_text(
                &entry
                    .started
                    .as_deref()
                    .map(started_label)
                    .unwrap_or_default(),
                cx,
            ));

        let content = match self.tab {
            PanelTab::Overview => self.render_overview(entry, cx),
            PanelTab::Request => self.render_request(entry, entry_index, cx),
            PanelTab::Response => self.render_response(entry, entry_index, cx),
        };
        div()
            .w_full()
            .h_full()
            .min_h_0()
            .min_w_0()
            .v_flex()
            .child(header)
            .child(tabs)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .id(format!("traffic-detail-scroll-{entry_index}"))
                    .overflow_y_scroll()
                    .child(content),
            )
            .into_any_element()
    }

    fn render_overview(&self, entry: &Entry, cx: &App) -> AnyElement {
        let mut rows = vec![
            info_row("URL", entry.url.clone(), true, cx),
            info_row_colored(
                "Status",
                status_text(entry),
                status_color(entry, cx),
                cx,
                false,
            ),
            info_row("Method", entry.method.clone(), false, cx),
            info_row("Protocol", scheme(&entry.url).to_string(), false, cx),
            info_row("Content type", mime_label(&entry.mime), false, cx),
            info_row(
                "Size",
                entry
                    .size
                    .map(format_bytes)
                    .unwrap_or_else(|| "—".to_string()),
                false,
                cx,
            ),
            info_row(
                "Duration",
                entry
                    .duration_ms
                    .map(format_duration)
                    .unwrap_or_else(|| "—".to_string()),
                false,
                cx,
            ),
            info_row(
                "Started",
                entry
                    .started
                    .as_deref()
                    .map(started_detail_label)
                    .unwrap_or_else(|| "—".to_string()),
                true,
                cx,
            ),
            info_row(
                "Remote address",
                entry
                    .remote_address
                    .clone()
                    .unwrap_or_else(|| "—".to_string()),
                true,
                cx,
            ),
        ];
        if let Some(query) = entry.query.as_deref() {
            push_query_count(&mut rows, query, cx);
        }
        if let Some(note) = entry.note.as_deref() {
            rows.push(info_row_colored(
                "Error",
                note.to_string(),
                cx.theme().danger,
                cx,
                false,
            ));
        }
        div()
            .v_flex()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(table_band("GENERAL", None, None, cx))
            .child(div().w_full().v_flex().children(rows))
            .into_any_element()
    }

    fn render_request(
        &self,
        entry: &Entry,
        entry_index: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut sections = Vec::new();
        if let Some(query) = entry.query.as_deref() {
            let pairs = capture::parse_query(query);
            if !pairs.is_empty() {
                sections.push(self.render_pairs_section("QUERY PARAMETERS", &pairs, cx));
            }
        }
        sections.push(self.render_headers_section(&entry.request_headers, cx));
        sections.push(self.render_body_section(
            entry.request_body.as_ref(),
            BodySide::Request,
            entry_index,
            cx,
        ));
        div().v_flex().children(sections).into_any_element()
    }

    fn render_response(
        &self,
        entry: &Entry,
        entry_index: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .v_flex()
            .child(self.render_headers_section(&entry.response_headers, cx))
            .child(self.render_body_section(
                entry.response_body.as_ref(),
                BodySide::Response,
                entry_index,
                cx,
            ))
            .into_any_element()
    }

    /// 行尾的复制按钮：值被单行截断时，完整内容靠它取。
    fn copy_row_button(&self, id: String, text: String, cx: &mut Context<Self>) -> AnyElement {
        ui::row_icon_button(id, AppIcon::Copy, cx)
            .tooltip("Copy value")
            .on_click(cx.listener(move |this, _, window, cx| {
                this.copy_text(text.clone(), window, cx);
            }))
            .into_any_element()
    }

    /// 键值表：一条表头带加若干行，行与行之间靠分隔线和悬停底色区分。
    fn kv_table(
        &self,
        rows: &[(String, String)],
        id_prefix: &str,
        foreground: Hsla,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut elements = Vec::with_capacity(rows.len());
        for (index, (key, value)) in rows.iter().enumerate() {
            let copy = self.copy_row_button(format!("copy-{id_prefix}-{index}"), value.clone(), cx);
            elements.push(
                kv_row(
                    key.clone(),
                    value_text(value.clone(), true, foreground, cx),
                    Some(copy),
                    cx,
                )
                .into_any_element(),
            );
        }
        elements
    }

    fn render_pairs_section(
        &self,
        title: &str,
        pairs: &[(String, String)],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let copy_all = pairs
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("\n");
        let foreground = cx.theme().foreground;
        let rows = self.kv_table(pairs, "param", foreground, cx);
        div()
            .v_flex()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(table_band(
                title,
                Some(ui::counted(pairs.len(), "parameter", "parameters")),
                Some(
                    ui::icon_button("copy-parameters", AppIcon::Copy, cx)
                        .tooltip("Copy parameters")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.copy_text(copy_all.clone(), window, cx);
                        }))
                        .into_any_element(),
                ),
                cx,
            ))
            .children(rows)
            .into_any_element()
    }

    fn render_headers_section(&self, headers: &[Header], cx: &mut Context<Self>) -> AnyElement {
        let copy_all = headers
            .iter()
            .map(|header| format!("{}: {}", header.name, header.value))
            .collect::<Vec<_>>()
            .join("\n");
        let foreground = cx.theme().foreground;
        let rows: Vec<(String, String)> = headers
            .iter()
            .map(|header| (header.name.clone(), header.value.clone()))
            .collect();
        let band = table_band(
            "HEADERS",
            Some(ui::counted(headers.len(), "header", "headers")),
            (!headers.is_empty()).then(|| {
                ui::icon_button("copy-headers", AppIcon::Copy, cx)
                    .tooltip("Copy headers")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.copy_text(copy_all.clone(), window, cx);
                    }))
                    .into_any_element()
            }),
            cx,
        );
        let mut view = div()
            .v_flex()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(band);
        if headers.is_empty() {
            view = view.child(div().px_4().py_3().child(note_text("No headers", cx)));
        } else {
            view = view.children(self.kv_table(&rows, "header", foreground, cx));
        }
        view.into_any_element()
    }

    /// Form 正文：与参数表同一套行，但不带表头带（它已经在 BODY 区块里了）。
    fn render_form_rows(
        &self,
        pairs: &[(String, String)],
        side: BodySide,
        entry_index: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let foreground = cx.theme().foreground;
        let prefix = format!("form-{}-{entry_index}", side.key());
        div()
            .w_full()
            .v_flex()
            .children(self.kv_table(pairs, &prefix, foreground, cx))
            .into_any_element()
    }

    fn render_body_section(
        &self,
        body: Option<&Body>,
        side: BodySide,
        entry_index: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let available = body.map(available_modes).unwrap_or_default();
        let active = body
            .map(|body| self.mode_for(body, side))
            .unwrap_or(BodyMode::Raw);
        let meta = body
            .and_then(|body| body.size)
            .map(format_bytes)
            .unwrap_or_else(|| "empty".to_string());
        let copy_text = body.and_then(|body| body.preview.clone());

        let mut header = div()
            .px_4()
            .py_2()
            .h_flex()
            .items_center()
            .gap_2()
            .child(section_title("BODY", cx))
            .child(meta_text(&meta, cx))
            .child(div().flex_1());
        for mode in available {
            header = header.child(
                Button::new(format!("body-mode-{}-{}", side.key(), mode.key()))
                    .ghost()
                    .xsmall()
                    .selected(active == mode)
                    .label(mode.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_mode(side, mode);
                        cx.notify();
                    })),
            );
        }
        if let Some(copy_text) = copy_text {
            header = header.child(
                ui::icon_button(format!("copy-body-{}", side.key()), AppIcon::Copy, cx)
                    .tooltip("Copy body")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.copy_text(copy_text.clone(), window, cx);
                    })),
            );
        }

        let content = match body {
            None => div()
                .px_4()
                .pb_3()
                .child(note_text("No body", cx))
                .into_any_element(),
            Some(body) => {
                let mut block = div().px_4().pb_3().v_flex().gap_2();
                if body.truncated {
                    let shown = body
                        .preview
                        .as_deref()
                        .map_or(0, |preview| preview.len() as u64);
                    let message = match body.size {
                        Some(size) => format!(
                            "Showing the first {} of {}; formatting uses the preview only.",
                            format_bytes(shown),
                            format_bytes(size)
                        ),
                        None => format!(
                            "Showing the first {}; formatting uses the preview only.",
                            format_bytes(shown)
                        ),
                    };
                    block = block.child(note_text(&message, cx));
                }
                let formatted = match active {
                    BodyMode::Pretty => json_value(body)
                        .map(|value| highlighted_code_block(pretty_json(&value), cx))
                        .unwrap_or_else(|| self.render_plain_body(body, cx)),
                    BodyMode::Tree => json_value(body)
                        .map(|value| self.render_tree(&value, entry_index, side, cx))
                        .unwrap_or_else(|| self.render_plain_body(body, cx)),
                    BodyMode::Form => body_pairs(body)
                        .map(|pairs| self.render_form_rows(&pairs, side, entry_index, cx))
                        .unwrap_or_else(|| self.render_plain_body(body, cx)),
                    BodyMode::Raw => self.render_plain_body(body, cx),
                };
                block.child(formatted).into_any_element()
            }
        };

        div()
            .v_flex()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(header)
            .child(content)
            .into_any_element()
    }

    fn render_plain_body(&self, body: &Body, cx: &App) -> AnyElement {
        if let Some(preview) = body.preview.as_ref() {
            code_block(preview.clone(), cx)
        } else if body.binary {
            note_text(
                &format!(
                    "Binary body not shown{}",
                    body.size
                        .map(|size| format!(" · {}", format_bytes(size)))
                        .unwrap_or_default()
                ),
                cx,
            )
        } else {
            note_text(
                &format!(
                    "Body not captured{}",
                    body.size
                        .map(|size| format!(" · {}", format_bytes(size)))
                        .unwrap_or_default()
                ),
                cx,
            )
        }
    }

    fn render_tree(
        &self,
        value: &serde_json::Value,
        entry_index: usize,
        side: BodySide,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (rows, truncated) = json_rows(value, &self.expanded_nodes);
        let mut tree = div()
            .w_full()
            .v_flex()
            .children(rows.into_iter().map(|row| {
                let path = row.path.clone();
                let toggle = row.expandable;
                let expanded = row.expanded;
                let color = row.color(cx);
                div()
                    .id(format!(
                        "traffic-tree-{}-{}-{}",
                        entry_index,
                        side.key(),
                        path
                    ))
                    .w_full()
                    .h_flex()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().table_row_border)
                    .when(toggle, |this| {
                        this.cursor_pointer()
                            .hover(|this| this.bg(cx.theme().table_hover))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.expanded_nodes.remove(&path) {
                                    this.expanded_nodes.insert(path.clone());
                                }
                                cx.notify();
                            }))
                    })
                    .child(
                        ui::cell(Some(ui::DETAIL_KEY_COLUMN))
                            .pl(px(6. + row.depth as f32 * 14.))
                            .gap_1()
                            .children(toggle.then(|| {
                                Icon::new(if expanded {
                                    AppIcon::HideChildren
                                } else {
                                    AppIcon::ShowChildren
                                })
                                .small()
                                .text_color(cx.theme().muted_foreground)
                            }))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .font(ui::mono_font(cx))
                                    .child(row.name),
                            ),
                    )
                    .child(
                        ui::cell(None).child(
                            div()
                                .w_full()
                                .truncate()
                                .text_xs()
                                .font(ui::mono_font(cx))
                                .text_color(color)
                                .child(row.preview),
                        ),
                    )
                    // 空的操作列，只为与表头带的列宽对齐。
                    .child(ui::cell(Some(ui::DETAIL_ACTION_COLUMN)))
                    .into_any_element()
            }));
        if truncated {
            tree = tree.child(div().px_2().py_1().child(note_text(
                &format!("Tree stops after {MAX_TREE_ROWS} rows; use Raw or Pretty for the rest."),
                cx,
            )));
        }
        tree.into_any_element()
    }
}

impl Focusable for TrafficModule {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TrafficModule {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = self.render_header(cx);
        let body = self.render_body(cx);
        div()
            .track_focus(&self.focus_handle)
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(Self::on_import))
            .on_action(cx.listener(Self::on_focus_filter))
            // 列宽拖拽靠整块内容区的鼠标移动来跟手；只在按下左键时生效。
            .on_mouse_move(cx.listener(Self::drag_column))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.end_column_drag()),
            )
            // 捕获阶段处理 Esc：搜索框聚焦时也能先关详情。
            .capture_key_down(cx.listener(Self::on_close_detail_key))
            .size_full()
            .v_flex()
            .child(header)
            .child(body)
    }
}

fn info_row(label: &str, value: String, mono: bool, cx: &App) -> AnyElement {
    info_row_colored(label, value, cx.theme().foreground, cx, mono)
}

fn info_row_colored(label: &str, value: String, color: Hsla, cx: &App, mono: bool) -> AnyElement {
    kv_row(
        label.to_string(),
        value_text(value, mono, color, cx),
        None,
        cx,
    )
    .into_any_element()
}

/// 详情表格的表头带：与主表共用底色、单元格与分隔线，标题落在键列上。
fn table_band(title: &str, meta: Option<String>, action: Option<AnyElement>, cx: &App) -> Div {
    ui::table_header(cx)
        .child(
            ui::cell(Some(ui::DETAIL_KEY_COLUMN))
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .child(title.to_string()),
        )
        .child(ui::cell(None).child(match meta {
            Some(meta) => meta_text(&meta, cx),
            None => div().into_any_element(),
        }))
        .child(
            ui::cell(Some(ui::DETAIL_ACTION_COLUMN))
                .justify_end()
                .children(action),
        )
}

/// 详情表格的一行：键列固定，值列吃掉剩余宽度，行尾留给一个复制按钮。
fn kv_row(key: String, value: AnyElement, action: Option<AnyElement>, cx: &App) -> Div {
    div()
        .w_full()
        .h_flex()
        .items_center()
        .border_b_1()
        .border_color(cx.theme().table_row_border)
        .hover(|this| this.bg(cx.theme().table_hover))
        .child(
            ui::cell(Some(ui::DETAIL_KEY_COLUMN))
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(key),
        )
        .child(ui::cell(None).child(value))
        .child(
            ui::cell(Some(ui::DETAIL_ACTION_COLUMN))
                .justify_end()
                .children(action),
        )
}

/// 值单元格里的文本：等宽、单行截断，空值写成 `—`。
///
/// 截断是刻意的：cookie / token 这类没有断点的长值一旦换行会把整块内容撑出详情
/// 宽度，完整值靠行内的复制按钮取。
fn value_text(text: String, mono: bool, color: Hsla, cx: &App) -> AnyElement {
    let shown = if text.is_empty() {
        "—".to_string()
    } else {
        text
    };
    div()
        .w_full()
        .min_w_0()
        .truncate()
        .text_xs()
        .text_color(color)
        .when(mono, |this| this.font(ui::mono_font(cx)))
        .child(shown)
        .into_any_element()
}

fn section_title(text: &str, cx: &App) -> AnyElement {
    div()
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .text_color(cx.theme().muted_foreground)
        .child(text.to_string())
        .into_any_element()
}

fn meta_text(text: &str, cx: &App) -> AnyElement {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text.to_string())
        .into_any_element()
}

fn note_text(text: &str, cx: &App) -> AnyElement {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text.to_string())
        .into_any_element()
}

/// 正文块的容器：Raw 与 Pretty 共用，区别只在里面的文本有没有着色。
fn code_shell(cx: &App) -> Div {
    div()
        .p_3()
        .rounded(cx.theme().radius)
        .bg(cx.theme().accent.opacity(0.4))
        .font(ui::mono_font(cx))
        .text_xs()
}

fn code_block(text: String, cx: &App) -> AnyElement {
    code_shell(cx).child(text).into_any_element()
}

/// Pretty 视图：按 JSON 分词逐段着色，颜色与 Tree 的取值同一套主题色。
fn highlighted_code_block(text: String, cx: &App) -> AnyElement {
    let runs = highlight::json(&text)
        .into_iter()
        .map(|(range, token)| (range, highlight_style(token, cx)))
        .collect::<Vec<_>>();
    code_shell(cx)
        .child(StyledText::new(text).with_highlights(runs))
        .into_any_element()
}

fn highlight_style(token: Token, cx: &App) -> HighlightStyle {
    let color = match token {
        // 成员名保持前景色、值按类型着色，这样 Pretty 与 Tree 读起来是同一份数据。
        Token::Key => cx.theme().foreground,
        Token::String => cx.theme().success,
        Token::Number => cx.theme().info,
        Token::Bool => cx.theme().warning,
        Token::Null => cx.theme().muted_foreground,
    };
    HighlightStyle {
        color: Some(color),
        ..Default::default()
    }
}

// ------------------------------------------------------------------ 正文格式

fn available_modes(body: &Body) -> Vec<BodyMode> {
    let mut modes = Vec::new();
    if json_value(body).is_some() {
        modes.push(BodyMode::Pretty);
        modes.push(BodyMode::Tree);
    }
    if body_pairs(body).is_some() {
        modes.push(BodyMode::Form);
    }
    modes.push(BodyMode::Raw);
    modes
}

fn json_value(body: &Body) -> Option<serde_json::Value> {
    if body.binary || body.truncated {
        return None;
    }
    let preview = body.preview.as_deref()?;
    let trimmed = preview.trim_start_matches('\u{feff}').trim_start();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return None;
    }
    serde_json::from_str(trimmed).ok()
}

fn body_pairs(body: &Body) -> Option<Vec<(String, String)>> {
    if body.binary || body.truncated {
        return None;
    }
    let mime = body.mime.to_ascii_lowercase();
    if !mime.contains("application/x-www-form-urlencoded") {
        return None;
    }
    let preview = body.preview.as_deref()?.trim();
    if preview.is_empty() {
        return None;
    }
    Some(capture::parse_query(preview))
}

fn pretty_json(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

struct JsonRow {
    path: String,
    depth: usize,
    name: String,
    preview: String,
    kind: ValueKind,
    expandable: bool,
    expanded: bool,
}

#[derive(Clone, Copy)]
enum ValueKind {
    Text,
    Number,
    Bool,
    Null,
    Container,
}

impl JsonRow {
    fn color(&self, cx: &App) -> Hsla {
        match self.kind {
            ValueKind::Text => cx.theme().success,
            ValueKind::Number => cx.theme().info,
            ValueKind::Bool => cx.theme().warning,
            ValueKind::Null | ValueKind::Container => cx.theme().muted_foreground,
        }
    }
}

/// 展开状态只记「张开」的节点；默认只铺第一层，深层结构按需点开。
fn json_rows(value: &serde_json::Value, expanded: &HashSet<String>) -> (Vec<JsonRow>, bool) {
    let mut rows = Vec::new();
    let mut truncated = false;
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                push_json_node(
                    child,
                    key,
                    format!("/{}", escape_pointer(key)),
                    0,
                    expanded,
                    &mut rows,
                    &mut truncated,
                );
            }
        }
        serde_json::Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                push_json_node(
                    child,
                    &format!("[{index}]"),
                    format!("/{index}"),
                    0,
                    expanded,
                    &mut rows,
                    &mut truncated,
                );
            }
        }
        scalar => rows.push(JsonRow {
            path: String::new(),
            depth: 0,
            name: "body".to_string(),
            preview: scalar_preview(scalar),
            kind: ValueKind::Container,
            expandable: false,
            expanded: false,
        }),
    }
    (rows, truncated)
}

#[allow(clippy::too_many_arguments)]
fn push_json_node(
    value: &serde_json::Value,
    name: &str,
    path: String,
    depth: usize,
    expanded: &HashSet<String>,
    rows: &mut Vec<JsonRow>,
    truncated: &mut bool,
) {
    if rows.len() >= MAX_TREE_ROWS {
        *truncated = true;
        return;
    }
    let expandable = matches!(value, serde_json::Value::Object(map) if !map.is_empty())
        || matches!(value, serde_json::Value::Array(items) if !items.is_empty());
    let kind = match value {
        serde_json::Value::String(_) => ValueKind::Text,
        serde_json::Value::Number(_) => ValueKind::Number,
        serde_json::Value::Bool(_) => ValueKind::Bool,
        serde_json::Value::Null => ValueKind::Null,
        _ => ValueKind::Container,
    };
    let preview = match value {
        serde_json::Value::Object(map) => format!("{{ {} }}", map.len()),
        serde_json::Value::Array(items) => format!("[ {} ]", items.len()),
        scalar => scalar_preview(scalar),
    };
    let is_expanded = expandable && expanded.contains(&path);
    rows.push(JsonRow {
        path: path.clone(),
        depth,
        name: name.to_string(),
        preview,
        kind,
        expandable,
        expanded: is_expanded,
    });
    if !is_expanded {
        return;
    }
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                if rows.len() >= MAX_TREE_ROWS {
                    *truncated = true;
                    break;
                }
                push_json_node(
                    child,
                    key,
                    format!("{path}/{}", escape_pointer(key)),
                    depth + 1,
                    expanded,
                    rows,
                    truncated,
                );
            }
        }
        serde_json::Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                if rows.len() >= MAX_TREE_ROWS {
                    *truncated = true;
                    break;
                }
                push_json_node(
                    child,
                    &format!("[{index}]"),
                    format!("{path}/{index}"),
                    depth + 1,
                    expanded,
                    rows,
                    truncated,
                );
            }
        }
        _ => {}
    }
}

fn scalar_preview(value: &serde_json::Value) -> String {
    let text = match value {
        serde_json::Value::String(text) => format!("\"{text}\""),
        scalar => scalar.to_string(),
    };
    // 树是结构速览：长字符串留个开头即可，完整内容去 Pretty / Raw。
    truncate_preview(&text, 60)
}

fn truncate_preview(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

fn escape_pointer(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

// ------------------------------------------------------------------ 小工具

fn push_query_count(rows: &mut Vec<AnyElement>, query: &str, cx: &App) {
    let count = capture::parse_query(query).len();
    rows.push(info_row(
        "Query",
        ui::counted(count, "parameter", "parameters"),
        false,
        cx,
    ));
}

fn status_text(entry: &Entry) -> String {
    match (entry.status, &entry.note) {
        (Some(status), _) => status.to_string(),
        (None, Some(_)) => "Failed".to_string(),
        (None, None) => "—".to_string(),
    }
}

fn status_color(entry: &Entry, cx: &App) -> Hsla {
    if entry.note.is_some() {
        return cx.theme().danger;
    }
    match status_class(entry.status) {
        StatusClass::Success => cx.theme().success,
        StatusClass::Redirect => cx.theme().info,
        StatusClass::ClientError => cx.theme().warning,
        StatusClass::ServerError => cx.theme().danger,
        StatusClass::Pending => cx.theme().muted_foreground,
    }
}

fn scheme(url: &str) -> &str {
    url.split_once("://").map_or("—", |(scheme, _)| scheme)
}

fn mime_label(mime: &str) -> String {
    if mime.is_empty() {
        return "—".to_string();
    }
    mime.split(';').next().unwrap_or(mime).trim().to_string()
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_duration(ms: f64) -> String {
    if ms < 1.0 {
        "<1 ms".to_string()
    } else if ms < 1000.0 {
        format!("{ms:.0} ms")
    } else {
        format!("{:.2} s", ms / 1000.0)
    }
}

/// 只取 ISO-8601 的时间部分；日期在详情里完整显示。
fn started_label(started: &str) -> String {
    match clock_part(started, Local) {
        Some(clock) => clock,
        None => raw_clock_part(started).unwrap_or_else(|| started.to_string()),
    }
}

/// 详情里的完整时刻：本地日期、时间加上它相对 UTC 的偏移。
fn started_detail_label(started: &str) -> String {
    match parse_started(started, Local) {
        Some(at) => format!(
            "{} {}",
            at.format("%Y-%m-%d %H:%M:%S%.3f"),
            at.format("%:z")
        ),
        None => started.to_string(),
    }
}

/// 抓包里存的是绝对时刻，界面按给定时区渲染，这样四种格式共用同一条展示规则；
/// 解析不出来的串（例如 Charles XML 直接写了本地时间）返回 `None`，由调用方原样展示。
fn parse_started<T: TimeZone>(started: &str, tz: T) -> Option<DateTime<T>> {
    DateTime::parse_from_rfc3339(started)
        .ok()
        .map(|at| at.with_timezone(&tz))
}

fn clock_part<T: TimeZone>(started: &str, tz: T) -> Option<String>
where
    T::Offset: std::fmt::Display,
{
    Some(parse_started(started, tz)?.format("%H:%M:%S").to_string())
}

fn raw_clock_part(started: &str) -> Option<String> {
    let (_, time) = started.split_once(['T', ' '])?;
    Some(time.get(..8).unwrap_or(time).to_string())
}

#[cfg(test)]
mod tests {
    use chrono::FixedOffset;

    use super::*;

    fn body(mime: &str, preview: &str) -> Body {
        Body {
            preview: Some(preview.to_string()),
            size: Some(preview.len() as u64),
            truncated: false,
            binary: false,
            mime: mime.to_string(),
        }
    }

    #[test]
    fn byte_labels_switch_units_at_1024() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn durations_read_as_ms_until_a_second() {
        assert_eq!(format_duration(0.4), "<1 ms");
        assert_eq!(format_duration(123.4), "123 ms");
        assert_eq!(format_duration(1500.0), "1.50 s");
    }

    #[test]
    fn started_times_render_in_the_viewing_timezone() {
        // Charles 里这条是 12:30:29：会话存的是绝对时刻，不能直接印 UTC。
        let east_eight = FixedOffset::east_opt(8 * 3600).expect("offset");
        assert_eq!(
            clock_part("2026-10-27T04:30:29.883Z", east_eight),
            Some("12:30:29".to_string())
        );
        assert_eq!(
            clock_part("2026-10-27T12:30:29.883+08:00", east_eight),
            Some("12:30:29".to_string()),
            "自带偏移的串换算后要和二进制会话的读数一致"
        );
        assert_eq!(clock_part("2026-10-27 12:30:29", east_eight), None);
    }

    #[test]
    fn unreadable_timestamps_stay_visible() {
        // 认不出的串（Charles XML 直接写了本地时间）退回首写的时间部分。
        assert_eq!(started_label("2026-10-27 12:30:29.883"), "12:30:29");
        assert_eq!(
            started_label("not-a-date"),
            "not-a-date",
            "解析失败要原样显示，不能变成空白"
        );
        assert_eq!(
            started_detail_label("2026-10-27 12:30:29.883"),
            "2026-10-27 12:30:29.883"
        );
    }

    #[test]
    fn mime_label_drops_parameters() {
        assert_eq!(
            mime_label("application/json; charset=utf-8"),
            "application/json"
        );
        assert_eq!(mime_label(""), "—");
    }

    #[test]
    fn json_bodies_offer_pretty_tree_and_raw() {
        let json = body("application/json", r#"{"a":1,"b":[true,null]}"#);
        assert_eq!(
            available_modes(&json),
            vec![BodyMode::Pretty, BodyMode::Tree, BodyMode::Raw]
        );
        let mut truncated = body("application/json", r#"{"a":1"#);
        truncated.truncated = true;
        assert_eq!(
            available_modes(&truncated),
            vec![BodyMode::Raw],
            "预览被截断时不能假装能格式化成 JSON"
        );
        let text = body("text/plain", "hello");
        assert_eq!(available_modes(&text), vec![BodyMode::Raw]);
    }

    #[test]
    fn form_bodies_offer_the_key_value_view() {
        let form = body(
            "application/x-www-form-urlencoded; charset=utf-8",
            "user=alice&token=x%20y",
        );
        assert_eq!(available_modes(&form), vec![BodyMode::Form, BodyMode::Raw]);
        assert_eq!(
            body_pairs(&form).expect("form pairs"),
            vec![
                ("user".to_string(), "alice".to_string()),
                ("token".to_string(), "x y".to_string()),
            ]
        );
    }

    #[test]
    fn json_tree_starts_collapsed_and_expands_by_path() {
        let value: serde_json::Value =
            serde_json::from_str(r#"{"user":{"name":"alice"},"count":2}"#).expect("json");
        let (rows, truncated) = json_rows(&value, &HashSet::new());
        assert!(!truncated);
        assert_eq!(rows.len(), 2, "默认只铺第一层");
        assert_eq!(rows[0].name, "user");
        assert!(rows[0].expandable);
        assert!(!rows[0].expanded);
        assert_eq!(rows[1].name, "count");

        let mut expanded = HashSet::new();
        expanded.insert("/user".to_string());
        let (rows, _) = json_rows(&value, &expanded);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].name, "name");
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[1].preview, "\"alice\"");
        assert!(!rows[1].expandable);
    }

    #[test]
    fn pretty_json_indents_nested_values() {
        let value: serde_json::Value = serde_json::from_str(r#"{"a":{"b":1}}"#).expect("json");
        assert_eq!(pretty_json(&value), "{\n  \"a\": {\n    \"b\": 1\n  }\n}");
    }

    #[test]
    fn tree_paths_escape_json_pointer_characters() {
        assert_eq!(escape_pointer("a/b~c"), "a~1b~0c");
    }
}
