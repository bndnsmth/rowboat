//! Bounded file preview and atomic streaming transfer with operation-specific cancellation.
use crate::{
    app::{Event, Page, Workbench},
    forms::{self, RowTarget},
    view::{action, column, label, row},
    workspace_model::Route,
};
use gpui_kit::{prelude::*, *};
use gpui_omarchy::ActiveTheme;
use rowboat_services::{
    ExportRequest, ImportPreview, TransferControl, TransferFormat, TransferOptions,
    TransferProgress, TransferSummary,
};
use std::path::PathBuf;

pub struct DataTransfer {
    id: uuid::Uuid,
    route: Route,
    target: Option<RowTarget>,
    import: bool,
    format: TransferFormat,
    sql: String,
    path: Option<PathBuf>,
    picking: bool,
    running: bool,
    preview: Option<ImportPreview>,
    control: TransferControl,
    progress: tokio::sync::watch::Receiver<TransferProgress>,
    message: Option<String>,
}
pub enum DataTransferEvent {
    Chosen {
        id: uuid::Uuid,
        result: Result<Option<PathBuf>, String>,
    },
    Preview {
        id: uuid::Uuid,
        result: Result<Box<ImportPreview>, String>,
    },
    Finished {
        id: uuid::Uuid,
        result: Result<TransferSummary, String>,
        transaction: Result<bool, String>,
    },
}
impl Workbench {
    pub fn show_data_transfer(
        &mut self,
        import: bool,
        format: TransferFormat,
        cx: &mut Context<Self>,
    ) {
        if !import && format == TransferFormat::JsonArray {
            self.fail("JSON arrays are import-only; choose CSV, JSON Lines or SQL export".into());
            cx.notify();
            return;
        }
        if !self.can_leave() || self.close_requested || self.has_extension_dialog() {
            cx.notify();
            return;
        }
        let Some(db) = self.database.as_ref() else {
            return;
        };
        if import && (!self.grid_writable() || self.transaction || !self.transaction_known) {
            self.fail("Import requires an idle writable base table, with no open transaction or pending edits.".into());
            cx.notify();
            return;
        }
        let target = if self.page == Page::Table {
            self.current_target()
        } else {
            None
        };
        let sql = if import {
            String::new()
        } else if let Some(target) = &target {
            format!(
                "SELECT * FROM {}.{}",
                rowboat_core::sql::quote_ident(db.engine(), &target.table.schema),
                rowboat_core::sql::quote_ident(db.engine(), &target.table.name)
            )
        } else {
            let tab = &self.tabs[self.active_tab];
            if tab.connection_id.as_ref() != self.profile.as_ref().map(|p| &p.id) {
                self.fail("Choose an editor owned by this connection".into());
                cx.notify();
                return;
            }
            let selected = tab.editor.read(cx).selected_value();
            let sql = if selected.is_empty() {
                tab.editor.read(cx).value().to_string()
            } else {
                selected.to_string()
            };
            match forms::parameters(&tab.parameters.read(cx).value())
                .and_then(|p| rowboat_admin::query::expand_named(db.engine(), &sql, &p))
            {
                Ok(sql) => sql,
                Err(e) => {
                    self.fail(format!("Export query: {e:#}"));
                    cx.notify();
                    return;
                }
            }
        };
        let control = TransferControl::default();
        let progress = control.subscribe();
        self.data_transfer = Some(DataTransfer {
            id: uuid::Uuid::new_v4(),
            route: self.tx.route,
            target,
            import,
            format,
            sql,
            path: None,
            picking: false,
            running: false,
            preview: None,
            control,
            progress,
            message: None,
        });
        cx.notify();
    }
    fn choose_data_file(&mut self, cx: &mut Context<Self>) {
        let Some(d) = &mut self.data_transfer else {
            return;
        };
        if d.running || d.picking || self.close_requested {
            return;
        }
        d.picking = true;
        d.preview = None;
        d.path = None;
        let id = d.id;
        let tx = self.tx.clone();
        if d.import {
            let receiver = cx.prompt_for_paths(PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: Some("Choose lossless CSV, JSON Lines or JSON array input".into()),
            });
            cx.spawn(async move |_, _| {
                let result = match receiver.await {
                    Ok(Ok(paths)) => Ok(paths.and_then(|p| p.into_iter().next())),
                    Ok(Err(e)) => Err(format!("{e:#}")),
                    Err(e) => Err(e.to_string()),
                };
                let _ = tx.send(Event::DataTransfer(Box::new(DataTransferEvent::Chosen {
                    id,
                    result,
                })));
            })
            .detach();
        } else {
            let name = match d.format {
                TransferFormat::JsonArray => {
                    d.picking = false;
                    d.message = Some("JSON arrays are import-only".into());
                    return;
                }
                TransferFormat::Csv => "rowboat-export.csv",
                TransferFormat::JsonLines => "rowboat-export.jsonl",
                TransferFormat::Sql => "rowboat-export.sql",
            };
            let receiver = cx.prompt_for_new_path(
                std::path::Path::new(std::env::var("HOME").as_deref().unwrap_or(".")),
                Some(name),
            );
            cx.spawn(async move |_, _| {
                let result = match receiver.await {
                    Ok(Ok(path)) => Ok(path),
                    Ok(Err(e)) => Err(format!("{e:#}")),
                    Err(e) => Err(e.to_string()),
                };
                let _ = tx.send(Event::DataTransfer(Box::new(DataTransferEvent::Chosen {
                    id,
                    result,
                })));
            })
            .detach();
        }
        cx.notify();
    }
    fn start_data_transfer(&mut self, apply: bool, cx: &mut Context<Self>) {
        if self.busy || self.close_requested || self.has_row_draft() || !self.changes.is_empty() {
            return;
        }
        let Some(d) = &self.data_transfer else {
            return;
        };
        if d.running
            || d.picking
            || d.route != self.tx.route
            || d.target.is_some() && d.target != self.current_target()
        {
            return;
        }
        let Some(path) = d.path.clone() else {
            return;
        };
        let Some(db) = self.database.clone() else {
            return;
        };
        if d.import
            && (self.transaction
                || !self.transaction_known
                || self.profile.as_ref().is_none_or(|p| p.read_only))
        {
            return;
        }
        let id = d.id;
        let import = d.import;
        let format = d.format;
        let sql = d.sql.clone();
        let target = d.target.clone();
        let preview = d.preview.clone();
        if apply && preview.is_none() {
            return;
        }
        let d = self.data_transfer.as_mut().unwrap();
        if import && !apply {
            d.preview = None;
        }
        d.control = TransferControl::default();
        d.progress = d.control.subscribe();
        d.running = true;
        d.message = None;
        let control = d.control.clone();
        self.busy = true;
        self.transfer_running = true;
        let tx = self.tx.clone();
        self.runtime.spawn(async move {
            if import && !apply {
                let result = rowboat_services::preview_import(
                    &path,
                    format,
                    &TransferOptions::default(),
                    &control,
                )
                .await
                .map(Box::new)
                .map_err(|e| format!("{e:#}"));
                let _ = tx.send(Event::DataTransfer(Box::new(DataTransferEvent::Preview {
                    id,
                    result,
                })));
            } else {
                let result = if import {
                    rowboat_services::apply_import(
                        &db,
                        &target.unwrap().table,
                        &preview.unwrap(),
                        &control,
                    )
                    .await
                } else {
                    rowboat_services::export_query(
                        &db,
                        &ExportRequest {
                            sql,
                            destination: path,
                            format,
                            table: target.map(|t| t.table),
                            options: TransferOptions::default(),
                        },
                        &control,
                    )
                    .await
                }
                .map_err(|e| format!("{e:#}"));
                let transaction = db.in_transaction().await.map_err(|e| format!("{e:#}"));
                let _ = tx.send(Event::DataTransfer(Box::new(DataTransferEvent::Finished {
                    id,
                    result,
                    transaction,
                })));
            }
        });
        cx.notify();
    }
    pub fn receive_data_transfer(&mut self, event: DataTransferEvent, cx: &mut Context<Self>) {
        let id = match &event {
            DataTransferEvent::Chosen { id, .. }
            | DataTransferEvent::Preview { id, .. }
            | DataTransferEvent::Finished { id, .. } => *id,
        };
        let Some(d) = &mut self.data_transfer else {
            return;
        };
        if d.id != id || d.route != self.tx.route {
            return;
        }
        match event {
            DataTransferEvent::Chosen { result, .. } => {
                d.picking = false;
                if self.close_requested {
                    return;
                }
                match result {
                    Ok(path) => d.path = path,
                    Err(error) => d.message = Some(error),
                }
            }
            DataTransferEvent::Preview { result, .. } => {
                d.running = false;
                self.busy = false;
                self.transfer_running = false;
                match result {
                    Ok(preview) => d.preview = Some(*preview),
                    Err(error) => d.message = Some(error),
                }
            }
            DataTransferEvent::Finished {
                result,
                transaction,
                ..
            } => {
                d.running = false;
                self.busy = false;
                self.transfer_running = false;
                d.preview = None;
                d.message = Some(match result {
                    Ok(summary) => format!(
                        "Completed · {} rows · {} bytes",
                        summary.rows, summary.bytes
                    ),
                    Err(e) => format!("Transfer failed / cancelled: {e}"),
                });
                if d.import {
                    self.table_result = Default::default();
                    self.structure = Default::default();
                    self.selected = None;
                }
                match transaction {
                    Ok(active) => {
                        self.transaction = active;
                        self.transaction_known = true;
                    }
                    Err(e) => {
                        self.transaction_known = false;
                        self.fail(e);
                    }
                }
            }
        }
        cx.notify();
    }
    pub fn poll_data_transfer(&mut self, cx: &mut Context<Self>) {
        if let Some(d) = &mut self.data_transfer
            && d.progress.has_changed().unwrap_or(false)
        {
            d.progress.borrow_and_update();
            cx.notify();
        }
    }
    pub fn dismiss_data_transfer(&mut self) {
        if self.data_transfer.as_ref().is_some_and(|d| !d.running) {
            self.data_transfer = None;
        }
    }
    pub fn cancel_data_transfer(&mut self) -> bool {
        if let Some(d) = &self.data_transfer
            && d.running
        {
            d.control.cancel();
            return true;
        }
        false
    }
    pub fn render_data_transfer(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        use gpui_omarchy as ui;
        let d = self.data_transfer.as_ref().unwrap();
        let theme = cx.omarchy().clone();
        let progress = d.progress.borrow();
        let mut body=column().p_4().gap_3().text_size(px(13.)).child(row().flex_wrap().child(ui::icon(if d.import { ui::IconName::ArrowDownToLine } else { ui::IconName::ArrowUpFromLine }).text_color(theme.accent)).child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child(if d.import{"Import table data"}else{"Export query data"})).child(div().flex_1()).child(action("close-stream-transfer","Close",false,!d.running,cx,|s,w,cx|{s.dismiss_data_transfer();w.focus(&s.focus,cx);cx.notify();})))
            .child(label(format!("Format: {:?} · bounded batches and preview; file contents are not retained in full",d.format),cx))
            .child(div().h(px(1.)).bg(theme.divider()));
        if let Some(target) = &d.target {
            body = body.child(label(
                format!(
                    "Target: {}.{} · session {}",
                    target.table.schema, target.table.name, target.session_generation
                ),
                cx,
            ));
        }
        if !d.import {
            body=body.child(label("Runs the captured SQL once when you choose Export. Table export selects ALL rows (browse filters/page limits are not used). Destination must be a new file. Export preserves current transaction ownership.",cx))
            .child(div().p_3().bg(theme.inset).font_family(theme.mono_font.clone()).child(d.sql.clone()));
        } else {
            body=body.child(label("CSV uses Rowboat lossless NULL/binary encoding. JSON Lines is one object per line; JSON array is a top-level array of row objects. Preview streams and validates the entire file, including its tail; Apply verifies its fingerprint again and imports inside one owned transaction.",cx));
            let mut formats = row().flex_wrap().p_2().bg(theme.surface);
            for (index, format) in [
                TransferFormat::Csv,
                TransferFormat::JsonLines,
                TransferFormat::JsonArray,
            ]
            .into_iter()
            .enumerate()
            {
                formats = formats.child(
                    action(
                        ("import-format", index),
                        match format {
                            TransferFormat::Csv => "CSV",
                            TransferFormat::JsonLines => "JSON Lines",
                            TransferFormat::JsonArray => "JSON array",
                            TransferFormat::Sql => "SQL",
                        },
                        false,
                        !d.running && !d.picking && !self.busy,
                        cx,
                        move |s, _, cx| {
                            if let Some(d) = &mut s.data_transfer
                                && !d.running
                                && !d.picking
                            {
                                d.format = format;
                                d.preview = None;
                                d.message = Some("Format changed; scan again before Apply".into());
                            }
                            cx.notify();
                        },
                    )
                    .selected(d.format == format)
                    .styles(|s| {
                        s.selected(|s| s.text_color(theme.accent).border_color(theme.accent))
                    }),
                );
            }
            body = body.child(formats);
        }
        body = body
            .child(
                action(
                    "choose-stream-file",
                    if d.picking {
                        "Choosing…"
                    } else {
                        "Choose file…"
                    },
                    false,
                    !d.running && !d.picking,
                    cx,
                    |s, _, cx| s.choose_data_file(cx),
                )
                .debug_selector(|| "choose-stream-file".into()),
            )
            .child(label(
                d.path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "No file selected".into()),
                cx,
            ));
        if let Some(message) = &d.message {
            body = body.child(label(message.clone(), cx));
        }
        if d.running {
            body=body.child(label("Progress counts scanned / queued rows. Only a successful Completed summary proves import commit; cancellation after commit begins waits for the actual result.",cx));
            body = body
                .child(label(
                    format!(
                        "{:?} · {} rows · {} bytes",
                        progress.phase, progress.rows, progress.bytes
                    ),
                    cx,
                ))
                .child(
                    action(
                        "cancel-stream-transfer",
                        "Cancel transfer",
                        false,
                        true,
                        cx,
                        |s, _, cx| {
                            s.cancel_data_transfer();
                            cx.notify();
                        },
                    )
                    .debug_selector(|| "cancel-stream-transfer".into()),
                );
        } else {
            if let Some(preview) = &d.preview {
                body=body.child(label(format!("Reviewed file: {} rows · {} bytes · {} columns; showing {} bounded sample rows",preview.summary.rows,preview.summary.bytes,preview.columns.len(),preview.sample.len()),cx));
                for sample in &preview.sample {
                    body = body.child(
                        div()
                            .font_family(theme.mono_font.clone())
                            .text_size(px(12.))
                            .child(
                                sample
                                    .iter()
                                    .map(|(name, cell)| format!("{name}: {}", cell.display()))
                                    .collect::<Vec<_>>()
                                    .join(" | "),
                            ),
                    );
                }
                body = body.child(
                    action(
                        "apply-stream-import",
                        "Apply reviewed import (transaction)",
                        true,
                        !self.busy && self.transaction_known && !self.transaction,
                        cx,
                        |s, _, cx| s.start_data_transfer(true, cx),
                    )
                    .debug_selector(|| "apply-stream-import".into()),
                );
            }
            body = body.child(
                action(
                    "start-stream-transfer",
                    if d.import {
                        "Scan file for preview"
                    } else {
                        "Export captured SQL to new file"
                    },
                    true,
                    d.path.is_some() && !d.picking && !self.busy,
                    cx,
                    |s, _, cx| s.start_data_transfer(false, cx),
                )
                .debug_selector(|| "start-stream-transfer".into()),
            );
        }
        self.modal(
            div()
                .id("stream-transfer-scroll")
                .w(px(900.).min(window.viewport_size().width - px(48.)))
                .max_h(window.viewport_size().height - px(65.))
                .overflow_y_scroll()
                .child(body)
                .into_any_element(),
            cx,
        )
    }
}
