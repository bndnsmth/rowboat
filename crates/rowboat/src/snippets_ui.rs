//! Reusable literal templates and native linked multi-selection placeholder editing.
use crate::{
    app::Workbench,
    forms,
    view::{action, column, label, row},
    workspace_model::Route,
};
use gpui_kit::{
    base::input::{InputState, TextareaState},
    prelude::*,
    *,
};
use gpui_omarchy::{self as ui, ActiveTheme};
use rowboat_services::{SnippetTabstop, SnippetTemplate};
use std::{collections::BTreeMap, ops::Range};

pub struct SnippetForm {
    original: Option<String>,
    id: String,
    name: Entity<InputState>,
    trigger: Entity<InputState>,
    body: Entity<TextareaState>,
}
pub struct SnippetSession {
    route: Route,
    tab: usize,
    text: String,
    stops: Vec<SnippetTabstop>,
    index: usize,
    final_caret: usize,
}

// Native multi-caret typing replaces all active ranges together. Accept only an identical
// replacement at every linked occurrence, preserving every byte outside those occurrences.
fn linked_edits(
    old: &str,
    new: &str,
    ranges: &[Range<usize>],
) -> Option<Vec<(Range<usize>, usize)>> {
    if ranges.is_empty() {
        return None;
    }
    let mut ranges = ranges.to_vec();
    ranges.sort_by_key(|r| r.start);
    if ranges.windows(2).any(|w| w[0].end > w[1].start) {
        return None;
    }
    let delta = new.len() as isize - old.len() as isize;
    if delta % ranges.len() as isize != 0 {
        return None;
    }
    let delta = delta / ranges.len() as isize;
    let mut cursor = 0;
    let mut out = 0;
    let mut replacement = None;
    let mut edits = vec![];
    for range in ranges {
        let length = (range.len() as isize + delta).try_into().ok()?;
        let unchanged = old.get(cursor..range.start)?;
        if new.get(out..out + unchanged.len())? != unchanged {
            return None;
        }
        out += unchanged.len();
        let value = new.get(out..out + length)?;
        if replacement.is_some_and(|prior| prior != value) {
            return None;
        }
        replacement = Some(value);
        out += length;
        cursor = range.end;
        edits.push((range, length));
    }
    (old.get(cursor..)? == new.get(out..)?).then_some(edits)
}
fn shifted(offset: usize, edits: &[(Range<usize>, usize)], end: bool) -> usize {
    let mut delta = 0isize;
    for (range, len) in edits {
        if offset < range.start || (offset == range.start && !end) {
            break;
        }
        if offset < range.end || (range.is_empty() && offset == range.start) {
            return (range.start as isize + delta) as usize + if end { *len } else { 0 };
        }
        delta += *len as isize - range.len() as isize;
    }
    (offset as isize + delta) as usize
}
impl Workbench {
    pub fn snippet_active(&self, tab: usize) -> bool {
        self.snippet_session
            .as_ref()
            .is_some_and(|s| s.route == self.tx.route && s.tab == tab)
    }
    pub fn show_snippets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.close_requested || self.has_extension_dialog() {
            return;
        }
        self.edit_snippet(None, window, cx);
    }
    fn edit_snippet(
        &mut self,
        template: Option<SnippetTemplate>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let original = template
            .as_ref()
            .map(|t| serde_json::to_string(t).expect("snippet serializes"));
        let template = template.unwrap_or_else(|| SnippetTemplate {
            id: uuid::Uuid::new_v4().to_string(),
            name: String::new(),
            trigger: String::new(),
            body: "SELECT ${column:*} FROM ${table} WHERE ${column} = ${1:value};${0}".into(),
        });
        self.snippet_form = Some(SnippetForm {
            original,
            id: template.id,
            name: forms::input(template.name, "Name", window, cx),
            trigger: forms::input(template.trigger, "ASCII trigger", window, cx),
            body: forms::textarea(template.body, window, cx),
        });
        window.focus(&self.focus, cx);
        cx.notify();
    }
    fn save_snippet(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &self.snippet_form else {
            return;
        };
        let template = SnippetTemplate {
            id: form.id.clone(),
            name: form.name.read(cx).value().to_string(),
            trigger: form.trigger.read(cx).value().to_string(),
            body: form.body.read(cx).value().to_string(),
        };
        let result = (|| {
            anyhow::ensure!(self.shared.borrow().writable, "App data is not writable");
            template.validate()?;
            let existing = &self.shared.borrow().state.snippet_templates;
            anyhow::ensure!(
                existing
                    .iter()
                    .find(|t| t.id == template.id)
                    .map(serde_json::to_string)
                    .transpose()?
                    == form.original,
                "Template changed in another window; reopen it before saving"
            );
            anyhow::ensure!(
                existing
                    .iter()
                    .all(|t| t.id == template.id || t.trigger != template.trigger),
                "Trigger already belongs to another template"
            );
            anyhow::ensure!(
                existing.len() < 256 || existing.iter().any(|t| t.id == template.id),
                "At most 256 templates"
            );
            anyhow::Ok(())
        })();
        match result {
            Ok(()) => {
                let original = serde_json::to_string(&template).expect("snippet serializes");
                self.edit_state(|state| {
                    state.snippet_templates.retain(|t| t.id != template.id);
                    state.snippet_templates.push(template);
                });
                if let Some(form) = &mut self.snippet_form {
                    form.original = Some(original);
                }
                self.status = "Reusable snippet template saved".into();
            }
            Err(e) => self.fail(format!("{e:#}")),
        }
        cx.notify();
    }
    pub fn insert_snippet(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.close_requested {
            return;
        }
        let Some(template) = self
            .shared
            .borrow()
            .state
            .snippet_templates
            .iter()
            .find(|t| t.id == id)
            .cloned()
        else {
            return;
        };
        let expansion = match rowboat_services::expand_snippet(&template, &BTreeMap::new()) {
            Ok(e) => e,
            Err(e) => {
                self.fail(format!("{e:#}"));
                return;
            }
        };
        let tab = &self.tabs[self.active_tab];
        let range = tab.editor.read(cx).selected_range();
        let base = range.start;
        let mut stops = expansion.tabstops;
        for stop in &mut stops {
            for r in &mut stop.ranges {
                r.start += base;
                r.end += base;
            }
        }
        let final_caret = base + expansion.final_caret;
        tab.editor.update(cx, |e, cx| {
            crate::sql_editor::replace_range(e, range, expansion.text, window, cx);
            e.focus(window, cx);
        });
        self.snippet_session = Some(SnippetSession {
            route: self.tx.route,
            tab: tab.id,
            text: tab.editor.read(cx).value().to_string(),
            stops,
            index: 0,
            final_caret,
        });
        self.snippet_form = None;
        self.select_snippet_stop(window, cx);
        self.status = "Snippet inserted literally · Tab / Shift+Tab navigate linked placeholders; Esc ends navigation".into();
        cx.notify();
    }
    fn select_snippet_stop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = &self.snippet_session else {
            return;
        };
        let ranges = session
            .stops
            .get(session.index)
            .map(|s| s.ranges.clone())
            .unwrap_or_else(|| std::iter::once(session.final_caret..session.final_caret).collect());
        self.tabs[self.active_tab].editor.update(cx, |e, cx| {
            if let Some(first) = ranges.first() {
                e.set_selected_range(first.clone(), cx);
                for range in &ranges[1..] {
                    e.add_selection(range.clone(), cx);
                }
            }
            e.focus(window, cx);
        });
        if session.index >= session.stops.len() {
            self.snippet_session = None;
        }
    }
    pub fn advance_snippet(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.navigate_snippet(false, window, cx)
    }
    pub fn previous_snippet(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.navigate_snippet(true, window, cx)
    }
    pub fn expand_snippet_trigger(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.close_requested || self.has_extension_dialog() {
            return;
        }
        let editor = self.tabs[self.active_tab].editor.clone();
        let text = editor.read(cx).value().to_string();
        let mut range = editor.read(cx).selected_range();
        if range.is_empty() {
            let caret = editor.read(cx).cursor();
            let mut start = caret;
            while start > 0
                && (text.as_bytes()[start - 1].is_ascii_alphanumeric()
                    || b"_-.".contains(&text.as_bytes()[start - 1]))
            {
                start -= 1;
            }
            range = start..caret;
        }
        let Some(id) = self
            .shared
            .borrow()
            .state
            .snippet_templates
            .iter()
            .find(|t| Some(t.trigger.as_str()) == text.get(range.clone()))
            .map(|t| t.id.clone())
        else {
            self.status="Select an exact snippet trigger or place the caret immediately after it, then Ctrl+Shift+Space".into();
            cx.notify();
            return;
        };
        editor.update(cx, |e, cx| e.set_selected_range(range, cx));
        self.insert_snippet(&id, window, cx);
    }
    fn navigate_snippet(
        &mut self,
        reverse: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.close_requested || self.busy {
            return false;
        }
        let Some(mut session) = self.snippet_session.take() else {
            return false;
        };
        let tab = &self.tabs[self.active_tab];
        if session.route != self.tx.route || session.tab != tab.id {
            return false;
        }
        let text = tab.editor.read(cx).value().to_string();
        if text != session.text {
            let Some(edits) = session
                .stops
                .get(session.index)
                .and_then(|stop| linked_edits(&session.text, &text, &stop.ranges))
            else {
                self.status =
                    "Snippet navigation ended: editing moved outside the linked placeholder".into();
                return false;
            };
            for stop in &mut session.stops {
                for range in &mut stop.ranges {
                    *range = shifted(range.start, &edits, false)..shifted(range.end, &edits, true);
                }
            }
            session.final_caret = shifted(session.final_caret, &edits, true);
        }
        session.text = text;
        session.index = if reverse {
            session.index.saturating_sub(1)
        } else {
            session.index + 1
        };
        self.snippet_session = Some(session);
        self.select_snippet_stop(window, cx);
        cx.notify();
        true
    }
    pub fn render_snippets(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let form = self.snippet_form.as_ref().unwrap();
        let theme = cx.omarchy().clone();
        let mut body = column().gap_3().p_4().text_size(px(13.))
            .child(row().child(ui::icon(ui::IconName::Code).text_color(theme.accent)).child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child("SQL templates")))
            .child(label("Reusable text with linked placeholders. Saved-query favorites keep complete SQL; templates insert text into the active editor without running it.",cx))
            .child(div().h(px(1.)).bg(theme.divider()))
            .child(row().flex_wrap().gap_3()
                .child(column().flex_1().min_w(px(220.)).gap_1().child(label("Template name", cx)).child(ui::input("snippet-name",&form.name,window,cx)))
                .child(column().flex_1().min_w(px(180.)).gap_1().child(label("Trigger · ASCII text", cx)).child(ui::input("snippet-trigger",&form.trigger,window,cx))))
            .child(label("SQL template", cx))
            .child(ui::textarea("snippet-body",&form.body,window,cx).h(px(180.)))
            .child(label("${name:default}, ${1:value}, ${0} final caret, $$ literal dollar. Repeated names create linked selections. Insertion replaces only the active selection; Tab navigates while editing a snippet.",cx))
            .child(row().flex_wrap().py_2().child(action("save-snippet","Save template",true,true,cx,|s,_,cx|s.save_snippet(cx)))
                .child(action("new-snippet","New",false,true,cx,|s,w,cx|s.edit_snippet(None,w,cx)))
                .child(div().flex_1())
                .child(action("close-snippets","Close",false,true,cx,|s,w,cx|{s.snippet_form=None;w.focus(&s.focus,cx);cx.notify();})))
            .child(div().pt_3().border_t_1().border_color(theme.divider()).font_weight(FontWeight::SEMIBOLD).child("Saved templates"));
        if self.state.snippet_templates.is_empty() {
            body = body.child(label("Save your first template above, then insert it here or expand its trigger in the editor.", cx));
        }
        for template in self.state.snippet_templates.iter().take(256) {
            let edit = template.clone();
            let insert = template.id.clone();
            let delete = template.clone();
            body = body.child(
                row()
                    .flex_wrap()
                    .py_2()
                    .child(
                        label(format!("{} · {}", template.name, template.trigger), cx)
                            .flex_1()
                            .min_w(px(180.))
                            .text_size(px(13.))
                            .text_color(theme.foreground),
                    )
                    .child(action(
                        format!("edit-template-{}", template.id),
                        "Edit",
                        false,
                        true,
                        cx,
                        move |s, w, cx| s.edit_snippet(Some(edit.clone()), w, cx),
                    ))
                    .child(action(
                        format!("insert-template-{}", template.id),
                        "Insert",
                        false,
                        true,
                        cx,
                        move |s, w, cx| s.insert_snippet(&insert, w, cx),
                    ))
                    .child(action(
                        format!("delete-template-{}", template.id),
                        "Delete",
                        false,
                        true,
                        cx,
                        move |s, _, cx| {
                            let current = s
                                .shared
                                .borrow()
                                .state
                                .snippet_templates
                                .iter()
                                .find(|t| t.id == delete.id)
                                .cloned();
                            if current
                                .as_ref()
                                .map(serde_json::to_value)
                                .transpose()
                                .ok()
                                .flatten()
                                != serde_json::to_value(&delete).ok()
                            {
                                s.fail(
                                    "Template changed in another window; reopen before deleting"
                                        .into(),
                                );
                                cx.notify();
                                return;
                            }
                            s.edit_state(|state| {
                                state.snippet_templates.retain(|t| t.id != delete.id)
                            });
                            cx.notify();
                        },
                    )),
            );
        }
        self.modal(
            div()
                .id("templates-scroll")
                .w(px(880.).min(window.viewport_size().width - px(48.)))
                .max_h(window.viewport_size().height - px(70.))
                .overflow_y_scroll()
                .child(body)
                .into_any_element(),
            cx,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{linked_edits, shifted};
    #[test]
    fn snippet_tab_binding_only_overrides_native_editing_in_its_scoped_editor_context() {
        use crate::app::{Dismiss, NextSnippet};
        use gpui_kit::{KeyBinding, KeyContext, Keymap, Keystroke};
        let map = Keymap::new(vec![
            KeyBinding::new("tab", Dismiss, Some("Input")),
            KeyBinding::new("tab", NextSnippet, Some("RowboatSnippet > Input")),
        ]);
        let keys = [Keystroke::parse("tab").unwrap()];
        let plain = [
            KeyContext::parse("Rowboat").unwrap(),
            KeyContext::parse("RowboatSqlEditor").unwrap(),
            KeyContext::parse("Input").unwrap(),
        ];
        let linked = [
            KeyContext::parse("Rowboat").unwrap(),
            KeyContext::parse("RowboatSqlEditor RowboatSnippet").unwrap(),
            KeyContext::parse("Input").unwrap(),
        ];
        let (normal, _) = map.bindings_for_input(&keys, &plain);
        assert!(normal[0].action().as_any().is::<Dismiss>());
        let (snippet, _) = map.bindings_for_input(&keys, &linked);
        assert!(snippet[0].action().as_any().is::<NextSnippet>());
    }
    #[test]
    fn adjacent_tabstops_and_final_caret_remain_after_the_replaced_placeholder() {
        let edits = linked_edits("ab", "longb", std::slice::from_ref(&(0..1))).unwrap();
        assert_eq!(shifted(1, &edits, false), 4);
        assert_eq!(shifted(2, &edits, true), 5);
    }
    #[test]
    fn linked_multicaret_unicode_replacements_rebase_later_tabstops_and_reject_outside_edits() {
        let old = "é foo + foo end";
        let new = "é λλ + λλ end";
        let edits = linked_edits(old, new, &[3..6, 9..12]).unwrap();
        assert_eq!(shifted(13, &edits, false), 15);
        assert_eq!(shifted(6, &edits, true), 7);
        assert!(linked_edits(old, "é foo + bar end", &[3..6, 9..12]).is_none());
        assert!(linked_edits(old, "x foo + foo end", &[3..6, 9..12]).is_none());
        assert!(linked_edits(old, "é foo + foo END", &[3..6, 9..12]).is_none());
    }
}
