//! Connects the Android document picker and input connection to the shared editor.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use winc_core::{Lines, Selection};
use winc_editor::{Command, DocumentId, HandedSave, PreviewMode};
use winc_i18n::t;
use winit::event::Ime;

use super::{App, Focus, NoticeKind};
use crate::android::{Activity, Event, text};

#[derive(Default)]
pub(crate) struct Mobile {
    pub(crate) activity: Option<Arc<Activity>>,
    pub(crate) started: Option<Instant>,
    serial: i64,
    picking: Option<i64>,
    document: Option<Source>,
    saving: Option<PendingSave>,
    exporting: Option<PendingExport>,
    input: Option<Input>,
    recentering: Option<Input>,
}

struct Source {
    id: DocumentId,
    mirror: PathBuf,
    uri: String,
    writable: bool,
    exported_revision: Option<u64>,
}

struct PendingSave {
    token: i64,
    id: DocumentId,
    staged: PathBuf,
    job: HandedSave,
}

struct PendingExport {
    token: i64,
    draft: Option<(DocumentId, u64, PathBuf)>,
}

struct Input {
    token: i64,
    sequence: i64,
    id: DocumentId,
    revision: u64,
    selection: Selection,
    context: text::InputText,
    composing: bool,
    valid: bool,
}

impl Mobile {
    fn busy(&self) -> bool {
        self.picking.is_some() || self.saving.is_some() || self.exporting.is_some()
    }

    fn can_open(&self, doc: &winc_editor::Document) -> bool {
        !doc.is_dirty()
            || self.document.as_ref().is_some_and(|source| {
                source.id == doc.id && source.exported_revision == Some(doc.revision)
            })
    }

    fn token(&mut self) -> i64 {
        self.serial += 1;
        self.serial
    }
}

impl App {
    pub(super) fn android_event(&mut self, event: Event) {
        match event {
            Event::Open => self.android_open(),
            Event::Save => self.save_active(),
            Event::ExportDraft => self.android_export(false),
            Event::ExportOriginal => self.android_export(true),
            Event::ExportEnded(token, status) => self.android_exported(token, status),
            Event::Keyboard => self.android_sync_input(true),
            Event::Undo => self.run(Command::Undo),
            Event::Input {
                token,
                sequence,
                positions,
                text,
            } => self.android_input(token, sequence, positions, text),
            Event::InputRecentered(token, applied) => {
                if self
                    .android
                    .recentering
                    .as_ref()
                    .is_some_and(|input| input.token == token)
                {
                    let input = self.android.recentering.take();
                    if applied {
                        self.android.input = input;
                    }
                }
            }
            Event::Opened {
                token,
                path,
                uri,
                writable,
            } => {
                self.android_opened(token, path, uri, writable);
            }
            Event::OpenEnded(token, status) => {
                if self.android.picking == Some(token) {
                    self.android.picking = None;
                    match status {
                        0 => {}
                        2 => self.notify(NoticeKind::Error, t!("workspace-android-text-only")),
                        _ => self.notify(NoticeKind::Error, t!("workspace-android-open-failed")),
                    }
                }
            }
            Event::Saved(token, status) => self.android_saved(token, status),
            Event::Paused => self.android_suspend(),
            Event::Layout => {
                if let Some(renderer) = &mut self.frame.renderer {
                    renderer.resize();
                }
                self.relayout();
                self.ensure_caret_visible();
                self.request_redraw();
            }
        }
    }

    pub(super) fn android_open(&mut self) {
        if self.android.busy() {
            return;
        }
        if self
            .editor
            .documents
            .iter()
            .any(|doc| !self.android.can_open(doc))
        {
            self.notify(
                NoticeKind::Warning,
                t!("workspace-android-save-before-open"),
            );
            return;
        }
        let token = self.android.token();
        self.android_close_input();
        if self
            .android
            .activity
            .as_ref()
            .is_some_and(|activity| activity.pick(token))
        {
            self.android.picking = Some(token);
        } else {
            self.notify(NoticeKind::Error, t!("workspace-android-open-failed"));
        }
    }

    fn android_opened(&mut self, token: i64, path: PathBuf, uri: String, writable: bool) {
        if self.android.picking != Some(token) {
            remove_mirror(&path);
            return;
        }
        self.android.picking = None;
        if self
            .editor
            .documents
            .iter()
            .any(|doc| !self.android.can_open(doc))
        {
            remove_mirror(&path);
            self.notify(
                NoticeKind::Warning,
                t!("workspace-android-save-before-open"),
            );
            return;
        }
        let index = self.editor.open_tab(&path);
        self.editor.load(index);
        let doc = &mut self.editor.documents[index];
        doc.set_preview_mode(PreviewMode::Source);
        let id = doc.id;
        if !doc.edits_text() {
            self.close_documents(&[id]);
            remove_mirror(&path);
            self.notify(NoticeKind::Error, t!("workspace-android-text-only"));
            return;
        }
        self.activate_tab(index);
        let old: Vec<_> = self
            .editor
            .documents
            .iter()
            .filter_map(|doc| (doc.id != id).then_some(doc.id))
            .collect();
        self.close_documents(&old);
        if let Some(previous) = self.android.document.replace(Source {
            id,
            mirror: path,
            uri,
            writable,
            exported_revision: None,
        }) {
            remove_mirror(&previous.mirror);
        }
        self.focus = Focus::Editor;
        self.relayout();
        self.ensure_caret_visible();
        self.request_redraw();
    }

    pub(super) fn android_save(&mut self, id: DocumentId) {
        if self.android.busy() {
            return;
        }
        let token = self.android.token();
        let Some(source) = self
            .android
            .document
            .as_ref()
            .filter(|source| source.id == id)
        else {
            self.notify(NoticeKind::Warning, t!("workspace-android-open-first"));
            return;
        };
        if !source.writable {
            self.notify(NoticeKind::Error, t!("workspace-android-read-only"));
            return;
        }
        let Some(doc) = self.editor.document_mut(id) else {
            return;
        };
        let Some(parent) = source.mirror.parent().and_then(Path::parent) else {
            return;
        };
        let staged = parent.join("pending-save");
        let job = match doc.begin_handed_save(&staged) {
            Ok(job) => job,
            Err(error) => {
                self.notify(
                    NoticeKind::Error,
                    t!("workspace-android-save-failed", error = error.to_string()),
                );
                return;
            }
        };
        if self.android.activity.as_ref().is_some_and(|activity| {
            activity.save(
                token,
                &source.uri,
                &source.mirror.to_string_lossy(),
                &staged.to_string_lossy(),
            )
        }) {
            self.android.saving = Some(PendingSave {
                token,
                id,
                staged,
                job,
            });
        } else {
            self.notify(NoticeKind::Error, t!("workspace-android-write-failed"));
        }
    }

    fn android_saved(&mut self, token: i64, status: i32) {
        if self
            .android
            .saving
            .as_ref()
            .is_none_or(|save| save.token != token)
        {
            return;
        }
        let Some(save) = self.android.saving.take() else {
            return;
        };
        match status {
            0 => {}
            3 => {
                self.notify(NoticeKind::Error, t!("workspace-android-text-only"));
                return;
            }
            2 => {
                self.notify(NoticeKind::Error, t!("workspace-android-save-conflict"));
                return;
            }
            4 => {
                self.notify(NoticeKind::Error, t!("workspace-android-write-uncertain"));
                return;
            }
            _ => {
                self.notify(NoticeKind::Error, t!("workspace-android-write-failed"));
                return;
            }
        }
        let result = winc_config::files::replace_file(&save.staged, save.job.target());
        let saved = save.job.finish(result);
        if let Some(doc) = self.editor.document_mut(save.id) {
            match doc.finish_save(saved) {
                Ok(()) => self.notify(NoticeKind::Info, t!("workspace-android-saved")),
                Err(error) => self.notify(
                    NoticeKind::Error,
                    t!("workspace-android-save-failed", error = error.to_string()),
                ),
            }
        }
    }

    fn android_export(&mut self, original: bool) {
        if self.android.busy() {
            return;
        }
        let (Some(source), Some(doc)) = (&self.android.document, self.editor.active()) else {
            self.notify(NoticeKind::Warning, t!("workspace-android-open-first"));
            return;
        };
        if source.id != doc.id {
            return;
        }
        let name = source
            .mirror
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("document.txt");
        let uri = source.uri.clone();
        let (path, draft) = if original {
            (source.mirror.clone(), None)
        } else {
            let Some(parent) = source.mirror.parent().and_then(Path::parent) else {
                return;
            };
            let path = parent.join("export-copy");
            if let Err(error) = doc.write_copy(&path) {
                self.notify(
                    NoticeKind::Error,
                    t!("workspace-android-save-failed", error = error.to_string()),
                );
                return;
            }
            (path.clone(), Some((doc.id, doc.revision, path)))
        };
        let name = name.to_owned();
        self.android_close_input();
        let token = self.android.token();
        self.android.exporting = Some(PendingExport { token, draft });
        if !self
            .android
            .activity
            .as_ref()
            .is_some_and(|activity| activity.export(token, &path.to_string_lossy(), &uri, &name))
        {
            self.android_exported(token, 1);
        }
    }

    fn android_exported(&mut self, token: i64, status: i32) {
        if self
            .android
            .exporting
            .as_ref()
            .is_none_or(|export| export.token != token)
        {
            return;
        }
        let Some(export) = self.android.exporting.take() else {
            return;
        };
        if let Some((id, revision, path)) = export.draft {
            remove_private(&path);
            if status == 0
                && let Some(source) = &mut self.android.document
                && source.id == id
            {
                source.exported_revision = Some(revision);
            }
        }
        match status {
            -1 => {}
            0 => self.notify(NoticeKind::Info, t!("workspace-android-exported")),
            4 => self.notify(NoticeKind::Error, t!("workspace-android-export-uncertain")),
            _ => self.notify(NoticeKind::Error, t!("workspace-android-export-failed")),
        }
    }

    fn android_close_input(&mut self) {
        self.android.input = None;
        self.android.recentering = None;
        self.on_ime(Ime::Disabled);
        if let Some(activity) = &self.android.activity {
            activity.hide_input();
        }
    }

    pub(super) fn android_suspend(&mut self) {
        self.android_close_input();
        self.cancel_touch();
    }

    pub(crate) fn android_sync_input(&mut self, show: bool) {
        if !show && self.android.input.is_none() {
            return;
        }
        let Some(doc) = self.editor.active().filter(|doc| {
            self.window.handle.is_some()
                && self.focus == Focus::Editor
                && doc.edits_text()
                && self
                    .android
                    .document
                    .as_ref()
                    .is_some_and(|source| source.id == doc.id)
        }) else {
            if self.android.input.is_some() {
                self.android_close_input();
            }
            return;
        };
        let previous = if !show
            && let Some(input) = self.android.input.as_ref().filter(|input| {
                input.valid
                    && input.id == doc.id
                    && input.revision == doc.revision
                    && input.selection == doc.selection
            }) {
            if input.composing
                || self.android.recentering.is_some()
                || !input.context.near_edge(doc)
            {
                return;
            }
            Some((input.token, input.sequence))
        } else {
            None
        };
        let id = doc.id;
        let revision = doc.revision;
        let selection = doc.selection;
        let context = text::InputText::capture(doc);
        let anchor = context.utf16_at(selection.anchor);
        let head = context.utf16_at(selection.head);
        let token = self.android.token();
        let Some(activity) = &self.android.activity else {
            return;
        };
        if let Some(previous) = previous {
            if !activity.recenter(previous, token, &context.text, anchor, head) {
                return;
            }
        } else if !activity.input(token, &context.text, anchor, head, show) {
            return;
        }
        let input = Input {
            token,
            sequence: 0,
            id,
            revision,
            selection,
            context,
            composing: false,
            valid: true,
        };
        if previous.is_some() {
            self.android.recentering = Some(input);
        } else {
            self.android.recentering = None;
            self.android.input = Some(input);
            self.on_ime(Ime::Disabled);
        }
    }

    fn android_input(
        &mut self,
        token: i64,
        sequence: i64,
        [anchor, head, compose_start, compose_end]: [i32; 4],
        next: String,
    ) {
        let Some(input) = &mut self.android.input else {
            return;
        };
        if input.token != token
            || sequence <= input.sequence
            || self.focus != Focus::Editor
            || !self.editor.active().is_some_and(|doc| {
                doc.id == input.id
                    && doc.revision == input.revision
                    && doc.selection == input.selection
            })
        {
            return;
        }
        input.sequence = sequence;
        input.composing = compose_start >= 0 && compose_end > compose_start;
        if input.composing {
            let offset = |pos| text::char_offset(&next, pos);
            if let (Some(start), Some(end)) = (offset(compose_start), offset(compose_end)) {
                let preedit = next.chars().skip(start).take(end - start).collect();
                self.on_ime(Ime::Preedit(preedit, None));
            }
            return;
        }
        self.on_ime(Ime::Disabled);
        let (Some(input), Some(doc)) = (&mut self.android.input, self.editor.active_mut()) else {
            return;
        };
        let lines = doc.text().line_count();
        let Some(context) = input.context.apply(doc, next, anchor, head) else {
            input.valid = false;
            return;
        };
        input.revision = doc.revision;
        input.selection = doc.selection;
        input.valid = true;
        input.context = context;
        if lines != doc.text().line_count() {
            self.relayout();
        }
        self.ensure_caret_visible();
        self.reset_blink();
        self.request_redraw();
    }
}

/// Removes an activity-created mirror and its private staging directory.
#[allow(
    clippy::disallowed_methods,
    reason = "only removes Android app-private files"
)]
fn remove_mirror(path: &Path) {
    remove_private(path);
    if let Some(dir) = path.parent() {
        let _ = std::fs::remove_dir(dir);
        if let Some(parent) = dir.parent() {
            remove_private(&parent.join("pending-save"));
            let _ = std::fs::remove_dir(parent);
        }
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "only removes Android app-private files"
)]
fn remove_private(path: &Path) {
    let _ = std::fs::remove_file(path);
}
