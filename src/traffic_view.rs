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

/// 窄窗口下详情改成底部通栏，给它的高度。
const DETAIL_BOTTOM_HEIGHT: Pixels = px(420.);

/// 窗口窄于此值时详情改成底部通栏：560px 的侧栏会盖住列表的路径列，
/// 而路径是这一列表唯一还剩的信息。
const NARROW_WINDOW_WIDTH: Pixels = px(1100.);

/// JSON 树一次最多铺多少行：再多的结构用 Raw / Pretty 看更合适。
const MAX_TREE_ROWS: usize = 1_000;

/// Hex 视图一次最多铺多少行：16 KB 足够看出内容类型，再多就是拖慢滚动。
const MAX_HEX_ROWS: usize = 1_024;

/// 总览里「来源」这一组的折叠键。
const SOURCE_GROUP: &str = "source";

/// URL 折行时的目标行长。GPUI 没有断词换行，只能自己在分隔符处切。
const URL_LINE_WIDTH: usize = 96;

/// 文件选择器里的默认过滤项；内容嗅探才是准绳，这里只帮用户少翻几页。
const CAPTURE_EXTENSIONS: &[&str] = &["har", "chls", "chlsj", "xml"];

actions!(traffic, [ImportCapture, FocusFilter]);

/// 请求段的页签。请求与响应分成两段，每段各管自己的一组证据——
/// 混在一列里，读者只能靠滚动位置记「我看到哪了」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestTab {
    Overview,
    Params,
    Headers,
    Body,
}

impl RequestTab {
    const ALL: [Self; 4] = [Self::Overview, Self::Params, Self::Headers, Self::Body];

    fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Params => "Params",
            Self::Headers => "Headers",
            Self::Body => "Body",
        }
    }
}

/// 响应段的页签。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseTab {
    Headers,
    Body,
}

impl ResponseTab {
    const ALL: [Self; 2] = [Self::Headers, Self::Body];

    fn label(self) -> &'static str {
        match self {
            Self::Headers => "Headers",
            Self::Body => "Body",
        }
    }
}

/// 状态类筛选：与状态码分类一一对应，另加「失败」（没有状态码的条目）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusFilter {
    Success,
    Redirect,
    ClientError,
    ServerError,
    Failed,
}

impl StatusFilter {
    const ALL: [Self; 5] = [
        Self::Success,
        Self::Redirect,
        Self::ClientError,
        Self::ServerError,
        Self::Failed,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Success => "2xx",
            Self::Redirect => "3xx",
            Self::ClientError => "4xx",
            Self::ServerError => "5xx",
            Self::Failed => "Failed",
        }
    }

    fn matches(self, entry: &Entry) -> bool {
        if entry.note.is_some() || entry.status.is_none() {
            return self == Self::Failed;
        }
        matches!(
            (self, status_class(entry.status)),
            (Self::Success, StatusClass::Success)
                | (Self::Redirect, StatusClass::Redirect)
                | (Self::ClientError, StatusClass::ClientError)
                | (Self::ServerError, StatusClass::ServerError)
        )
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
    /// 二进制的兜底视图：按字节两列 16 进制 + ASCII 侧栏。
    Hex,
}

impl BodyMode {
    fn label(self) -> &'static str {
        match self {
            Self::Pretty => "Pretty",
            Self::Tree => "Tree",
            Self::Form => "Form",
            Self::Raw => "Raw",
            Self::Hex => "Hex",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Pretty => "pretty",
            Self::Tree => "tree",
            Self::Form => "form",
            Self::Raw => "raw",
            Self::Hex => "hex",
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
    /// 请求段与响应段各有一个页签；两段的证据性质不同，分开翻。
    request_tab: RequestTab,
    response_tab: ResponseTab,
    /// 状态类筛选；`None` 表示不过滤。
    status_filter: Option<StatusFilter>,
    /// 折叠起来的分组（总览里的「来源」）。
    collapsed_groups: HashSet<String>,
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
            request_tab: RequestTab::Overview,
            response_tab: ResponseTab::Headers,
            status_filter: None,
            collapsed_groups: HashSet::new(),
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
        let status_filter = self.status_filter;
        self.filtered = self
            .capture
            .as_ref()
            .map(|capture| {
                capture
                    .entries
                    .iter()
                    .enumerate()
                    .filter(|(_, entry)| {
                        let matches_query = query.is_empty()
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
                                .is_some_and(|note| note.to_lowercase().contains(&query));
                        // 状态类与搜索词是「与」的关系：两个轴各自收敛，再取交集。
                        let matches_status =
                            status_filter.is_none_or(|filter| filter.matches(entry));
                        matches_query && matches_status
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

    /// 切换状态类筛选。`None` 表示不过滤。
    fn set_status_filter(&mut self, filter: Option<StatusFilter>, cx: &mut Context<Self>) {
        self.status_filter = filter;
        self.apply_filter(cx);
        cx.notify();
    }

    /// 每个状态类在这份会话里各有多少条。chip 上要写这个数。
    fn filter_counts(&self) -> Vec<(StatusFilter, usize)> {
        let Some(capture) = self.capture.as_ref() else {
            return Vec::new();
        };
        StatusFilter::ALL
            .into_iter()
            .map(|filter| {
                let count = capture
                    .entries
                    .iter()
                    .filter(|entry| filter.matches(entry))
                    .count();
                (filter, count)
            })
            .collect()
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

    /// 当前详情在过滤后列表里的位置与列表长度，用来判断上一条/下一条还有没有。
    fn detail_position(&self) -> Option<(usize, usize)> {
        let current = self.detail?;
        let position = self.filtered.iter().position(|index| *index == current)?;
        Some((position, self.filtered.len()))
    }

    /// 在过滤后的列表里前后走一条。浮层不关，只换内容。
    fn step_detail(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some((position, len)) = self.detail_position() else {
            return;
        };
        let next = position as isize + delta;
        if next < 0 || next as usize >= len {
            return;
        }
        let next_index = self.filtered[next as usize];
        self.selected = Some(next_index);
        self.detail = Some(next_index);
        self.reset_detail_state();
        cx.notify();
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
                self.request_tab = RequestTab::Overview;
                self.response_tab = ResponseTab::Headers;
                self.status_filter = None;
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
        self.request_tab = RequestTab::Overview;
        self.response_tab = ResponseTab::Headers;
        self.status_filter = None;
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
                div().h_flex().items_center().gap_2().child(
                    div().w_96().child(
                        Input::new(&self.search)
                            .w_full()
                            .cleanable(true)
                            .prefix(Icon::new(AppIcon::Search).small()),
                    ),
                ),
            )
            .child(self.render_filter_chips(cx))
            .into_any_element()
    }

    /// 过滤器 chip 行：一个轴（状态类）。选中即高亮，再点一次取消。
    ///
    /// chip 上带这一类的条数——筛完能一眼验算，不必回头数行。
    fn render_filter_chips(&self, cx: &mut Context<Self>) -> AnyElement {
        let total = self
            .capture
            .as_ref()
            .map_or(0, |capture| capture.entries.len());
        let mut chips = div().h_flex().items_center().gap_1();
        chips = chips.child(
            Button::new("filter-all")
                .ghost()
                .xsmall()
                .selected(self.status_filter.is_none())
                .label("All")
                .on_click(cx.listener(|this, _, _, cx| this.set_status_filter(None, cx))),
        );
        for (filter, count) in self.filter_counts() {
            chips = chips.child(
                Button::new(format!("filter-{}", filter.label()))
                    .ghost()
                    .xsmall()
                    .selected(self.status_filter == Some(filter))
                    .label(format!("{} ({count})", filter.label()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_status_filter(Some(filter), cx);
                    })),
            );
        }
        chips = chips.child(div().flex_1());
        if self.capture.is_some() {
            chips = chips.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("{} of {total}", self.filtered.len())),
            );
        }
        chips.into_any_element()
    }

    fn render_body(&self, narrow: bool, cx: &mut Context<Self>) -> AnyElement {
        let content = if let Some(capture) = self.capture.as_ref() {
            self.render_capture(capture, narrow, cx)
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

    fn render_capture(
        &self,
        capture: &CaptureLog,
        narrow: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
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
            Some(self.render_detail(narrow, cx))
        } else {
            None
        };
        let list = self.render_list(cx);
        let content = match detail {
            // 窄窗口：列表在上、详情在底部通栏，请求 / 响应左右并排。
            // 底部通栏是 flex 兄弟节点而不是浮层——它给列表让出高度，不遮路径列。
            Some(detail) if narrow => div()
                .flex_1()
                .min_h_0()
                .v_flex()
                .child(list)
                .child(
                    div()
                        .occlude()
                        .w_full()
                        .h(DETAIL_BOTTOM_HEIGHT)
                        .border_t_1()
                        .border_color(cx.theme().border)
                        .bg(cx.theme().background)
                        .child(detail),
                )
                .into_any_element(),
            // 宽窗口：详情是浮层，不参与列表的宽度分配——打开/关闭都不会让列宽跳动。
            Some(detail) => div()
                .flex_1()
                .min_h_0()
                .relative()
                .child(list)
                .child(
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
                )
                .into_any_element(),
            None => div()
                .flex_1()
                .min_h_0()
                .relative()
                .child(list)
                .into_any_element(),
        };

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

    /// 详情浮层：一个头 + 请求段 + 响应段。
    ///
    /// 两段各带自己的标签栏与滚动，于是「请求头带的 token」与「响应头回的
    /// status」能同时看到——审失败会话时，这两组证据必须并排。
    fn render_detail(&self, narrow: bool, cx: &mut Context<Self>) -> AnyElement {
        let Some((entry_index, entry)) = self.detail_entry() else {
            return self.render_notice(
                AppIcon::Empty,
                "Select a request",
                "Pick a row to see its headers and bodies.",
                None,
                cx,
            );
        };
        let request = self.render_request_section(entry, entry_index, narrow, cx);
        let response = self.render_response_section(entry, entry_index, cx);
        let sections = if narrow {
            // 竖向布局：请求在左、响应在右，两段并排。
            // items_stretch 不能省：h_flex 默认在交叉轴上居中，两段会各自按内容
            // 高度居中——高的一段上下溢出被裁，低的一段悬在中间，两段都读不全。
            div()
                .flex_1()
                .min_h_0()
                .h_flex()
                .items_stretch()
                .child(request)
                .child(response)
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_h_0()
                .v_flex()
                .child(request)
                .child(response)
                .into_any_element()
        };
        div()
            .w_full()
            .h_full()
            .min_h_0()
            .min_w_0()
            .v_flex()
            .child(self.render_detail_header(entry, cx))
            .child(sections)
            .into_any_element()
    }

    /// 浮层头部：方法与状态 + 上一条/下一条 + 关闭。
    ///
    /// 上一条/下一条是这份列表唯一的效率杠杆：一份会话动辄几百条，靠回列表
    /// 点行翻要几十次往返。
    fn render_detail_header(&self, entry: &Entry, cx: &mut Context<Self>) -> AnyElement {
        let (can_previous, can_next) = match self.detail_position() {
            Some((index, len)) => (index > 0, index + 1 < len),
            None => (false, false),
        };
        let duration = entry
            .duration_ms
            .map(format_duration)
            .unwrap_or_else(|| "—".to_string());
        let size = entry
            .size
            .map(format_bytes)
            .unwrap_or_else(|| "—".to_string());
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
                    .child(method_badge(&entry.method, cx))
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(status_color(entry, cx))
                            .child(status_text(entry)),
                    )
                    .child(meta_text(&duration, cx))
                    .child(meta_text(&size, cx))
                    .child(div().flex_1())
                    .child(
                        ui::icon_button("detail-previous", AppIcon::PreviousEntry, cx)
                            .tooltip("Previous request")
                            .disabled(!can_previous)
                            .on_click(cx.listener(|this, _, _, cx| this.step_detail(-1, cx))),
                    )
                    .child(
                        ui::icon_button("detail-next", AppIcon::NextEntry, cx)
                            .tooltip("Next request")
                            .disabled(!can_next)
                            .on_click(cx.listener(|this, _, _, cx| this.step_detail(1, cx))),
                    )
                    .child(
                        ui::icon_button("close-detail", AppIcon::Remove, cx)
                            .tooltip("Close details")
                            .on_click(cx.listener(|this, _, _, cx| this.close_detail(cx))),
                    ),
            )
            .child(meta_text(
                &entry
                    .started
                    .as_deref()
                    .map(started_label)
                    .unwrap_or_default(),
                cx,
            ))
            .into_any_element()
    }

    /// URL 独立成行：它是这次请求的身份，其余字段都能由它推出。
    ///
    /// 整行可点，交给系统默认浏览器打开；URL 用链接色、不截断，完整内容
    /// 由旁边的复制按钮取。
    fn render_url_row(&self, entry: &Entry, cx: &mut Context<Self>) -> AnyElement {
        let open = entry.url.clone();
        let open_for_link = open.clone();
        let copy = entry.url.clone();
        div()
            .px_4()
            .py_2()
            .h_flex()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .id("detail-url")
                    .flex_1()
                    .min_w_0()
                    .font(ui::mono_font(cx))
                    .text_xs()
                    .text_color(cx.theme().link)
                    .cursor_pointer()
                    .hover(|this| this.text_color(cx.theme().link_hover))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.open_url(&open_for_link);
                    }))
                    .children(url_lines(&entry.url)),
            )
            .child(
                ui::icon_button("open-url", AppIcon::OpenLink, cx)
                    .tooltip("Open in browser")
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.open_url(&open);
                    })),
            )
            .child(
                ui::icon_button("copy-url", AppIcon::Copy, cx)
                    .tooltip("Copy URL")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.copy_text(copy.clone(), window, cx);
                    })),
            )
            .into_any_element()
    }

    fn render_request_section(
        &self,
        entry: &Entry,
        entry_index: usize,
        narrow: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tab = self.request_tab_for(entry);
        let (toolbar, content) = match tab {
            RequestTab::Overview => (None, self.render_overview(entry, cx)),
            RequestTab::Params => (None, self.render_params(entry, cx)),
            RequestTab::Headers => (
                None,
                self.render_headers_section(&entry.request_headers, cx),
            ),
            RequestTab::Body => (
                Some(self.render_body_toolbar(entry.request_body.as_ref(), BodySide::Request, cx)),
                self.render_body_content(
                    entry.request_body.as_ref(),
                    BodySide::Request,
                    entry_index,
                    cx,
                ),
            ),
        };
        div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .v_flex()
            .when(narrow, |this| {
                this.border_r_1().border_color(cx.theme().border)
            })
            .child(self.render_url_row(entry, cx))
            .child(self.render_request_tabs(entry, tab, cx))
            .when_some(toolbar, |this, toolbar| this.child(toolbar))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .id(format!("traffic-request-scroll-{entry_index}"))
                    .overflow_y_scroll()
                    .child(content),
            )
            .into_any_element()
    }

    fn render_response_section(
        &self,
        entry: &Entry,
        entry_index: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tab = self.response_tab_for(entry);
        let (toolbar, content) = match tab {
            ResponseTab::Headers => (
                None,
                self.render_headers_section(&entry.response_headers, cx),
            ),
            ResponseTab::Body => (
                Some(self.render_body_toolbar(
                    entry.response_body.as_ref(),
                    BodySide::Response,
                    cx,
                )),
                self.render_body_content(
                    entry.response_body.as_ref(),
                    BodySide::Response,
                    entry_index,
                    cx,
                ),
            ),
        };
        div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .v_flex()
            // 段之间要有分隔线：两段是两组性质不同的证据，
            // 界线不清时读者分不清哪一行属于请求、哪一行属于响应。
            .border_t_1()
            .border_color(cx.theme().border)
            .child(self.render_response_tabs(entry, tab, cx))
            .when_some(toolbar, |this, toolbar| this.child(toolbar))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .id(format!("traffic-response-scroll-{entry_index}"))
                    .overflow_y_scroll()
                    .child(content),
            )
            .into_any_element()
    }

    /// 请求段的标签栏：标签名带计数，右端挂方法徽标。
    fn render_request_tabs(
        &self,
        entry: &Entry,
        active: RequestTab,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let param_count = entry
            .query
            .as_deref()
            .map(|query| capture::parse_query(query).len())
            .unwrap_or(0);
        let mut tabs = div()
            .px_4()
            .py_2()
            .h_flex()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(section_title("REQUEST", cx));
        for tab in RequestTab::ALL {
            if !request_tab_available(tab, entry) {
                continue;
            }
            let label = match tab {
                RequestTab::Params => format!("Params ({param_count})"),
                RequestTab::Headers => format!("Headers ({})", entry.request_headers.len()),
                _ => tab.label().to_string(),
            };
            tabs = tabs.child(
                Button::new(format!("request-tab-{}", tab.label()))
                    .ghost()
                    .xsmall()
                    .selected(active == tab)
                    .label(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.request_tab = tab;
                        cx.notify();
                    })),
            );
        }
        tabs.child(div().flex_1())
            .child(method_badge(&entry.method, cx))
            .into_any_element()
    }

    /// 响应段的标签栏：右端挂协议与状态码徽标。
    fn render_response_tabs(
        &self,
        entry: &Entry,
        active: ResponseTab,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut tabs = div()
            .px_4()
            .py_2()
            .h_flex()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(section_title("RESPONSE", cx));
        for tab in ResponseTab::ALL {
            if !response_tab_available(tab, entry) {
                continue;
            }
            let label = match tab {
                ResponseTab::Headers => format!("Headers ({})", entry.response_headers.len()),
                ResponseTab::Body => "Body".to_string(),
            };
            tabs = tabs.child(
                Button::new(format!("response-tab-{}", tab.label()))
                    .ghost()
                    .xsmall()
                    .selected(active == tab)
                    .label(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.response_tab = tab;
                        cx.notify();
                    })),
            );
        }
        // 协议只有 HAR 记录了；拿不到就不挂这个徽标，不拿别的东西凑。
        let protocol = entry
            .protocol
            .as_ref()
            .map(|protocol| soft_badge(protocol, cx.theme().muted_foreground, cx));
        tabs.child(div().flex_1())
            .when_some(protocol, |this, badge| this.child(badge))
            .child(soft_badge(&status_text(entry), status_color(entry, cx), cx))
            .into_any_element()
    }

    /// 选中的页签在这条请求上不存在时（换了一条没有正文的请求），退到第一个可用的。
    fn request_tab_for(&self, entry: &Entry) -> RequestTab {
        if request_tab_available(self.request_tab, entry) {
            return self.request_tab;
        }
        RequestTab::ALL
            .into_iter()
            .find(|tab| request_tab_available(*tab, entry))
            .unwrap_or(RequestTab::Overview)
    }

    fn response_tab_for(&self, entry: &Entry) -> ResponseTab {
        if response_tab_available(self.response_tab, entry) {
            return self.response_tab;
        }
        ResponseTab::ALL
            .into_iter()
            .find(|tab| response_tab_available(*tab, entry))
            .unwrap_or(ResponseTab::Headers)
    }

    fn render_params(&self, entry: &Entry, cx: &mut Context<Self>) -> AnyElement {
        let Some(query) = entry.query.as_deref() else {
            return div()
                .px_4()
                .pb_3()
                .child(note_text("No query parameters", cx))
                .into_any_element();
        };
        let pairs = capture::parse_query(query);
        if pairs.is_empty() {
            return div()
                .px_4()
                .pb_3()
                .child(note_text("No query parameters", cx))
                .into_any_element();
        }
        self.render_pairs_section("QUERY PARAMETERS", &pairs, cx)
    }

    /// 总览：一张事实表，分「基本」与「来源」两组，后者可折叠。
    ///
    /// URL 不在这里——它是浮层里独立的一行（P0-3），要读的东西不该被折叠。
    fn render_overview(&self, entry: &Entry, cx: &mut Context<Self>) -> AnyElement {
        let basic = vec![
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
        ];
        let mut source = vec![
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
            push_query_count(&mut source, query, cx);
        }
        if let Some(note) = entry.note.as_deref() {
            source.push(info_row_colored(
                "Error",
                note.to_string(),
                cx.theme().danger,
                cx,
                false,
            ));
        }

        let collapsed = self.collapsed_groups.contains(SOURCE_GROUP);
        div()
            .v_flex()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(self.render_group_band("basic", "BASIC", false, cx))
            .child(div().w_full().v_flex().children(basic))
            .child(self.render_group_band("source", "SOURCE", collapsed, cx))
            .when(!collapsed, |this| {
                this.child(div().w_full().v_flex().children(source))
            })
            .into_any_element()
    }

    /// 可折叠分组的标题带。标题可点，点了收起/展开这一组。
    fn render_group_band(
        &self,
        id: &'static str,
        title: &str,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = id.to_string();
        div()
            .id(format!("group-band-{id}"))
            .w_full()
            .h_flex()
            .items_center()
            .gap_1()
            .px_4()
            .py_2()
            .bg(cx.theme().tokens.table_head)
            .border_b_1()
            .border_color(cx.theme().table_row_border)
            .cursor_pointer()
            .hover(|this| this.bg(cx.theme().table_hover))
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.collapsed_groups.remove(&key) {
                    this.collapsed_groups.insert(key.clone());
                }
                cx.notify();
            }))
            .child(
                Icon::new(if collapsed {
                    AppIcon::ShowChildren
                } else {
                    AppIcon::HideChildren
                })
                .small()
                .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(cx.theme().table_head_foreground)
                    .child(title.to_string()),
            )
            .child(div().flex_1())
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

    /// 正文工具栏：视图切换（Pretty / Tree / Raw / Hex）+ 大小 + 复制。
    ///
    /// 它挂在段标签栏下面，不重复写「BODY」——段标签栏已经说了这是正文。
    fn render_body_toolbar(
        &self,
        body: Option<&Body>,
        side: BodySide,
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

        let mut toolbar = div()
            .px_4()
            .py_2()
            .h_flex()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(meta_text(&meta, cx));
        for mode in available {
            toolbar = toolbar.child(
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
        toolbar = toolbar.child(div().flex_1());
        if let Some(copy_text) = copy_text {
            toolbar = toolbar.child(
                ui::icon_button(format!("copy-body-{}", side.key()), AppIcon::Copy, cx)
                    .tooltip("Copy body")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.copy_text(copy_text.clone(), window, cx);
                    })),
            );
        }
        toolbar.into_any_element()
    }

    /// 正文内容：按当前视图渲染这一侧的身体。
    fn render_body_content(
        &self,
        body: Option<&Body>,
        side: BodySide,
        entry_index: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(body) = body else {
            return div()
                .px_4()
                .pb_3()
                .child(note_text("No body", cx))
                .into_any_element();
        };
        let active = self.mode_for(body, side);
        let mut block = div().px_4().pb_3().v_flex().gap_2();
        if body.truncated {
            // 二进制正文没有 preview，只有 bytes；按 preview 取长度会得到 "first 0 B"。
            let shown = body_bytes(body).map_or(0, |bytes| bytes.len() as u64);
            let tail = if active == BodyMode::Hex {
                "; Hex shows the captured bytes only."
            } else {
                "; formatting uses the preview only."
            };
            let message = match body.size {
                Some(size) => format!(
                    "Showing the first {} of {}{tail}",
                    format_bytes(shown),
                    format_bytes(size)
                ),
                None => format!("Showing the first {}{tail}", format_bytes(shown)),
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
            BodyMode::Hex => self.render_hex_body(body, cx),
        };
        block.child(formatted).into_any_element()
    }

    /// Hex 视图：二进制正文的兜底。没有它，这类条目在详情里等于空白。
    fn render_hex_body(&self, body: &Body, cx: &App) -> AnyElement {
        let Some(bytes) = body_bytes(body) else {
            return note_text("No bytes captured", cx).into_any_element();
        };
        let (rows, truncated) = hex_dump(bytes);
        let mut view = code_shell(cx).children(rows);
        if truncated {
            view = view.child(note_text(
                &format!("Hex stops after {MAX_HEX_ROWS} rows; the rest is not shown."),
                cx,
            ));
        }
        view.into_any_element()
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 窄窗口时详情改成底部通栏：560px 的右侧浮层会盖住列表的路径列。
        let narrow = window.viewport_size().width < NARROW_WINDOW_WIDTH;
        let header = self.render_header(cx);
        let body = self.render_body(narrow, cx);
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

/// 方法徽标：`POST` 这类关键字用等宽体，不着色——着色是留给状态码的。
fn method_badge(method: &str, cx: &App) -> AnyElement {
    div()
        .px_1()
        .rounded(cx.theme().radius)
        .bg(cx.theme().secondary)
        .text_xs()
        .font(ui::mono_font(cx))
        .child(method.to_string())
        .into_any_element()
}

/// 软徽标：一枚带底色的短文本，用于协议与状态码。颜色由调用方给（主题语义色）。
fn soft_badge(text: &str, color: Hsla, cx: &App) -> AnyElement {
    div()
        .px_1()
        .rounded(cx.theme().radius)
        .bg(color.opacity(0.14))
        .text_xs()
        .font(ui::mono_font(cx))
        .text_color(color)
        .child(text.to_string())
        .into_any_element()
}

/// 标签是上下文相关的：没有参数就不出「参数」标签，没有正文就不出「正文」标签。
fn request_tab_available(tab: RequestTab, entry: &Entry) -> bool {
    match tab {
        RequestTab::Overview => true,
        RequestTab::Params => entry
            .query
            .as_deref()
            .is_some_and(|query| !capture::parse_query(query).is_empty()),
        RequestTab::Headers => !entry.request_headers.is_empty(),
        RequestTab::Body => entry.request_body.is_some(),
    }
}

fn response_tab_available(tab: ResponseTab, entry: &Entry) -> bool {
    match tab {
        ResponseTab::Headers => !entry.response_headers.is_empty(),
        ResponseTab::Body => entry.response_body.is_some(),
    }
}

/// URL 太长时按分隔符折行。
///
/// GPUI 没有断词换行，一个没有断点的长 URL 只能截断——而 URL 恰恰是要读的东西。
/// 所以自己切：尽量落在 `/ ? & #` 这些语义边界上，并且至少留 8 个字符，
/// 免得把 `https://` 拆开（拆开了读者会以为地址变了）。
fn url_lines(url: &str) -> Vec<String> {
    let chars: Vec<char> = url.chars().collect();
    if chars.len() <= URL_LINE_WIDTH {
        return vec![url.to_string()];
    }
    let mut lines = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let end = (start + URL_LINE_WIDTH).min(chars.len());
        if end == chars.len() {
            lines.push(chars[start..].iter().collect());
            break;
        }
        let mut cut = end;
        while cut > start + 8 && !matches!(chars[cut - 1], '/' | '?' | '&' | '#') {
            cut -= 1;
        }
        lines.push(chars[start..cut].iter().collect());
        start = cut;
    }
    lines
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
    if body.binary {
        // 二进制没有别的看法，只有字节。
        modes.push(BodyMode::Hex);
        return modes;
    }
    if json_value(body).is_some() {
        modes.push(BodyMode::Pretty);
        modes.push(BodyMode::Tree);
    }
    if body_pairs(body).is_some() {
        modes.push(BodyMode::Form);
    }
    modes.push(BodyMode::Raw);
    modes.push(BodyMode::Hex);
    modes
}

/// 正文要看的字节：二进制正文取存下来的原始字节，文本正文取预览的字节。
fn body_bytes(body: &Body) -> Option<&[u8]> {
    if let Some(bytes) = body.bytes.as_deref() {
        return Some(bytes);
    }
    body.preview.as_deref().map(|preview| preview.as_bytes())
}

/// Hex 视图的一行：偏移 + 两组 8 字节 + ASCII 侧栏。
fn hex_dump(bytes: &[u8]) -> (Vec<String>, bool) {
    let mut rows = Vec::new();
    let mut truncated = false;
    for (index, chunk) in bytes.chunks(16).enumerate() {
        if index >= MAX_HEX_ROWS {
            truncated = true;
            break;
        }
        let ascii: String = chunk
            .iter()
            .map(|byte| {
                if byte.is_ascii_graphic() || *byte == b' ' {
                    *byte as char
                } else {
                    '.'
                }
            })
            .collect();
        rows.push(format!(
            "{:08x}  {}  {}  |{ascii}|",
            index * 16,
            hex_group(chunk.get(..8).unwrap_or(chunk)),
            hex_group(chunk.get(8..).unwrap_or(&[])),
        ));
    }
    (rows, truncated)
}

/// Hex 视图里的半行：8 个字节槽，缺的补空格，两组对齐才读得出边界。
fn hex_group(chunk: &[u8]) -> String {
    let mut group = String::new();
    for index in 0..8 {
        match chunk.get(index) {
            Some(byte) => group.push_str(&format!("{byte:02x} ")),
            None => group.push_str("   "),
        }
    }
    group
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
            bytes: None,
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
            vec![
                BodyMode::Pretty,
                BodyMode::Tree,
                BodyMode::Raw,
                BodyMode::Hex
            ]
        );
        let mut truncated = body("application/json", r#"{"a":1"#);
        truncated.truncated = true;
        assert_eq!(
            available_modes(&truncated),
            vec![BodyMode::Raw, BodyMode::Hex],
            "预览被截断时不能假装能格式化成 JSON"
        );
        let text = body("text/plain", "hello");
        assert_eq!(available_modes(&text), vec![BodyMode::Raw, BodyMode::Hex]);
    }

    #[test]
    fn form_bodies_offer_the_key_value_view() {
        let form = body(
            "application/x-www-form-urlencoded; charset=utf-8",
            "user=alice&token=x%20y",
        );
        assert_eq!(
            available_modes(&form),
            vec![BodyMode::Form, BodyMode::Raw, BodyMode::Hex]
        );
        assert_eq!(
            body_pairs(&form).expect("form pairs"),
            vec![
                ("user".to_string(), "alice".to_string()),
                ("token".to_string(), "x y".to_string()),
            ]
        );
    }

    #[test]
    fn binary_bodies_only_offer_hex() {
        let mut binary = body("application/octet-stream", "");
        binary.preview = None;
        binary.binary = true;
        binary.bytes = Some(vec![0x1f, 0x8b, 0x08, 0x00]);
        assert_eq!(
            available_modes(&binary),
            vec![BodyMode::Hex],
            "二进制没有别的看法，只有字节"
        );
        assert_eq!(body_bytes(&binary), Some(&[0x1f_u8, 0x8b, 0x08, 0x00][..]));
    }

    #[test]
    fn hex_dump_lays_out_offsets_bytes_and_ascii() {
        let (rows, truncated) = hex_dump(b"Hi\x00!");
        assert!(!truncated);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert!(row.starts_with("00000000  48 69 00 21"), "{row}");
        assert!(row.ends_with("|Hi.!|"), "{row}");
    }

    #[test]
    fn hex_dump_stops_at_the_row_cap() {
        let bytes = vec![0x41_u8; 16 * (MAX_HEX_ROWS + 8)];
        let (rows, truncated) = hex_dump(&bytes);
        assert_eq!(rows.len(), MAX_HEX_ROWS);
        assert!(truncated, "超出的字节要说明，不能默默少给");
    }

    #[test]
    fn long_urls_break_on_separators_and_keep_the_scheme() {
        let url = format!(
            "https://example.com/api/v1/{}/resource?token=abc&page=1",
            "a".repeat(120)
        );
        let lines = url_lines(&url);
        assert!(lines.len() > 1, "长 URL 要折行，不能被截断");
        let joined: String = lines.concat();
        assert_eq!(joined, url, "折行不能吃掉字符");
        assert!(
            lines[0].starts_with("https://example.com/"),
            "第一行要带完整 scheme，读起来才像一个地址：{}",
            lines[0]
        );
        for line in &lines {
            assert!(line.chars().count() <= URL_LINE_WIDTH + 8);
        }
    }

    #[test]
    fn short_urls_stay_on_one_line() {
        let url = "https://example.com/a?b=1";
        assert_eq!(url_lines(url), vec![url.to_string()]);
    }

    #[test]
    fn status_filter_matches_only_its_own_class() {
        let entry = |status: Option<u16>| Entry {
            method: "GET".to_string(),
            host: "example.com".to_string(),
            path: "/".to_string(),
            query: None,
            url: "https://example.com/".to_string(),
            status,
            note: None,
            mime: "text/plain".to_string(),
            size: None,
            duration_ms: None,
            started: None,
            remote_address: None,
            request_headers: Vec::new(),
            response_headers: Vec::new(),
            request_body: None,
            response_body: None,
            protocol: None,
        };
        let ok = entry(Some(200));
        let moved = entry(Some(302));
        let broken = entry(Some(500));
        let failed = entry(None);

        assert!(StatusFilter::Success.matches(&ok));
        assert!(!StatusFilter::Success.matches(&moved));
        assert!(StatusFilter::Redirect.matches(&moved));
        assert!(StatusFilter::ServerError.matches(&broken));
        assert!(
            !StatusFilter::Success.matches(&failed),
            "没有状态码的条目不属于任何状态类"
        );
        assert!(StatusFilter::Failed.matches(&failed));
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
