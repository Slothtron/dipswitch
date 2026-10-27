//! 界面的共享语汇：表格度量、层级文本与行内操作控件。
//!
//! 视觉规则（色彩、排版、间距、交互态、文案）写在项目之外的设计文档里（不随仓库
//! 发布）；这里只放能被编译器检查的那一半——列宽、表头、单元格，以及图标按钮的
//! 悬停与危险色。新界面先从这里取，不要在视图里另写一套数值。

use gpui_kit::{
    App, Div, ElementId, Font, Pixels, Styled,
    base::StyledExt as _,
    component::{
        ActiveTheme, Sizable,
        button::{Button, ButtonCustomVariant, ButtonVariants},
    },
    div, px,
};

use crate::icon::AppIcon;

/// 变量名列：容纳常见的 `JAVA_HOME` / `AWS_SECRET_ACCESS_KEY` 而不截断。
pub const KEY_COLUMN: Pixels = px(232.);
/// 分组内变量表比系统变量表多一个状态列，key 列相应收窄，给 Value 留空间。
pub const GROUP_KEY_COLUMN: Pixels = px(200.);
/// 类型列：`REG_SZ` / `REG_EXPAND_SZ` / `REG_MULTI_SZ` 三者中任一都能放下。
pub const TYPE_COLUMN: Pixels = px(120.);
/// 来源列（仅系统变量表）：`Group · <name>` 截断由调用方负责。
pub const SOURCE_COLUMN: Pixels = px(150.);
/// 状态列（仅分组内变量表）：`Overridden` 一个词。
pub const STATE_COLUMN: Pixels = px(96.);
/// 操作列：三个 xsmall 图标按钮 + 两个间距 + 单元格内边距。
/// 加到第四个按钮之前先改这里，否则按钮会被挤出可视区。
pub const ACTIONS_COLUMN: Pixels = px(96.);

/// 抓包列表的列宽：状态、方法、主机、MIME、大小、耗时；路径吃掉剩余宽度。
/// 抓包列表只留「结果 + 方法 + 路径 + 大小 + 时间」五列：主机、类型与参数
/// 都进右侧详情，窄窗口下路径才有可读宽度。
pub const TRAFFIC_STATUS_COLUMN: Pixels = px(56.);
pub const TRAFFIC_METHOD_COLUMN: Pixels = px(68.);
pub const TRAFFIC_PATH_COLUMN: Pixels = px(560.);
pub const TRAFFIC_SIZE_COLUMN: Pixels = px(72.);
pub const TRAFFIC_TIME_COLUMN: Pixels = px(76.);

/// 详情表格的键列：HTTP 头名与 JSON 成员名基本都能在这个宽度内读完。
pub const DETAIL_KEY_COLUMN: Pixels = px(184.);
/// 详情表格的行操作列：一个 xsmall 复制按钮。值被单行截断时，完整内容靠它取。
pub const DETAIL_ACTION_COLUMN: Pixels = px(34.);

/// 表单对话框统一宽度：一行说明 + 一个输入框读起来不散，两列并排也放得下。
pub const DIALOG_WIDTH: Pixels = px(460.);

/// 分组列表分栏的默认宽度与可拖范围。上限压到 320，是为了在最小窗口下
/// 详情区仍放得下「key / value / type / state / actions」这组固定列。
pub const GROUP_LIST_WIDTH: Pixels = px(260.);
pub const GROUP_LIST_MIN: Pixels = px(200.);
pub const GROUP_LIST_MAX: Pixels = px(320.);

const CELL_PADDING_X: Pixels = px(6.);
const CELL_PADDING_Y: Pixels = px(3.);

/// 表格单元格：固定宽度的列不参与伸缩，`None` 的列吃掉剩余空间。
pub fn cell(width: Option<Pixels>) -> Div {
    let cell = div()
        .h_flex()
        .items_center()
        .px(CELL_PADDING_X)
        .py(CELL_PADDING_Y);
    match width {
        Some(width) => cell.w(width).flex_shrink_0(),
        None => cell.flex_1().min_w_0(),
    }
}

/// 有初始宽度、但窄窗口里允许收缩的单元格（抓包路径列）。
///
/// 与 [`cell`] 的固定宽度不同：`flex_shrink_1` 让它在列宽总和超过容器时
/// 先让出空间，避免整行横向溢出；拖宽它由调用方的列宽状态负责。
pub fn shrinkable_cell(width: Pixels) -> Div {
    div()
        .h_flex()
        .items_center()
        .px(CELL_PADDING_X)
        .py(CELL_PADDING_Y)
        .w(width)
        .min_w_0()
        .flex_shrink_1()
}

/// 表头外框。列宽由调用方的 [`cell`] 决定，宽列由 `None` 自动分到。
pub fn table_header(cx: &App) -> Div {
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

/// 数量短语：`counted(1, "variable", "variables")` → `"1 variable"`。
/// 界面上的计数一律经过这里，避免出现 “1 variables” 这类错句。
pub fn counted(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

/// 值列与 key 列用的等宽字体；变量值按原样排版，比例字体读不出空格差异。
pub fn mono_font(cx: &App) -> Font {
    Font {
        family: cx.theme().mono_font_family.clone(),
        ..Default::default()
    }
}

/// 表格行内的图标按钮（复制 / 编辑 / 删除 / 更多）。
///
/// 不用 `.ghost()`：组件的 ghost 悬停色是 `accent`，而表格行的悬停色
/// `table_hover` 在默认主题里与它同色（浅色都是 `#f5f5f5`），指针移到按钮上
/// 看起来毫无反应。行内控件改用更深一级的 `secondary`，并把危险色直接设为前景色，
/// 这样图标不需要在调用点再染色。
pub fn row_icon_button(id: impl Into<ElementId>, icon: AppIcon, cx: &App) -> Button {
    Button::new(id)
        .custom(icon_variant(icon, cx))
        .xsmall()
        .icon(icon)
}

/// 工具栏 / 区块标题上的图标按钮，与 [`row_icon_button`] 同一套悬停规则，
/// 只是尺寸大一号、周围没有行高亮可借。
pub fn icon_button(id: impl Into<ElementId>, icon: AppIcon, cx: &App) -> Button {
    Button::new(id)
        .custom(icon_variant(icon, cx))
        .small()
        .icon(icon)
}

fn icon_variant(icon: AppIcon, cx: &App) -> ButtonCustomVariant {
    let variant = ButtonCustomVariant::new(cx)
        .hover(cx.theme().secondary)
        .active(cx.theme().secondary_active);
    match icon.role_color(cx) {
        Some(color) => variant.foreground(color),
        None => variant,
    }
}

#[cfg(test)]
mod tests {
    use super::counted;

    /// 0 用复数、1 用单数——界面里最常见的错句就是 “1 variables”。
    #[test]
    fn counts_use_the_singular_only_for_one() {
        assert_eq!(counted(0, "variable", "variables"), "0 variables");
        assert_eq!(counted(1, "variable", "variables"), "1 variable");
        assert_eq!(counted(24, "group", "groups"), "24 groups");
    }
}
