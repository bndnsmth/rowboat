//! Shared, window-local dialog sizing. Resizing never changes database state or
//! persisted preferences; each dialog kind remembers its own size in memory.
use gpui_kit::{prelude::*, *};
use gpui_omarchy::{self as ui, ActiveTheme};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

#[derive(Clone, Default)]
pub struct DialogLayout(Rc<RefCell<Layout>>);
#[derive(Default)]
struct Layout {
    entries: BTreeMap<&'static str, Geometry>,
    grips: BTreeMap<&'static str, FocusHandle>,
    drag: Option<Drag>,
    active: Option<&'static str>,
}
#[derive(Clone, Copy, Default)]
struct Geometry {
    requested: Option<Size<Pixels>>,
    measured: Option<Size<Pixels>>,
    maximized: bool,
}
#[derive(Clone, Copy)]
struct Drag {
    kind: &'static str,
    origin: Point<Pixels>,
    size: Size<Pixels>,
}

fn available(viewport: Size<Pixels>) -> Size<Pixels> {
    size(
        (viewport.width - px(32.)).max(px(200.)),
        (viewport.height - px(32.)).max(px(160.)),
    )
}
fn clamp(wanted: Size<Pixels>, viewport: Size<Pixels>) -> Size<Pixels> {
    let max = available(viewport);
    size(
        wanted.width.max(px(420.).min(max.width)).min(max.width),
        wanted.height.max(px(240.).min(max.height)).min(max.height),
    )
}
impl DialogLayout {
    pub fn stop_drag(&self) {
        self.0.borrow_mut().drag = None;
    }
    pub fn size(&self, kind: &'static str, viewport: Size<Pixels>) -> Option<Size<Pixels>> {
        let geometry = self
            .0
            .borrow()
            .entries
            .get(kind)
            .copied()
            .unwrap_or_default();
        if geometry.maximized {
            Some(available(viewport))
        } else {
            geometry.requested.map(|s| clamp(s, viewport))
        }
    }
    fn begin(&self, kind: &'static str, origin: Point<Pixels>) {
        let mut layout = self.0.borrow_mut();
        if let Some(geometry) = layout.entries.get_mut(kind)
            && let Some(size) = geometry.measured
        {
            if geometry.maximized {
                geometry.requested = Some(size);
            }
            geometry.maximized = false;
            layout.drag = Some(Drag { kind, origin, size });
        }
    }
    fn move_to(&self, position: Point<Pixels>, pressed: bool, viewport: Size<Pixels>) -> bool {
        let mut layout = self.0.borrow_mut();
        if !pressed {
            layout.drag = None;
            return false;
        }
        let Some(drag) = layout.drag else {
            return false;
        };
        // The modal remains centered, so each edge moves half the size delta.
        let wanted = size(
            drag.size.width + (position.x - drag.origin.x) * 2.,
            drag.size.height + (position.y - drag.origin.y) * 2.,
        );
        layout.entries.entry(drag.kind).or_default().requested = Some(clamp(wanted, viewport));
        true
    }
    fn toggle_maximize(&self, kind: &'static str) {
        let mut layout = self.0.borrow_mut();
        layout.drag = None;
        let geometry = layout.entries.entry(kind).or_default();
        geometry.maximized = !geometry.maximized;
    }

    /// Wrap an existing dialog surface. Its original natural size remains the
    /// default; explicit user sizing constrains a scrollable content viewport.
    pub fn overlay<T: 'static, S: IntoElement + Styled>(
        &self,
        kind: &'static str,
        viewport: Size<Pixels>,
        focus: &FocusHandle,
        surface: S,
        cx: &mut Context<T>,
    ) -> AnyElement {
        {
            let mut layout = self.0.borrow_mut();
            if layout.active != Some(kind) {
                layout.drag = None;
                layout.active = Some(kind);
            }
        }
        let theme = cx.omarchy().clone();
        let explicit = self.size(kind, viewport);
        let maximized = self
            .0
            .borrow()
            .entries
            .get(kind)
            .is_some_and(|g| g.maximized);
        let resize = self.clone();
        let resize_key = self.clone();
        let maximize = self.clone();
        let measure = self.clone();
        let movement = self.clone();
        let release = self.clone();
        let leave = self.clone();
        let selector = format!("dialog-frame-{kind}");
        let grip_selector = format!("dialog-resize-{kind}");
        let max_selector = format!("dialog-maximize-{kind}");
        let grip_focus = self
            .0
            .borrow_mut()
            .grips
            .entry(kind)
            .or_insert_with(|| cx.focus_handle())
            .clone();
        let resize_bar = div()
            .flex()
            .items_center()
            .justify_end()
            .gap_1()
            .h(px(24.))
            .flex_shrink_0()
            .border_t_1()
            .border_color(theme.divider())
            .child(
                div()
                    .flex_1()
                    .px_3()
                    .text_size(px(10.))
                    .text_color(theme.secondary)
                    .child("Drag corner to resize"),
            )
            .child(ui::with_tooltip(
                ui::icon_button(
                    SharedString::from(max_selector.clone()),
                    if maximized {
                        ui::IconName::Minimize2
                    } else {
                        ui::IconName::Maximize2
                    },
                    if maximized {
                        "Restore dialog size"
                    } else {
                        "Maximize dialog"
                    },
                    ui::ButtonVariant::Secondary,
                    cx,
                )
                .debug_selector(move || max_selector.clone())
                .size(px(24.))
                .on_click(cx.listener(move |_, _, _, cx| {
                    maximize.toggle_maximize(kind);
                    cx.notify();
                })),
                if maximized {
                    "Restore dialog size"
                } else {
                    "Maximize dialog"
                },
            ))
            .child(
                ui::button(
                    SharedString::from(grip_selector.clone()),
                    "",
                    ui::ButtonVariant::Secondary,
                    cx,
                )
                .debug_selector(move || grip_selector.clone())
                .track_focus(&grip_focus)
                .accessibility_label("Resize dialog; use arrow keys when focused")
                .size(px(24.))
                .p_1()
                .cursor(CursorStyle::ResizeUpLeftDownRight)
                .child(
                    ui::icon(ui::IconName::Grip)
                        .size(px(14.))
                        .text_color(theme.secondary),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |_, event: &MouseDownEvent, window, cx| {
                        window.focus(&grip_focus, cx);
                        resize.begin(kind, event.position);
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
                .on_key_down(cx.listener(move |_, event: &KeyDownEvent, _, cx| {
                    let step = if event.keystroke.modifiers.shift {
                        px(8.)
                    } else {
                        px(32.)
                    };
                    let delta = match event.keystroke.key.as_str() {
                        "left" => size(-step, px(0.)),
                        "right" => size(step, px(0.)),
                        "up" => size(px(0.), -step),
                        "down" => size(px(0.), step),
                        _ => return,
                    };
                    let mut layout = resize_key.0.borrow_mut();
                    let geometry = layout.entries.entry(kind).or_default();
                    if let Some(current) = geometry.measured {
                        geometry.maximized = false;
                        geometry.requested = Some(clamp(
                            size(current.width + delta.width, current.height + delta.height),
                            viewport,
                        ));
                    }
                    cx.stop_propagation();
                    cx.notify();
                })),
            );
        let frame = div()
            .id(SharedString::from(selector.clone()))
            .debug_selector(move || selector.clone())
            .track_focus(focus)
            // Include the resize/maximize footer in the modal's keyboard scope,
            // not just the original surface, so Tab cannot escape the dialog.
            .key_context("RowboatModal")
            .flex()
            .flex_col()
            .min_w_0()
            .min_h_0()
            .max_w(available(viewport).width)
            .max_h(available(viewport).height)
            .when_some(explicit, |frame, size| frame.w(size.width).h(size.height))
            .bg(theme.surface.opacity(1.))
            .border_1()
            .border_color(theme.border)
            .rounded(px(8.))
            .shadow_lg()
            .overflow_hidden()
            .child(
                div()
                    .id(SharedString::from(format!("dialog-content-{kind}")))
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .min_h_0()
                    .flex_1()
                    .overflow_scroll()
                    .child(surface.when(explicit.is_some(), |surface| {
                        surface.w_full().h_full().max_w_full().max_h_full()
                    })),
            )
            .child(resize_bar);
        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(black().opacity(0.55))
            .on_children_prepainted(move |bounds, _, _| {
                if let Some(bounds) = bounds.first() {
                    measure
                        .0
                        .borrow_mut()
                        .entries
                        .entry(kind)
                        .or_default()
                        .measured = Some(bounds.size);
                }
            })
            .on_mouse_move(cx.listener(move |_, event: &MouseMoveEvent, _, cx| {
                if movement.move_to(event.position, event.dragging(), viewport) {
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |_, _, _, _| release.stop_drag()),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(move |_, _, _, _| leave.stop_drag()),
            )
            .child(frame)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{DialogLayout, Geometry, available};
    use gpui_kit::{point, px, size};
    #[test]
    fn resizing_is_bounded_per_dialog_and_maximize_restores_the_previous_size() {
        let layout = DialogLayout::default();
        let viewport = size(px(1000.), px(700.));
        layout.0.borrow_mut().entries.insert(
            "first",
            Geometry {
                measured: Some(size(px(600.), px(400.))),
                ..Default::default()
            },
        );
        layout.begin("first", point(px(400.), px(400.)));
        assert!(layout.move_to(point(px(500.), px(450.)), true, viewport));
        assert_eq!(
            layout.size("first", viewport),
            Some(size(px(800.), px(500.)))
        );
        assert!(layout.size("second", viewport).is_none());
        layout.toggle_maximize("first");
        assert_eq!(layout.size("first", viewport), Some(available(viewport)));
        layout.toggle_maximize("first");
        assert_eq!(
            layout.size("first", viewport),
            Some(size(px(800.), px(500.)))
        );
        let narrow = size(px(500.), px(350.));
        assert_eq!(layout.size("first", narrow), Some(available(narrow)));
        layout.stop_drag();
        assert!(!layout.move_to(point(px(0.), px(0.)), true, viewport));
    }
}
