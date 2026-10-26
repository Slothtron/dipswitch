//! 带预设刻度的字号滑杆。
//!
//! 行为契约：
//! - 拖动 / 点击时按像素位置连续取值（32–96），预览实时跟随；
//! - 预设字号是轨道上的圆点，并在下方标注数值；
//! - 松开鼠标后吸附到距离当前位置最近的预设点；
//! - 聚焦后 ←/→（或 ↓/↑）在预设点之间移动，Home/End 到两端。
//!
//! 几何用 `on_prepaint` 记录轨道实际 bounds；交互只在轨道的 x 轴上映射，
//! 不依赖固定的像素宽度，因此窗口缩放和 rem 变化都成立。

use std::{rc::Rc, sync::Arc};

use gpui_kit::{
    App, AppContext, Bounds, Context, DragMoveEvent, ElementId, Empty, FocusHandle, FontWeight,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement,
    Pixels, Render, RenderOnce, Role, StatefulInteractiveElement, Styled, Window,
    base::{ElementExt, StyledExt},
    component::ActiveTheme,
    div, px, relative,
};

#[derive(Default)]
struct SliderGeometry {
    bounds: Option<Bounds<Pixels>>,
    /// 最新一次交互的值；松手时从这里取，避免依赖上一帧的 prop。
    value: f32,
}

#[derive(Clone)]
struct DragPreviewSize;

impl Render for DragPreviewSize {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

type ValueCallback = Rc<dyn Fn(f32, &mut Window, &mut App)>;

#[derive(IntoElement)]
pub struct SizeSlider {
    id: ElementId,
    value: f32,
    presets: Vec<f32>,
    focus_handle: FocusHandle,
    on_change: ValueCallback,
    on_commit: ValueCallback,
}

impl SizeSlider {
    pub fn new(
        id: impl Into<ElementId>,
        value: f32,
        presets: Vec<f32>,
        focus_handle: FocusHandle,
    ) -> Self {
        Self {
            id: id.into(),
            value,
            presets,
            focus_handle,
            on_change: Rc::new(|_, _, _| {}),
            on_commit: Rc::new(|_, _, _| {}),
        }
    }

    pub fn on_change(mut self, handler: impl Fn(f32, &mut Window, &mut App) + 'static) -> Self {
        self.on_change = Rc::new(handler);
        self
    }

    pub fn on_commit(mut self, handler: impl Fn(f32, &mut Window, &mut App) + 'static) -> Self {
        self.on_commit = Rc::new(handler);
        self
    }

    fn min(&self) -> f32 {
        self.presets.first().copied().unwrap_or(0.0)
    }

    fn max(&self) -> f32 {
        self.presets.last().copied().unwrap_or(100.0)
    }
}

impl RenderOnce for SizeSlider {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let min = self.min();
        let max = self.max();
        let value = self.value.clamp(min, max);
        let percent = fraction(value, min, max);
        let rem = window.rem_size().as_f32();

        let bar_height = rem * 1.25;
        let track_height = rem * 0.25;
        let thumb_size = rem * 1.0;
        let tick_size = rem * 0.5;
        let label_width = rem * 2.0;
        let labels_height = rem * 1.25;

        let track_top = (bar_height - track_height) / 2.0;
        let thumb_top = (bar_height - thumb_size) / 2.0;
        let tick_top = (bar_height - tick_size) / 2.0;

        let primary = cx.theme().primary;
        let border = cx.theme().border;
        let background = cx.theme().background;
        let muted_foreground = cx.theme().muted_foreground;
        let ring = cx.theme().ring;
        let focused = self.focus_handle.is_focused(window);

        let geometry = window.use_keyed_state(
            ElementId::NamedChild(Arc::new(self.id.clone()), "geometry".into()),
            cx,
            |_, _| SliderGeometry::default(),
        );

        // 外部（应用层）可能在键盘、重扫或代码里改值；保持几何状态跟随。
        if (geometry.read(cx).value - value).abs() > f32::EPSILON {
            geometry.update(cx, |geometry, _| geometry.value = value);
        }

        let on_change = self.on_change.clone();
        let on_commit = self.on_commit.clone();
        let focus_handle = self.focus_handle.clone();
        let presets = self.presets.clone();

        let commit = {
            let geometry = geometry.clone();
            let on_commit = on_commit.clone();
            let presets = presets.clone();
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let value = geometry.read(cx).value;
                let snapped = nearest_preset(value, &presets);
                geometry.update(cx, |geometry, _| geometry.value = snapped);
                on_commit(snapped, window, cx);
            })
        };

        // 轨道本身只是绘制层；交互挂在外层，这样下方的刻度标签也可点击。
        let track = div()
            .relative()
            .w_full()
            .h(px(bar_height))
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .top(px(track_top))
                    .h(px(track_height))
                    .rounded_full()
                    .bg(border),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top(px(track_top))
                    .h(px(track_height))
                    .w(relative(percent))
                    .rounded_full()
                    .bg(primary),
            )
            .children(presets.iter().map(|&preset| {
                let preset_percent = fraction(preset, min, max);
                let passed = preset <= value + 0.01;
                let dot_color = if passed { primary } else { muted_foreground };

                div()
                    .absolute()
                    .left(relative(preset_percent))
                    .ml(px(-tick_size / 2.0))
                    .top(px(tick_top))
                    .size(px(tick_size))
                    .rounded_full()
                    .bg(background)
                    .border_2()
                    .border_color(dot_color)
                    .into_any_element()
            }))
            .child(
                div()
                    .absolute()
                    .left(relative(percent))
                    .ml(px(-thumb_size / 2.0))
                    .top(px(thumb_top))
                    .size(px(thumb_size))
                    .rounded_full()
                    .bg(primary)
                    // 不用包裹整条滑杆的聚焦边框；焦点落在 thumb 的描边上。
                    .border_2()
                    .border_color(if focused { ring } else { background })
                    .into_any_element(),
            );

        let labels = div()
            .relative()
            .w_full()
            .h(px(labels_height))
            .children(presets.iter().map(|&preset| {
                let preset_percent = fraction(preset, min, max);
                let active = (value - preset).abs() < 0.5;

                div()
                    .absolute()
                    .left(relative(preset_percent))
                    .ml(px(-label_width / 2.0))
                    .top_0()
                    .w(px(label_width))
                    .text_center()
                    .text_xs()
                    .font_weight(if active {
                        FontWeight::SEMIBOLD
                    } else {
                        FontWeight::NORMAL
                    })
                    .text_color(if active { primary } else { muted_foreground })
                    .child(format!("{preset:.0}"))
                    .into_any_element()
            }));

        let interactive = div()
            .id(self.id.clone())
            .relative()
            .w_full()
            .v_flex()
            .gap_1()
            .track_focus(&self.focus_handle)
            .role(Role::Slider)
            .aria_label("Preview size")
            .aria_value(format!("{value:.0} pixels"))
            .aria_numeric_value(value as f64)
            .aria_min_numeric_value(min as f64)
            .aria_max_numeric_value(max as f64)
            .aria_numeric_value_step(1.0)
            .on_prepaint({
                let geometry = geometry.clone();
                move |bounds, _, cx| {
                    if geometry.read(cx).bounds != Some(bounds) {
                        geometry.update(cx, |geometry, _| geometry.bounds = Some(bounds));
                    }
                }
            })
            .on_mouse_down(MouseButton::Left, {
                let geometry = geometry.clone();
                let on_change = on_change.clone();
                let focus_handle = focus_handle.clone();
                move |event: &MouseDownEvent, window, cx| {
                    focus_handle.focus(window, cx);
                    if let Some(bounds) = geometry.read(cx).bounds {
                        let value = value_at(event.position.x, bounds, min, max);
                        geometry.update(cx, |geometry, _| geometry.value = value);
                        on_change(value, window, cx);
                        cx.stop_propagation();
                    }
                }
            })
            .on_drag(DragPreviewSize, |drag, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| drag.clone())
            })
            .on_drag_move({
                let geometry = geometry.clone();
                let on_change = on_change.clone();
                move |event: &DragMoveEvent<DragPreviewSize>, window, cx| {
                    if let Some(bounds) = geometry.read(cx).bounds {
                        let value = value_at(event.event.position.x, bounds, min, max);
                        geometry.update(cx, |geometry, _| geometry.value = value);
                        on_change(value, window, cx);
                    }
                }
            })
            .on_mouse_up(MouseButton::Left, {
                let commit = commit.clone();
                move |_, window, cx| commit(window, cx)
            })
            .on_mouse_up_out(MouseButton::Left, {
                let commit = commit.clone();
                move |_, window, cx| commit(window, cx)
            })
            .on_key_down({
                let geometry = geometry.clone();
                let on_commit = on_commit.clone();
                let presets = presets.clone();
                move |event: &KeyDownEvent, window, cx| {
                    let current = geometry.read(cx).value;
                    let next = match event.keystroke.key.as_str() {
                        "left" | "down" => presets
                            .iter()
                            .rev()
                            .find(|preset| **preset < current - 0.01)
                            .copied()
                            .unwrap_or(min),
                        "right" | "up" => presets
                            .iter()
                            .find(|preset| **preset > current + 0.01)
                            .copied()
                            .unwrap_or(max),
                        "home" => min,
                        "end" => max,
                        _ => return,
                    };

                    geometry.update(cx, |geometry, _| geometry.value = next);
                    on_commit(next, window, cx);
                    cx.stop_propagation();
                }
            })
            .child(track)
            .child(labels);

        div()
            .w_full()
            .v_flex()
            // 保留一点水平内边距，避免 0%/100% 的 thumb 被裁切；
            // 不再画外层边框，焦点由 thumb 描边表达。
            .px_2()
            .py_1()
            .child(interactive)
    }
}

fn fraction(value: f32, min: f32, max: f32) -> f32 {
    if (max - min).abs() < f32::EPSILON {
        0.0
    } else {
        ((value - min) / (max - min)).clamp(0.0, 1.0)
    }
}

fn value_at(x: Pixels, bounds: Bounds<Pixels>, min: f32, max: f32) -> f32 {
    let width = bounds.size.width.as_f32().max(1.0);
    let percent = ((x - bounds.left()).as_f32() / width).clamp(0.0, 1.0);
    min + percent * (max - min)
}

/// 取距离 `value` 最近的预设点；正好位于两点中间时取较小的那个。
pub(crate) fn nearest_preset(value: f32, presets: &[f32]) -> f32 {
    presets
        .iter()
        .copied()
        .min_by(|a, b| (value - *a).abs().total_cmp(&(value - *b).abs()))
        .unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{point, size};

    const PRESETS: [f32; 4] = [32.0, 48.0, 72.0, 96.0];

    #[test]
    fn snapping_picks_the_nearest_preset() {
        assert_eq!(nearest_preset(33.0, &PRESETS), 32.0);
        assert_eq!(nearest_preset(59.0, &PRESETS), 48.0);
        assert_eq!(nearest_preset(61.0, &PRESETS), 72.0);
        assert_eq!(nearest_preset(95.0, &PRESETS), 96.0);
        // 60 到 48 和 72 等距，规则是取较小值。
        assert_eq!(nearest_preset(60.0, &PRESETS), 48.0);
        // 60.4 更靠近 72；不能先取整成 60 再判断。
        assert_eq!(nearest_preset(60.4, &PRESETS), 72.0);
    }

    #[test]
    fn pixel_positions_map_to_the_full_range() {
        let bounds = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(100.0), px(20.0)),
        };

        assert_eq!(value_at(px(0.0), bounds, 32.0, 96.0), 32.0);
        assert_eq!(value_at(px(50.0), bounds, 32.0, 96.0), 64.0);
        assert_eq!(value_at(px(100.0), bounds, 32.0, 96.0), 96.0);
        // 拖出轨道后夹紧到两端。
        assert_eq!(value_at(px(-20.0), bounds, 32.0, 96.0), 32.0);
        assert_eq!(value_at(px(140.0), bounds, 32.0, 96.0), 96.0);
    }

    #[test]
    fn fractions_are_linear_and_clamped() {
        assert_eq!(fraction(48.0, 32.0, 96.0), 0.25);
        assert_eq!(fraction(72.0, 32.0, 96.0), 0.625);
        assert_eq!(fraction(0.0, 32.0, 96.0), 0.0);
        assert_eq!(fraction(120.0, 32.0, 96.0), 1.0);
    }
}
