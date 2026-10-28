//! Shell-owned profile browser. Browsing never creates a workspace or reads credentials.
use super::*;
use crate::view::{column, icon, label, row};
use gpui_kit::base::input::{InputEvent, InputState};

pub(super) struct Manager {
    filter: Entity<InputState>,
    scroll: ScrollHandle,
    editor: Option<Editor>,
    removal: Option<Removal>,
    message: Option<String>,
    error: Option<String>,
    return_focus: Option<FocusHandle>,
    _filter_subscription: Subscription,
}
impl Manager {
    pub(super) fn has_pending_dialog(&self) -> bool {
        self.editor.is_some() || self.removal.is_some()
    }
}
struct Editor {
    entity: Entity<Workbench>,
    original: Option<ConnectionProfile>,
    saved: bool,
    connect: bool,
    _observer: Subscription,
}
struct Removal {
    profile: ConnectionProfile,
    forget_password: bool,
    request: Option<uuid::Uuid>,
}

fn target(profile: &ConnectionProfile) -> String {
    if profile.engine == rowboat_core::Engine::Sqlite {
        profile
            .file
            .as_ref()
            .map(|f| f.path.display().to_string())
            .unwrap_or_else(|| "No database file".into())
    } else {
        format!("{}:{} / {}", profile.host, profile.port, profile.database)
    }
}
fn same_profile(a: &ConnectionProfile, b: &ConnectionProfile) -> bool {
    serde_json::to_value(a).ok() == serde_json::to_value(b).ok()
}
fn button(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    primary: bool,
    cx: &App,
) -> ui::Button {
    let t = cx.omarchy();
    ui::button(id, text, ui::ButtonVariant::Secondary, cx)
        .text_size(px(13.))
        .min_h(px(28.))
        .rounded(px(4.))
        .when(primary, |b| {
            b.bg(t.accent)
                .text_color(t.on_accent)
                .font_weight(FontWeight::SEMIBOLD)
                .hover(|s| s.bg(t.accent).border_color(t.on_accent))
                .focus_visible(|s| s.bg(t.accent).border_color(t.on_accent))
        })
}

impl WorkspaceShell {
    pub(super) fn has_main_workspace(&self) -> bool {
        self.workspaces
            .iter()
            .any(|workspace| workspace.host.is_none())
    }
    pub(super) fn connection_manager_blocks_workspaces(&self) -> bool {
        self.connection_manager
            .as_ref()
            .is_some_and(|manager| self.has_main_workspace() || manager.has_pending_dialog())
    }
    pub(super) fn ensure_connection_manager(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if !self.has_main_workspace() {
            // The empty main window is a browser, not a modal over satellite
            // workspaces. Do not raise this window or freeze their live sessions.
            self.open_connection_manager(false, w, cx);
        }
    }
    pub(super) fn show_connection_manager(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        self.open_connection_manager(true, w, cx);
    }
    fn open_connection_manager(&mut self, activate: bool, w: &mut Window, cx: &mut Context<Self>) {
        if !self.initialized
            || self.shutting_down
            || self.closing.is_some()
            || self.restore.is_some()
            || self.live_diff.is_some()
        {
            return;
        }
        if self.connection_manager.is_some() {
            if activate {
                w.activate_window();
            }
            return;
        }
        if activate
            && self.workspaces.iter().any(|workspace| {
                let s = workspace.entity.read(cx);
                s.modal_open() || s.has_row_draft() || s.close_requested
            })
        {
            self.error = Some(
                "Finish or close the open workspace dialog before managing connections.".into(),
            );
            cx.notify();
            return;
        }
        let return_focus = w.focused(cx);
        let filter = crate::forms::input("", "Search name, group, engine or target…", w, cx);
        let subscription = cx.subscribe(&filter, |s, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                if let Some(manager) = &s.connection_manager {
                    manager.scroll.set_offset(point(px(0.), px(0.)));
                }
                cx.notify();
            }
        });
        self.connection_manager = Some(Manager {
            filter: filter.clone(),
            scroll: ScrollHandle::new(),
            editor: None,
            removal: None,
            message: None,
            error: None,
            return_focus,
            _filter_subscription: subscription,
        });
        self.shared.borrow_mut().shell_modal_open = self.connection_manager_blocks_workspaces();
        if activate {
            w.activate_window();
        }
        filter.update(cx, |s, cx| s.focus(w, cx));
        cx.notify();
    }
    pub(super) fn manager_busy(&self, cx: &App) -> bool {
        self.connection_manager.as_ref().is_some_and(|m| {
            m.removal.as_ref().is_some_and(|r| r.request.is_some())
                || m.editor.as_ref().is_some_and(|e| e.entity.read(cx).busy)
        })
    }
    pub(super) fn dismiss_connection_manager(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if self.manager_busy(cx) {
            return;
        }
        let home = !self.has_main_workspace();
        let Some(manager) = &mut self.connection_manager else {
            return;
        };
        if manager.removal.take().is_some() || manager.editor.take().is_some() {
            manager.error = None;
            manager.filter.update(cx, |s, cx| s.focus(w, cx));
        } else if home {
            // There is no workspace to return to. Escape stays on Connections;
            // the native window close action still runs the normal close guard.
            manager.filter.update(cx, |s, cx| s.focus(w, cx));
        } else {
            let focus = manager.return_focus.clone();
            self.connection_manager = None;
            self.shared.borrow_mut().shell_modal_open = false;
            if let Some(focus) = focus {
                w.focus(&focus, cx);
            } else {
                self.activate(self.active, w, cx);
            }
        }
        self.shared.borrow_mut().shell_modal_open = self.connection_manager_blocks_workspaces();
        cx.notify();
    }
    fn profile_current(&self, profile: &ConnectionProfile) -> bool {
        self.shared
            .borrow()
            .state
            .connections
            .iter()
            .any(|p| p.id == profile.id && same_profile(p, profile))
    }
    fn remove_reason(&self, id: &str, cx: &App) -> Option<&'static str> {
        if !self.shared.borrow().writable {
            return Some("Profile storage is read-only or unavailable");
        }
        if self.workspaces.iter().any(|w| {
            let s = w.entity.read(cx);
            s.owner.as_deref() == Some(id) || s.form.as_ref().is_some_and(|f| f.id == id)
        }) {
            return Some("Close this profile's workspace or connection form before removing it");
        }
        None
    }
    fn manager_profiles(&self, cx: &App) -> Vec<ConnectionProfile> {
        let filter = self
            .connection_manager
            .as_ref()
            .map(|m| m.filter.read(cx).value().to_lowercase())
            .unwrap_or_default();
        let mut profiles = self
            .shared
            .borrow()
            .state
            .connections
            .iter()
            .filter(|p| {
                let text = format!(
                    "{} {} {} {}",
                    p.name,
                    p.group,
                    p.engine.descriptor().name,
                    target(p)
                )
                .to_lowercase();
                filter.split_whitespace().all(|word| text.contains(word))
            })
            .cloned()
            .collect::<Vec<_>>();
        profiles.sort_by(|a, b| (&a.group, &a.name, &a.id).cmp(&(&b.group, &b.name, &b.id)));
        profiles
    }
    fn manager_open(&mut self, profile: ConnectionProfile, w: &mut Window, cx: &mut Context<Self>) {
        if self
            .connection_manager
            .as_ref()
            .is_none_or(|m| m.editor.is_some() || m.removal.is_some())
            || !self.profile_current(&profile)
        {
            return;
        }
        self.connection_manager = None;
        self.shared.borrow_mut().shell_modal_open = false;
        self.open_managed_profile(profile, None, w, cx);
    }
    fn open_managed_profile(
        &mut self,
        profile: ConnectionProfile,
        password: Option<String>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let owner = Some(profile.id.clone());
        if let Some(id) = self.ownership.find(&owner) {
            // Even an offline retained workspace is only activated. Never replace
            // its physical session, pending form, SQL or results from the browser.
            self.activate(id, w, cx);
            self.ensure_connection_manager(w, cx);
            return;
        }
        let archive = self
            .archived
            .iter()
            .position(|a| a.owner == owner)
            .map(|i| self.archived.remove(i));
        let (tabs, layout) = archive
            .map(|a| (a.tabs, Some(a.layout)))
            .unwrap_or_default();
        let id = self.create_workspace(
            owner,
            tabs,
            layout,
            Startup {
                connection: Some(profile.id),
                password_supplied: password.is_some(),
                password: password.unwrap_or_default(),
                autorun: false,
                ..Default::default()
            },
            w,
            cx,
        );
        self.activate(id, w, cx);
        self.ensure_connection_manager(w, cx);
    }
    fn manager_draft(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if self
            .connection_manager
            .as_ref()
            .is_none_or(|m| m.editor.is_some() || m.removal.is_some())
        {
            return;
        }
        self.connection_manager = None;
        self.shared.borrow_mut().shell_modal_open = false;
        let id = if let Some(id) = self.ownership.find(&None) {
            id
        } else {
            let archive = self
                .archived
                .iter()
                .position(|a| a.owner.is_none())
                .map(|i| self.archived.remove(i));
            let (tabs, layout) = archive
                .map(|a| (a.tabs, Some(a.layout)))
                .unwrap_or_default();
            self.create_workspace(None, tabs, layout, Startup::default(), w, cx)
        };
        self.activate(id, w, cx);
        self.ensure_connection_manager(w, cx);
    }
    fn profile_form_owner(&mut self, w: &mut Window, cx: &mut Context<Self>) -> Entity<Workbench> {
        let id = self.next_workspace;
        self.next_workspace += 1;
        let init = WorkspaceInit {
            id,
            owner: None,
            shared: self.shared.clone(),
            shell_tx: self.command_tx.clone(),
        };
        cx.new(|cx| {
            let mut s = Workbench::new(self.runtime.clone(), Startup::default(), init, w, cx);
            s.profile_editor = true;
            s
        })
    }
    pub(super) fn manager_edit(
        &mut self,
        profile: Option<ConnectionProfile>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.shared.borrow().writable
            || self
                .connection_manager
                .as_ref()
                .is_none_or(|m| m.editor.is_some() || m.removal.is_some())
        {
            return;
        }
        if profile.as_ref().is_some_and(|p| !self.profile_current(p)) {
            return;
        }
        let entity = self.profile_form_owner(w, cx);
        entity.update(cx, |s, cx| s.edit_connection(profile.clone(), w, cx));
        let observer = cx.observe(&entity, |_, _, cx| cx.notify());
        let manager = self.connection_manager.as_mut().unwrap();
        manager.error = None;
        manager.editor = Some(Editor {
            entity,
            original: profile,
            saved: false,
            connect: false,
            _observer: observer,
        });
        self.shared.borrow_mut().shell_modal_open = true;
        cx.notify();
    }
    fn manager_transfer(&mut self, import: bool, w: &mut Window, cx: &mut Context<Self>) {
        if self
            .connection_manager
            .as_ref()
            .is_none_or(|m| m.editor.is_some() || m.removal.is_some())
            || !self.shared.borrow().writable
        {
            return;
        }
        let entity = self.profile_form_owner(w, cx);
        entity.update(cx, |s, cx| {
            if import {
                s.import_profiles(cx);
            } else {
                s.export_profiles(cx);
            }
        });
        let observer = cx.observe(&entity, |_, _, cx| cx.notify());
        self.connection_manager.as_mut().unwrap().editor = Some(Editor {
            entity,
            original: None,
            saved: false,
            connect: false,
            _observer: observer,
        });
        self.shared.borrow_mut().shell_modal_open = true;
        cx.notify();
    }
    pub(super) fn manager_save(
        &mut self,
        route: crate::workspace_model::Route,
        save: &crate::app::ProfileSave,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(editor) = self
            .connection_manager
            .as_ref()
            .and_then(|m| m.editor.as_ref())
        else {
            return false;
        };
        if editor.entity.read(cx).tx.route != route {
            return false;
        }
        let valid = self.shared.borrow().writable
            && self.closing.is_none()
            && self.restore.is_none()
            && match &editor.original {
                Some(original) => original.id == save.profile.id && self.profile_current(original),
                None => !self
                    .shared
                    .borrow()
                    .state
                    .connections
                    .iter()
                    .any(|p| p.id == save.profile.id),
            }
            && editor
                .entity
                .read(cx)
                .form
                .as_ref()
                .is_some_and(|f| f.profile(cx).is_ok_and(|p| same_profile(&p, &save.profile)));
        if !valid {
            editor.entity.update(cx, |s, cx| { s.busy = false; s.fail("Saved profile changed or storage is unavailable. Close this form and reopen it before saving.".into()); cx.notify(); });
            return true;
        }
        let editor = self
            .connection_manager
            .as_mut()
            .unwrap()
            .editor
            .as_mut()
            .unwrap();
        editor.saved = true;
        editor.connect = save.connect;
        editor.original = Some(save.profile.clone());
        editor.entity.update(cx, |s, cx| {
            s.error = None;
            s.commit_profile_save(
                crate::app::ProfileSave {
                    profile: save.profile.clone(),
                    password: save.password.clone(),
                    connect: false,
                    remember: save.remember,
                    exists: save.exists,
                },
                cx,
            );
        });
        true
    }
    pub(super) fn poll_connection_manager(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let done = self
            .connection_manager
            .as_ref()
            .and_then(|m| m.editor.as_ref())
            .is_some_and(|e| {
                let s = e.entity.read(cx);
                !s.busy && s.form.is_none()
            });
        if !done {
            return;
        }
        let manager = self.connection_manager.as_mut().unwrap();
        let editor = manager.editor.take().unwrap();
        let s = editor.entity.read(cx);
        manager.error = s.error.clone();
        manager.message = Some(s.status.clone());
        let open = editor
            .original
            .filter(|_| editor.saved && editor.connect && s.error.is_none());
        let password = open.as_ref().and_then(|p| s.passwords.get(&p.id).cloned());
        if let Some(profile) = open {
            self.connection_manager = None;
            self.shared.borrow_mut().shell_modal_open = false;
            self.open_managed_profile(profile, password, w, cx);
        } else {
            manager.filter.update(cx, |s, cx| s.focus(w, cx));
        }
        self.shared.borrow_mut().shell_modal_open = self.connection_manager_blocks_workspaces();
        cx.notify();
    }
    fn manager_remove(
        &mut self,
        profile: ConnectionProfile,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .connection_manager
            .as_ref()
            .is_none_or(|m| m.editor.is_some() || m.removal.is_some())
            || self.remove_reason(&profile.id, cx).is_some()
            || !self.profile_current(&profile)
        {
            return;
        }
        let manager = self.connection_manager.as_mut().unwrap();
        manager.error = None;
        manager.removal = Some(Removal {
            forget_password: profile.engine != rowboat_core::Engine::Sqlite,
            profile,
            request: None,
        });
        self.shared.borrow_mut().shell_modal_open = true;
        w.focus(&self.modal_focus, cx);
        cx.notify();
    }
    fn manager_confirm_remove(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let Some(removal) = self
            .connection_manager
            .as_ref()
            .and_then(|m| m.removal.as_ref())
        else {
            return;
        };
        if removal.request.is_some() {
            return;
        }
        let error = self.remove_reason(&removal.profile.id, cx).map(str::to_owned)
            .or_else(|| (!self.profile_current(&removal.profile)).then(|| "Saved profile changed. Cancel and review its current details before removing it.".into()));
        if let Some(error) = error {
            self.connection_manager.as_mut().unwrap().error = Some(error);
            cx.notify();
            return;
        }
        let id = removal.profile.id.clone();
        let forget = removal.forget_password;
        let request = uuid::Uuid::new_v4();
        self.connection_manager
            .as_mut()
            .unwrap()
            .removal
            .as_mut()
            .unwrap()
            .request = Some(request);
        if forget {
            let tx = self.command_tx.clone();
            self.runtime.spawn_blocking(move || {
                let result = rowboat_services::credential_delete(&id).map_err(|e| {
                    format!(
                        "Could not forget the stored password: {e:#}. The saved profile was kept."
                    )
                });
                let _ = tx.send(Command::ProfileRemoved { request, result });
            });
        } else {
            self.manager_removed(request, Ok(()), w, cx);
        }
        self.shared.borrow_mut().shell_modal_open = self.connection_manager_blocks_workspaces();
        cx.notify();
    }
    pub(super) fn manager_removed(
        &mut self,
        request: uuid::Uuid,
        result: Result<(), String>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(removal) = self
            .connection_manager
            .as_ref()
            .and_then(|m| m.removal.as_ref())
            .filter(|r| r.request == Some(request))
        else {
            return;
        };
        let error = result
            .err()
            .or_else(|| {
                self.remove_reason(&removal.profile.id, cx)
                    .map(str::to_owned)
            })
            .or_else(|| {
                (!self.profile_current(&removal.profile)).then(|| {
                    "Saved profile changed during removal; its configuration was kept.".into()
                })
            });
        if let Some(error) = error {
            let m = self.connection_manager.as_mut().unwrap();
            m.error = Some(error);
            m.removal.as_mut().unwrap().request = None;
        } else {
            let id = removal.profile.id.clone();
            self.shared.borrow_mut().edit(|state| {
                state.connections.retain(|p| p.id != id);
                if state.last_connection_id.as_deref() == Some(&id) {
                    state.last_connection_id = None;
                }
            });
            let m = self.connection_manager.as_mut().unwrap();
            m.removal = None;
            m.error = None;
            m.message = Some(
                "Saved connection removed. Database files, query drafts and history were kept."
                    .into(),
            );
            m.filter.update(cx, |s, cx| s.focus(w, cx));
        }
        self.shared.borrow_mut().shell_modal_open = self.connection_manager_blocks_workspaces();
        cx.notify();
    }

    pub(super) fn render_connection_manager(
        &self,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let manager = self.connection_manager.as_ref().unwrap();
        let t = cx.omarchy().clone();
        let busy = self.manager_busy(cx);
        if let Some(editor) = manager
            .editor
            .as_ref()
            .filter(|e| e.entity.read(cx).form.is_some())
        {
            return div()
                .absolute()
                .inset_0()
                .occlude()
                .track_focus(&self.modal_focus)
                .child(editor.entity.clone())
                .into_any_element();
        }
        let writable = self.shared.borrow().writable;
        let profiles = self.manager_profiles(cx);
        let mut body = column().gap_1();
        if let Some(removal) = &manager.removal {
            body = body.p_4().gap_3()
                .child(div().text_size(px(18.)).font_weight(FontWeight::SEMIBOLD).child("Remove saved connection?"))
                .child(div().font_weight(FontWeight::SEMIBOLD).child(removal.profile.name.clone()))
                .child(label(target(&removal.profile), cx))
                .child(div().child("Removes only this saved configuration. Database files, SQL drafts and query history stay on this machine. No workspace is closed."))
                .when(removal.profile.engine != rowboat_core::Engine::Sqlite, |d| d.child(
                    button("manager-forget-password", "Also forget this profile's system-keyring password", false, cx)
                        .child(icon(if removal.forget_password { ui::IconName::SquareCheck } else { ui::IconName::Square }))
                        .disabled(busy).on_click(cx.listener(|s, _, _, cx| {
                            if let Some(r) = s.connection_manager.as_mut().and_then(|m| m.removal.as_mut()).filter(|r| r.request.is_none()) { r.forget_password = !r.forget_password; }
                            cx.notify();
                        }))))
                .child(row().flex_wrap().pt_3()
                    .child(button("manager-remove-cancel", "Keep connection", false, cx).disabled(busy)
                        .debug_selector(|| "manager-remove-cancel".into()).on_click(cx.listener(|s, _, w, cx| s.dismiss_connection_manager(w, cx))))
                    .child(button("manager-remove-confirm", if busy { "Removing…" } else { "Remove saved connection" }, false, cx)
                        .text_color(t.danger).disabled(busy || !writable)
                        .debug_selector(|| "manager-remove-confirm".into()).on_click(cx.listener(|s, _, w, cx| s.manager_confirm_remove(w, cx)))));
        } else if profiles.is_empty() {
            body = body
                .p_8()
                .gap_3()
                .items_center()
                .child(
                    icon(ui::IconName::Database)
                        .size(px(32.))
                        .text_color(t.accent),
                )
                .child(
                    div()
                        .id("manager-empty")
                        .debug_selector(|| "manager-empty".into())
                        .text_size(px(16.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(if self.shared.borrow().state.connections.is_empty() {
                            "No saved connections yet"
                        } else {
                            "No matching connections"
                        }),
                )
                .child(label(
                    "Add a connection, import profiles, or adjust your search.",
                    cx,
                ));
        } else {
            let mut group = None;
            for profile in profiles.iter().cloned() {
                if group.as_ref() != Some(&profile.group) {
                    group = Some(profile.group.clone());
                    body = body.child(
                        label(
                            if profile.group.is_empty() {
                                "Ungrouped".into()
                            } else {
                                profile.group.clone()
                            },
                            cx,
                        )
                        .flex_shrink_0()
                        .font_weight(FontWeight::SEMIBOLD)
                        .px_4()
                        .pt_3()
                        .pb_1(),
                    );
                }
                let open = self.ownership.find(&Some(profile.id.clone())).is_some();
                let reason = self.remove_reason(&profile.id, cx);
                let open_profile = profile.clone();
                let edit_profile = profile.clone();
                let remove_profile = profile.clone();
                let open_id = profile.id.clone();
                let edit_id = profile.id.clone();
                let remove_id = profile.id.clone();
                body = body.child(
                    column()
                        .flex_shrink_0()
                        .px_4()
                        .py_3()
                        .gap_2()
                        .border_b_1()
                        .border_color(t.divider())
                        .child(
                            row()
                                .child(icon(ui::IconName::Database).text_color(t.accent))
                                .child(ui::with_tooltip(
                                    div()
                                        .id(format!("profile-name-{}", profile.id))
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(profile.name.clone()),
                                    profile.name.clone(),
                                ))
                                .child(
                                    label(profile.engine.descriptor().name, cx)
                                        .px_2()
                                        .py_1()
                                        .bg(t.inset)
                                        .rounded(px(4.)),
                                ),
                        )
                        .child(ui::with_tooltip(
                            div()
                                .id(format!("profile-target-{}", profile.id))
                                .debug_selector({
                                    let id = profile.id.clone();
                                    move || format!("profile-target-{id}")
                                })
                                .truncate()
                                .text_size(px(12.))
                                .text_color(t.secondary)
                                .child(target(&profile)),
                            target(&profile),
                        ))
                        .child(
                            row()
                                .flex_wrap()
                                .child(
                                    button(
                                        format!("manager-open-{}", profile.id),
                                        if open {
                                            "Switch to workspace"
                                        } else {
                                            "Open workspace"
                                        },
                                        false,
                                        cx,
                                    )
                                    .disabled(busy)
                                    .debug_selector(move || format!("manager-open-{open_id}"))
                                    .on_click(cx.listener(
                                        move |s, _, w, cx| {
                                            s.manager_open(open_profile.clone(), w, cx)
                                        },
                                    )),
                                )
                                .child(
                                    button(
                                        format!("manager-edit-{}", profile.id),
                                        "Edit",
                                        false,
                                        cx,
                                    )
                                    .disabled(busy || !writable)
                                    .debug_selector(move || format!("manager-edit-{edit_id}"))
                                    .on_click(cx.listener(
                                        move |s, _, w, cx| {
                                            s.manager_edit(Some(edit_profile.clone()), w, cx)
                                        },
                                    )),
                                )
                                .child(ui::with_tooltip(
                                    button(
                                        format!("manager-remove-{}", profile.id),
                                        "Remove",
                                        false,
                                        cx,
                                    )
                                    .text_color(t.danger)
                                    .disabled(busy || reason.is_some())
                                    .debug_selector(move || format!("manager-remove-{remove_id}"))
                                    .on_click(cx.listener(
                                        move |s, _, w, cx| {
                                            s.manager_remove(remove_profile.clone(), w, cx)
                                        },
                                    )),
                                    reason.unwrap_or("Remove saved configuration…"),
                                )),
                        )
                        .when_some(reason, |d, reason| d.child(label(reason, cx))),
                );
            }
        }
        let header = column()
            .p_4()
            .gap_3()
            .flex_shrink_0()
            .bg(t.surface)
            .border_b_1()
            .border_color(t.divider())
            .child(
                row()
                    .child(icon(ui::IconName::Database).text_color(t.accent))
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Connections"),
                    )
                    .child(div().flex_1())
                    .when(
                        self.has_main_workspace() || manager.removal.is_some(),
                        |d| {
                            d.child(
                                button(
                                    "manager-close",
                                    if manager.removal.is_some() {
                                        "Back · Esc"
                                    } else {
                                        "Close · Esc"
                                    },
                                    false,
                                    cx,
                                )
                                .disabled(busy)
                                .debug_selector(|| "manager-close".into())
                                .on_click(
                                    cx.listener(|s, _, w, cx| s.dismiss_connection_manager(w, cx)),
                                ),
                            )
                        },
                    ),
            )
            .when(manager.removal.is_none(), |d| {
                d.child(
                    ui::input("manager-search", &manager.filter, w, cx)
                        .debug_selector(|| "manager-search".into()),
                )
            });
        let footer = row()
            .flex_shrink_0()
            .flex_wrap()
            .p_4()
            .bg(t.surface)
            .border_t_1()
            .border_color(t.divider())
            .when(manager.removal.is_none(), |d| {
                d.child(
                    button("manager-new", "New connection", true, cx)
                        .disabled(busy || !writable)
                        .debug_selector(|| "manager-new".into())
                        .on_click(cx.listener(|s, _, w, cx| s.manager_edit(None, w, cx))),
                )
                .child(
                    button("manager-draft", "SQL draft workspace", false, cx)
                        .disabled(busy)
                        .debug_selector(|| "manager-draft".into())
                        .on_click(cx.listener(|s, _, w, cx| s.manager_draft(w, cx))),
                )
                .child(div().flex_1())
                .child(
                    button("manager-import", "Import…", false, cx)
                        .disabled(busy || !writable)
                        .on_click(cx.listener(|s, _, w, cx| s.manager_transfer(true, w, cx))),
                )
                .child(
                    button("manager-export", "Export…", false, cx)
                        .disabled(
                            busy || !writable || self.shared.borrow().state.connections.is_empty(),
                        )
                        .on_click(cx.listener(|s, _, w, cx| s.manager_transfer(false, w, cx))),
                )
            });
        let surface = column().id("connection-manager").debug_selector(|| "connection-manager".into())
                .key_context("RowboatModal")
                .w(px(760.).min(w.viewport_size().width - px(32.)))
                .h(px(660.).min(w.viewport_size().height - px(32.)))
                .bg(t.background.opacity(1.)).text_size(px(13.)).border_1().border_color(t.border).rounded(px(8.)).shadow_lg()
                .child(header)
                .child(body.id("manager-list").debug_selector(|| "manager-list".into()).track_scroll(&manager.scroll).flex_1().min_h_0().overflow_y_scroll())
                 .when_some(manager.error.clone(), |d, error| d.child(div().px_4().py_2().text_color(t.danger).child(error)))
                 .when_some(self.error.clone().filter(|_| !self.has_main_workspace()), |d, error| d.child(div().px_4().py_2().text_color(t.danger).child(error)))
                .when_some(manager.message.clone().filter(|_| manager.error.is_none()), |d, message| d.child(label(message, cx).px_4().py_2()))
                .when(!writable, |d| d.child(label("Profile storage is read-only or unavailable. Browsing and switching workspaces remain available.", cx).px_4().py_2()))
                 .child(footer);
        self.dialogs.overlay(
            "connections",
            w.viewport_size(),
            &self.modal_focus,
            surface,
            cx,
        )
    }
    #[cfg(test)]
    #[allow(dead_code)] // Called by the isolated actual-GPUI harness.
    pub(crate) fn test_manager_pending_removal(&mut self) -> uuid::Uuid {
        // Inject the worker boundary without accessing the user's keyring.
        let request = uuid::Uuid::new_v4();
        self.connection_manager
            .as_mut()
            .unwrap()
            .removal
            .as_mut()
            .unwrap()
            .request = Some(request);
        request
    }
    #[cfg(test)]
    #[allow(dead_code)] // Called by the isolated actual-GPUI harness.
    pub(crate) fn test_workspace_host(&self, id: u64) -> Option<AnyWindowHandle> {
        self.workspaces
            .iter()
            .find(|w| w.id == id)
            .and_then(|w| w.host)
    }
    #[cfg(test)]
    #[allow(dead_code)] // Called by the isolated actual-GPUI harness.
    pub(crate) fn test_manager_editor(&self) -> Option<Entity<Workbench>> {
        self.connection_manager
            .as_ref()
            .and_then(|m| m.editor.as_ref())
            .map(|e| e.entity.clone())
    }
}
