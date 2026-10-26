//! Omarchy presentation for the native editor's provider/menu state.
//! Deferred prepaint reads this frame's caret geometry, including editor scroll.
use crate::inline_completion::Menu;
use gpui_kit::{
    base::input::{EditorState, Enter, MoveDown, MoveUp},
    prelude::*,
    *,
};
use gpui_omarchy::ActiveTheme;
use std::{cell::RefCell, rc::Rc};

fn accept(
    menu: &Menu,
    index: usize,
    e: &mut EditorState,
    w: &mut Window,
    cx: &mut Context<EditorState>,
) -> bool {
    let Some(offered) = &menu.offered else {
        return false;
    };
    if e.value().as_ref() != offered.sql
        || e.cursor() != offered.caret
        || !e.selected_range().is_empty()
    {
        return false;
    }
    let Some(item) = menu.items.get(index) else {
        return false;
    };
    e.insert_completion(item, offered.caret..offered.caret, w, cx);
    e.dismiss_lsp_overlays(cx);
    true
}
fn sync(menu: &mut Menu, e: &EditorState) -> bool {
    let native = e.completion_menu_state();
    if !native.open
        || !e.selected_range().is_empty()
        || !menu
            .offered
            .as_ref()
            .is_some_and(|o| e.value().as_ref() == o.sql && e.cursor() == o.caret)
    {
        menu.deliberate = false;
        return false;
    }
    if menu.revision != native.revision() {
        menu.revision = native.revision();
        menu.cursor = 0;
        menu.deliberate = false;
        menu.items = native.items.clone();
    }
    !menu.items.is_empty()
}
pub fn handle_action(
    menu: &Rc<RefCell<Menu>>,
    action: &dyn Action,
    e: &mut EditorState,
    w: &mut Window,
    cx: &mut Context<EditorState>,
) -> bool {
    let mut state = menu.borrow_mut();
    if !sync(&mut state, e) {
        return false;
    }
    let count = state.items.len();
    if action.partial_eq(&MoveUp) {
        state.cursor = if state.deliberate {
            (state.cursor + count - 1) % count
        } else {
            count - 1
        };
        state.deliberate = true;
        cx.notify();
        return true;
    }
    if action.partial_eq(&MoveDown) {
        state.cursor = if state.deliberate {
            (state.cursor + 1) % count
        } else {
            0
        };
        state.deliberate = true;
        cx.notify();
        return true;
    }
    if action.partial_eq(&Enter {
        secondary: false,
        shift: false,
    }) && state.deliberate
    {
        let index = state.cursor;
        // Consume the choice synchronously against the checked snapshot.
        state.deliberate = false;
        let handled = accept(&state, index, e, w, cx);
        drop(state);
        *menu.borrow_mut() = Menu::default();
        return handled;
    }
    if Enter::is_primary(action) {
        *state = Menu::default();
        e.dismiss_lsp_overlays(cx);
    }
    false
}
pub fn accept_tab(
    editor: &Entity<EditorState>,
    menu: &Rc<RefCell<Menu>>,
    w: &mut Window,
    cx: &mut App,
) -> bool {
    if !editor.read(cx).completion_menu_state().open {
        return false;
    }
    editor.update(cx, |e, cx| {
        let mut state = menu.borrow_mut();
        if !sync(&mut state, e) {
            return false;
        }
        let accepted = accept(&state, state.cursor, e, w, cx);
        *state = Menu::default();
        accepted
    })
}
pub fn render(
    editor: &Entity<EditorState>,
    menu: &Rc<RefCell<Menu>>,
    workbench: WeakEntity<crate::app::Workbench>,
    tab: usize,
    cx: &mut App,
) -> AnyElement {
    let e = editor.read(cx);
    let mut state = menu.borrow_mut();
    if !sync(&mut state, e) {
        return div().into_any_element();
    }
    let t = cx.omarchy().clone();
    let start = state.cursor / 7 * 7;
    let mut list = div()
        .id("sql-completion-popup")
        .debug_selector(|| "sql-completion-popup".into())
        .w(px(340.))
        .flex()
        .flex_col()
        .border_1()
        .border_color(t.border)
        .bg(t.surface)
        .shadow_md()
        .text_size(px(12.));
    for (index, item) in state.items.iter().enumerate().skip(start).take(7) {
        let editor = editor.clone();
        let menu = menu.clone();
        let wb = workbench.clone();
        list = list.child(
            div()
                .id(("sql-candidate", index))
                .debug_selector(move || format!("sql-candidate-{index}"))
                .h(px(27.))
                .px_2()
                .flex()
                .items_center()
                .overflow_hidden()
                .cursor_pointer()
                .bg(if index == state.cursor && state.deliberate {
                    t.accent.opacity(0.18)
                } else {
                    t.surface
                })
                .text_color(t.foreground)
                .child(item.label.clone())
                .when(index == state.cursor, |d| {
                    d.child(
                        div()
                            .ml_2()
                            .text_size(px(10.))
                            .text_color(t.secondary)
                            .child(if state.deliberate {
                                "Enter ↵"
                            } else {
                                "Tab ⇥"
                            }),
                    )
                })
                .child(div().flex_1())
                .child(
                    div()
                        .text_color(t.secondary)
                        .text_size(px(10.))
                        .child(item.detail.clone().unwrap_or_default()),
                )
                .on_mouse_down(MouseButton::Left, move |_, w, cx| {
                    cx.stop_propagation();
                    w.prevent_default();
                    if wb.upgrade().is_some_and(|s| {
                        let s = s.read(cx);
                        s.inline_completion_allowed(tab, s.host_epoch)
                            && s.inline_offer_current(&menu.borrow())
                    }) {
                        editor.update(cx, |e, cx| {
                            accept(&menu.borrow(), index, e, w, cx);
                        });
                    }
                }),
        );
    }
    list = list.child(
        div()
            .px_2()
            .h(px(22.))
            .text_size(px(10.))
            .text_color(t.secondary)
            .debug_selector(|| "sql-completion-hint".into())
            .child(if state.deliberate {
                "↑↓ choose · Tab / Enter insert · Esc dismiss"
            } else {
                "↑↓ choose · Tab insert · Enter newline"
            }),
    );
    deferred(CaretPopup {
        editor: editor.clone(),
        child: list.into_any_element(),
    })
    .with_priority(2)
    .into_any_element()
}
struct CaretPopup {
    editor: Entity<EditorState>,
    child: AnyElement,
}
impl IntoElement for CaretPopup {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
impl Element for CaretPopup {
    type RequestLayoutState = LayoutId;
    type PrepaintState = bool;
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        w: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, LayoutId) {
        let child = self.child.request_layout(w, cx);
        (
            w.request_layout(
                Style {
                    position: Position::Absolute,
                    ..Default::default()
                },
                [child],
                cx,
            ),
            child,
        )
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        child: &mut LayoutId,
        w: &mut Window,
        cx: &mut App,
    ) -> bool {
        let Some(g) = self.editor.read(cx).prepaint_caret_geometry() else {
            return false;
        };
        let mut caret = g.cursor_bounds();
        caret.origin += g.scroll_offset();
        let input = g.input_bounds();
        if !input.intersects(&caret) {
            return false;
        }
        let size = w.layout_bounds(*child).size;
        let viewport = w.viewport_size();
        let x = caret
            .left()
            .clamp(px(4.), (viewport.width - size.width - px(4.)).max(px(4.)));
        let below = caret.bottom() + px(3.);
        let above = caret.top() - size.height - px(3.);
        let y = if below + size.height <= input.bottom().min(viewport.height - px(4.)) {
            below
        } else if above >= input.top() {
            above
        } else {
            below
                .min(viewport.height - size.height - px(4.))
                .max(px(4.))
        };
        w.with_element_offset(point(x, y) - bounds.origin, |w| self.child.prepaint(w, cx));
        true
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut LayoutId,
        shown: &mut bool,
        w: &mut Window,
        cx: &mut App,
    ) {
        if *shown {
            self.child.paint(w, cx);
        }
    }
}
