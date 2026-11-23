//! 打开的 Rust 和 Teal 文档、项目会话、后台查询和诊断。

mod client;
mod projects;
mod protocol;
mod results;
#[cfg(test)]
mod tests;
mod watch;

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use serde_json::Value;
use winc_editor::Document;
use winc_i18n::t;

use super::*;
use client::{Connection, Input, Output, Project, Query, Server};
pub(crate) use protocol::Range as LocationRange;
use protocol::{Position, file_path, file_uri};
use results::{Completion, Diagnostic};

const EDIT_DELAY: Duration = Duration::from_millis(120);
const QUERY_TIMEOUT: Duration = Duration::from_secs(18);
const MAX_TEXT: usize = 4 * 1024 * 1024;

struct Session {
    generation: u64,
    connection: Result<Connection, String>,
    ready: bool,
    versioned_diagnostics: bool,
    linked_projects: Vec<PathBuf>,
    open_documents: usize,
}

struct Tracked {
    path: PathBuf,
    uri: String,
    project: Project,
    revision: Option<u64>,
    version: u64,
    due: Option<Instant>,
    diagnostics: Option<Vec<Diagnostic>>,
    seen: u64,
}

#[derive(Clone)]
struct Origin {
    doc: DocumentId,
    path: PathBuf,
    revision: u64,
    selection: Selection,
}

impl Origin {
    fn new(doc: &Document) -> Option<Self> {
        Some(Self {
            doc: doc.id,
            path: doc.path.clone()?,
            revision: doc.revision,
            selection: doc.selection,
        })
    }

    fn matches(&self, doc: &Document) -> bool {
        self.doc == doc.id
            && doc.path.as_ref() == Some(&self.path)
            && self.revision == doc.revision
            && self.selection == doc.selection
    }
}

struct Pending {
    id: u64,
    operation: Operation,
    origin: Origin,
    project: Project,
    until: Instant,
}

enum Operation {
    Query(Query),
    Resolve(Value),
}

struct Deferred {
    kind: Query,
    origin: Origin,
    until: Instant,
}

type SessionMessage = (Project, u64, Output);

/// 窗口的会话、文档版本及编辑器中展示的语言服务结果。
pub(crate) struct LanguageServers {
    sessions: HashMap<Project, Session>,
    documents: HashMap<DocumentId, Tracked>,
    messages: (Sender<SessionMessage>, Receiver<SessionMessage>),
    folders: Vec<PathBuf>,
    rust_projects: projects::RustProjects,
    deferred: Option<Deferred>,
    pending: Option<Pending>,
    completion: Option<(Origin, Vec<Completion>)>,
    sync_pass: u64,
    serial: u64,
}

impl Default for LanguageServers {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            documents: HashMap::new(),
            messages: mpsc::channel(),
            folders: Vec::new(),
            rust_projects: Default::default(),
            deferred: None,
            pending: None,
            completion: None,
            sync_pass: 0,
            serial: 1, // 0 和 1 由协议初始化及关闭使用。
        }
    }
}

impl LanguageServers {
    fn next_id(&mut self) -> u64 {
        self.serial += 1;
        self.serial
    }

    fn send(&self, project: &Project, input: Input) -> bool {
        self.sessions
            .get(project)
            .and_then(|session| session.connection.as_ref().ok())
            .is_some_and(|connection| connection.send(input))
    }

    fn forget(&mut self, id: DocumentId) {
        if let Some(doc) = self.documents.remove(&id) {
            self.send(&doc.project, Input::Close(doc.uri));
            if let Some(session) = self.sessions.get_mut(&doc.project) {
                session.open_documents -= 1;
            }
        }
    }

    fn cancel(&mut self) {
        self.deferred = None;
        if let Some(pending) = self.pending.take() {
            self.send(&pending.project, Input::Cancel(pending.id));
        }
    }

    fn diagnostics(&self, doc: &Document) -> &[Diagnostic] {
        self.documents
            .get(&doc.id)
            .filter(|tracked| {
                tracked.revision == Some(doc.revision) && doc.path.as_ref() == Some(&tracked.path)
            })
            .and_then(|tracked| tracked.diagnostics.as_deref())
            .unwrap_or_default()
    }

    /// 指定行上最严重的诊断。结果必须对应当前缓冲区。
    pub(crate) fn diagnostic_severity(&self, doc: &Document, line: usize) -> Option<u8> {
        let list = self.diagnostics(doc);
        let start = list.partition_point(|item| item.range.start.line < line);
        list[start..]
            .iter()
            .take_while(|item| item.range.start.line == line)
            .map(|item| item.severity)
            .min()
    }
}

fn candidate(doc: &Document) -> Option<(&Path, Server, &'static str)> {
    let path = doc.path.as_deref()?;
    let ext = path.extension()?.to_str()?;
    let (server, language) = if ext.eq_ignore_ascii_case("rs") {
        (Server::Rust, "rust")
    } else if ext.eq_ignore_ascii_case("tl") {
        (Server::Teal, "teal")
    } else if ext.eq_ignore_ascii_case("tli") {
        (Server::Teal, "teal-interface")
    } else {
        return None;
    };
    (path.is_absolute()
        && !winc_backend::is_remote(path)
        && doc.shows_text()
        && doc
            .text()
            .rope()
            .is_some_and(|text| text.len_bytes() <= MAX_TEXT))
    .then_some((path, server, language))
}

impl App {
    /// 检查文档打开、关闭、改名及编辑，合并连续键入，并安排下一次同步。
    pub(in crate::app) fn language_server_tick(&mut self, now: Instant) -> Option<Instant> {
        self.link.proxy.as_ref()?;
        self.sync_lsp(now, None);
        if let Some(deferred) = self.language_servers.deferred.take()
            && self.lsp_origin_current(&deferred.origin)
        {
            if now >= deferred.until {
                self.lsp_failed(deferred.kind, t!("editor-lsp-timeout"));
            } else if self.query_route_ready(&deferred.origin) {
                self.query_lsp_until(deferred.kind, deferred.until);
            } else {
                self.language_servers.deferred = Some(deferred);
            }
        }
        if !self
            .overlays
            .picker
            .places
            .as_ref()
            .is_some_and(|p| p.kind == super::definition::PlacesKind::Completions)
        {
            self.language_servers.completion = None;
        }
        let servers = &mut self.language_servers;
        if let Some(pending) = &servers.pending {
            let still_here = self
                .editor
                .active()
                .is_some_and(|doc| pending.origin.matches(doc))
                && self.focus == Focus::Editor
                && !self.overlays.picker.open;
            if !still_here {
                servers.cancel();
            } else if now >= pending.until {
                let pending = servers.pending.take().expect("pending LSP request");
                servers.send(&pending.project, Input::Cancel(pending.id));
                self.lsp_request_failed(pending.operation, t!("editor-lsp-timeout"));
            }
        }
        self.language_servers
            .documents
            .values()
            .filter_map(|doc| doc.due)
            .chain(
                self.language_servers
                    .pending
                    .as_ref()
                    .map(|pending| pending.until),
            )
            .chain(
                self.language_servers
                    .deferred
                    .as_ref()
                    .map(|deferred| deferred.until),
            )
            .min()
    }

    fn sync_lsp(&mut self, now: Instant, force: Option<DocumentId>) {
        let Some(proxy) = self.link.proxy.clone() else {
            return;
        };
        self.sync_lsp_with(now, force, move || {
            let _ = proxy.send_event(UserEvent::LanguageServer);
        });
    }

    fn sync_lsp_with(
        &mut self,
        now: Instant,
        force: Option<DocumentId>,
        wake: impl Fn() + Clone + Send + 'static,
    ) {
        let folders = || {
            self.editor
                .workspace
                .iter()
                .flat_map(|workspace| workspace.folders())
                .filter(|folder| !winc_backend::is_remote(folder))
        };
        let servers = &mut self.language_servers;
        if !folders().eq(servers.folders.iter().map(PathBuf::as_path)) {
            servers.folders = folders().map(Path::to_owned).collect();
            servers
                .documents
                .retain(|_, tracked| tracked.project.server != Server::Teal);
            servers
                .sessions
                .retain(|project, _| project.server != Server::Teal);
            if servers
                .pending
                .as_ref()
                .is_some_and(|request| request.project.server == Server::Teal)
            {
                servers.cancel();
            }
        }
        servers.rust_projects.poll();

        servers.sync_pass += 1;
        let pass = servers.sync_pass;
        let mut teal = servers
            .sessions
            .keys()
            .find(|project| project.server == Server::Teal)
            .cloned();
        let mut additions = Vec::new();
        let mut modified = Vec::new();
        let mut linked = HashMap::<Project, Vec<PathBuf>>::new();
        for doc in &self.editor.documents {
            let Some((path, server, language)) = candidate(doc) else {
                continue;
            };
            let project = match server {
                Server::Rust => servers.rust_projects.requested(
                    path,
                    &servers.folders,
                    self.format.formatter.shell(),
                    wake.clone(),
                ),
                Server::Teal => Some(
                    teal.get_or_insert_with(|| projects::teal_project(&servers.folders, path))
                        .clone(),
                ),
            };
            if let Some(project) = &project
                && project.is_standalone()
            {
                linked
                    .entry(project.clone())
                    .or_default()
                    .push(path.to_owned());
            }
            if let Some(tracked) = servers.documents.get_mut(&doc.id)
                && tracked.path == path
                && project
                    .as_ref()
                    .is_none_or(|project| &tracked.project == project)
            {
                tracked.seen = pass;
                if tracked.revision != Some(doc.revision) {
                    modified.push((doc, language));
                } else {
                    tracked.due = None;
                }
            } else if let Some(project) = project
                && let Some(uri) = file_uri(path)
            {
                additions.push((doc, path, language, project, uri));
            }
        }
        for paths in linked.values_mut() {
            paths.sort_unstable();
            paths.dedup();
        }

        // 一次变更可能关闭并重新打开同一 URI，先发完旧文档的关闭消息。
        let closed: Vec<_> = servers
            .documents
            .iter()
            .filter_map(|(&id, tracked)| (tracked.seen != pass).then_some(id))
            .collect();
        for id in closed {
            servers.forget(id);
        }
        for (doc, path, language, project, uri) in additions {
            if !servers.sessions.contains_key(&project) {
                let generation = servers.next_id();
                let messages = servers.messages.0.clone();
                let project_key = project.clone();
                let wake = wake.clone();
                let entries = linked.get(&project).cloned().unwrap_or_default();
                let connection = Connection::start(
                    project.clone(),
                    entries.clone(),
                    self.format.formatter.shell().clone(),
                    move |message| {
                        if messages
                            .send((project_key.clone(), generation, message))
                            .is_ok()
                        {
                            wake();
                        }
                    },
                )
                .map_err(|error| error.to_string());
                servers.sessions.insert(
                    project.clone(),
                    Session {
                        generation,
                        connection,
                        ready: false,
                        versioned_diagnostics: false,
                        linked_projects: entries,
                        open_documents: 0,
                    },
                );
            }
            servers
                .sessions
                .get_mut(&project)
                .expect("tracked session")
                .open_documents += 1;
            servers.documents.insert(
                doc.id,
                Tracked {
                    path: path.to_owned(),
                    uri,
                    project,
                    revision: None,
                    version: 0,
                    due: None,
                    diagnostics: None,
                    seen: pass,
                },
            );
            modified.push((doc, language));
        }

        let force_project = force
            .and_then(|id| servers.documents.get(&id))
            .map(|doc| doc.project.clone());
        for (doc, language) in modified {
            let tracked = servers
                .documents
                .get_mut(&doc.id)
                .expect("tracked document");
            if tracked.revision.is_some()
                && force_project.as_ref() != Some(&tracked.project)
                && now < *tracked.due.get_or_insert(now + EDIT_DELAY)
            {
                continue;
            }
            let project = tracked.project.clone();
            let uri = tracked.uri.clone();
            let unchanged_lines = tracked
                .revision
                .map_or(0, |revision| doc.unchanged_lines_since(revision));
            let version = servers.next_id();
            let Some(text) = doc.text().rope().cloned() else {
                continue;
            };
            servers.send(
                &project,
                Input::Sync {
                    uri,
                    language,
                    version,
                    text,
                    unchanged_lines,
                },
            );
            let tracked = servers
                .documents
                .get_mut(&doc.id)
                .expect("tracked document");
            tracked.revision = Some(doc.revision);
            tracked.version = version;
            tracked.due = None;
            tracked.diagnostics = None;
        }

        for (project, paths) in linked {
            let Some(session) = servers.sessions.get_mut(&project) else {
                continue;
            };
            if session.linked_projects != paths {
                session.linked_projects = paths.clone();
                if let Ok(connection) = &session.connection {
                    connection.send(Input::LinkedProjects(paths));
                }
            }
        }
        servers
            .sessions
            .retain(|_, session| session.open_documents != 0);
    }

    /// 保存成功后通知服务器，先同步仍在等待的缓冲区更改。
    pub(in crate::app) fn language_server_saved(&mut self, id: DocumentId) {
        self.sync_lsp(Instant::now(), Some(id));
        let servers = &self.language_servers;
        if let Some(tracked) = servers.documents.get(&id) {
            let text = self
                .editor
                .document(id)
                .filter(|d| !d.is_dirty())
                .and_then(|d| d.text().rope())
                .cloned();
            servers.send(
                &tracked.project,
                Input::Save {
                    uri: tracked.uri.clone(),
                    text,
                },
            );
        }
    }

    /// 将文件树收到的 Teal 输入变化交给已运行的语言服务。
    pub(in crate::app) fn language_server_disk_changes(&mut self, paths: &[PathBuf], rescan: bool) {
        if rescan
            || paths.iter().any(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| matches!(name, "Cargo.toml" | "rust-project.json"))
            })
        {
            self.language_servers.rust_projects = Default::default();
        }
        let servers = &self.language_servers;
        for project in servers
            .sessions
            .keys()
            .filter(|project| project.server == Server::Teal)
        {
            let changes: Vec<_> = paths
                .iter()
                .filter(|path| {
                    path.starts_with(&project.root)
                        || project
                            .extra_folders
                            .iter()
                            .any(|root| path.starts_with(root))
                })
                .filter(|path| {
                    path.extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| matches!(ext, "tl" | "tli" | "tlm"))
                        || path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| {
                                matches!(name, "package.toml" | "package.lock" | "catalog.toml")
                            })
                })
                .cloned()
                .collect();
            if rescan || !changes.is_empty() {
                servers.send(project, Input::Watched(changes));
            }
        }
    }

    fn query_lsp(&mut self, kind: Query) -> bool {
        self.query_lsp_until(kind, Instant::now() + QUERY_TIMEOUT)
    }

    fn query_route_ready(&self, origin: &Origin) -> bool {
        let servers = &self.language_servers;
        let Some(tracked) = servers.documents.get(&origin.doc) else {
            return false;
        };
        !tracked.project.is_standalone()
            || !self.editor.documents.iter().any(|doc| {
                let Some((path, Server::Rust, _)) = candidate(doc) else {
                    return false;
                };
                !servers.documents.contains_key(&doc.id)
                    && projects::standalone_root(path, &servers.folders) == tracked.project.root
            })
    }

    fn query_lsp_until(&mut self, kind: Query, until: Instant) -> bool {
        let Some(doc) = self.editor.active() else {
            return false;
        };
        let Some((_, server, _)) = candidate(doc) else {
            return false;
        };
        if self.link.proxy.is_none()
            || (kind == Query::Completion && (doc.readonly_reason().is_some() || doc.has_cursors()))
        {
            return false;
        }
        let Some(origin) = Origin::new(doc) else {
            return false;
        };
        let now = Instant::now();
        let position = Position::at(doc.text(), doc.selection.head);
        self.sync_lsp(now, Some(origin.doc));
        let can_route = self.query_route_ready(&origin);
        let servers = &mut self.language_servers;
        servers.cancel();
        if !can_route {
            if server == Server::Rust && file_uri(&origin.path).is_some() {
                servers.deferred = Some(Deferred {
                    kind,
                    origin,
                    until,
                });
                self.hint_at_name(t!("editor-lsp-loading"));
                return true;
            }
            return false;
        }
        let tracked = &servers.documents[&origin.doc];
        let (project, uri) = (tracked.project.clone(), tracked.uri.clone());
        let id = servers.next_id();
        if !servers.send(
            &project,
            Input::Query {
                id,
                kind,
                uri,
                position,
                until,
            },
        ) {
            let error = servers
                .sessions
                .get(&project)
                .and_then(|session| session.connection.as_ref().err())
                .cloned()
                .unwrap_or_else(|| project.server.program().into());
            if matches!(kind, Query::Definition | Query::References) {
                return false;
            }
            self.lsp_failed(kind, &error);
            return true;
        }
        self.jumps.definitions = None;
        let loading = !servers
            .sessions
            .get(&project)
            .is_some_and(|session| session.ready);
        self.language_servers.pending = Some(Pending {
            id,
            operation: Operation::Query(kind),
            origin,
            project,
            until,
        });
        if loading {
            self.hint_at_name(t!("editor-lsp-loading"));
        }
        true
    }

    pub(super) fn lsp_definition(&mut self) -> bool {
        self.query_lsp(Query::Definition)
    }

    pub(super) fn lsp_references(&mut self) -> bool {
        self.query_lsp(Query::References)
    }

    /// 请求光标处的类型和文档。
    pub(in crate::app) fn lsp_hover(&mut self) {
        if !self.query_lsp(Query::Hover) {
            self.hint_at_name(t!("editor-lsp-unavailable"));
        }
    }

    /// 请求光标处的补全项。
    pub(in crate::app) fn lsp_complete(&mut self) {
        if !self.query_lsp(Query::Completion) {
            self.hint_at_name(t!("editor-lsp-unavailable"));
        }
    }

    /// 当前标签是否可交给语言服务器。
    pub(in crate::app) fn lsp_action_available(&self, completion: bool) -> bool {
        self.editor.active().is_some_and(|doc| {
            candidate(doc).is_some()
                && (!completion || (doc.readonly_reason().is_none() && !doc.has_cursors()))
        })
    }

    /// 重新查找可执行文件并重连当前窗口的服务器。
    pub(in crate::app) fn restart_language_servers(&mut self) {
        self.language_servers.documents.clear();
        self.language_servers.sessions.clear();
        self.language_servers.pending = None;
        self.language_servers.deferred = None;
        self.language_servers.rust_projects = Default::default();
        self.language_servers.completion = None;
        self.sync_lsp(Instant::now(), None);
        self.request_redraw();
    }

    /// 采用仍对应当前项目和文档版本的结果。
    pub(in crate::app) fn poll_language_servers(&mut self) {
        while let Ok((project, generation, output)) = self.language_servers.messages.1.try_recv() {
            let Some(session) = self
                .language_servers
                .sessions
                .get_mut(&project)
                .filter(|session| session.generation == generation)
            else {
                continue;
            };
            match output {
                Output::Stopped(error) => {
                    session.connection = Err(error.clone());
                    for doc in self
                        .language_servers
                        .documents
                        .values_mut()
                        .filter(|doc| doc.project == project)
                    {
                        doc.diagnostics = None;
                    }
                    self.request_redraw();
                    if let Some(pending) = self
                        .language_servers
                        .pending
                        .take_if(|p| p.project == project)
                        && self.lsp_origin_current(&pending.origin)
                    {
                        self.lsp_request_failed(pending.operation, &error);
                    }
                }
                Output::Ready {
                    versioned_diagnostics,
                } => {
                    session.ready = true;
                    session.versioned_diagnostics = versioned_diagnostics;
                }
                Output::Loading => session.ready = false,
                Output::Diagnostics(params) => {
                    let versioned = session.versioned_diagnostics;
                    self.lsp_diagnostics(&project, versioned, &params);
                }
                Output::Reply { id, result } => {
                    if let Some(pending) = self
                        .language_servers
                        .pending
                        .take_if(|p| p.project == project && p.id == id)
                        && self.lsp_origin_current(&pending.origin)
                        && self
                            .language_servers
                            .documents
                            .get(&pending.origin.doc)
                            .is_some_and(|doc| doc.project == project)
                    {
                        match (pending.operation, result) {
                            (Operation::Query(kind), Ok(value)) => {
                                self.dismiss_hint();
                                self.lsp_result(kind, pending.origin, &value);
                            }
                            (Operation::Resolve(mut original), Ok(value)) => {
                                if let Some(edits) = value.get("additionalTextEdits") {
                                    original["additionalTextEdits"] = edits.clone();
                                }
                                let needs_import = project.server == Server::Rust
                                    && original["data"]["imports"]
                                        .as_array()
                                        .is_some_and(|imports| !imports.is_empty());
                                if value.is_object()
                                    && (!needs_import
                                        || original["additionalTextEdits"]
                                            .as_array()
                                            .is_some_and(|edits| !edits.is_empty()))
                                {
                                    self.apply_lsp_completion(&original);
                                } else {
                                    self.hint_at_name(t!("editor-lsp-invalid-completion"));
                                }
                            }
                            (operation, Err(error)) => self.lsp_request_failed(operation, &error),
                        }
                    }
                }
            }
        }
    }

    fn lsp_origin_current(&self, origin: &Origin) -> bool {
        self.focus == Focus::Editor
            && !self.overlays.picker.open
            && self.editor.active().is_some_and(|doc| origin.matches(doc))
    }

    fn lsp_failed(&mut self, kind: Query, error: &str) {
        match kind {
            Query::Definition => self.go_to_text_definition(),
            Query::References => self.find_text_references(),
            _ => self.hint_at_name(t!("editor-lsp-error", error = results::one_line(error))),
        }
    }

    fn lsp_request_failed(&mut self, operation: Operation, error: &str) {
        match operation {
            Operation::Query(kind) => self.lsp_failed(kind, error),
            Operation::Resolve(_) => {
                self.hint_at_name(t!("editor-lsp-error", error = results::one_line(error)))
            }
        }
    }

    fn lsp_diagnostics(&mut self, project: &Project, versioned: bool, params: &Value) {
        let Some(uri) = params["uri"].as_str() else {
            return;
        };
        let normalized = file_path(uri).and_then(|path| file_uri(&path));
        let version = params["version"].as_u64();
        let Some((id, tracked)) = self.language_servers.documents.iter_mut().find(|(_, doc)| {
            doc.project == *project && (doc.uri == uri || normalized.as_ref() == Some(&doc.uri))
        }) else {
            return;
        };
        if (versioned && version.is_none())
            || version.is_some_and(|version| version != tracked.version)
            || !self.editor.document(*id).is_some_and(|doc| {
                Some(doc.revision) == tracked.revision && doc.path.as_ref() == Some(&tracked.path)
            })
        {
            return;
        }
        tracked.diagnostics = Some(results::diagnostics(&params["diagnostics"]));
        self.request_redraw();
    }
}
