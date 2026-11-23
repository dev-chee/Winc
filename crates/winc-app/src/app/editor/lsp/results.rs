//! 将服务器返回的位置、诊断、类型信息和补全项接入编辑器。

use std::ops::Range as Offsets;

use winc_core::edit::{Change, EditKind, Transaction};

use super::super::definition::{Place, PlacesKind};
use super::protocol::Range;
use super::*;

pub(super) struct Diagnostic {
    pub range: Range,
    pub severity: u8,
    message: String,
}

pub(super) fn diagnostics(value: &Value) -> Vec<Diagnostic> {
    let mut result: Vec<_> = value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            Some(Diagnostic {
                range: Range::parse(item.get("range")?)?,
                severity: item["severity"]
                    .as_u64()
                    .filter(|n| (1..=4).contains(n))
                    .unwrap_or(1) as u8,
                message: one_line(item["message"].as_str()?),
            })
        })
        .collect();
    result.sort_by_key(|item| item.range);
    result
}

pub(super) fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn hover(value: &Value) -> String {
    fn append(out: &mut Vec<String>, value: &Value) {
        if let Some(items) = value.as_array() {
            for item in items {
                append(out, item);
            }
        } else if let Some(text) = value.as_str().or_else(|| value["value"].as_str()) {
            out.extend(
                text.lines()
                    .filter(|line| !line.trim_start().starts_with("\x60\x60\x60"))
                    .map(str::to_owned),
            );
        }
    }
    let mut lines = Vec::new();
    append(&mut lines, &value["contents"]);
    lines.join("\n").trim().to_owned()
}

fn locations(value: &Value) -> Vec<(PathBuf, Range)> {
    let mut seen = HashSet::new();
    value
        .as_array()
        .map_or_else(|| std::slice::from_ref(value), Vec::as_slice)
        .iter()
        .filter_map(|item| {
            let uri = item["targetUri"]
                .as_str()
                .or_else(|| item["uri"].as_str())?;
            let range = Range::parse(
                item.get("targetSelectionRange")
                    .or_else(|| item.get("range"))?,
            )?;
            let path = file_path(uri)?;
            (!winc_backend::is_remote(&path) && seen.insert((path.clone(), range)))
                .then_some((path, range))
        })
        .collect()
}

struct TextEdit {
    range: Range,
    text: String,
}

pub(super) struct Completion {
    label: String,
    detail: String,
    sort: String,
    item: Value,
}

fn completions(value: &Value) -> Vec<Completion> {
    let items = value.as_array().or_else(|| value["items"].as_array());
    let mut result: Vec<_> = items
        .into_iter()
        .flatten()
        .filter_map(|item| {
            if item["insertTextFormat"] == 2 {
                return None;
            }
            let label = item["label"].as_str()?;
            Some(Completion {
                label: label.to_owned(),
                detail: item["detail"].as_str().map(one_line).unwrap_or_default(),
                sort: item["sortText"].as_str().unwrap_or(label).to_owned(),
                item: item.clone(),
            })
        })
        .collect();
    result.sort_by(|a, b| a.sort.cmp(&b.sort));
    result.truncate(500);
    result
}

fn offsets(doc: &Document, range: Range) -> Option<Offsets<usize>> {
    Some(range.start.offset(doc.text())?..range.end.offset(doc.text())?)
}

fn completion_transaction(
    doc: &Document,
    item: &Value,
    fallback: Offsets<usize>,
) -> Option<Transaction> {
    let mut main = fallback;
    let edit = &item["textEdit"];
    let insert = if edit.is_object() {
        let range = Range::parse(edit.get("range").or_else(|| edit.get("insert"))?)?;
        main = offsets(doc, range)?;
        if !(main.start..=main.end).contains(&doc.selection.head) {
            return None;
        }
        edit["newText"].as_str()?
    } else {
        item["insertText"]
            .as_str()
            .or_else(|| item["label"].as_str())?
    };
    let mut edits = Vec::new();
    for edit in item["additionalTextEdits"].as_array().into_iter().flatten() {
        let edit = TextEdit {
            range: Range::parse(edit.get("range")?)?,
            text: edit["newText"].as_str()?.to_owned(),
        };
        edits.push((offsets(doc, edit.range)?, edit.text, false));
    }
    edits.push((main, insert.to_owned(), true));
    edits.sort_by_key(|(range, _, _)| (range.start, range.end));
    if edits
        .windows(2)
        .any(|pair| pair[0].0.end > pair[1].0.start || pair[0].0.start == pair[1].0.start)
    {
        return None;
    }
    let mut shift = 0;
    let mut caret = doc.selection.head;
    let mut changes = Vec::with_capacity(edits.len());
    for (range, inserted, main) in edits {
        let removed = doc.text_range(range.start, range.end)?;
        let inserted = inserted
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\n', doc.line_ending.as_str());
        let start = range.start.checked_add_signed(shift)?;
        let length = inserted.chars().count();
        if main {
            caret = start + length;
        }
        shift += length as isize - range.len() as isize;
        if inserted != removed {
            changes.push(Change {
                start,
                inserted,
                removed,
            });
        }
    }
    Some(Transaction::new(
        changes,
        doc.selection,
        Selection::point(caret),
        EditKind::Other,
    ))
}

impl App {
    pub(super) fn lsp_result(&mut self, kind: Query, origin: Origin, value: &Value) {
        match kind {
            Query::Definition | Query::References => self.show_lsp_locations(kind, value),
            Query::Hover => {
                let hover = hover(value);
                if hover.is_empty() {
                    self.hint_at_name(t!("editor-lsp-no-hover"));
                } else {
                    self.detail_at_name(hover);
                }
            }
            Query::Completion => {
                let choices = completions(value);
                if choices.is_empty() {
                    self.hint_at_name(t!("editor-lsp-no-completions"));
                    return;
                }
                let items = choices
                    .iter()
                    .map(|item| Place {
                        path: origin.path.clone(),
                        relative: item.label.clone(),
                        line: 0,
                        col: None,
                        preview: item.detail.clone(),
                        marks: Vec::new(),
                        lsp_range: None,
                    })
                    .collect();
                self.language_servers.completion = Some((origin, choices));
                self.show_places(PlacesKind::Completions, String::new(), items);
                self.request_redraw();
            }
        }
    }

    fn show_lsp_locations(&mut self, kind: Query, value: &Value) {
        let name = self
            .name_span()
            .and_then(|(from, to)| self.editor.active()?.text_range(from, to))
            .unwrap_or_default();
        let targets = locations(value);
        if targets.is_empty() {
            let message = if kind == Query::References {
                t!("editor-lsp-no-references").to_owned()
            } else {
                t!("editor-definition-not-found", name = name)
            };
            self.hint_at_name(message);
            return;
        }
        if kind == Query::Definition && targets.len() == 1 {
            let (path, range) = &targets[0];
            let already_there = self.editor.active().is_some_and(|doc| {
                let position = Position::at(doc.text(), doc.selection.head);
                (range.start..=range.end).contains(&position)
                    && doc.path.as_deref().and_then(file_uri) == file_uri(path)
            });
            if already_there {
                if !self.lsp_references() {
                    self.find_text_references();
                }
                return;
            }
            self.open_lsp_location(path, *range);
            return;
        }
        let items = targets
            .into_iter()
            .map(|(path, range)| {
                let relative = winc_editor::display_path(&path, self.editor.root());
                let preview = self
                    .editor
                    .documents
                    .iter()
                    .find(|doc| doc.path.as_ref() == Some(&path))
                    .and_then(|doc| {
                        (range.start.line < doc.text().line_count()).then(|| {
                            doc.text()
                                .line_content(range.start.line)
                                .slice()
                                .chars()
                                .take(300)
                                .collect::<String>()
                        })
                    })
                    .map_or_else(|| relative.clone(), |text| text.trim_start().to_owned());
                Place {
                    path,
                    relative,
                    line: range.start.line,
                    col: None,
                    preview,
                    marks: Vec::new(),
                    lsp_range: Some(range),
                }
            })
            .collect();
        let list = if kind == Query::References {
            PlacesKind::Usages
        } else {
            PlacesKind::Definitions
        };
        self.show_places(list, name, items);
        self.request_redraw();
    }

    pub(in crate::app::editor) fn open_lsp_location(&mut self, path: &Path, range: LocationRange) {
        let target = winc_editor::canonical_path(path);
        self.open_path_then(path, move |app| {
            let Some(doc) = app
                .editor
                .active()
                .filter(|doc| doc.path.as_ref() == Some(&target))
            else {
                return;
            };
            let Some(span) = offsets(doc, range) else {
                return;
            };
            app.focus = Focus::Editor;
            app.select(span.start, span.end);
            app.request_redraw();
        });
    }

    pub(in crate::app::editor) fn accept_lsp_completion(&mut self, index: usize) {
        let Some((origin, mut items)) = self.language_servers.completion.take() else {
            return;
        };
        if !self.lsp_origin_current(&origin) || index >= items.len() {
            return;
        }
        let item = items.swap_remove(index).item;
        if !item["data"].is_null()
            && let Some(project) = self
                .language_servers
                .documents
                .get(&origin.doc)
                .map(|tracked| tracked.project.clone())
        {
            let servers = &mut self.language_servers;
            servers.cancel();
            let id = servers.next_id();
            let until = Instant::now() + QUERY_TIMEOUT;
            if servers.send(
                &project,
                Input::Resolve {
                    id,
                    item: item.clone(),
                    until,
                },
            ) {
                servers.pending = Some(Pending {
                    id,
                    operation: Operation::Resolve(item),
                    origin,
                    project,
                    until,
                });
            } else {
                self.hint_at_name(t!("editor-lsp-unavailable"));
            }
            return;
        }
        self.apply_lsp_completion(&item);
    }

    pub(super) fn apply_lsp_completion(&mut self, item: &Value) {
        let prefix = self.name_span();
        let Some(doc) = self.editor.active_mut() else {
            return;
        };
        if !doc.edits_text() || doc.readonly_reason().is_some() || doc.has_cursors() {
            return;
        }
        let caret = doc.selection.head;
        let fallback = if doc.selection.is_empty() {
            prefix
                .filter(|&(from, to)| (from..=to).contains(&caret))
                .map_or(caret..caret, |(from, _)| from..caret)
        } else {
            doc.selection.from()..doc.selection.to()
        };
        if let Some(transaction) = completion_transaction(doc, item, fallback) {
            doc.apply(transaction);
            self.relayout();
            self.ensure_caret_visible();
            self.reset_blink();
            self.request_redraw();
        } else {
            self.hint_at_name(t!("editor-lsp-invalid-completion"));
        }
    }

    /// 在位置列表中显示当前文档的语言服务诊断。
    pub(in crate::app) fn show_lsp_diagnostics(&mut self) {
        let mut items = Vec::new();
        if let Some(doc) = self.editor.active()
            && let Some(path) = &doc.path
        {
            items.extend(
                self.language_servers
                    .diagnostics(doc)
                    .iter()
                    .map(|item| Place {
                        path: path.clone(),
                        relative: winc_editor::display_path(path, self.editor.root()),
                        line: item.range.start.line,
                        col: None,
                        preview: item.message.clone(),
                        marks: Vec::new(),
                        lsp_range: Some(item.range),
                    }),
            );
        }
        if items.is_empty() {
            let tracked = self.editor.active().and_then(|doc| {
                let tracked = self.language_servers.documents.get(&doc.id)?;
                Some((doc, tracked))
            });
            let message = match tracked {
                Some((doc, tracked)) => {
                    match self.language_servers.sessions.get(&tracked.project) {
                        Some(Session {
                            connection: Err(error),
                            ..
                        }) => {
                            self.hint_at_name(t!("editor-lsp-error", error = one_line(error)));
                            return;
                        }
                        _ if tracked.revision == Some(doc.revision)
                            && tracked.diagnostics.is_some() =>
                        {
                            t!("editor-lsp-no-diagnostics")
                        }
                        _ => t!("editor-lsp-diagnostics-pending"),
                    }
                }
                None if self.link.proxy.is_some() => t!("editor-lsp-loading"),
                None => t!("editor-lsp-unavailable"),
            };
            self.hint_at_name(message);
        } else {
            self.show_places(PlacesKind::Diagnostics, String::new(), items);
            self.request_redraw();
        }
    }
}
