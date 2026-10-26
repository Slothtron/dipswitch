//! 应用内图标的唯一出口：语义名 → 图形。
//!
//! **调用点只说「删除」「新建分组」，永远不直接写图形名。** 所以要换掉某个图标的
//! 样子——换成另一个内置图形，或换成应用自己画的 SVG——只改 `AppIcon::target`
//! 里的那一行，不必回项目里搜引用。
//!
//! 为什么要这层间接。`gpui_component::IconName` 只是 `gpui_kit_assets::IconName`
//! 的兼容子集（106 个），但组件 API 收的是 `impl Into<Icon>`，而 `Icon` 对**任何**
//! 实现了 `IconNamed` 的枚举都有 blanket `From`——所以全量 1,830 个 Lucide 图形都能
//! 用。代价是共享枚举给出的是路径（`icons/variable.svg`），而默认 `Assets` 只嵌了那
//! 106 个的字节，用了没注册的图形运行期取不到字节。
//!
//! 于是本模块负责三件事，并用测试把它们焊在一起：
//!
//! - [`AppIcon`]：按**操作含义**命名的语义枚举（`Delete` 而不是 `Trash`）；
//! - `Target`：该语义当前指向一个内置 Lucide 图形，还是指向 [`custom`] 里应用自带的
//!   SVG 字节——这就是「改指向不动调用点」的落点；
//! - `ExtraIcons` + [`AppAssets`]：把子集之外的内置图形嵌进二进制并注册资产源。
//!   `every_app_icon_resolves_to_real_svg` 逐个断言语义图标取得到真实 SVG：漏登记不会
//!   编译报错，只会在界面上画出一片空白，所以必须在测试里拦住。
//!
//! 风格统一靠「只用 Lucide」：全部图形同为 24×24 网格、2px 描边、圆头端点；组件自带
//! 的窗口按钮（最小化/最大化/关闭）也是 Lucide，不需要额外对齐。自定义图形要守同一套
//! 语言，因为 `Icon` 走 alpha 蒙版单色着色——`brand_mark_is_mask_safe_not_the_exe_tile`
//! 就是拦「把带底板的全彩 exe 图标直接拿进应用内用」的。

use gpui_kit::AssetSource;
use gpui_kit::assets::{Assets, IconName};
use gpui_kit::component::Icon;

// 子集之外、本应用用到的图形。加图标时这里必须同步，否则下面的注册完整性测试会红。
gpui_kit::assets::icon_assets!(
    ExtraIcons,
    [
        Braces, Clock, Columns2, FolderPlus, Languages, Pencil, SearchX, Trash, Type, Variable
    ]
);

/// 注册给 `application().with_assets(...)` 的资产源。
///
/// 顺序有讲究：自定义源未命中返回 `Ok(None)`，而 `Assets::load` 未命中返回
/// `Err`，所以必须自定义在前、默认包兜底，反过来会让默认图标全都加载失败。
#[derive(Clone, Copy, Debug, Default)]
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<gpui_kit::SharedString>> {
        let mut paths = Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

/// 应用内所有图标的语义名。命名跟着「这个控件在做什么」走，不跟着图形形状走。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AppIcon {
    // 目的地与外壳
    ModuleFonts,
    ModuleEnvironment,
    CollapseNavigation,
    // 通用操作
    Add,
    Refresh,
    Search,
    NoResults,
    Copy,
    Edit,
    Delete,
    Remove,
    More,
    // 环境变量分组
    NewGroup,
    // 字体模块
    Compare,
    Favorite,
    FavoriteOn,
    Recent,
    Monospace,
    Samples,
    Surface,
    TypeScale,
    // 状态
    Warning,
    Error,
    Empty,
    // 品牌标（应用自己的标记，不是操作图标）
    Brand,
}

impl AppIcon {
    /// 全部语义图标，供注册完整性测试与未来的图标总览页使用。
    pub const ALL: &'static [AppIcon] = &[
        AppIcon::ModuleFonts,
        AppIcon::ModuleEnvironment,
        AppIcon::CollapseNavigation,
        AppIcon::Add,
        AppIcon::Refresh,
        AppIcon::Search,
        AppIcon::NoResults,
        AppIcon::Copy,
        AppIcon::Edit,
        AppIcon::Delete,
        AppIcon::Remove,
        AppIcon::More,
        AppIcon::NewGroup,
        AppIcon::Compare,
        AppIcon::Favorite,
        AppIcon::FavoriteOn,
        AppIcon::Recent,
        AppIcon::Monospace,
        AppIcon::Samples,
        AppIcon::Surface,
        AppIcon::TypeScale,
        AppIcon::Warning,
        AppIcon::Error,
        AppIcon::Empty,
        AppIcon::Brand,
    ];

    /// 语义 → 具体图形。**改指向只动这一个 `match`，调用点一个字都不用改。**
    ///
    /// 默认全部指向 gpui-kit 自带的 Lucide 图形；当内置图形表达不准、或必须用应用
    /// 自己的形状时，把该变体改成 [`Target::Custom`] 并把 SVG 放进 [`custom`]。
    const fn target(self) -> Target {
        match self {
            // 目的地用领域符号，不用泛化的面板图标。
            Self::ModuleFonts => Target::Builtin(IconName::Type),
            Self::ModuleEnvironment => Target::Builtin(IconName::Variable),
            Self::CollapseNavigation => Target::Builtin(IconName::PanelLeft),
            // 通用操作。
            Self::Add => Target::Builtin(IconName::Plus),
            Self::Refresh => Target::Builtin(IconName::RefreshCw),
            Self::Search => Target::Builtin(IconName::Search),
            // 「搜了但没命中」和「搜索框」要能区分开。
            Self::NoResults => Target::Builtin(IconName::SearchX),
            Self::Copy => Target::Builtin(IconName::Copy),
            // 编辑用铅笔：原来的 Replace 是「查找替换」，语义不对。
            Self::Edit => Target::Builtin(IconName::Pencil),
            // 删除用垃圾桶：原来的 Delete 是退格键形状，读起来像「清除输入」。
            Self::Delete => Target::Builtin(IconName::Trash),
            // 从对比集合里移出一项，不是删数据，所以用圈叉而不是垃圾桶。
            Self::Remove => Target::Builtin(IconName::CircleX),
            Self::More => Target::Builtin(IconName::Ellipsis),
            // 新建分组原来是 Folder，和「分组」这个概念对不上。
            Self::NewGroup => Target::Builtin(IconName::FolderPlus),
            Self::Compare => Target::Builtin(IconName::Columns2),
            Self::Favorite => Target::Builtin(IconName::Star),
            Self::FavoriteOn => Target::Builtin(IconName::StarFill),
            // 最近使用用时钟，原来的 RotateCw 与「重新扫描」撞形。
            Self::Recent => Target::Builtin(IconName::Clock),
            // 等宽字体用花括号（代码字形），原来的 CaseSensitive 表示大小写。
            Self::Monospace => Target::Builtin(IconName::Braces),
            // 示例文本预设覆盖 Latin/中文/Code/Numbers，用多语言符号。
            Self::Samples => Target::Builtin(IconName::Languages),
            Self::Surface => Target::Builtin(IconName::Palette),
            Self::TypeScale => Target::Builtin(IconName::ALargeSmall),
            // 警告（可继续但有后果）与错误（已经做不到）分形、分色。
            Self::Warning => Target::Builtin(IconName::TriangleAlert),
            Self::Error => Target::Builtin(IconName::CircleAlert),
            Self::Empty => Target::Builtin(IconName::Inbox),
            // 应用自己的标记，内置目录里没有，走自定义字节。
            Self::Brand => Target::Custom(custom::BRAND),
        }
    }

    /// 语义图标的元素形式。`From<AppIcon> for Icon` 也走这里，所以调用点写
    /// `AppIcon::Add` 或 `AppIcon::Add.icon()` 得到的是同一个东西。
    pub fn icon(self) -> Icon {
        match self.target() {
            // 内置图形给出的是路径，字节由 AppAssets 注册（见 icon_assets!）。
            Target::Builtin(name) => Icon::new(name),
            Target::Custom(bytes) => Icon::default().data(bytes),
        }
    }
}

/// 一个语义图标当前指向哪儿。
enum Target {
    /// gpui-kit 的 1,830 个 Lucide 图形之一，需要时由 `icon_assets!` 嵌入字节。
    Builtin(IconName),
    /// 应用自带的 SVG，编译期内嵌，不经过 `AssetSource`。
    /// 注意 `Icon` 走 alpha 蒙版单色着色——自定义图形要按描边语言画，别带底板。
    Custom(&'static [u8]),
}

/// 自定义图形的字节。放在这里是为了和内置图形共用同一张指向表，
/// 而不是散落到各视图文件里各自 `include_bytes!`。
pub mod custom {
    /// 与 `assets/brand.svg`（exe 图标）同一枚 d，但按 Lucide 的单色描边语言重画过：
    /// GPUI 的 `Icon` 用 alpha 蒙版着色，带底板的全彩版在标题栏会被压成实心方块。
    /// 两份文件是「同一形状、两种渲染语境」，改形状要同时改两处并重出 `.ico`。
    pub const BRAND: &[u8] = include_bytes!("../assets/brand-mark.svg");
}

/// 让 `AppIcon` 直接出现在任何收 `impl Into<Icon>` 的位置上——`Button::icon`、
/// `SidebarMenu::icon`、`Icon::new(..).small()`、`.child(..)`，调用点因此写成
/// `AppIcon::Add` 而不是再套一层构造函数。
impl From<AppIcon> for Icon {
    fn from(icon: AppIcon) -> Self {
        icon.icon()
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;

    /// 两类指向都要拦：内置图形可能忘了加进 `icon_assets!`（不报错，只在界面上画
    /// 一片空白），自定义图形可能内嵌错了内容。
    #[test]
    fn every_app_icon_resolves_to_real_svg() {
        let source = AppAssets;
        for icon in AppIcon::ALL {
            let bytes: Cow<'static, [u8]> = match icon.target() {
                Target::Builtin(name) => {
                    let path = name.path();
                    source
                        .load(path.as_str())
                        .unwrap_or_else(|error| panic!("{icon:?} → {path} 取字节失败：{error}"))
                        .unwrap_or_else(|| {
                            panic!("{icon:?} 用的 {path} 没嵌进二进制：把它加进 icon_assets! 清单")
                        })
                }
                Target::Custom(bytes) => Cow::Borrowed(bytes),
            };
            assert!(bytes.starts_with(b"<svg"), "{icon:?} 拿到的不是 SVG");
        }
    }

    /// ALL 必须覆盖每个变体；内置图形之间不允许同形不同义（要重复得有意识地确认）。
    #[test]
    fn all_lists_every_variant_and_builtin_shapes_are_distinct() {
        assert_eq!(AppIcon::ALL.len(), 25, "增删 AppIcon 变体时同步这条断言");
        let mut paths: Vec<String> = AppIcon::ALL
            .iter()
            .filter_map(|icon| match icon.target() {
                Target::Builtin(name) => Some(name.path().to_string()),
                Target::Custom(_) => None,
            })
            .collect();
        let total = paths.len();
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), total, "两个语义图标共用了同一个内置图形");
    }

    /// 品牌标必须**描边 + currentColor、不带底板**：`Icon` 走 alpha 蒙版单色着色，
    /// 实测把带 `#10141A` 底板的全彩版放进标题栏会渲染成一个实心方块。这条断言就是
    /// 防止有人把 exe 图标那份直接拿进应用内用。
    #[test]
    fn brand_mark_is_mask_safe_not_the_exe_tile() {
        let mark = std::str::from_utf8(custom::BRAND).expect("品牌标要是合法 UTF-8");
        assert!(
            mark.contains("viewBox=\"0 0 24 24\""),
            "品牌标要用 Lucide 的 24 网格"
        );
        assert!(
            mark.contains("stroke=\"currentColor\""),
            "品牌标要跟随前景色"
        );
        assert!(mark.contains("fill=\"none\""), "品牌标不能带实心填充");
        assert!(
            !mark.contains("#10141A"),
            "品牌标带了 exe 图标的底板色——它在 Icon 里会被压成实心方块"
        );
    }
}
