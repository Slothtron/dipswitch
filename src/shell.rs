//! 工作台外壳：标题栏 + 左侧导航 + 当前模块内容区 + 状态栏。
//!
//! 状态所有权：
//! - `Dipswitch` 只拥有「当前模块」「导航是否折叠」与两个模块视图的实体；
//! - 每个模块自己拥有它的数据与交互状态；切走时实体仍然存活，因此切回来是
//!   即时的、不重新加载；
//! - 导航目的地与折叠状态记在 `Settings` Global，由 [`settings::persist`] 落库；
//! - 标题栏只放窗口装饰与全局主题开关，模块自己的命令留在模块工具栏里。

use gpui_kit::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, FontWeight, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, ParentElement, Render, SharedString, Styled, Window,
    actions,
    base::StyledExt,
    component::{
        ActiveTheme, Icon, IconName, Sizable, Theme, ThemeMode, TitleBar,
        button::{Button, ButtonVariants},
        sidebar::{Sidebar, SidebarMenu, SidebarMenuItem},
        status_bar::StatusBar,
        switch::Switch,
    },
    div,
};

use crate::{
    environment_view::EnvironmentModule,
    fonts_view::FontsModule,
    settings::{self, Settings},
};

pub const FONTS_MODULE: &str = "fonts";
pub const ENVIRONMENT_MODULE: &str = "environment";

/// 工作台层的快捷键上下文；模块自己的绑定仍留在模块容器上。
pub const KEY_CONTEXT: &str = "Dipswitch";

actions!(dipswitch, [ShowFonts, ShowEnvironment, ToggleNavigation]);

/// 标题栏整体是系统的 HTCAPTION 拖拽区：按下会被 WndProc 交给窗口移动的模态
/// 循环，控件只能收到按下、收不到抬起，点击永远不触发。控件在自己的按下事件里
/// 阻止冒泡，平台层就不会把这次按下交出去，点击才能正常完成。
fn intercept_title_bar_press(_: &MouseDownEvent, _: &mut Window, cx: &mut App) {
    cx.stop_propagation();
}

/// 工作台的导航目的地。新增模块时在这里加一项即可。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Module {
    Fonts,
    Environment,
}

impl Module {
    pub const ALL: [Self; 2] = [Self::Fonts, Self::Environment];

    /// 持久化用的稳定标识。
    pub fn key(self) -> &'static str {
        match self {
            Self::Fonts => FONTS_MODULE,
            Self::Environment => ENVIRONMENT_MODULE,
        }
    }

    /// 认不出的标识回到默认模块，避免配置损坏时打开一个空白工作台。
    pub fn parse(key: &str) -> Self {
        match key {
            ENVIRONMENT_MODULE => Self::Environment,
            _ => Self::Fonts,
        }
    }

    pub fn label(self) -> SharedString {
        match self {
            Self::Fonts => "Fonts".into(),
            Self::Environment => "Environment".into(),
        }
    }

    pub fn icon(self) -> IconName {
        match self {
            Self::Fonts => IconName::ALargeSmall,
            Self::Environment => IconName::SquareTerminal,
        }
    }
}

pub struct Dipswitch {
    active: Module,
    nav_collapsed: bool,
    fonts: Entity<FontsModule>,
    environment: Entity<EnvironmentModule>,
    configuration_error: Option<String>,
}

impl Dipswitch {
    /// `configuration_error` 由 `main` 传入：配置库没打开成功时工作台仍然可用，
    /// 但必须在内容区顶部说明原因，不能假装一切正常。
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        configuration_error: Option<String>,
    ) -> Self {
        let settings = cx.global::<Settings>().clone();
        let fonts = cx.new(|cx| FontsModule::new(window, cx));
        let environment = cx.new(|cx| EnvironmentModule::new(window, cx));

        Self {
            active: Module::parse(&settings.active_module),
            nav_collapsed: settings.nav_collapsed,
            fonts,
            environment,
            configuration_error,
        }
    }

    /// 当前停靠的模块，供测试与状态栏读取。
    pub fn active(&self) -> Module {
        self.active
    }

    fn select(&mut self, module: Module, cx: &mut Context<Self>) {
        if self.active == module {
            return;
        }
        self.active = module;
        cx.global_mut::<Settings>().active_module = module.key().to_string();
        settings::persist(cx);
        cx.notify();
    }

    fn toggle_navigation(&mut self, cx: &mut Context<Self>) {
        self.nav_collapsed = !self.nav_collapsed;
        cx.global_mut::<Settings>().nav_collapsed = self.nav_collapsed;
        settings::persist(cx);
        cx.notify();
    }

    /// 状态栏左侧文本，也由 UI 集成测试用来确认当前停靠的模块。
    pub fn status_text(&self, cx: &App) -> String {
        match self.active {
            Module::Fonts => self.fonts.read(cx).status_text(cx),
            Module::Environment => self.environment.read(cx).status_text(),
        }
    }

    /// 状态栏右侧：当前模块自己的键盘路径。导航快捷键放在导航底部，
    /// 因为那才是它的适用面。
    fn status_hint(&self, cx: &App) -> String {
        match self.active {
            Module::Fonts => FontsModule::status_hint(),
            Module::Environment => self.environment.read(cx).status_hint(),
        }
    }

    fn render_title_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dark = cx.theme().is_dark();
        let active_label = self.active.label();

        TitleBar::new().child(
            div()
                .h_full()
                .h_flex()
                .items_center()
                .gap_2()
                .flex_1()
                .min_w_0()
                .pr_2()
                // 标题栏整体是系统的 HTCAPTION 拖拽区：按下会被 WndProc 交给窗口
                // 移动的模态循环，控件只能收到按下、收不到抬起，点击永远不触发。
                // 控件在自己的按下事件里阻止冒泡，平台层就不会把这次按下交出去。
                .child(
                    div()
                        .h_full()
                        .h_flex()
                        .items_center()
                        .on_mouse_down(MouseButton::Left, intercept_title_bar_press)
                        .child(
                            Button::new("toggle-navigation")
                                .ghost()
                                .small()
                                .icon(IconName::PanelLeft)
                                .tooltip(if self.nav_collapsed {
                                    "Show navigation"
                                } else {
                                    "Hide navigation"
                                })
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_navigation(cx))),
                        ),
                )
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Dipswitch"),
                )
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(cx.theme().muted_foreground)
                        .child(active_label),
                )
                .child(div().flex_1())
                .child(
                    // 同上：主题开关也必须在标题栏拖拽区里自己吞掉按下事件。
                    div()
                        .h_full()
                        .h_flex()
                        .items_center()
                        .on_mouse_down(MouseButton::Left, intercept_title_bar_press)
                        .child(
                            Switch::new("dark-mode")
                                .small()
                                .checked(is_dark)
                                .label("Dark")
                                .on_change(|checked, window, cx| {
                                    let mode = if *checked {
                                        ThemeMode::Dark
                                    } else {
                                        ThemeMode::Light
                                    };
                                    Theme::change(mode, Some(window), cx);
                                    cx.global_mut::<Settings>().is_dark = *checked;
                                    settings::persist(cx);
                                }),
                        ),
                ),
        )
    }

    fn render_navigation(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Sidebar::new("dipswitch-navigation")
            .collapsed(self.nav_collapsed)
            .child(
                SidebarMenu::new().children(Module::ALL.into_iter().map(|module| {
                    SidebarMenuItem::new(module.label())
                        .icon(module.icon())
                        .active(self.active == module)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select(module, cx);
                        }))
                })),
            )
    }

    fn render_content(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self.active {
            Module::Fonts => self.fonts.update(cx, |module, cx| {
                module.render(window, cx).into_any_element()
            }),
            Module::Environment => self.environment.update(cx, |module, cx| {
                module.render(window, cx).into_any_element()
            }),
        }
    }

    fn on_show_fonts(&mut self, _: &ShowFonts, _: &mut Window, cx: &mut Context<Self>) {
        self.select(Module::Fonts, cx);
    }

    fn on_show_environment(&mut self, _: &ShowEnvironment, _: &mut Window, cx: &mut Context<Self>) {
        self.select(Module::Environment, cx);
    }

    fn on_toggle_navigation(
        &mut self,
        _: &ToggleNavigation,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_navigation(cx);
    }

    fn render_configuration_warning(&self, cx: &App) -> Option<gpui_kit::AnyElement> {
        let message = self.configuration_error.as_ref()?;
        Some(
            div()
                .px_4()
                .py_2()
                .h_flex()
                .items_center()
                .gap_2()
                .text_sm()
                .text_color(cx.theme().danger)
                .child(Icon::new(IconName::CircleAlert).small())
                .child(message.clone())
                .into_any_element(),
        )
    }
}

impl Render for Dipswitch {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = self.status_text(cx);
        let hint = self.status_hint(cx);
        let warning = self.render_configuration_warning(cx);

        div()
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(Self::on_show_fonts))
            .on_action(cx.listener(Self::on_show_environment))
            .on_action(cx.listener(Self::on_toggle_navigation))
            .size_full()
            .v_flex()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_title_bar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .h_flex()
                    .items_stretch()
                    .child(self.render_navigation(cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .v_flex()
                            .children(warning)
                            .child(self.render_content(window, cx)),
                    ),
            )
            .child(
                StatusBar::new()
                    .left(div().child(status))
                    .right(div().child(hint)),
            )
    }
}

impl Focusable for Dipswitch {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self.active {
            Module::Fonts => self.fonts.focus_handle(cx),
            Module::Environment => self.environment.focus_handle(cx),
        }
    }
}
