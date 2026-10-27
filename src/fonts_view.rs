//! 字体模块：左侧字体集合 + 右侧预览 / 对比。
//!
//! 作为工作台的一个模块存在：标题栏、导航与状态栏由 [`crate::shell`] 负责，
//! 本模块只提供自己的工具栏与内容区。
//!
//! 状态所有权：
//! - `FontListDelegate` 持有字体数据、过滤结果、收藏/最近与选中下标；
//! - `FontsModule` 只保留「当前选中 family + face」的快照，以及预览/对比偏好；
//! - 搜索框、示例文本框是 GPUI 的 `InputState` 实体；
//! - 长期偏好（主题、字号、底色、收藏、最近、窗口）在 `Settings` Global，
//!   落盘走 [`settings::persist`] 写入 SQLite。

use std::collections::BTreeSet;

use gpui_kit::{
    App, AppContext, ClipboardItem, Context, Entity, FocusHandle, Focusable, Font, FontStyle,
    FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, Render, ScrollStrategy,
    StatefulInteractiveElement, Styled, Subscription, Task, Window, actions,
    base::StyledExt,
    component::{
        ActiveTheme, Disableable, Icon, IndexPath, Selectable, Sizable, WindowExt,
        button::{Button, ButtonVariants},
        input::{Input, InputEvent, InputState},
        list::{List, ListDelegate, ListEvent, ListItem, ListState},
        menu::{DropdownMenu, PopupMenuItem},
        notification::NotificationType,
        progress::ProgressCircle,
        resizable::{h_resizable, resizable_panel},
    },
    div,
    prelude::FluentBuilder,
    px, rgb,
};

use crate::{
    fonts::{self, FaceStyle, FontFace, FontFamily},
    icon::AppIcon,
    settings::{self, LATIN_SAMPLE, PreviewBackground, SamplePreset, Settings},
    size_slider::SizeSlider,
    ui,
};

pub const KEY_CONTEXT: &str = "FontManager";

/// 预览里的三级样张字号，与偏好里的「预览字号」互不影响：
/// 72 看字形轮廓，18 看字母表，16 看元数据区的紧凑样张。
const SPECIMEN_GLYPH_SIZE: f32 = 72.0;
const SPECIMEN_ALPHABET_SIZE: f32 = 18.0;
const SPECIMEN_METADATA_SIZE: f32 = 16.0;

actions!(fonts, [CopyFamilyName, FocusSearch, RefreshFonts]);

const PREVIEW_SIZES: [f32; 4] = [32.0, 48.0, 72.0, 96.0];
const MAX_COMPARE: usize = 4;
const RECENT_LIMIT: usize = 12;

#[derive(Debug, Clone)]
enum LoadState {
    Loading,
    Ready,
    Failed(String),
}

impl LoadState {
    fn is_loading(&self) -> bool {
        matches!(self, Self::Loading)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Favorites,
    Recent,
    Monospace,
}

impl Filter {
    const ALL: [Self; 4] = [Self::All, Self::Favorites, Self::Recent, Self::Monospace];

    fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Favorites => "Favorites",
            Self::Recent => "Recent",
            Self::Monospace => "Mono",
        }
    }
}

#[derive(Debug, Clone)]
struct CompareEntry {
    family: FontFamily,
    face_ix: usize,
}

impl CompareEntry {
    fn face(&self) -> Option<&FontFace> {
        self.family
            .faces
            .get(self.face_ix)
            .or_else(|| self.family.faces.first())
    }

    fn key(&self) -> String {
        let face = self.face();
        format!(
            "{}#{}#{}",
            self.family.name,
            face.map(|face| face.weight).unwrap_or_default(),
            face.map(|face| face.label.as_str()).unwrap_or_default()
        )
    }
}

#[derive(Clone, Copy)]
struct PreviewColors {
    bg: Hsla,
    fg: Hsla,
    muted: Hsla,
    border: Hsla,
}

pub struct FontsModule {
    load_state: LoadState,
    search_state: Entity<InputState>,
    sample_state: Entity<InputState>,
    list_state: Entity<ListState<FontListDelegate>>,
    selected: Option<FontFamily>,
    selected_face_ix: usize,
    preview_size: f32,
    preview_background: PreviewBackground,
    sample_preset: SamplePreset,
    filter: Filter,
    favorites: BTreeSet<String>,
    recent: Vec<String>,
    compare: Vec<CompareEntry>,
    compare_open: bool,
    slider_focus: FocusHandle,
    load_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl FontsModule {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = cx.global::<Settings>().clone();
        let favorites: BTreeSet<String> = settings.favorites.iter().cloned().collect();
        let recent = settings.recent.clone();
        let preview_size = settings
            .preview_size
            .clamp(PREVIEW_SIZES[0], PREVIEW_SIZES[PREVIEW_SIZES.len() - 1]);
        let preview_background = settings.preview_background;
        let sample_preset = settings.sample_preset;
        let sample_text = if settings.sample_text.trim().is_empty() {
            LATIN_SAMPLE.to_string()
        } else {
            settings.sample_text.clone()
        };

        let search_state = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Search fonts…")
                .clean_on_escape()
        });
        let sample_state = cx.new(|cx| InputState::new(window, cx).default_value(sample_text));
        let list_state = cx.new(|cx| {
            let mut delegate = FontListDelegate::new();
            delegate.set_filter_state(Filter::All, favorites.clone(), recent.clone());
            ListState::new(delegate, window, cx)
        });
        let slider_focus = cx.focus_handle().tab_stop(true);

        // 搜索框变化 → 过滤列表；Enter 把焦点交给列表。
        let search_sub = cx.subscribe_in(
            &search_state,
            window,
            |this, state, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    let query = state.read(cx).value().to_string();
                    this.apply_query(query, window, cx);
                }
                InputEvent::PressEnter { .. } => {
                    this.list_state
                        .update(cx, |list, cx| list.focus(window, cx));
                }
                _ => {}
            },
        );

        // 鼠标点击发 Confirm，键盘 ↑↓ 发 Select；两种都同步预览。
        let list_sub = cx.subscribe(&list_state, |this, _, event: &ListEvent, cx| {
            if matches!(
                event,
                ListEvent::Select(_) | ListEvent::Confirm(_) | ListEvent::Cancel
            ) {
                this.sync_selected(cx);
            }
        });

        // 示例文本：输入变化即时预览；失焦时落盘。
        let sample_sub =
            cx.subscribe(
                &sample_state,
                |this, state, event: &InputEvent, cx| match event {
                    InputEvent::Change => {
                        let text = state.read(cx).value().to_string();
                        let preset = SamplePreset::PRESETS
                            .iter()
                            .copied()
                            .find(|preset| preset.text() == text)
                            .unwrap_or(SamplePreset::Custom);
                        this.sample_preset = preset;
                        let settings = cx.global_mut::<Settings>();
                        settings.sample_text = text;
                        settings.sample_preset = preset;
                        cx.notify();
                    }
                    InputEvent::Blur => settings::persist(cx),
                    _ => {}
                },
            );

        let this = Self {
            load_state: LoadState::Loading,
            search_state,
            sample_state,
            list_state,
            selected: None,
            selected_face_ix: 0,
            preview_size,
            preview_background,
            sample_preset,
            filter: Filter::All,
            favorites,
            recent,
            compare: Vec::new(),
            compare_open: false,
            slider_focus,
            load_task: Task::ready(()),
            _subscriptions: vec![search_sub, list_sub, sample_sub],
        };

        // 首帧：焦点给列表，开始扫描字体。
        cx.on_next_frame(window, |this, window, cx| {
            this.list_state
                .update(cx, |list, cx| list.focus(window, cx));
            this.load_fonts(window, cx);
        });

        this
    }

    fn load_fonts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.load_state = LoadState::Loading;
        self.list_state.update(cx, |list, cx| {
            list.delegate_mut().set_loading(true);
            cx.notify();
        });
        cx.notify();

        self.load_task = cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { fonts::scan_system_fonts() })
                .await;
            this.update_in(cx, |this, window, cx| this.apply_fonts(result, window, cx))
                .ok();
        });
    }

    fn apply_fonts(
        &mut self,
        result: Result<Vec<FontFamily>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(families) => {
                let last_selected = cx.global::<Settings>().last_selected.clone();
                self.list_state.update(cx, |list, cx| {
                    list.delegate_mut().set_fonts(families);

                    let index = last_selected
                        .as_deref()
                        .and_then(|name| list.delegate().filtered_index_of(name))
                        .map(IndexPath::new)
                        .unwrap_or_default();
                    let count = list.delegate().items_count(0, cx);
                    if count > 0 {
                        list.set_selected_index(Some(index), window, cx);
                        list.scroll_to_item(index, ScrollStrategy::Top, window, cx);
                    } else {
                        list.set_selected_index(None, window, cx);
                    }
                    cx.notify();
                });
                self.load_state = LoadState::Ready;
                self.sync_selected(cx);
            }
            Err(message) => {
                self.list_state.update(cx, |list, cx| {
                    list.delegate_mut().set_loading(false);
                    list.set_selected_index(None, window, cx);
                    cx.notify();
                });
                self.selected = None;
                self.selected_face_ix = 0;
                self.load_state = LoadState::Failed(message);
            }
        }

        cx.notify();
    }

    fn apply_query(&mut self, query: String, window: &mut Window, cx: &mut Context<Self>) {
        self.list_state.update(cx, |list, cx| {
            list.delegate_mut().set_query(&query);

            let count = list.delegate().items_count(0, cx);
            if count > 0 {
                list.set_selected_index(Some(IndexPath::default()), window, cx);
                list.scroll_to_item(IndexPath::default(), ScrollStrategy::Top, window, cx);
            } else {
                list.set_selected_index(None, window, cx);
            }
            cx.notify();
        });

        self.sync_selected(cx);
    }

    fn set_filter(&mut self, filter: Filter, window: &mut Window, cx: &mut Context<Self>) {
        if self.filter == filter {
            return;
        }
        self.filter = filter;
        self.apply_filter(window, cx);
    }

    fn apply_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let favorites = self.favorites.clone();
        let recent = self.recent.clone();
        let filter = self.filter;

        self.list_state.update(cx, |list, cx| {
            list.delegate_mut()
                .set_filter_state(filter, favorites, recent);
            let count = list.delegate().items_count(0, cx);
            if count > 0 {
                list.set_selected_index(Some(IndexPath::default()), window, cx);
                list.scroll_to_item(IndexPath::default(), ScrollStrategy::Top, window, cx);
            } else {
                list.set_selected_index(None, window, cx);
            }
            cx.notify();
        });

        self.sync_selected(cx);
    }

    /// 列表选择变化后，把选中 family 的克隆快照同步到预览状态。
    fn sync_selected(&mut self, cx: &mut Context<Self>) {
        let next = self
            .list_state
            .read(cx)
            .delegate()
            .selected_family()
            .cloned();

        let previous_name = self.selected.as_ref().map(|family| family.name.clone());
        let changed = match (&self.selected, &next) {
            (None, None) => false,
            (Some(a), Some(b)) => a.name != b.name || a.faces.len() != b.faces.len(),
            _ => true,
        };

        if changed {
            self.selected = next;
            if let Some(font) = &self.selected {
                self.selected_face_ix = font.default_face_index();
            } else {
                self.selected_face_ix = 0;
            }
            let selected_name = self.selected.as_ref().map(|font| font.name.clone());
            cx.global_mut::<Settings>().last_selected = selected_name.clone();
            let recent_name =
                selected_name.filter(|name| previous_name.as_deref() != Some(name.as_str()));
            if let Some(name) = recent_name {
                self.remember_recent(&name, cx);
            }
            cx.notify();
        }
    }

    fn remember_recent(&mut self, name: &str, cx: &mut Context<Self>) {
        self.recent.retain(|recent| recent != name);
        self.recent.insert(0, name.to_string());
        self.recent.truncate(RECENT_LIMIT);
        cx.global_mut::<Settings>().recent = self.recent.clone();
    }

    fn current_entry(&self) -> Option<CompareEntry> {
        let family = self.selected.clone()?;
        let face_ix = self
            .selected_face_ix
            .min(family.faces.len().saturating_sub(1));
        family.faces.get(face_ix)?;
        Some(CompareEntry { family, face_ix })
    }

    fn is_in_compare(&self, entry: &CompareEntry) -> bool {
        let key = entry.key();
        self.compare.iter().any(|item| item.key() == key)
    }

    fn add_current_to_compare(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.current_entry() else {
            return;
        };

        if self.is_in_compare(&entry) {
            window.push_notification((NotificationType::Info, "Already in comparison"), cx);
            return;
        }
        if self.compare.len() >= MAX_COMPARE {
            window.push_notification(
                (
                    NotificationType::Warning,
                    format!("Compare up to {MAX_COMPARE} fonts"),
                ),
                cx,
            );
            return;
        }

        self.compare.push(entry);
        self.compare_open = true;
        cx.notify();
    }

    fn remove_compare_entry(&mut self, key: &str, cx: &mut Context<Self>) {
        self.compare.retain(|entry| entry.key() != key);
        if self.compare.is_empty() {
            self.compare_open = false;
        }
        cx.notify();
    }

    fn toggle_compare(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.compare.is_empty() {
            window.push_notification(
                (
                    NotificationType::Info,
                    "Open a font and choose Compare to add it",
                ),
                cx,
            );
            return;
        }
        self.compare_open = !self.compare_open;
        cx.notify();
    }

    fn toggle_favorite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(font) = self.selected.clone() else {
            return;
        };

        if !self.favorites.remove(&font.name) {
            self.favorites.insert(font.name.clone());
        }

        let favorites = self.favorites.clone();
        {
            let settings = cx.global_mut::<Settings>();
            settings.favorites = favorites.iter().cloned().collect();
        }
        settings::persist(cx);

        if self.filter == Filter::Favorites {
            self.apply_filter(window, cx);
        } else {
            self.list_state.update(cx, |list, cx| {
                list.delegate_mut().set_favorites(favorites);
                cx.notify();
            });
            cx.notify();
        }
    }

    fn copy_selected_family(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(font) = self.selected.clone() else {
            return;
        };
        self.copy_value(font.name, "Copied family name", window, cx);
    }

    fn copy_selected_family_style(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((font, face, _)) = self.selected_font_face() else {
            return;
        };
        let text = if face.label == "Regular" {
            font.name
        } else {
            format!("{} {}", font.name, face.label)
        };
        self.copy_value(text, "Copied family + style", window, cx);
    }

    fn copy_selected_post_script(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((font, face, _)) = self.selected_font_face() else {
            return;
        };
        let text = if face.post_script_name.is_empty() {
            font.name
        } else {
            face.post_script_name
        };
        self.copy_value(text, "Copied PostScript name", window, cx);
    }

    fn copy_selected_css(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((font, face, _)) = self.selected_font_face() else {
            return;
        };
        let fallback = if font.monospaced {
            "monospace"
        } else {
            "sans-serif"
        };
        let style = match face.style {
            FaceStyle::Normal => "normal",
            FaceStyle::Italic => "italic",
            FaceStyle::Oblique => "oblique",
        };
        let text = format!(
            "font-family: \"{}\", {};\nfont-weight: {};\nfont-style: {};",
            font.name, fallback, face.weight, style
        );
        self.copy_value(text, "Copied CSS", window, cx);
    }

    fn copy_compare_entry(
        &mut self,
        entry: &CompareEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(face) = entry.face() else {
            return;
        };
        let text = if face.label == "Regular" {
            entry.family.name.clone()
        } else {
            format!("{} {}", entry.family.name, face.label)
        };
        self.copy_value(text, "Copied family + style", window, cx);
    }

    fn copy_value(
        &mut self,
        text: String,
        message: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        window.push_notification((NotificationType::Success, message), cx);
    }

    fn selected_font_face(&self) -> Option<(FontFamily, FontFace, usize)> {
        let font = self.selected.clone()?;
        let face_ix = self
            .selected_face_ix
            .min(font.faces.len().saturating_sub(1));
        let face = font.faces.get(face_ix)?.clone();
        Some((font, face, face_ix))
    }

    fn face_font(family: &str, face: &FontFace) -> Font {
        Font {
            family: family.to_string().into(),
            weight: FontWeight(face.weight as f32),
            style: match face.style {
                FaceStyle::Normal => FontStyle::Normal,
                FaceStyle::Italic => FontStyle::Italic,
                FaceStyle::Oblique => FontStyle::Oblique,
            },
            ..Default::default()
        }
    }

    fn preview_colors(&self, cx: &Context<Self>) -> PreviewColors {
        match self.preview_background {
            PreviewBackground::Theme => PreviewColors {
                bg: cx.theme().background,
                fg: cx.theme().foreground,
                muted: cx.theme().muted_foreground,
                border: cx.theme().border,
            },
            PreviewBackground::Light => PreviewColors {
                bg: Hsla::from(rgb(0xFFFFFF)),
                fg: Hsla::from(rgb(0x111111)),
                muted: Hsla::from(rgb(0x666666)),
                border: Hsla::from(rgb(0xE2E2E2)),
            },
            PreviewBackground::Dark => PreviewColors {
                bg: Hsla::from(rgb(0x111111)),
                fg: Hsla::from(rgb(0xF2F2F2)),
                muted: Hsla::from(rgb(0xA0A0A0)),
                border: Hsla::from(rgb(0x333333)),
            },
        }
    }

    fn sample_text(&self, cx: &Context<Self>) -> String {
        let text = self.sample_state.read(cx).value().to_string();
        if text.trim().is_empty() {
            "Type a sample above…".to_string()
        } else {
            text
        }
    }

    fn set_preview_background(&mut self, background: PreviewBackground, cx: &mut Context<Self>) {
        self.preview_background = background;
        cx.global_mut::<Settings>().preview_background = background;
        settings::persist(cx);
        cx.notify();
    }

    fn apply_sample_preset(
        &mut self,
        preset: SamplePreset,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = preset.text().to_string();
        self.sample_state.update(cx, |state, cx| {
            state.set_value(text.clone(), window, cx);
        });
        self.sample_preset = preset;
        {
            let settings = cx.global_mut::<Settings>();
            settings.sample_preset = preset;
            settings.sample_text = text;
        }
        settings::persist(cx);
        cx.notify();
    }

    fn on_copy_family_name(
        &mut self,
        _: &CopyFamilyName,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 文本输入里的 Ctrl/Cmd+C 应该复制选中的文字，而不是字体名。
        let text_input_focused = self
            .search_state
            .read(cx)
            .focus_handle(cx)
            .is_focused(window)
            || self
                .sample_state
                .read(cx)
                .focus_handle(cx)
                .is_focused(window);

        if !text_input_focused {
            self.copy_selected_family(window, cx);
        }
    }

    fn on_focus_search(&mut self, _: &FocusSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.search_state
            .update(cx, |state, cx| state.focus(window, cx));
    }

    fn on_refresh_fonts(&mut self, _: &RefreshFonts, window: &mut Window, cx: &mut Context<Self>) {
        if !self.load_state.is_loading() {
            self.load_fonts(window, cx);
        }
    }

    fn counts(&self, cx: &Context<Self>) -> (usize, usize) {
        let delegate = self.list_state.read(cx).delegate();
        (delegate.matched_count(), delegate.total_count())
    }

    /// 模块工具栏：字体计数、对比开关与重新扫描。窗口装饰和主题开关在工作台的
    /// 标题栏上，这里只放字体自己的命令。
    fn render_toolbar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let (matched, total) = self.counts(cx);
        let count_label = match &self.load_state {
            LoadState::Loading => "Reading installed fonts…".to_string(),
            LoadState::Failed(_) => "Font scan failed".to_string(),
            LoadState::Ready if matched == total => format!("{total} families"),
            LoadState::Ready => format!("{matched} of {total}"),
        };
        let is_loading = self.load_state.is_loading();
        let compare_label = if self.compare.is_empty() {
            "Compare".to_string()
        } else {
            format!("Compare {}", self.compare.len())
        };

        div()
            .px_4()
            .pt_3()
            .pb_2()
            .h_flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Fonts"),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(count_label),
            )
            .child(div().flex_1())
            .child(
                Button::new("compare-toggle")
                    .ghost()
                    .small()
                    .icon(AppIcon::Compare)
                    .label(compare_label)
                    .selected(self.compare_open)
                    .disabled(self.compare.is_empty())
                    .tooltip("Compare pinned fonts")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_compare(window, cx);
                    })),
            )
            .child(
                Button::new("refresh-fonts")
                    .ghost()
                    .small()
                    .icon(AppIcon::Refresh)
                    .label("Rescan")
                    .loading(is_loading)
                    .disabled(is_loading)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.load_fonts(window, cx);
                    })),
            )
    }

    /// 状态栏文本，由工作台统一绘制。
    pub fn status_text(&self, cx: &App) -> String {
        let delegate = self.list_state.read(cx).delegate();
        let (matched, total) = (delegate.matched_count(), delegate.total_count());
        let faces = delegate.total_faces();
        let mut left = match &self.load_state {
            LoadState::Loading => "Reading installed fonts…".to_string(),
            LoadState::Failed(_) => "Font scan failed".to_string(),
            LoadState::Ready if matched == total => {
                format!("{total} families · {faces} faces")
            }
            LoadState::Ready => format!("{matched} of {total} families"),
        };
        if !self.compare.is_empty() {
            left.push_str(&format!(" · {} in compare", self.compare.len()));
        }
        left
    }

    fn render_sidebar(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (matched, total) = self.counts(cx);
        let count = match &self.load_state {
            LoadState::Loading => "—".to_string(),
            LoadState::Failed(_) => "—".to_string(),
            LoadState::Ready => format!("{matched} / {total}"),
        };

        div()
            .size_full()
            .min_h_0()
            .v_flex()
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                div().p_3().child(
                    Input::new(&self.search_state)
                        .w_full()
                        .cleanable(true)
                        .prefix(Icon::new(AppIcon::Search).small()),
                ),
            )
            .child(
                div()
                    .px_3()
                    .pb_2()
                    .h_flex()
                    .items_center()
                    .gap_1()
                    .children(Filter::ALL.map(|filter| {
                        Button::new(format!("filter-{}", filter.label()))
                            .ghost()
                            .small()
                            .selected(self.filter == filter)
                            .label(filter.label())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.set_filter(filter, window, cx);
                            }))
                    })),
            )
            .child(
                div()
                    .px_3()
                    .pb_2()
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(cx.theme().muted_foreground)
                            .child("INSTALLED"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(count),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(List::new(&self.list_state).size_full().py_1()),
            )
    }

    fn render_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.compare_open && !self.compare.is_empty() {
            return div()
                .size_full()
                .min_h_0()
                .child(self.render_compare(window, cx))
                .into_any_element();
        }

        let state = self.load_state.clone();
        div()
            .size_full()
            .min_h_0()
            .child(match state {
                LoadState::Loading => self.render_loading(cx).into_any_element(),
                LoadState::Failed(message) => self.render_error(message, cx).into_any_element(),
                LoadState::Ready => match self.selected.clone() {
                    Some(font) => self
                        .render_detail(font, self.selected_face_ix, window, cx)
                        .into_any_element(),
                    None => self.render_empty_preview(cx).into_any_element(),
                },
            })
            .into_any_element()
    }

    fn render_loading(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .v_flex()
            .items_center()
            .justify_center()
            .gap_3()
            .child(ProgressCircle::new("fonts-loading").loading(true))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("Reading installed fonts…"),
            )
    }

    fn render_error(&mut self, message: String, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
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
                    .child("Couldn’t read installed fonts"),
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
                Button::new("retry-fonts")
                    .label("Try again")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.load_fonts(window, cx);
                    })),
            )
    }

    fn render_empty_preview(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (matched, total) = self.counts(cx);
        let (title, detail) = if total > 0 && matched == 0 {
            (
                "No matching fonts",
                "Clear the search or switch the filter to browse installed fonts.",
            )
        } else {
            (
                "Select a font",
                "Pick a family from the list to preview it.",
            )
        };

        div()
            .size_full()
            .v_flex()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                Icon::new(AppIcon::TypeScale)
                    .size_12()
                    .text_color(cx.theme().muted_foreground.opacity(0.6)),
            )
            .child(div().text_lg().child(title))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(detail),
            )
    }

    fn render_detail(
        &mut self,
        font: FontFamily,
        face_ix: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let face_ix = face_ix.min(font.faces.len().saturating_sub(1));
        let Some(face) = font.faces.get(face_ix).cloned() else {
            return self.render_empty_preview(cx).into_any_element();
        };
        let is_favorite = self.favorites.contains(&font.name);
        let current_entry = self.current_entry();
        let in_compare = current_entry
            .as_ref()
            .is_some_and(|entry| self.is_in_compare(entry));
        let compare_full = self.compare.len() >= MAX_COMPARE;

        let header = div()
            .px_6()
            .py_3()
            .v_flex()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .w_full()
                    .h_flex()
                    .items_start()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_2xl()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(font.name.clone()),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .truncate()
                                    .child(format!("{} · {}", font.metadata_line(), face.label)),
                            ),
                    )
                    .child(
                        ui::icon_button(
                            "favorite-font",
                            if is_favorite {
                                AppIcon::FavoriteOn
                            } else {
                                AppIcon::Favorite
                            },
                            cx,
                        )
                        .tooltip(if is_favorite {
                            "Remove from favorites"
                        } else {
                            "Add to favorites"
                        })
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.toggle_favorite(window, cx);
                        })),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("add-compare")
                            .ghost()
                            .small()
                            .icon(AppIcon::AddToCompare)
                            .label(if in_compare { "In compare" } else { "Compare" })
                            .disabled(in_compare || compare_full)
                            .tooltip(if compare_full {
                                "Compare up to 4 fonts"
                            } else {
                                "Add this face to comparison"
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_current_to_compare(window, cx);
                            })),
                    )
                    .child(
                        Button::new("copy-as")
                            .outline()
                            .small()
                            .label("Copy as")
                            .dropdown_caret(true)
                            .dropdown_menu({
                                let weak = cx.weak_entity();
                                move |menu, _, _| {
                                    let family = weak.clone();
                                    let family_style = weak.clone();
                                    let post_script = weak.clone();
                                    let css = weak.clone();
                                    menu.item(PopupMenuItem::new("Family name").on_click(
                                        move |_, window, cx| {
                                            family
                                                .update(cx, |this, cx| {
                                                    this.copy_selected_family(window, cx)
                                                })
                                                .ok();
                                        },
                                    ))
                                    .item(PopupMenuItem::new("Family + style").on_click(
                                        move |_, window, cx| {
                                            family_style
                                                .update(cx, |this, cx| {
                                                    this.copy_selected_family_style(window, cx)
                                                })
                                                .ok();
                                        },
                                    ))
                                    .item(PopupMenuItem::new("PostScript name").on_click(
                                        move |_, window, cx| {
                                            post_script
                                                .update(cx, |this, cx| {
                                                    this.copy_selected_post_script(window, cx)
                                                })
                                                .ok();
                                        },
                                    ))
                                    .separator()
                                    .item(
                                        PopupMenuItem::new("CSS").on_click(move |_, window, cx| {
                                            css.update(cx, |this, cx| {
                                                this.copy_selected_css(window, cx)
                                            })
                                            .ok();
                                        }),
                                    )
                                }
                            }),
                    )
                    .child(
                        Button::new("copy-font-name")
                            .primary()
                            .small()
                            .icon(AppIcon::Copy)
                            .label("Copy name")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.copy_selected_family(window, cx);
                            })),
                    ),
            );

        let style_row = div()
            .h_flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .w_20()
                    .flex_shrink_0()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("Style"),
            )
            .child(self.render_face_picker(font.clone(), face_ix, cx));

        let options = div()
            .h_flex()
            .items_center()
            .justify_end()
            .gap_2()
            .child(self.render_sample_presets_button(cx))
            .child(self.render_surface_button(cx));

        let controls = div()
            .px_6()
            .py_3()
            .v_flex()
            .gap_3()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(options)
            .child(style_row)
            .child(self.render_sample_row(cx))
            .child(self.render_size_row(cx));

        let scroll = div()
            .id("font-preview-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(
                div()
                    .p_6()
                    .v_flex()
                    .gap_6()
                    .child(self.render_preview_card(&font, &face, cx))
                    .child(self.render_metadata(&font, &face, cx)),
            );

        div()
            .size_full()
            .min_w_0()
            .min_h_0()
            .v_flex()
            .child(header)
            .child(controls)
            .child(scroll)
            .into_any_element()
    }

    fn render_face_picker(
        &mut self,
        font: FontFamily,
        current_ix: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let current_label = font
            .faces
            .get(current_ix)
            .map(|face| face.label.clone())
            .unwrap_or_else(|| "Regular".to_string());

        Button::new("face-picker")
            .ghost()
            .small()
            .label(current_label)
            .dropdown_caret(true)
            .dropdown_menu({
                let weak = cx.weak_entity();
                let faces = font.faces.clone();
                move |menu, _, _| {
                    let mut menu = menu;
                    for (ix, face) in faces.iter().enumerate() {
                        let weak = weak.clone();
                        menu = menu.item(
                            PopupMenuItem::new(face.label.clone())
                                .checked(ix == current_ix)
                                .on_click(move |_, _, cx| {
                                    weak.update(cx, |this, cx| {
                                        this.selected_face_ix = ix;
                                        cx.notify();
                                    })
                                    .ok();
                                }),
                        );
                    }
                    menu
                }
            })
    }

    fn render_sample_row(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .h_flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .w_20()
                    .flex_shrink_0()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("Sample"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&self.sample_state).w_full().cleanable(true)),
            )
    }

    fn render_sample_presets_button(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        Button::new("sample-presets")
            .ghost()
            .small()
            .icon(AppIcon::Samples)
            .label("Samples")
            .dropdown_menu({
                let weak = cx.weak_entity();
                let current = self.sample_preset;
                move |menu, _, _| {
                    let mut menu = menu;
                    for preset in SamplePreset::PRESETS {
                        let weak = weak.clone();
                        menu = menu.item(
                            PopupMenuItem::new(preset.label())
                                .checked(preset == current)
                                .on_click(move |_, window, cx| {
                                    weak.update(cx, |this, cx| {
                                        this.apply_sample_preset(preset, window, cx)
                                    })
                                    .ok();
                                }),
                        );
                    }
                    menu
                }
            })
    }

    fn render_surface_button(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        Button::new("preview-surface")
            .ghost()
            .small()
            .icon(AppIcon::Surface)
            .label(self.preview_background.label())
            .dropdown_menu({
                let weak = cx.weak_entity();
                let current = self.preview_background;
                move |menu, _, _| {
                    let mut menu = menu;
                    for background in [
                        PreviewBackground::Theme,
                        PreviewBackground::Light,
                        PreviewBackground::Dark,
                    ] {
                        let weak = weak.clone();
                        menu = menu.item(
                            PopupMenuItem::new(background.label())
                                .checked(background == current)
                                .on_click(move |_, _, cx| {
                                    weak.update(cx, |this, cx| {
                                        this.set_preview_background(background, cx)
                                    })
                                    .ok();
                                }),
                        );
                    }
                    menu
                }
            })
    }

    fn render_size_row(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let change_target = cx.weak_entity();
        let commit_target = cx.weak_entity();

        let slider = SizeSlider::new(
            "preview-size-slider",
            self.preview_size,
            PREVIEW_SIZES.to_vec(),
            self.slider_focus.clone(),
        )
        .on_change(move |value, _window, cx| {
            let _ = change_target.update(cx, |this, cx| {
                this.preview_size = value;
                cx.notify();
            });
        })
        .on_commit(move |value, _window, cx| {
            let _ = commit_target.update(cx, |this, cx| {
                this.preview_size = value;
                cx.global_mut::<Settings>().preview_size = value;
                settings::persist(cx);
                cx.notify();
            });
        });

        div()
            .h_flex()
            .items_start()
            .gap_3()
            .child(
                div()
                    .w_20()
                    .flex_shrink_0()
                    .pt_1()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("Size (px)"),
            )
            .child(div().flex_1().min_w_0().child(slider))
            .child(
                div()
                    .w_12()
                    .flex_shrink_0()
                    .pt_1()
                    .text_right()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("{:.0}px", self.preview_size)),
            )
    }

    fn render_preview_card(
        &self,
        font: &FontFamily,
        face: &FontFace,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let sample = self.sample_text(cx);
        let colors = self.preview_colors(cx);
        let face_font = Self::face_font(&font.name, face);

        div()
            .w_full()
            .rounded_lg()
            .border_1()
            .border_color(colors.border)
            .bg(colors.bg)
            .p_5()
            .v_flex()
            .gap_3()
            .min_w_0()
            .child(
                div()
                    .font(face_font.clone())
                    .text_size(px(SPECIMEN_GLYPH_SIZE))
                    .text_color(colors.fg)
                    .child("Aa"),
            )
            .child(
                div()
                    .font(face_font.clone())
                    .text_size(px(self.preview_size))
                    .text_color(colors.fg)
                    .child(sample),
            )
            .child(
                div()
                    .font(face_font)
                    .text_size(px(SPECIMEN_ALPHABET_SIZE))
                    .text_color(colors.muted)
                    .child("ABCDEFGHIJKLMNOPQRSTUVWXYZ abcdefghijklmnopqrstuvwxyz 0123456789"),
            )
    }

    fn render_metadata(
        &self,
        font: &FontFamily,
        face: &FontFace,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let styles = if font.faces.is_empty() {
            "—".to_string()
        } else {
            font.faces
                .iter()
                .map(|face| face.label.as_str())
                .collect::<Vec<_>>()
                .join(" · ")
        };

        div()
            .v_flex()
            .gap_3()
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(cx.theme().muted_foreground)
                    .child("DETAILS"),
            )
            .child(self.render_meta_row("Face", face.label.clone(), false, cx))
            .child(self.render_meta_row("Styles", styles, true, cx))
            .child(self.render_meta_row("File", font.file_path_label(), false, cx))
            .child(self.render_meta_row(
                "PostScript",
                if face.post_script_name.is_empty() {
                    font.post_script_name
                        .clone()
                        .unwrap_or_else(|| "—".to_string())
                } else {
                    face.post_script_name.clone()
                },
                false,
                cx,
            ))
    }

    fn render_meta_row(
        &self,
        label: &'static str,
        value: String,
        wrap: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .h_flex()
            .items_start()
            .gap_4()
            .child(
                div()
                    .w_24()
                    .flex_shrink_0()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(label),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .when(!wrap, |this| this.truncate())
                    .child(value),
            )
    }

    fn render_compare(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = self.compare.clone();
        let can_add = self
            .current_entry()
            .is_some_and(|entry| !self.is_in_compare(&entry) && self.compare.len() < MAX_COMPARE);

        let header = div()
            .px_6()
            .py_3()
            .h_flex()
            .items_center()
            .gap_3()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Compare"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("{} of {}", entries.len(), MAX_COMPARE)),
            )
            .child(div().flex_1())
            .child(
                Button::new("compare-add-selected")
                    .ghost()
                    .small()
                    .icon(AppIcon::AddToCompare)
                    .label("Add selected")
                    .disabled(!can_add)
                    .tooltip(if can_add {
                        "Add the currently selected face"
                    } else {
                        "Select another face, or remove one to make room"
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.add_current_to_compare(window, cx);
                    })),
            )
            .child(
                Button::new("compare-done")
                    .small()
                    .label("Done")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.compare_open = false;
                        cx.notify();
                    })),
            );

        let options = div()
            .h_flex()
            .items_center()
            .justify_end()
            .gap_2()
            .child(self.render_sample_presets_button(cx))
            .child(self.render_surface_button(cx));

        let controls = div()
            .px_6()
            .py_3()
            .v_flex()
            .gap_3()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(options)
            .child(self.render_sample_row(cx))
            .child(self.render_size_row(cx));

        let body = if entries.is_empty() {
            div()
                .size_full()
                .v_flex()
                .items_center()
                .justify_center()
                .gap_3()
                .child(
                    Icon::new(AppIcon::Compare)
                        .size_10()
                        .text_color(cx.theme().muted_foreground.opacity(0.6)),
                )
                .child(div().text_lg().child("Nothing to compare"))
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("Open a font and choose Compare to add it."),
                )
                .into_any_element()
        } else {
            let mut rows: Vec<gpui_kit::AnyElement> = Vec::new();
            for chunk in entries.chunks(2) {
                let mut columns: Vec<gpui_kit::AnyElement> = Vec::new();
                for entry in chunk {
                    columns.push(self.render_compare_column(entry, cx).into_any_element());
                }
                rows.push(
                    div()
                        .h_flex()
                        .items_stretch()
                        .gap_4()
                        .children(columns)
                        .into_any_element(),
                );
            }

            div()
                .id("compare-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(div().p_6().v_flex().gap_4().children(rows))
                .into_any_element()
        };

        div()
            .size_full()
            .min_h_0()
            .v_flex()
            .child(header)
            .child(controls)
            .child(body)
    }

    fn render_compare_column(
        &mut self,
        entry: &CompareEntry,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(face) = entry.face().cloned() else {
            return div().into_any_element();
        };
        let colors = self.preview_colors(cx);
        let face_font = Self::face_font(&entry.family.name, &face);
        let sample = self.sample_text(cx);
        let key = entry.key();
        let copy_key = key.clone();
        let remove_key = key.clone();
        let copy_entry = entry.clone();
        let copy_id = format!("compare-copy-{copy_key}");
        let remove_id = format!("compare-remove-{remove_key}");

        div()
            .flex_1()
            .min_w_0()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .p_4()
            .v_flex()
            .gap_3()
            .child(
                div()
                    .h_flex()
                    .items_start()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(entry.family.name.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(face.label.clone()),
                            ),
                    )
                    .child(
                        ui::row_icon_button(copy_id, AppIcon::Copy, cx)
                            .tooltip("Copy name")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.copy_compare_entry(&copy_entry, window, cx);
                            })),
                    )
                    .child(
                        ui::row_icon_button(remove_id, AppIcon::Remove, cx)
                            .tooltip("Remove from comparison")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.remove_compare_entry(&remove_key, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .rounded_md()
                    .bg(colors.bg)
                    .border_1()
                    .border_color(colors.border)
                    .p_4()
                    .v_flex()
                    .gap_3()
                    .min_w_0()
                    .child(
                        div()
                            .font(face_font.clone())
                            .text_size(px(self.preview_size))
                            .text_color(colors.fg)
                            .child(sample),
                    )
                    .child(
                        div()
                            .font(face_font)
                            .text_size(px(SPECIMEN_METADATA_SIZE))
                            .text_color(colors.muted)
                            .child(
                                "ABCDEFGHIJKLMNOPQRSTUVWXYZ abcdefghijklmnopqrstuvwxyz 0123456789",
                            ),
                    ),
            )
            .into_any_element()
    }

    /// 状态栏右侧的键盘提示，由工作台统一绘制。
    pub fn status_hint() -> String {
        format!(
            "↑↓ browse · {} search · {} copy",
            search_shortcut_hint(),
            copy_shortcut_hint()
        )
    }
}

impl Render for FontsModule {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rem = window.rem_size();

        div()
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(Self::on_copy_family_name))
            .on_action(cx.listener(Self::on_focus_search))
            .on_action(cx.listener(Self::on_refresh_fonts))
            .size_full()
            .min_w_0()
            .v_flex()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_toolbar(cx))
            .child(
                div().flex_1().min_h_0().child(
                    h_resizable("fonts-split")
                        .child(
                            resizable_panel()
                                .size(rem * 20.0)
                                .size_range((rem * 16.0)..(rem * 30.0))
                                .child(self.render_sidebar(window, cx)),
                        )
                        .child(
                            resizable_panel()
                                .min_w_0()
                                .child(self.render_preview(window, cx)),
                        ),
                ),
            )
    }
}

impl Focusable for FontsModule {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.list_state.read(cx).focus_handle(cx)
    }
}

struct FontListDelegate {
    families: Vec<FontFamily>,
    filtered: Vec<usize>,
    query: String,
    filter: Filter,
    favorites: BTreeSet<String>,
    recent: Vec<String>,
    selected_index: Option<IndexPath>,
    loading: bool,
}

impl FontListDelegate {
    fn new() -> Self {
        Self {
            families: Vec::new(),
            filtered: Vec::new(),
            query: String::new(),
            filter: Filter::All,
            favorites: BTreeSet::new(),
            recent: Vec::new(),
            selected_index: None,
            loading: true,
        }
    }

    fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
    }

    fn set_fonts(&mut self, families: Vec<FontFamily>) {
        self.families = families;
        self.loading = false;
        self.recompute();
    }

    fn set_query(&mut self, query: &str) {
        self.query = query.trim().to_lowercase();
        self.recompute();
    }

    fn set_filter_state(
        &mut self,
        filter: Filter,
        favorites: BTreeSet<String>,
        recent: Vec<String>,
    ) {
        self.filter = filter;
        self.favorites = favorites;
        self.recent = recent;
        self.recompute();
    }

    fn set_favorites(&mut self, favorites: BTreeSet<String>) {
        self.favorites = favorites;
    }

    fn recompute(&mut self) {
        let base: Vec<usize> = match self.filter {
            Filter::All => (0..self.families.len()).collect(),
            Filter::Favorites => self
                .families
                .iter()
                .enumerate()
                .filter(|(_, family)| self.favorites.contains(&family.name))
                .map(|(ix, _)| ix)
                .collect(),
            Filter::Recent => self
                .recent
                .iter()
                .filter_map(|name| self.families.iter().position(|family| &family.name == name))
                .collect(),
            Filter::Monospace => self
                .families
                .iter()
                .enumerate()
                .filter(|(_, family)| family.monospaced)
                .map(|(ix, _)| ix)
                .collect(),
        };

        self.filtered = base
            .into_iter()
            .filter(|&ix| {
                self.query.is_empty() || self.families[ix].name.to_lowercase().contains(&self.query)
            })
            .collect();
    }

    fn total_count(&self) -> usize {
        self.families.len()
    }

    fn matched_count(&self) -> usize {
        self.filtered.len()
    }

    fn total_faces(&self) -> usize {
        self.families.iter().map(|family| family.faces.len()).sum()
    }

    fn filtered_index_of(&self, name: &str) -> Option<usize> {
        self.filtered.iter().position(|&row| {
            self.families
                .get(row)
                .is_some_and(|family| family.name == name)
        })
    }

    fn selected_family(&self) -> Option<&FontFamily> {
        let ix = self.selected_index?;
        self.filtered
            .get(ix.row)
            .and_then(|&row| self.families.get(row))
    }
}

impl ListDelegate for FontListDelegate {
    type Item = ListItem;

    fn items_count(&self, _section: usize, _cx: &gpui_kit::App) -> usize {
        self.filtered.len()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let row = *self.filtered.get(ix.row)?;
        let family = self.families.get(row)?;
        let face_ix = family.default_face_index();
        let face = family.faces.get(face_ix);
        let face_font = face
            .map(|face| FontsModule::face_font(&family.name, face))
            .unwrap_or_else(Font::default);
        let meta = family.metadata_line();
        let is_favorite = self.favorites.contains(&family.name);

        Some(
            ListItem::new(format!("font-{}", family.name))
                .h_12()
                .px_3()
                .rounded_md()
                .accessibility_label(family.name.clone())
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_3()
                        .child(
                            div()
                                .size_8()
                                .flex_shrink_0()
                                .h_flex()
                                .items_center()
                                .justify_center()
                                .rounded_md()
                                .bg(cx.theme().muted)
                                .child(div().font(face_font.clone()).text_lg().child("Aa")),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .v_flex()
                                .gap_1()
                                .child(div().text_sm().truncate().child(family.name.clone()))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .truncate()
                                        .child(meta),
                                ),
                        )
                        .child(div().size_4().flex_shrink_0().when(is_favorite, |this| {
                            this.child(
                                Icon::new(AppIcon::FavoriteOn)
                                    .size_3()
                                    .text_color(cx.theme().primary),
                            )
                        })),
                ),
        )
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn loading(&self, _cx: &gpui_kit::App) -> bool {
        self.loading
    }

    fn render_empty(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        let (icon, title, detail) = if self.families.is_empty() {
            (
                AppIcon::Empty,
                "No fonts found",
                "The system font directory appears to be empty.",
            )
        } else {
            match self.filter {
                Filter::Favorites => (
                    AppIcon::Favorite,
                    "No favorites yet",
                    "Star a font to keep it here.",
                ),
                Filter::Recent => (
                    AppIcon::Recent,
                    "No recent fonts",
                    "Fonts you open will appear here.",
                ),
                Filter::Monospace => (
                    AppIcon::Monospace,
                    "No monospace fonts",
                    "No installed family is marked monospace.",
                ),
                Filter::All => (
                    AppIcon::NoResults,
                    "No matching fonts",
                    "Try a different family name.",
                ),
            }
        };

        div()
            .size_full()
            .v_flex()
            .items_center()
            .justify_center()
            .gap_3()
            .p_6()
            .child(
                Icon::new(icon)
                    .size_8()
                    .text_color(cx.theme().muted_foreground.opacity(0.6)),
            )
            .child(div().text_sm().child(title))
            .child(
                div()
                    .text_xs()
                    .text_center()
                    .text_color(cx.theme().muted_foreground)
                    .child(detail),
            )
    }
}

fn copy_shortcut_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "⌘C"
    } else {
        "Ctrl+C"
    }
}

fn search_shortcut_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "⌘F"
    } else {
        "Ctrl+F"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn face(weight: u16, style: FaceStyle, label: &str) -> FontFace {
        FontFace {
            weight,
            style,
            post_script_name: format!("Test-{label}"),
            monospaced: false,
            label: label.to_string(),
        }
    }

    fn family(name: &str) -> FontFamily {
        FontFamily {
            name: name.to_string(),
            faces: vec![face(400, FaceStyle::Normal, "Regular")],
            formats: vec!["TrueType".to_string()],
            file: None,
            post_script_name: None,
            monospaced: false,
        }
    }

    #[test]
    fn query_filters_case_insensitively_and_tracks_selection() {
        let mut delegate = FontListDelegate::new();
        delegate.set_fonts(vec![
            family("Arial"),
            family("Arial Narrow"),
            family("Calibri"),
        ]);

        delegate.set_query("aRiAl");
        assert_eq!(delegate.matched_count(), 2);

        delegate.selected_index = Some(IndexPath::default());
        assert_eq!(delegate.selected_family().unwrap().name, "Arial");

        delegate.selected_index = Some(IndexPath::new(1));
        assert_eq!(delegate.selected_family().unwrap().name, "Arial Narrow");

        delegate.set_query("zzz");
        assert_eq!(delegate.matched_count(), 0);
        assert!(delegate.selected_family().is_none());
    }

    #[test]
    fn total_faces_sums_every_family() {
        let mut delegate = FontListDelegate::new();
        let mut second = family("Calibri");
        second.faces.push(face(700, FaceStyle::Normal, "Bold"));
        second.faces.push(face(400, FaceStyle::Italic, "Italic"));
        second
            .faces
            .push(face(700, FaceStyle::Italic, "Bold Italic"));
        delegate.set_fonts(vec![family("Arial"), second]);
        assert_eq!(delegate.total_faces(), 5);
    }

    #[test]
    fn favorite_and_recent_filters_use_domain_identity() {
        let mut delegate = FontListDelegate::new();
        delegate.set_fonts(vec![family("Arial"), family("Calibri"), family("Consolas")]);

        let favorites = BTreeSet::from(["Calibri".to_string()]);
        delegate.set_filter_state(Filter::Favorites, favorites, Vec::new());
        assert_eq!(delegate.matched_count(), 1);

        delegate.set_filter_state(
            Filter::Recent,
            BTreeSet::new(),
            vec!["Consolas".to_string(), "Arial".to_string()],
        );
        assert_eq!(delegate.matched_count(), 2);
        delegate.selected_index = Some(IndexPath::default());
        assert_eq!(delegate.selected_family().unwrap().name, "Consolas");
    }
}
