//! 最小工作流画布探针：节点用 absolute div，连线用 canvas + PathBuilder 自绘。
//!
//! 验证目标：gpui-kit 0.8 下「可拖拽节点 + 贝塞尔连线 + 中键平移」这条节点图
//! 链路是否需要自定义 Element，还是 div + canvas 两层就够。
//!
//! 运行：cargo run --example canvas_probe

use gpui_kit::component::ActiveTheme;
use gpui_kit::{
    AppContext, Context, ElementId, Empty, InteractiveElement, IntoElement, MouseButton,
    ParentElement, PathBuilder, Pixels, Point, Render, StatefulInteractiveElement, Styled, Window,
    canvas, div, point, px,
};

const NODE_W: f32 = 150.0;
const NODE_H: f32 = 52.0;

#[derive(Clone)]
struct DragNode(usize);

impl Render for DragNode {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

pub struct GraphCanvas {
    nodes: Vec<(String, Pixels, Pixels)>,
    edges: Vec<(usize, usize)>,
    /// 画布平移量（world → screen 的偏移）。
    origin: Point<Pixels>,
    /// 中键拖拽时的上一个指针位置；None 表示没在平移。
    pan_anchor: Option<Point<Pixels>>,
}

impl GraphCanvas {
    fn new() -> Self {
        Self {
            nodes: vec![
                ("fetch".into(), px(120.0), px(80.0)),
                ("transform".into(), px(380.0), px(190.0)),
                ("sink".into(), px(660.0), px(90.0)),
            ],
            edges: vec![(0, 1), (1, 2)],
            origin: point(px(0.0), px(0.0)),
            pan_anchor: None,
        }
    }

    fn node_pos(&self, ix: usize) -> Point<Pixels> {
        let (_, x, y) = &self.nodes[ix];
        point(self.origin.x + *x, self.origin.y + *y)
    }
}

impl Render for GraphCanvas {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme_bg = cx.theme().background;
        let border = cx.theme().border;
        let primary = cx.theme().primary;
        let muted = cx.theme().muted_foreground;

        // 连线层：canvas 的 paint 回调拿不到 Context，所以把「世界坐标 → 屏幕坐标」
        // 算好的端点克隆进闭包，绘制期只加 bounds 原点。
        let edge_layer = {
            let mut segments = Vec::with_capacity(self.edges.len());
            for (from, to) in &self.edges {
                let start = self.node_pos(*from);
                let end = self.node_pos(*to);
                segments.push((point(start.x + px(NODE_W), start.y + px(NODE_H / 2.0)), end));
            }
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    for (start, end) in segments {
                        let from = point(bounds.origin.x + start.x, bounds.origin.y + start.y);
                        let to = point(bounds.origin.x + end.x, bounds.origin.y + end.y);
                        let bend = (to.x - from.x).as_f32() * 0.5;
                        let mut builder = PathBuilder::stroke(px(2.0));
                        builder.move_to(from);
                        builder.cubic_bezier_to(
                            to,
                            point(from.x + px(bend), from.y),
                            point(to.x - px(bend), to.y),
                        );
                        if let Ok(path) = builder.build() {
                            window.paint_path(path, primary);
                        }
                    }
                },
            )
            .absolute()
            .left(px(0.0))
            .top(px(0.0))
            .size_full()
        };

        let weak = cx.entity().downgrade();
        let mut nodes_layer = div().absolute().left(px(0.0)).top(px(0.0)).size_full();
        for (ix, (label, _, _)) in self.nodes.iter().enumerate() {
            let pos = self.node_pos(ix);
            let target = weak.clone();
            nodes_layer = nodes_layer.child(
                div()
                    .id(ElementId::Name(format!("node-{ix}").into()))
                    .absolute()
                    .left(pos.x)
                    .top(pos.y)
                    .w(px(NODE_W))
                    .h(px(NODE_H))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(border)
                    .bg(theme_bg)
                    .text_color(muted)
                    .child(label.clone())
                    .on_drag(DragNode(ix), move |_, _, _, cx| {
                        cx.stop_propagation();
                        cx.new(|_| DragNode(ix))
                    })
                    .on_drag_move(
                        move |event: &gpui_kit::DragMoveEvent<DragNode>, window, cx| {
                            let Some(target) = target.upgrade() else {
                                return;
                            };
                            let ix = event.drag(cx).0;
                            // MouseMoveEvent 没有 delta，用相对原点的位移反推增量。
                            let rel = point(
                                event.event.position.x - window.bounds().origin.x,
                                event.event.position.y - window.bounds().origin.y,
                            );
                            target.update(cx, |this, cx| {
                                let (_, x, y) = &mut this.nodes[ix];
                                *x = rel.x - px(NODE_W / 2.0) - this.origin.x;
                                *y = rel.y - px(NODE_H / 2.0) - this.origin.y;
                                cx.notify();
                            });
                            cx.stop_propagation();
                        },
                    ),
            );
        }

        div()
            .id("graph-viewport")
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(cx.theme().accent)
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, event: &gpui_kit::MouseDownEvent, _, _| {
                    this.pan_anchor = Some(event.position);
                }),
            )
            .on_mouse_move(
                cx.listener(|this, event: &gpui_kit::MouseMoveEvent, _, cx| {
                    let Some(anchor) = this.pan_anchor else {
                        return;
                    };
                    this.origin.x += event.position.x - anchor.x;
                    this.origin.y += event.position.y - anchor.y;
                    this.pan_anchor = Some(event.position);
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|this, _, _, _| {
                    this.pan_anchor = None;
                }),
            )
            .child(edge_layer)
            .child(nodes_layer)
            .child(
                div()
                    .absolute()
                    .left(px(12.0))
                    .bottom(px(8.0))
                    .text_xs()
                    .text_color(muted)
                    .child("拖动节点 / 按住中键平移 —— 验证 absolute div + canvas 两层链路"),
            )
    }
}

fn main() {
    let options = gpui_kit::WindowOptions {
        titlebar: Some(gpui_kit::TitlebarOptions {
            title: Some("Canvas Probe".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            gpui_kit::init(cx);
            gpui_kit::open_window(options, cx, |_, cx| cx.new(|_| GraphCanvas::new()))
                .expect("failed to open window");
        });
}
