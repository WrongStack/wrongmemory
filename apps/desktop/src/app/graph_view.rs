use super::*;

impl Workbench {
    pub(super) fn relations(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let graph = self.graph_layout.clone();
        let zoom = self.graph_zoom;
        let edges = graph
            .edges
            .iter()
            .filter(|(a, b, _)| a != b)
            .map(|(a, b, relation)| {
                let a = &graph.nodes[*a];
                let b = &graph.nodes[*b];
                (
                    point(px((a.x + 210.) * zoom), px((a.y + 38.) * zoom)),
                    point(px(b.x * zoom), px((b.y + 38.) * zoom)),
                    relation == "contradicts",
                )
            })
            .collect::<Vec<_>>();
        let graph_height = (graph.height * zoom).max(400.);
        let graph_width = (graph.width * zoom).max(500.);
        let canvas = canvas(
            |_, _, _| (),
            move |bounds, _, window, cx| {
                for (a, b, contradiction) in &edges {
                    let a = bounds.origin + *a;
                    let b = bounds.origin + *b;
                    let bend = px(65. * zoom);
                    let mut path = PathBuilder::stroke(px(1.2));
                    path.move_to(a);
                    path.cubic_bezier_to(b, point(a.x + bend, a.y), point(b.x - bend, b.y));
                    if let Ok(path) = path.build() {
                        window.paint_path(
                            path,
                            if *contradiction {
                                cx.theme().danger
                            } else {
                                cx.theme().border
                            },
                        );
                    }
                }
            },
        )
        .absolute()
        .size_full();
        let board = div()
            .relative()
            .w(px(graph_width))
            .h(px(graph_height))
            .child(canvas)
            .children(graph.nodes.iter().enumerate().map(|(ix, node)| {
                let id = node.id.clone();
                let memory = node.memory_id.clone();
                div()
                    .id(("relation-node", ix))
                    .role(Role::Button)
                    .aria_label(format!("{}: {}", node.id, node.label))
                    .absolute()
                    .left(px(node.x * zoom))
                    .top(px(node.y * zoom))
                    .w(px(210. * zoom))
                    .h(px(76. * zoom))
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(if memory.is_some() {
                        cx.theme().primary
                    } else {
                        cx.theme().border
                    })
                    .bg(cx.theme().background)
                    .overflow_hidden()
                    .cursor_pointer()
                    .hover(|style| style.bg(cx.theme().accent))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(id) = memory.clone() {
                            this.inspect_id(id, window, cx);
                        } else {
                            this.query
                                .update(cx, |state, cx| state.set_value(id.clone(), window, cx));
                            this.load(window, cx);
                        }
                    }))
                    .child(caption(
                        if node.memory_id.is_some() {
                            "MEMORY"
                        } else {
                            node.id.split(':').next().unwrap_or("ANCHOR")
                        },
                        cx,
                    ))
                    .child(div().text_xs().child(preview(&node.label, 48)))
            }));
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(Input::new(&self.query))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .items_center()
                            .child(
                                Button::new("zoom-out")
                                    .ghost()
                                    .small()
                                    .label("−")
                                    .disabled(zoom <= 0.5)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.graph_zoom = (this.graph_zoom - 0.1).max(0.5);
                                        this.persist(cx);
                                        cx.notify();
                                    })),
                            )
                            .child(caption(format!("{:.0}%", zoom * 100.), cx))
                            .child(
                                Button::new("zoom-in")
                                    .ghost()
                                    .small()
                                    .label("+")
                                    .disabled(zoom >= 1.6)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.graph_zoom = (this.graph_zoom + 0.1).min(1.6);
                                        this.persist(cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("fit-graph")
                                    .ghost()
                                    .small()
                                    .label("Fit")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.graph_zoom = 0.6;
                                        this.persist(cx);
                                        this.graph_scroll.set_offset(point(px(0.), px(0.)));
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(caption(
                        format!(
                            "{} nodes · {} relations{}",
                            graph.nodes.len(),
                            graph.edges.len(),
                            if graph.truncated {
                                " · TRUNCATED: narrow the query"
                            } else {
                                ""
                            }
                        ),
                        cx,
                    ))
                    .child(caption(
                        "Scroll to pan · middle-button drag · select a memory to inspect",
                        cx,
                    )),
            )
            .child(
                div()
                    .id("relation-viewport")
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .track_scroll(&self.graph_scroll)
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.graph_drag = Some(event.position);
                            cx.notify();
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Middle,
                        cx.listener(|this, _, _, _| this.graph_drag = None),
                    )
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                        if event.pressed_button != Some(MouseButton::Middle) {
                            this.graph_drag = None;
                            return;
                        }
                        if let Some(last) = this.graph_drag.replace(event.position) {
                            this.graph_scroll
                                .set_offset(this.graph_scroll.offset() + event.position - last);
                            cx.notify();
                        }
                    }))
                    .child(if graph.nodes.is_empty() {
                        this_empty(self, cx)
                    } else {
                        board.into_any_element()
                    }),
            )
            .into_any_element()
    }
}

fn this_empty(this: &Workbench, cx: &App) -> AnyElement {
    this.empty("Explore a connection","Search for a memory, file, directory or symbol. Only the service-authorized subgraph is shown.",cx)
}
