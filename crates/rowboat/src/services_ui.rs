//! Service integration; native dialogs on GPUI, credential/process/file work off-thread.
use crate::{
    app::*,
    forms,
    view::{action, column, label, row},
};
use gpui_kit::{base::input::InputState, prelude::*, *};
use gpui_omarchy::{self as ui, ActiveTheme};
use rowboat_core::ConnectionProfile;
use rowboat_services::ClientRuntime;
use std::{path::PathBuf, time::SystemTime};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferKind {
    Backup,
    Restore,
}
impl TransferKind {
    fn operation(self) -> rowboat_services::ServiceOperation {
        match self {
            Self::Backup => rowboat_services::ServiceOperation::Backup,
            Self::Restore => rowboat_services::ServiceOperation::Restore,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransferRequest {
    dialog: uuid::Uuid,
    request: uuid::Uuid,
    kind: TransferKind,
    generation: u64,
}
impl TransferRequest {
    fn accepts(
        self,
        pending: Option<Self>,
        dialog: uuid::Uuid,
        kind: TransferKind,
        generation: u64,
    ) -> bool {
        pending == Some(self)
            && self.dialog == dialog
            && self.kind == kind
            && self.generation == generation
    }
}
pub struct ChosenFile {
    pub path: PathBuf,
    pub preview: String,
    pub size: u64,
    pub modified: Option<SystemTime>,
}
pub struct TransferDialog {
    id: uuid::Uuid,
    pub kind: TransferKind,
    pub generation: u64,
    pub connection_id: String,
    pub database: String,
    pub file_provider: bool,
    pub image: Entity<InputState>,
    pub confirmation: Entity<InputState>,
    pub file: Option<ChosenFile>,
    pending: Option<TransferRequest>,
}

fn transfer_target(profile: &ConnectionProfile) -> anyhow::Result<String> {
    if profile.engine.descriptor().connection_kind == rowboat_core::ConnectionKind::File {
        let file = profile
            .file
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("File target is missing"))?;
        anyhow::ensure!(!file.path.as_os_str().is_empty(), "File target is empty");
        let path = std::fs::canonicalize(&file.path)?;
        Ok(path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("The target path cannot be displayed exactly"))?
            .to_owned())
    } else {
        anyhow::ensure!(!profile.database.is_empty(), "Database target is empty");
        Ok(profile.database.clone())
    }
}

/// Bounded binary metadata for snapshots; plain SQL keeps its bounded text preview.
pub(crate) async fn review_transfer_file(
    path: PathBuf,
    file_provider: bool,
) -> anyhow::Result<ChosenFile> {
    use tokio::io::AsyncReadExt;
    let file = tokio::fs::File::open(&path).await?;
    let metadata = file.metadata().await?;
    anyhow::ensure!(metadata.is_file(), "Choose a regular backup file");
    let mut bytes = Vec::new();
    file.take(if file_provider { 100 } else { 65536 })
        .read_to_end(&mut bytes)
        .await?;
    let preview = if file_provider {
        anyhow::ensure!(
            bytes.len() == 100 && bytes.starts_with(b"SQLite format 3\0"),
            "This file is not a SQLite snapshot"
        );
        let page_size = u16::from_be_bytes([bytes[16], bytes[17]]);
        let page_size = if page_size == 1 {
            65536
        } else {
            u32::from(page_size)
        };
        let user_version = u32::from_be_bytes(bytes[60..64].try_into().unwrap());
        format!(
            "SQLite 3 binary snapshot\nFile: {}\nSize: {} bytes\nPage size: {page_size} bytes\nUser version: {user_version}\nThe native restore provider validates the snapshot before replacing database contents.",
            path.display(),
            metadata.len()
        )
    } else {
        String::from_utf8(bytes)
            .map_err(|_| anyhow::anyhow!("This file is not a UTF-8 plain SQL backup"))?
    };
    Ok(ChosenFile {
        path,
        preview,
        size: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

impl Workbench {
    /// Restore revokes displayed results and their execution-bound edit evidence.
    /// The shell also uses this for other workspaces pointing at the restored file.
    pub(crate) fn invalidate_restore_results(&mut self) {
        self.table = None;
        self.table_result = Default::default();
        self.structure = Default::default();
        self.selected = None;
        self.catalog.clear();
        self.query_edit = Default::default();
        self.change_target = None;
        self.relationships = None;
        self.relationship_origin = None;
        self.relationship_return = None;
        self.namespace.known = false;
        self.namespace.result_context.clear();
        self.advanced.reset_connection();
        for tab in &mut self.tabs {
            if tab.connection_id.as_deref() == self.profile.as_ref().map(|p| p.id.as_str()) {
                tab.results.clear();
                tab.result_index = 0;
                tab.metadata_incomplete.clear();
                tab.query_progress = None;
            }
        }
    }
    pub(crate) fn maybe_startup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.loaded || !self.session_ready {
            return;
        }
        let Some(name) = self.startup.connection.take() else {
            return;
        };
        let Some(profile) = self
            .state
            .connections
            .iter()
            .find(|p| p.id == name || p.name == name)
            .cloned()
        else {
            self.startup.autorun = false;
            self.fail(format!("Saved connection not found: {name}"));
            return;
        };
        if let Some(tab) = self
            .tabs
            .iter_mut()
            .find(|t| Some(t.id) == self.startup.tab_id)
        {
            tab.connection_id = Some(profile.id.clone());
        }
        if profile.engine.descriptor().connection_kind == rowboat_core::ConnectionKind::File {
            self.startup.password.clear();
            self.connect_profile(profile, String::new(), cx);
        } else if self.startup.password_supplied {
            let password = std::mem::take(&mut self.startup.password);
            self.passwords.insert(profile.id.clone(), password.clone());
            self.connect_profile(profile, password, cx);
        } else {
            self.edit_connection(Some(profile.clone()), window, cx);
            self.startup_connect = Some(profile);
        }
    }
    pub(crate) fn request_credential(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &mut self.form else {
            return;
        };
        if form.is_file() {
            return;
        }
        if !self.state.connections.iter().any(|p| p.id == form.id) {
            return;
        }
        form.keyring_loading = true;
        let request = uuid::Uuid::new_v4();
        form.credential_request = Some(request);
        let id = form.id.clone();
        let tx = self.tx.clone();
        self.runtime.spawn_blocking(move || {
            let result = rowboat_services::credential_get(&id).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::CredentialLookup {
                request,
                connection_id: id,
                result,
            });
        });
        cx.notify();
    }
    pub(crate) fn receive_credential(
        &mut self,
        id: String,
        result: Result<Option<String>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .form
            .as_ref()
            .is_some_and(|f| f.id == id && f.is_file())
        {
            self.startup_connect = None;
            return;
        }
        let startup = self.startup_connect.as_ref().is_some_and(|p| p.id == id);
        if startup && self.form.as_ref().is_none_or(|f| f.id != id) {
            self.startup_connect = None;
            return;
        }
        if startup {
            let profile = self.startup_connect.take().unwrap();
            if let Some(form) = &mut self.form {
                form.keyring_loading = false;
            }
            match result {
                Ok(password) => {
                    if let Some(form) = &mut self.form {
                        form.credential_exists = password.is_some();
                        if !form.remember_touched {
                            form.remember = password.is_some();
                        }
                        if form.password_dirty || form.remember_touched {
                            // The user took over the form during the lookup.
                            cx.notify();
                            return;
                        }
                        form.password.update(cx, |p, cx| {
                            p.set_value(password.clone().unwrap_or_default(), window, cx)
                        });
                    }
                    let password = password.unwrap_or_default();
                    self.passwords.insert(id, password.clone());
                    self.connect_profile(profile, password, cx);
                }
                Err(e) => {
                    if let Some(f) = &mut self.form {
                        f.keyring_loading = false;
                    }
                    self.fail(format!("Keyring lookup failed: {e}. Enter a session password or supply ROWBOAT_PASSWORD."));
                }
            }
            return;
        }
        let Some(form) = self.form.as_mut().filter(|f| f.id == id) else {
            return;
        };
        form.keyring_loading = false;
        match result {
            Ok(Some(password)) => {
                form.credential_exists = true;
                if !form.remember_touched {
                    form.remember = true;
                }
                if !form.password_dirty && form.password.read(cx).value().is_empty() {
                    form.password
                        .update(cx, |p, cx| p.set_value(password.clone(), window, cx));
                    self.passwords.insert(id, password);
                }
            }
            Ok(None) => {}
            Err(e) => self.fail(format!(
                "Keyring lookup failed: {e}. Manual session passwords still work."
            )),
        }
        cx.notify();
    }
    pub(crate) fn save_credential_then_connect(
        &mut self,
        profile: ConnectionProfile,
        password: String,
        connect: bool,
        remember: bool,
        exists: bool,
        cx: &mut Context<Self>,
    ) {
        if profile.engine.descriptor().connection_kind == rowboat_core::ConnectionKind::File {
            if connect {
                self.connect_profile(profile, String::new(), cx);
            } else {
                self.form = None;
                self.status = "File connection saved".into();
            }
            return;
        }
        if !remember && !exists {
            if connect {
                self.connect_profile(profile, password, cx);
            } else {
                self.form = None;
                self.status = "Connection saved · password remains in memory only".into();
            }
            return;
        }
        self.busy = true;
        self.error = None;
        self.status = if remember {
            "Saving password to system keyring…"
        } else {
            "Removing saved keyring password…"
        }
        .into();
        let tx = self.tx.clone();
        self.runtime.spawn_blocking(move || {
            let result = if remember {
                rowboat_services::credential_set(&profile.id, &password)
            } else {
                rowboat_services::credential_delete(&profile.id)
            }
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::CredentialSaved {
                profile: Box::new(profile),
                password,
                connect,
                result,
            });
        });
        cx.notify();
    }

    pub fn show_transfer(
        &mut self,
        kind: TransferKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.require_service(kind.operation(), cx) {
            return;
        }
        if self.close_requested
            || self.has_extension_dialog()
            || self.form.is_some()
            || !self.can_leave()
        {
            cx.notify();
            return;
        }
        let Some(profile) = &self.profile else {
            return;
        };
        if self.transaction || !self.transaction_known {
            self.fail("Commit or roll back the current transaction before backup or restore. These tools use a separate connection.".into());
            cx.notify();
            return;
        }
        if kind == TransferKind::Restore && profile.read_only {
            self.fail("Restore is disabled for read-only profiles.".into());
            cx.notify();
            return;
        }
        let file_provider =
            profile.engine.descriptor().connection_kind == rowboat_core::ConnectionKind::File;
        let database = match transfer_target(profile) {
            Ok(target) => target,
            Err(error) => {
                self.fail(error.to_string());
                cx.notify();
                return;
            }
        };
        let image = if file_provider {
            String::new()
        } else {
            let (variable, default_image) = match profile.engine.descriptor().tools.backup {
                rowboat_core::ToolSupport::Supported(rowboat_core::ToolProvider::Postgres) => (
                    "ROWBOAT_POSTGRES_CLIENT_IMAGE",
                    "docker.io/library/postgres:18",
                ),
                rowboat_core::ToolSupport::Supported(rowboat_core::ToolProvider::MySql) => {
                    ("ROWBOAT_MYSQL_CLIENT_IMAGE", "docker.io/library/mysql:8.4")
                }
                _ => {
                    self.fail("No native backup client provider is installed".into());
                    cx.notify();
                    return;
                }
            };
            match rowboat_services::environment_text(variable) {
                Ok(image) => image.unwrap_or_else(|| default_image.into()),
                Err(error) => {
                    self.fail(error.to_string());
                    cx.notify();
                    return;
                }
            }
        };
        self.transfer = Some(TransferDialog {
            id: uuid::Uuid::new_v4(),
            kind,
            generation: self.session_generation,
            connection_id: profile.id.clone(),
            database,
            file_provider,
            image: forms::input(image, "Cached Podman client image", window, cx),
            confirmation: forms::input(
                "",
                if file_provider {
                    "Type the full target file path to confirm"
                } else {
                    "Type the database name to confirm"
                },
                window,
                cx,
            ),
            file: None,
            pending: None,
        });
        if let Some(dialog) = &self.transfer {
            if dialog.file_provider {
                dialog.confirmation.update(cx, |s, cx| s.focus(window, cx));
            } else {
                dialog.image.update(cx, |s, cx| s.focus(window, cx));
            }
        }
        cx.notify();
    }
    pub fn choose_transfer_file(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.transfer else {
            return;
        };
        if dialog.pending.is_some() || self.busy || self.close_requested {
            return;
        }
        let request = TransferRequest {
            dialog: dialog.id,
            request: uuid::Uuid::new_v4(),
            kind: dialog.kind,
            generation: dialog.generation,
        };
        dialog.pending = Some(request);
        dialog.file = None;
        let kind = dialog.kind;
        let file_provider = dialog.file_provider;
        let tx = self.tx.clone();
        let runtime = self.runtime.clone();
        if kind == TransferKind::Backup {
            let receiver = cx.prompt_for_new_path(
                std::path::Path::new(std::env::var("HOME").as_deref().unwrap_or(".")),
                Some(if file_provider {
                    "rowboat-backup.sqlite"
                } else {
                    "rowboat-backup.sql"
                }),
            );
            cx.spawn(async move |_, _| {
                let result = match receiver.await {
                    Ok(Ok(Some(path))) => Ok(ChosenFile {
                        path,
                        preview: String::new(),
                        size: 0,
                        modified: None,
                    }),
                    Ok(Ok(None)) => Err("File selection cancelled".into()),
                    Ok(Err(e)) => Err(format!("{e:#}")),
                    Err(e) => Err(e.to_string()),
                };
                let _ = tx.send(Event::TransferChosen { request, result });
            })
            .detach();
        } else {
            let receiver = cx.prompt_for_paths(PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: Some(
                    if file_provider {
                        "Choose a SQLite snapshot to review"
                    } else {
                        "Choose a plain SQL backup to review"
                    }
                    .into(),
                ),
            });
            cx.spawn(async move |_, _| match receiver.await {
                Ok(Ok(Some(paths))) => {
                    if let Some(path) = paths.into_iter().next() {
                        runtime.spawn(async move {
                            let result = review_transfer_file(path, file_provider)
                                .await
                                .map_err(|e| format!("{e:#}"));
                            let _ = tx.send(Event::TransferChosen { request, result });
                        });
                    } else {
                        let _ = tx.send(Event::TransferChosen {
                            request,
                            result: Err("File selection cancelled".into()),
                        });
                    }
                }
                Ok(Ok(None)) => {
                    let _ = tx.send(Event::TransferChosen {
                        request,
                        result: Err("File selection cancelled".into()),
                    });
                }
                error => {
                    let _ = tx.send(Event::TransferChosen {
                        request,
                        result: Err(format!("File dialog failed: {error:?}")),
                    });
                }
            })
            .detach();
        }
        cx.notify();
    }
    pub(crate) fn receive_transfer_file(
        &mut self,
        request: TransferRequest,
        result: Result<ChosenFile, String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(dialog) = self
            .transfer
            .as_mut()
            .filter(|d| request.accepts(d.pending, d.id, d.kind, self.session_generation))
        {
            dialog.pending = None;
            if self.close_requested {
                cx.notify();
                return;
            }
            match result {
                Ok(file) => dialog.file = Some(file),
                Err(e) if e == "File selection cancelled" => {}
                Err(e) => self.fail(e),
            }
        }
        cx.notify();
    }
    pub fn start_transfer(&mut self, cx: &mut Context<Self>) {
        if let Some(dialog) = self.transfer.as_ref()
            && !self.require_service(dialog.kind.operation(), cx)
        {
            return;
        }
        if self.busy || self.close_requested {
            return;
        }
        let Some(dialog) = &self.transfer else {
            return;
        };
        if dialog.pending.is_some() {
            return;
        }
        let (Some(profile), Some(endpoint), Some(db)) = (
            self.profile.as_ref(),
            self.endpoint.clone(),
            self.database.clone(),
        ) else {
            return;
        };
        let restore = dialog.kind == TransferKind::Restore;
        if dialog.generation != self.session_generation || dialog.connection_id != profile.id {
            self.fail(
                "The backup/restore dialog belongs to a different connection session.".into(),
            );
            cx.notify();
            return;
        }
        if self.transaction
            || !self.transaction_known
            || !self.changes.is_empty()
            || self.has_row_draft()
            || self.has_extension_dialog()
            || self.form.is_some()
            || self.management.is_some()
        {
            self.fail("Finish all staged changes and transactions before backup/restore.".into());
            cx.notify();
            return;
        }
        if restore
            && (profile.read_only
                || dialog.database.is_empty()
                || transfer_target(profile).ok().as_ref() != Some(&dialog.database)
                || transfer_target(&endpoint).ok().as_ref() != Some(&dialog.database)
                || dialog.confirmation.read(cx).value().as_ref() != dialog.database)
        {
            self.fail("Restore requires a writable profile and exact target confirmation.".into());
            cx.notify();
            return;
        }
        let Some(file) = &dialog.file else {
            self.fail("Choose a backup path first.".into());
            cx.notify();
            return;
        };
        let path = file.path.clone();
        let size = file.size;
        let modified = file.modified;
        let image = dialog.image.read(cx).value().trim().to_owned();
        let file_provider = dialog.file_provider;
        if !file_provider && image.is_empty() {
            self.fail(
                "Enter an existing Podman image containing the database client tools.".into(),
            );
            cx.notify();
            return;
        }
        let clients = if file_provider {
            ClientRuntime::Native
        } else {
            ClientRuntime::Podman { image }
        };
        let password = if file_provider {
            String::new()
        } else {
            self.session_password.clone()
        };
        let tx = self.tx.clone();
        let runtime = self.runtime.clone();
        self.busy = true;
        self.transfer_running = true;
        self.error = None;
        self.status = if file_provider && restore {
            "Restoring SQLite snapshot…"
        } else if file_provider {
            "Creating SQLite snapshot…"
        } else if restore {
            "Restoring database through Podman…"
        } else {
            "Backing up database through Podman…"
        }
        .into();
        if file_provider && restore {
            if self
                .shell_tx
                .send(crate::workspace_shell::Command::RestoreFile {
                    route: self.tx.route,
                    endpoint: Box::new(endpoint),
                    path,
                    size,
                    modified,
                })
                .is_err()
            {
                self.busy = false;
                self.transfer_running = false;
                self.fail("Restore coordinator unavailable; no restore started".into());
            }
            cx.notify();
            return;
        }
        self.runtime.spawn_blocking(move || {
            runtime.block_on(async move {
                let result = async {
                    anyhow::ensure!(
                        !db.in_transaction().await?,
                        "The database reports an active transaction; finish it before backup/restore"
                    );
                    if restore {
                        let metadata = tokio::fs::metadata(&path).await?;
                        anyhow::ensure!(
                            metadata.len() == size && metadata.modified().ok() == modified,
                            "The backup file changed after review; choose it again"
                        );
                        rowboat_services::restore(&endpoint, &password, &path, &clients).await?;
                        Ok::<_, anyhow::Error>("Database restore completed".into())
                    } else {
                        rowboat_services::backup(&endpoint, &password, &path, &clients).await?;
                        Ok(format!("Backup saved to {}", path.display()))
                    }
                }
                .await
                .map_err(|e| format!("{e:#}"));
                let _ = tx.send(Event::TransferFinished { restore, result });
            })
        });
        cx.notify();
    }
    pub fn render_transfer(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let dialog = self.transfer.as_ref().unwrap();
        let restore = dialog.kind == TransferKind::Restore;
        let t = cx.omarchy().clone();
        let mut body = column()
            .p_4()
            .gap_4()
            .text_size(px(13.))
            .child(
                row()
                    .flex_wrap()
                    .child(ui::icon(ui::IconName::Database).text_color(t.accent))
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(if restore {
                                "Restore database"
                            } else {
                                "Back up database"
                            }),
                    )
                    .child(div().flex_1())
                    .child(
                        action(
                            "close-transfer",
                            "Close",
                            false,
                            !self.busy,
                            cx,
                            |s, w, cx| {
                                s.transfer = None;
                                w.focus(&s.focus, cx);
                                cx.notify();
                            },
                        )
                        .debug_selector(|| "close-transfer".into()),
                    ),
            )
            .child(
                label(format!("Target: {}", dialog.database), cx)
                    .debug_selector(|| "transfer-target".into()),
            )
            .child(div().h(px(1.)).bg(t.divider()));
        if dialog.file_provider {
            body = body.child(label(
                "SQLite online backup API · local binary snapshot",
                cx,
            ));
        } else {
            body = body.child(label(format!("{} · uses the connected session's credentials and SSH endpoint",self.connection_label(Some(&dialog.connection_id))),cx))
                .child(column().gap_2().child(label("Cached Podman client image",cx)).child(ui::input("transfer-image",&dialog.image,window,cx).debug_selector(|| "transfer-image".into())).child(label("Already-present images only (--pull=never). Rowboat does not install tools or pull images.",cx)));
        }
        body = body.child(
            row()
                .flex_wrap()
                .p_3()
                .bg(t.surface)
                .child(
                    action(
                        "choose-transfer-file",
                        if restore && dialog.file_provider {
                            "Choose SQLite snapshot…"
                        } else if restore {
                            "Choose SQL backup…"
                        } else {
                            "Choose destination…"
                        },
                        false,
                        !self.busy && dialog.pending.is_none(),
                        cx,
                        |s, _, cx| s.choose_transfer_file(cx),
                    )
                    .debug_selector(|| "choose-transfer-file".into()),
                )
                .child(
                    div().flex_1().truncate().child(
                        dialog
                            .file
                            .as_ref()
                            .map(|f| f.path.display().to_string())
                            .unwrap_or_else(|| "No file selected".into()),
                    ),
                ),
        );
        if restore {
            body=body.child(div().text_color(t.warning).child(if dialog.file_provider { "Restore replaces the target database contents with the selected SQLite snapshot. Review the full target path before continuing. Prior results are invalidated; editor SQL is not rerun." } else { "Restore executes the selected SQL against this database. PostgreSQL uses a single transaction; MySQL DDL can commit partially. Review the file and target before continuing." }));
            if let Some(file) = &dialog.file {
                body = body
                    .child(label(
                        format!(
                            "{} bytes · {}",
                            file.size,
                            if dialog.file_provider {
                                "binary snapshot metadata"
                            } else {
                                "preview of first 64 KiB"
                            }
                        ),
                        cx,
                    ))
                    .child(
                        div()
                            .id("restore-preview")
                            .h(px(230.))
                            .overflow_y_scroll()
                            .p_3()
                            .bg(t.inset)
                            .font_family(t.mono_font.clone())
                            .text_size(px(12.))
                            .child(file.preview.clone()),
                    );
            }
            body = body
                .child(label(
                    format!("Type {} to confirm the restore target", dialog.database),
                    cx,
                ))
                .child(ui::input(
                    "restore-confirmation",
                    &dialog.confirmation,
                    window,
                    cx,
                ));
        } else {
            body = body.child(label(
                if dialog.file_provider { "Creates a private binary SQLite snapshot, including committed WAL contents. Existing destinations are never overwritten." } else { "Creates a private plain-SQL backup. Existing destinations are never overwritten." },
                cx,
            ));
        }
        body = body.child(
            row()
                .flex_wrap()
                .pt_3()
                .border_t_1()
                .border_color(t.divider())
                .child(div().flex_1())
                .child(
                    action(
                        "run-transfer",
                        if restore {
                            "Restore database"
                        } else {
                            "Create backup"
                        },
                        true,
                        !self.busy
                            && dialog.pending.is_none()
                            && dialog.file.is_some()
                            && (!restore
                                || (!dialog.database.is_empty()
                                    && dialog.confirmation.read(cx).value().as_ref()
                                        == dialog.database)),
                        cx,
                        |s, _, cx| s.start_transfer(cx),
                    )
                    .debug_selector(|| "run-transfer".into()),
                ),
        );
        self.modal(
            div()
                .id("transfer-scroll")
                .w(px(820.).min(window.viewport_size().width - px(48.)))
                .max_h((window.viewport_size().height - px(64.)).min(px(780.)))
                .overflow_y_scroll()
                .child(body)
                .into_any_element(),
            cx,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{TransferKind, TransferRequest};
    #[test]
    fn late_restore_preview_cannot_enter_reopened_or_repurposed_dialog() {
        let restore = TransferRequest {
            dialog: uuid::Uuid::new_v4(),
            request: uuid::Uuid::new_v4(),
            kind: TransferKind::Restore,
            generation: 9,
        };
        assert!(restore.accepts(Some(restore), restore.dialog, TransferKind::Restore, 9));
        // Closed dialog, reopened restore in the same session, then a backup dialog.
        assert!(!restore.accepts(None, restore.dialog, TransferKind::Restore, 9));
        for kind in [TransferKind::Restore, TransferKind::Backup] {
            let replacement = TransferRequest {
                dialog: uuid::Uuid::new_v4(),
                request: uuid::Uuid::new_v4(),
                kind,
                ..restore
            };
            assert!(!restore.accepts(Some(replacement), replacement.dialog, kind, 9));
        }
        let replacement = TransferRequest {
            request: uuid::Uuid::new_v4(),
            ..restore
        };
        assert!(!restore.accepts(Some(replacement), restore.dialog, TransferKind::Restore, 9));
        assert!(!restore.accepts(Some(restore), restore.dialog, TransferKind::Backup, 9));
        assert!(!restore.accepts(Some(restore), restore.dialog, TransferKind::Restore, 10));
    }
}
