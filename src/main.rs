//! 二进制入口：初始化框架、打开配置库、绑定快捷键、创建工作台窗口。

use dipswitch::{
    environment_view::{self, AddVariable, NewGroup, ReloadVariables},
    fonts_view::{CopyFamilyName, FocusSearch, KEY_CONTEXT as FONTS_KEY_CONTEXT, RefreshFonts},
    settings::{self, Settings},
    shell::{Dipswitch, KEY_CONTEXT, ShowEnvironment, ShowFonts, ToggleNavigation},
    store::DipswitchStore,
};
#[cfg(target_os = "linux")]
use gpui_kit::WindowDecorations;
use gpui_kit::{
    AppContext, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, component::TitleBar, px,
    size,
};

fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            // 必须先初始化 Component/Base，再创建任何组件视图。
            gpui_kit::init(cx);

            // 快捷键绑定要在创建窗口/菜单之前完成。
            cx.bind_keys([
                // 工作台层：模块切换与导航折叠。
                KeyBinding::new("alt-1", ShowFonts, Some(KEY_CONTEXT)),
                KeyBinding::new("alt-2", ShowEnvironment, Some(KEY_CONTEXT)),
                KeyBinding::new("cmd-b", ToggleNavigation, Some(KEY_CONTEXT)),
                // 字体模块层。
                KeyBinding::new("cmd-c", CopyFamilyName, Some(FONTS_KEY_CONTEXT)),
                KeyBinding::new("cmd-f", FocusSearch, Some(FONTS_KEY_CONTEXT)),
                KeyBinding::new("cmd-r", RefreshFonts, Some(FONTS_KEY_CONTEXT)),
                // 环境变量模块层。
                KeyBinding::new(
                    "alt-r",
                    ReloadVariables,
                    Some(environment_view::KEY_CONTEXT),
                ),
                KeyBinding::new("alt-n", NewGroup, Some(environment_view::KEY_CONTEXT)),
                KeyBinding::new(
                    "alt-shift-a",
                    AddVariable,
                    Some(environment_view::KEY_CONTEXT),
                ),
            ]);

            // 配置库：即使打不开也要让工作台起来，并把原因显示在内容区顶部，
            // 否则用户只看到一个空白分组列表。
            let (configuration_error, store) = match DipswitchStore::open_or_default() {
                Ok(store) => (None, store),
                Err(message) => (Some(message), DipswitchStore::memory()),
            };
            cx.set_global(store);

            // 升级前用的是 font-manager/settings.json，第一次启动时导入一次。
            settings::import_legacy_once(cx.global::<DipswitchStore>());
            let settings = Settings::load(cx.global::<DipswitchStore>());
            gpui_kit::component::Theme::change(
                if settings.is_dark {
                    gpui_kit::component::ThemeMode::Dark
                } else {
                    gpui_kit::component::ThemeMode::Light
                },
                None,
                cx,
            );

            let width = settings.window_width.unwrap_or(1180.0);
            let height = settings.window_height.unwrap_or(760.0);
            let centered = WindowBounds::centered(size(px(width), px(height)), cx);
            let is_maximized = settings.window_maximized.unwrap_or(false);
            cx.set_global(settings);

            // 退出时保存窗口状态与内存中的偏好（最近使用、示例文本等）。
            cx.on_app_quit(|cx| {
                if let Some(handle) = cx.windows().first().copied()
                    && let Ok((size, maximized)) = cx.update_window(handle, |_, window, _| {
                        (window.bounds().size, window.is_maximized())
                    })
                {
                    let settings = cx.global_mut::<Settings>();
                    settings.window_maximized = Some(maximized);
                    if !maximized {
                        settings.window_width = Some(size.width.as_f32());
                        settings.window_height = Some(size.height.as_f32());
                    }
                }
                let settings = cx.global::<Settings>().clone();
                settings.save(cx.global::<DipswitchStore>());
                async {}
            })
            .detach();

            // 自绘标题栏：隐藏系统标题栏，拖拽区与窗口按钮由 TitleBar 绘制。
            let mut options = WindowOptions {
                window_bounds: Some(if is_maximized {
                    WindowBounds::Maximized(centered.get_bounds())
                } else {
                    centered
                }),
                // 最小尺寸是窗口系统的物理边界：要容得下导航 + 模块的主从布局。
                window_min_size: Some(size(px(880.0), px(560.0))),
                // Linux 任务栏/桌面项分组用；其他平台忽略。
                app_id: Some("dipswitch".to_string()),
                #[cfg(target_os = "linux")]
                window_decorations: Some(WindowDecorations::Client),
                ..TitleBar::window_options()
            };
            // 保留窗口标题（任务栏 / 无障碍名称），但标题栏外观完全由组件绘制。
            options.titlebar = Some(TitlebarOptions {
                title: Some("Dipswitch".into()),
                ..TitleBar::title_bar_options()
            });

            // open_window 会把根视图包进 Root，通知 / 对话框等框架能力依赖它。
            gpui_kit::open_window(options, cx, move |window, cx| {
                cx.new(|cx| Dipswitch::new(window, cx, configuration_error))
            })
            .expect("failed to open window");
        });
}
