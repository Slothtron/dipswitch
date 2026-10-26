//! 应用内图标的唯一出口：语义名 → 图形，外加与 exe 同源的品牌标。
//!
//! 为什么要这一层。`gpui_component::IconName` 只是 `gpui_kit_assets::IconName`
//! 的兼容子集（106 个），但组件 API 收的是 `impl Into<Icon>`，而 `Icon` 对**任何**
//! 实现了 `IconNamed` 的枚举都有 blanket `From`——所以全量 1,830 个 Lucide 图形
//! 都能直接用。代价是：共享枚举给出的是路径（`icons/variable.svg`），而默认
//! `Assets` 只嵌了那 106 个的字节，用了没注册的图形会在运行期取不到字节。
//! 于是本模块同时负责两件事，并用一条测试把它们焊在一起：
//!
//! - [`AppIcon`]：按**操作含义**命名的语义枚举（`Delete` 而不是 `Trash`）；
//! - [`ExtraIcons`]：把子集之外、本应用真正用到的 SVG 嵌进二进制；
//! - [`every_app_icon_resolves_through_the_registered_source`]：断言每个语义图标
//!   都能从注册后的 [`AppAssets`] 取到字节——漏登记立刻失败，而不是等到界面上
//!   出现空白图标。
//!
//! 风格统一靠的是「只用 Lucide」：全部图形同为 24×24 网格、2px 描边、圆头端点。
//! 组件自带的窗口按钮（最小化/最大化/关闭）也是 Lucide，所以不需要额外对齐。

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
    ];

    /// 语义 → Lucide 图形。这里也是「为什么选这个图形」的注释位。
    pub const fn icon_name(self) -> IconName {
        match self {
            // 目的地用领域符号，不用泛化的面板图标。
            Self::ModuleFonts => IconName::Type,
            Self::ModuleEnvironment => IconName::Variable,
            Self::CollapseNavigation => IconName::PanelLeft,
            // 通用操作。
            Self::Add => IconName::Plus,
            Self::Refresh => IconName::RefreshCw,
            Self::Search => IconName::Search,
            // 「搜了但没命中」和「搜索框」要能区分开。
            Self::NoResults => IconName::SearchX,
            Self::Copy => IconName::Copy,
            // 编辑用铅笔：原来的 Replace 是「查找替换」，语义不对。
            Self::Edit => IconName::Pencil,
            // 删除用垃圾桶：原来的 Delete 是退格键形状，读起来像「清除输入」。
            Self::Delete => IconName::Trash,
            // 从对比集合里移出一项，不是删数据，所以用圈叉而不是垃圾桶。
            Self::Remove => IconName::CircleX,
            Self::More => IconName::Ellipsis,
            // 新建分组原来是 Folder，和「分组」这个概念对不上。
            Self::NewGroup => IconName::FolderPlus,
            Self::Compare => IconName::Columns2,
            Self::Favorite => IconName::Star,
            Self::FavoriteOn => IconName::StarFill,
            // 最近使用用时钟，原来的 RotateCw 与「重新扫描」撞形。
            Self::Recent => IconName::Clock,
            // 等宽字体用花括号（代码字形），原来的 CaseSensitive 表示大小写。
            Self::Monospace => IconName::Braces,
            // 示例文本预设覆盖 Latin/中文/Code/Numbers，用多语言符号。
            Self::Samples => IconName::Languages,
            Self::Surface => IconName::Palette,
            Self::TypeScale => IconName::ALargeSmall,
            // 警告（可继续但有后果）与错误（已经做不到）分形、分色。
            Self::Warning => IconName::TriangleAlert,
            Self::Error => IconName::CircleAlert,
            Self::Empty => IconName::Inbox,
        }
    }
}

/// 让 `AppIcon` 直接出现在任何收 `impl Into<Icon>` 的位置上——`Button::icon`、
/// `SidebarMenu::icon`、`Icon::new(..).small()`、`.child(..)`，调用点因此写成
/// `AppIcon::Add` 而不是再套一层构造函数。
impl From<AppIcon> for Icon {
    fn from(icon: AppIcon) -> Self {
        Self::new(icon.icon_name())
    }
}

/// 应用内的品牌标几何与 `assets/brand.svg`（exe 图标）同源，但**按 Lucide 的单色
/// 描边语言重画**过：GPUI 的 `Icon` 走 alpha 蒙版着色，多色填充会被压成一片剪影
/// （实测带底板的全彩版在标题栏渲染成一个实心方块）。
/// 两份文件的关系是「同一枚 d，两种渲染语境」，改形状要同时改两处。
pub const BRAND_SVG: &[u8] = include_bytes!("../assets/brand-mark.svg");

/// 品牌标元素，跟随界面前景色（与其余图标同一套着色规则）。
pub fn brand() -> Icon {
    Icon::default().data(BRAND_SVG)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用了没登记的图形不会编译报错，只会在界面上画出一片空白——所以在这里拦住。
    #[test]
    fn every_app_icon_resolves_through_the_registered_source() {
        let source = AppAssets;
        for icon in AppIcon::ALL {
            let path = icon.icon_name().path();
            let bytes = source
                .load(path.as_str())
                .unwrap_or_else(|error| panic!("{icon:?} → {path} 取字节失败：{error}"));
            assert!(
                bytes.is_some(),
                "{icon:?} 用的 {path} 没有嵌进二进制：把它加进 icon_assets! 的清单"
            );
            assert!(
                bytes.unwrap().starts_with(b"<svg"),
                "{icon:?} → {path} 拿到的不是 SVG"
            );
        }
    }

    /// 语义名不允许重复映射到同一个图形却漏掉某个变体：ALL 必须覆盖每个变体。
    #[test]
    fn all_lists_every_variant() {
        assert_eq!(AppIcon::ALL.len(), 24, "增删 AppIcon 变体时同步这条断言");
        let mut paths: Vec<_> = AppIcon::ALL
            .iter()
            .map(|icon| icon.icon_name().path().to_string())
            .collect();
        let unique = {
            paths.sort();
            paths.dedup();
            paths.len()
        };
        // 允许同形不同义（目前没有），出现重复时至少要有意识地确认过。
        assert_eq!(unique, AppIcon::ALL.len(), "两个语义图标共用了同一个图形");
    }

    #[test]
    fn brand_svg_is_embedded_and_self_contained() {
        assert!(BRAND_SVG.starts_with(b"<svg"));
        assert!(std::str::from_utf8(BRAND_SVG).is_ok());
    }
}
