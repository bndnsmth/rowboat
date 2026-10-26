//! Portable profiles; file I/O off-thread, credentials remain session-only.
use crate::{
    app::*,
    view::{action, column, label, row},
};
use gpui_kit::{base::input::InputState, prelude::*, *};
use gpui_omarchy as ui;
use rowboat_core::ConnectionProfile;

/// Presentation only: never use this shortened target as session identity.
pub(crate) struct ConnectionTarget {
    pub compact: String,
    pub file_path: Option<String>,
}

pub(crate) fn connection_target(profile: &ConnectionProfile) -> ConnectionTarget {
    if profile.engine.descriptor().connection_kind == rowboat_core::ConnectionKind::File {
        let path = profile.file.as_ref().map(|file| &file.path);
        ConnectionTarget {
            compact: path
                .and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "No database file".into()),
            file_path: path.map(|path| path.display().to_string()),
        }
    } else {
        ConnectionTarget {
            compact: profile.database.clone(),
            file_path: None,
        }
    }
}

pub enum ProfileEvent {
    Imported(Result<Option<Vec<ConnectionProfile>>, String>),
    Exported(Result<Option<String>, String>),
}
impl Workbench {
    pub fn choose_database_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.close_requested {
            return;
        }
        let Some(form) = self
            .form
            .as_mut()
            .filter(|f| f.is_file() && f.file_request.is_none())
        else {
            return;
        };
        let request = uuid::Uuid::new_v4();
        form.file_request = Some(request);
        let create = form.file_mode == rowboat_core::SqliteOpenMode::CreateIfMissing;
        let open = (!create).then(|| {
            cx.prompt_for_paths(PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: Some("Open existing SQLite database".into()),
            })
        });
        let new = create
            .then(|| cx.prompt_for_new_path(std::path::Path::new("."), Some("database.sqlite")));
        cx.spawn_in(window, async move |this, cx| {
            let result = if let Some(open) = open {
                open.await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r.map_err(|e| e.to_string()))
                    .map(|paths| paths.and_then(|p| p.into_iter().next()))
            } else {
                new.unwrap()
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r.map_err(|e| e.to_string()))
            };
            let _ = this.update_in(cx, |s, w, cx| {
                if s.close_requested || s.busy {
                    return;
                }
                let Some(form) = s
                    .form
                    .as_mut()
                    .filter(|f| f.is_file() && f.file_request == Some(request))
                else {
                    return;
                };
                form.file_request = None;
                match result {
                    Ok(Some(path)) => match path.to_str() {
                        Some(path) => form.fields["file_path"]
                            .update(cx, |input, cx| input.set_value(path, w, cx)),
                        None => s.fail("Database file path is not valid UTF-8".into()),
                    },
                    Ok(None) => {}
                    Err(error) => s.fail(format!("File dialog failed: {error}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub fn show_connection_url(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.loaded || !self.can_leave() || self.form.is_some() {
            cx.notify();
            return;
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("postgresql://user:password@host/database")
        });
        input.update(cx, |s, cx| s.focus(window, cx));
        self.url_input = Some(input);
        self.tabs[self.active_tab].show_parameters = false;
        self.quick_open = false;
        self.dismiss_completions(cx);
        cx.notify();
    }
    pub fn parse_connection_url(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = &self.url_input else {
            return;
        };
        let url = input.read(cx).value();
        if url.len() > 16_384 {
            self.fail("Connection URL exceeds 16 KiB".into());
            cx.notify();
            return;
        }
        match rowboat_services::parse_connection_url(&url) {
            Ok(parsed) => {
                self.url_input = None;
                if let Some(password) = parsed.password {
                    self.passwords.insert(parsed.profile.id.clone(), password);
                }
                self.show_connection(Some(parsed.profile), window, cx);
                self.status =
                    "URL parsed · review connection settings, then Save or Connect".into();
            }
            Err(e) => self.fail(format!("Unable to parse connection URL: {e}")),
        }
        cx.notify();
    }
    pub fn copy_connection_url(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &self.form else {
            return;
        };
        match form
            .profile(cx)
            .and_then(|p| rowboat_services::connection_url(&p))
        {
            Ok(url) => {
                cx.write_to_clipboard(ClipboardItem::new_string(url));
                self.status = "Password-free connection URL copied".into();
            }
            Err(e) => self.fail(e.to_string()),
        }
        cx.notify();
    }
    pub fn import_profiles(&mut self, cx: &mut Context<Self>) {
        if !self.loaded || !self.can_leave() || self.form.is_some() {
            cx.notify();
            return;
        }
        let dialog = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import portable Rowboat connections".into()),
        });
        let runtime = self.runtime.clone();
        let tx = self.tx.clone();
        self.busy = true;
        self.status = "Choose a connections file to import…".into();
        cx.spawn(async move |_, _| match dialog.await {
            Ok(Ok(Some(paths))) => {
                runtime.spawn_blocking(move || {
                    let result = (|| {
                        use std::io::Read;
                        let Some(path) = paths.into_iter().next() else {
                            return Ok(None);
                        };
                        let file = std::fs::File::open(path)?;
                        anyhow::ensure!(
                            file.metadata()?.is_file(),
                            "Choose a regular profile file"
                        );
                        let mut input = String::new();
                        file.take(5 * 1024 * 1024 + 1).read_to_string(&mut input)?;
                        anyhow::ensure!(
                            input.len() <= 5 * 1024 * 1024,
                            "Connections file exceeds 5 MiB"
                        );
                        Ok::<_, anyhow::Error>(Some(rowboat_services::import_connections(&input)?))
                    })()
                    .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Event::Profiles(ProfileEvent::Imported(result)));
                });
            }
            Ok(Ok(None)) => {
                let _ = tx.send(Event::Profiles(ProfileEvent::Imported(Ok(None))));
            }
            _ => {
                let _ = tx.send(Event::Profiles(ProfileEvent::Imported(Err(
                    "Native file dialog failed".into(),
                ))));
            }
        })
        .detach();
        cx.notify();
    }
    pub fn export_profiles(&mut self, cx: &mut Context<Self>) {
        if !self.loaded || !self.can_leave() || self.form.is_some() {
            cx.notify();
            return;
        }
        let profiles = self.state.connections.clone();
        let dialog = cx.prompt_for_new_path(
            std::path::Path::new(std::env::var("HOME").as_deref().unwrap_or(".")),
            Some("rowboat-connections.json"),
        );
        let runtime = self.runtime.clone();
        let tx = self.tx.clone();
        self.busy = true;
        self.status = "Choose a portable connections destination…".into();
        cx.spawn(async move |_, _| match dialog.await {
            Ok(Ok(Some(path))) => {
                runtime.spawn_blocking(move || {
                    let result = (|| {
                        use std::{io::Write, os::unix::fs::PermissionsExt};
                        let json = rowboat_services::export_connections(&profiles)?;
                        let parent = path.parent().ok_or_else(|| {
                            anyhow::anyhow!("Destination needs a parent directory")
                        })?;
                        let mut file = tempfile::NamedTempFile::new_in(parent)?;
                        file.as_file()
                            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
                        file.write_all(json.as_bytes())?;
                        file.as_file().sync_all()?;
                        file.persist_noclobber(&path)?;
                        std::fs::File::open(parent)?.sync_all()?;
                        Ok::<_, anyhow::Error>(Some(format!(
                            "Exported {} portable connections to {}",
                            profiles.len(),
                            path.display()
                        )))
                    })()
                    .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Event::Profiles(ProfileEvent::Exported(result)));
                });
            }
            Ok(Ok(None)) => {
                let _ = tx.send(Event::Profiles(ProfileEvent::Exported(Ok(None))));
            }
            _ => {
                let _ = tx.send(Event::Profiles(ProfileEvent::Exported(Err(
                    "Native file dialog failed".into(),
                ))));
            }
        })
        .detach();
        cx.notify();
    }
    pub fn receive_profiles(&mut self, event: ProfileEvent, cx: &mut Context<Self>) {
        self.busy = false;
        match event {
            ProfileEvent::Imported(Ok(Some(incoming))) => {
                let mut connections = self.shared.borrow().state.connections.clone();
                match rowboat_services::merge_connections(&mut connections, incoming) {
                    Ok(report) => {
                        self.edit_state(|state| state.connections = connections);
                        self.status = format!(
                            "Imported {} connections with fresh IDs · review local TLS/SSH paths before connecting",
                            report.added
                        );
                    }
                    Err(e) => self.fail(e.to_string()),
                }
            }
            ProfileEvent::Exported(Ok(Some(message))) => self.status = message,
            ProfileEvent::Imported(Ok(None)) | ProfileEvent::Exported(Ok(None)) => {
                self.status = "Connection file selection cancelled".into()
            }
            ProfileEvent::Imported(Err(e)) | ProfileEvent::Exported(Err(e)) => self.fail(e),
        }
        cx.notify();
    }
    pub fn render_connection_url(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        use gpui_omarchy::ActiveTheme;
        let theme = cx.omarchy().clone();
        self.modal(column().w(px(720.).min(window.viewport_size().width - px(48.))).p_4().gap_4().text_size(px(13.))
            .child(row().child(ui::icon(ui::IconName::Database).text_color(theme.accent)).child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child("Paste connection URL")))
            .child(label("PostgreSQL or MySQL URL. The input is masked because it may contain a password. Passwords stay in memory unless you explicitly enable Remember in the connection form.",cx))
            .child(div().h(px(1.)).bg(theme.divider()))
            .child(label("Connection URL", cx))
            .child(ui::input("connection-url",self.url_input.as_ref().unwrap(),window,cx))
            .child(row().flex_wrap().pt_3().border_t_1().border_color(theme.divider()).child(action("cancel-connection-url","Cancel",false,true,cx,|s,w,cx|{s.url_input=None;w.focus(&s.focus,cx);cx.notify();})).child(div().flex_1())
                .child(action("parse-connection-url","Review connection",true,true,cx,|s,w,cx|s.parse_connection_url(w,cx)))).into_any_element(),cx)
    }
}
