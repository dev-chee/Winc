//! 一个项目的语言服务器连接。进程、管道和 JSON 编解码均在后台线程。

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufReader, BufWriter};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use ropey::Rope;
use serde_json::{Value, json};
use winc_editor::shell::Shell;

use super::protocol::{self, Position, file_uri, notification, request};
use super::watch::Watches;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Server {
    Rust,
    Teal,
}

impl Server {
    pub fn program(self) -> &'static str {
        match self {
            Self::Rust => "rust-analyzer",
            Self::Teal => "teal",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Project {
    pub server: Server,
    pub root: PathBuf,
    pub rust_entry: Option<PathBuf>,
    pub extra_folders: Vec<PathBuf>,
}

impl Project {
    pub fn rust(root: PathBuf, entry: PathBuf) -> Self {
        Self {
            server: Server::Rust,
            root,
            rust_entry: Some(entry),
            extra_folders: Vec::new(),
        }
    }

    pub fn standalone(root: PathBuf) -> Self {
        Self {
            server: Server::Rust,
            root,
            rust_entry: None,
            extra_folders: Vec::new(),
        }
    }

    pub fn is_standalone(&self) -> bool {
        self.server == Server::Rust && self.rust_entry.is_none()
    }

    fn options(&self, standalone_entries: &[PathBuf]) -> Value {
        if self.server == Server::Rust {
            let entries: Vec<_> = self
                .rust_entry
                .as_slice()
                .iter()
                .chain(standalone_entries)
                .filter_map(|entry| entry.to_str())
                .collect();
            json!({
                "cargo":{"buildScripts":{"enable":false}},
                "procMacro":{"enable":false},
                "checkOnSave":false,
                "linkedProjects":entries,
            })
        } else {
            json!({})
        }
    }

    fn roots(&self) -> impl Iterator<Item = &PathBuf> {
        std::iter::once(&self.root).chain(&self.extra_folders)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Query {
    Definition,
    References,
    Hover,
    Completion,
}

impl Query {
    fn method(self) -> &'static str {
        match self {
            Self::Definition => "textDocument/definition",
            Self::References => "textDocument/references",
            Self::Hover => "textDocument/hover",
            Self::Completion => "textDocument/completion",
        }
    }

    fn capability(self) -> &'static str {
        match self {
            Self::Definition => "definitionProvider",
            Self::References => "referencesProvider",
            Self::Hover => "hoverProvider",
            Self::Completion => "completionProvider",
        }
    }
}

pub(super) enum Input {
    Sync {
        uri: String,
        language: &'static str,
        version: u64,
        text: Rope,
        unchanged_lines: usize,
    },
    Save {
        uri: String,
        text: Option<Rope>,
    },
    Close(String),
    Watched(Vec<PathBuf>),
    LinkedProjects(Vec<PathBuf>),
    Query {
        id: u64,
        kind: Query,
        uri: String,
        position: Position,
        until: Instant,
    },
    Resolve {
        id: u64,
        item: Value,
        until: Instant,
    },
    Cancel(u64),
}

pub(super) enum Output {
    Reply {
        id: u64,
        result: Result<Value, String>,
    },
    Diagnostics(Value),
    Ready {
        versioned_diagnostics: bool,
    },
    Loading,
    Stopped(String),
}

enum Event {
    Input(Input),
    Message(Value),
    Files(notify::Event),
    Lost(String),
    Stop,
}

impl Input {
    fn query_deadline(&self) -> Option<(u64, Instant)> {
        match self {
            Self::Query { id, until, .. } | Self::Resolve { id, until, .. } => Some((*id, *until)),
            _ => None,
        }
    }
}

pub(super) struct Connection(Sender<Event>);

impl Connection {
    pub fn start(
        project: Project,
        standalone_entries: Vec<PathBuf>,
        shell: Shell,
        publish: impl Fn(Output) + Send + 'static,
    ) -> io::Result<Self> {
        let (send, receive) = mpsc::channel();
        let worker = send.clone();
        thread::Builder::new()
            .name(format!("lsp-{}", project.server.program()))
            .spawn(move || {
                if let Err(error) = run(
                    &project,
                    &standalone_entries,
                    &shell,
                    &receive,
                    &worker,
                    &publish,
                ) {
                    publish(Output::Stopped(error.to_string()));
                }
            })?;
        Ok(Self(send))
    }

    pub fn send(&self, input: Input) -> bool {
        self.0.send(Event::Input(input)).is_ok()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.0.send(Event::Stop);
    }
}

const INITIALIZE: u64 = 0;
const SHUTDOWN: u64 = 1;
const START_TIMEOUT: Duration = Duration::from_secs(15);
const ANALYSIS_TIMEOUT: Duration = Duration::from_secs(12);
const STOP_TIMEOUT: Duration = Duration::from_millis(750);
const PROCESS_POLL: Duration = Duration::from_secs(2);
const WATCH_DELAY: Duration = Duration::from_millis(100);

fn run(
    project: &Project,
    standalone_entries: &[PathBuf],
    shell: &Shell,
    receive: &Receiver<Event>,
    events: &Sender<Event>,
    publish: &impl Fn(Output),
) -> io::Result<()> {
    let root = file_uri(&project.root).ok_or(io::ErrorKind::InvalidInput)?;
    let mut command = server_command(project, shell)?;
    let mut child = command
        .current_dir(&project.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let result = (|| {
        let stdin = child.stdin.take().ok_or(io::ErrorKind::BrokenPipe)?;
        let stdout = child.stdout.take().ok_or(io::ErrorKind::BrokenPipe)?;
        let (send, outgoing) = mpsc::channel();
        let writer_events = events.clone();
        thread::Builder::new()
            .name("lsp-write".into())
            .spawn(move || {
                let mut stdin = BufWriter::new(stdin);
                for message in outgoing {
                    if let Err(error) = protocol::write(&mut stdin, &message) {
                        let _ = writer_events.send(Event::Lost(error.to_string()));
                        break;
                    }
                }
            })?;
        let reader_events = events.clone();
        thread::Builder::new()
            .name("lsp-read".into())
            .spawn(move || {
                let mut stdout = BufReader::new(stdout);
                let reason = loop {
                    match protocol::read(&mut stdout) {
                        Ok(Some(message)) => {
                            if reader_events.send(Event::Message(message)).is_err() {
                                return;
                            }
                        }
                        Ok(None) => break "end of LSP stream".to_owned(),
                        Err(error) => break error.to_string(),
                    }
                };
                let _ = reader_events.send(Event::Lost(reason));
            })?;

        let folders = Value::Array(
            project
                .roots()
                .filter_map(|path| {
                    let name = path
                        .file_name()
                        .unwrap_or(path.as_os_str())
                        .to_string_lossy();
                    Some(json!({"uri": file_uri(path)?, "name": name}))
                })
                .collect(),
        );
        let mut options = project.options(standalone_entries);
        let mut watches = if project.server == Server::Teal {
            let events = events.clone();
            Watches::new(project.roots().cloned().collect(), move |event| {
                let _ = events.send(Event::Files(event));
            })
            .ok()
        } else {
            None
        };
        let mut capabilities = json!({
            "general":{"positionEncodings":["utf-16"]},
            "workspace":{"configuration":true,"workspaceFolders":true},
            "textDocument":{
                "publishDiagnostics":{"versionSupport":true},
                "definition":{"linkSupport":true},
                "hover":{"contentFormat":["plaintext","markdown"]},
                "completion":{"completionItem":{
                    "snippetSupport":false,
                    "resolveSupport":{"properties":["additionalTextEdits","documentation"]}
                }}
            }
        });
        if project.server == Server::Rust {
            capabilities["experimental"] = json!({"serverStatusNotification":true});
            capabilities["window"] = json!({"workDoneProgress":true});
        }
        if watches.is_some() {
            capabilities["workspace"]["didChangeWatchedFiles"] =
                json!({"dynamicRegistration":true,"relativePatternSupport":true});
        }
        send_message(
            &send,
            request(
                INITIALIZE,
                "initialize",
                json!({
                    "processId": std::process::id(),
                    "clientInfo":{"name":"Winc"},
                    "rootUri": root,
                    "workspaceFolders": folders,
                    "initializationOptions": options,
                    "capabilities": capabilities
                }),
            ),
        )?;

        let start_until = Instant::now() + START_TIMEOUT;
        let mut stopping = None;
        let mut ready: Option<State> = None;
        let mut pending: VecDeque<Input> = VecDeque::new();
        let mut linked_projects: Option<Vec<PathBuf>> = None;
        let mut changes = HashMap::new();
        let mut watch_due = None;
        let mut watch_rescan = false;
        loop {
            if let Some(status) = child.try_wait()? {
                return if stopping.is_some() {
                    Ok(())
                } else {
                    Err(io::Error::other(format!("process exited ({status})")))
                };
            }
            let now = Instant::now();
            if stopping.is_some_and(|until| now >= until) {
                return Ok(());
            }
            if ready.is_none() && now >= start_until {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "LSP initialize timed out",
                ));
            }
            if stopping.is_none()
                && let Some(state) = ready.as_mut()
            {
                state.check_probe(now);
                if !state.loading()
                    && let Some(entries) = linked_projects.take()
                {
                    let next = project.options(&entries);
                    if next != options {
                        options = next;
                        state.retryable_queries.clear();
                        state.analysis = Some(Analysis::probe(now, true));
                        send_message(
                            &send,
                            notification(
                                "workspace/didChangeConfiguration",
                                json!({"settings":options}),
                            ),
                        )?;
                    }
                }
                state.drive(&send, now, publish)?;
            }
            if watch_due.is_some_and(|until| now >= until) {
                watch_due = None;
                let catch_up = watches.as_mut().is_some_and(Watches::refresh);
                if std::mem::take(&mut watch_rescan) || catch_up || !changes.is_empty() {
                    send_watched(&send, changes.drain())?;
                }
            }
            let wait_until = [
                stopping,
                watch_due,
                ready.as_ref().and_then(State::deadline),
                ready.is_none().then_some(start_until),
            ]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(now + PROCESS_POLL);

            let event =
                receive.recv_timeout((wait_until.saturating_duration_since(now)).min(PROCESS_POLL));
            let now = Instant::now();
            match event {
                Ok(Event::Stop) => {
                    if ready.is_none() {
                        return Ok(());
                    }
                    send_message(&send, request(SHUTDOWN, "shutdown", Value::Null))?;
                    stopping = Some(now + STOP_TIMEOUT);
                }
                Ok(Event::Lost(reason)) => {
                    if stopping.is_some() {
                        return Ok(());
                    }
                    let reason = child
                        .try_wait()?
                        .map_or(reason, |s| format!("process exited ({s})"));
                    return Err(io::Error::other(reason));
                }
                Ok(Event::Input(Input::Watched(paths)))
                    if stopping.is_none() && ready.is_some() =>
                {
                    watch_rescan |= paths.is_empty();
                    for path in paths {
                        let kind = if winc_backend::fs::exists(&path) {
                            2
                        } else {
                            3
                        };
                        if !watches
                            .as_ref()
                            .is_some_and(|watches| watches.covers(&path, kind))
                            && let Some(uri) = file_uri(&path)
                        {
                            changes.entry(uri).or_insert(kind);
                        }
                    }
                    if watch_rescan || !changes.is_empty() {
                        watch_due.get_or_insert(now + WATCH_DELAY);
                    }
                }
                Ok(Event::Input(Input::LinkedProjects(entries)))
                    if stopping.is_none() && project.is_standalone() =>
                {
                    linked_projects = Some(entries);
                }
                Ok(Event::Input(Input::Cancel(id))) if ready.is_none() => {
                    pending.retain(|input| {
                        input
                            .query_deadline()
                            .is_none_or(|(queued, _)| queued != id)
                    });
                }
                Ok(Event::Input(input)) if stopping.is_none() => match &mut ready {
                    Some(state) => apply(&send, state, input, publish)?,
                    None => pending.push_back(input),
                },
                Ok(Event::Files(event)) if stopping.is_none() => {
                    if let Some(watches) = &mut watches {
                        watch_rescan |= watches.changes(event, &mut changes);
                        if watch_rescan || !changes.is_empty() {
                            watch_due.get_or_insert(now + WATCH_DELAY);
                        }
                    }
                }
                Ok(Event::Message(message)) => {
                    let id = message.get("id");
                    if let Some(method) = message["method"].as_str() {
                        if let Some(id) = id {
                            if let Some(watches) = &mut watches
                                && matches!(
                                    method,
                                    "client/registerCapability" | "client/unregisterCapability"
                                )
                            {
                                let params = &message["params"];
                                let result = if method == "client/registerCapability" {
                                    watches.register(params)
                                } else {
                                    watches.unregister(params)
                                };
                                if result.is_ok() {
                                    watch_due.get_or_insert(now + WATCH_DELAY);
                                }
                                let reply = match result {
                                    Ok(()) => json!({"jsonrpc":"2.0","id":id,"result":null}),
                                    Err(error) => {
                                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":error}})
                                    }
                                };
                                send_message(&send, reply)?;
                            } else {
                                answer(&send, id, method, &message["params"], &folders, &options)?;
                                if method == "workspace/configuration"
                                    && let Some(state) = &mut ready
                                    && let Some(Analysis::Probing { configuring, .. }) =
                                        &mut state.analysis
                                {
                                    *configuring = false;
                                }
                            }
                        } else if method == "textDocument/publishDiagnostics" && stopping.is_none()
                        {
                            publish(Output::Diagnostics(message["params"].clone()));
                        } else if method == "experimental/serverStatus"
                            && project.server == Server::Rust
                            && stopping.is_none()
                            && let Some(state) = &mut ready
                        {
                            state.rust_status(&message["params"]);
                        } else if method == "$/progress"
                            && project.server == Server::Rust
                            && stopping.is_none()
                            && message["params"]["token"] == "rustAnalyzer/Roots Scanned"
                            && let Some(state) = &mut ready
                        {
                            state.rust_scan(&message["params"]["value"]);
                        }
                    } else if let Some(id) = id.and_then(Value::as_u64) {
                        match id {
                            INITIALIZE if ready.is_none() => {
                                let caps = &message["result"]["capabilities"];
                                if !caps.is_object() {
                                    return Err(io::Error::other(error_message(&message)));
                                }
                                if caps["positionEncoding"]
                                    .as_str()
                                    .is_some_and(|s| s != "utf-16")
                                {
                                    return Err(io::Error::other(
                                        "server requires an unsupported position encoding",
                                    ));
                                }
                                send_message(&send, notification("initialized", json!({})))?;
                                let mut state = State {
                                    caps: caps.clone(),
                                    texts: HashMap::new(),
                                    analysis: (project.server == Server::Rust)
                                        .then(|| Analysis::probe(now, false)),
                                    queries: VecDeque::new(),
                                    retryable_queries: HashMap::new(),
                                    announced_ready: false,
                                };
                                for input in pending.drain(..) {
                                    apply(&send, &mut state, input, publish)?;
                                }
                                ready = Some(state);
                            }
                            SHUTDOWN if stopping.is_some() => {
                                send_message(&send, notification("exit", Value::Null))?;
                            }
                            _ if stopping.is_none() => {
                                if let Some(state) = &mut ready
                                    && state.retry_content_modified(&send, id, now, &message)?
                                {
                                    continue;
                                }
                                let result = if message.get("error").is_some() {
                                    Err(error_message(&message))
                                } else {
                                    Ok(message["result"].clone())
                                };
                                publish(Output::Reply { id, result });
                            }
                            _ => {}
                        }
                    }
                }
                Ok(Event::Input(_) | Event::Files(_)) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn server_command(project: &Project, shell: &Shell) -> io::Result<Command> {
    let name = project.server.program();
    let program = shell.find(name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("{name} was not found on PATH"),
        )
    })?;
    let mut command = shell.command(&program);
    if project.server == Server::Teal {
        command.arg("lsp");
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    Ok(command)
}

fn send_message(out: &Sender<Value>, value: Value) -> io::Result<()> {
    out.send(value)
        .map_err(|_| io::ErrorKind::BrokenPipe.into())
}

fn error_message(message: &Value) -> String {
    message["error"]["message"]
        .as_str()
        .unwrap_or("invalid LSP response")
        .to_owned()
}

fn answer(
    out: &Sender<Value>,
    id: &Value,
    method: &str,
    params: &Value,
    folders: &Value,
    options: &Value,
) -> io::Result<()> {
    let result = match method {
        "workspace/workspaceFolders" => folders.clone(),
        "workspace/configuration" => {
            let items = params["items"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|item| {
                    let section = item["section"].as_str().unwrap_or_default();
                    match section.strip_prefix("rust-analyzer.") {
                        Some(path) => path
                            .split('.')
                            .fold(options, |value, key| &value[key])
                            .clone(),
                        None if section.is_empty() || section == "rust-analyzer" => options.clone(),
                        None => Value::Null,
                    }
                });
            Value::Array(items.collect())
        }
        "window/workDoneProgress/create" | "window/showMessageRequest" => Value::Null,
        "workspace/applyEdit" => json!({"applied":false}),
        _ => {
            return send_message(
                out,
                json!({
                    "jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Unsupported client request"}
                }),
            );
        }
    };
    send_message(out, json!({"jsonrpc":"2.0","id":id,"result":result}))
}

struct State {
    caps: Value,
    texts: HashMap<String, Rope>,
    analysis: Option<Analysis>,
    queries: VecDeque<WaitingQuery>,
    retryable_queries: HashMap<u64, (Instant, Value)>,
    announced_ready: bool,
}

struct WaitingQuery {
    id: u64,
    until: Instant,
    input: Input,
}

enum Analysis {
    Probing { until: Instant, configuring: bool },
    Loading,
    Ready,
    Failed(String),
}

impl Analysis {
    fn probe(now: Instant, configuring: bool) -> Self {
        Self::Probing {
            until: now + ANALYSIS_TIMEOUT,
            configuring,
        }
    }
}

impl State {
    fn retry_content_modified(
        &mut self,
        out: &Sender<Value>,
        id: u64,
        now: Instant,
        response: &Value,
    ) -> io::Result<bool> {
        const CONTENT_MODIFIED: i32 = -32801;
        // rust-analyzer 可能在根扫描完成后继续应用 VFS 更新，并取消刚发出的只读查询。
        if let Some((until, request)) = self.retryable_queries.remove(&id)
            && response["error"]["code"] == CONTENT_MODIFIED
            && now < until
            && matches!(self.analysis, Some(Analysis::Ready))
        {
            send_message(out, request)?;
            return Ok(true);
        }
        Ok(false)
    }

    fn loading(&self) -> bool {
        matches!(
            self.analysis,
            Some(Analysis::Probing { .. } | Analysis::Loading)
        )
    }

    fn deadline(&self) -> Option<Instant> {
        let probe = match self.analysis {
            Some(Analysis::Probing { until, .. }) => Some(until),
            _ => None,
        };
        probe
            .into_iter()
            .chain(self.queries.iter().map(|query| query.until))
            .min()
    }

    fn check_probe(&mut self, now: Instant) {
        if matches!(self.analysis, Some(Analysis::Probing { until, .. }) if now >= until) {
            // 部分版本不报告加载进度，等首次探测期限后直接交给服务端处理。
            self.analysis = Some(Analysis::Ready);
        }
    }

    fn drive(
        &mut self,
        out: &Sender<Value>,
        now: Instant,
        publish: &impl Fn(Output),
    ) -> io::Result<()> {
        let ready = matches!(self.analysis, None | Some(Analysis::Ready));
        if ready != self.announced_ready {
            self.announced_ready = ready;
            if ready {
                publish(Output::Ready {
                    versioned_diagnostics: self.caps["experimental"]["tealVersionedOpenDiagnostics"]
                        == true,
                });
            } else {
                publish(Output::Loading);
            }
        }
        self.queries.retain(|query| {
            if now >= query.until {
                fail_query(query.id, "Rust workspace is still loading", publish);
                false
            } else {
                true
            }
        });
        if !self.loading() {
            while let Some(query) = self.queries.pop_front() {
                apply(out, self, query.input, publish)?;
            }
        }
        Ok(())
    }

    fn rust_status(&mut self, params: &Value) {
        if matches!(
            self.analysis,
            Some(Analysis::Probing {
                configuring: true,
                ..
            })
        ) {
            // 配置响应前的状态可能属于刚结束的上一轮加载。
            return;
        }
        if params["quiescent"] == true {
            self.analysis = Some(if params["health"] == "error" {
                Analysis::Failed(
                    params["message"]
                        .as_str()
                        .unwrap_or("Rust workspace could not be loaded")
                        .to_owned(),
                )
            } else {
                Analysis::Ready
            });
        } else if matches!(self.analysis, Some(Analysis::Probing { .. })) {
            self.analysis = Some(Analysis::Loading);
        }
    }

    fn rust_scan(&mut self, progress: &Value) {
        if matches!(
            self.analysis,
            Some(Analysis::Probing {
                configuring: true,
                ..
            })
        ) {
            return;
        }
        match progress["kind"].as_str() {
            Some("begin") => self.analysis = Some(Analysis::Loading),
            Some("report") if matches!(self.analysis, Some(Analysis::Probing { .. })) => {
                self.analysis = Some(Analysis::Loading);
            }
            Some("end") if self.loading() => self.analysis = Some(Analysis::Ready),
            _ => {}
        }
    }
}

fn fail_query(id: u64, problem: &str, publish: &impl Fn(Output)) {
    publish(Output::Reply {
        id,
        result: Err(problem.to_owned()),
    });
}

fn send_watched(
    out: &Sender<Value>,
    changes: impl IntoIterator<Item = (String, u8)>,
) -> io::Result<()> {
    let changes: Vec<_> = changes
        .into_iter()
        .map(|(uri, kind)| json!({"uri":uri,"type":kind}))
        .collect();
    send_message(
        out,
        notification(
            "workspace/didChangeWatchedFiles",
            json!({"changes":changes}),
        ),
    )
}

fn apply(
    out: &Sender<Value>,
    state: &mut State,
    input: Input,
    publish: &impl Fn(Output),
) -> io::Result<()> {
    if let Some((id, until)) = input.query_deadline() {
        let now = Instant::now();
        if now >= until {
            fail_query(id, "LSP request timed out", publish);
            return Ok(());
        }
        if state.loading() {
            state.queries.push_back(WaitingQuery {
                id,
                until: until.min(now + ANALYSIS_TIMEOUT),
                input,
            });
            return Ok(());
        }
        if let Some(Analysis::Failed(problem)) = &state.analysis {
            fail_query(id, problem, publish);
            return Ok(());
        }
    }
    let sync = &state.caps["textDocumentSync"];
    let change = sync
        .as_u64()
        .or_else(|| sync["change"].as_u64())
        .unwrap_or_default();
    let open_close = sync["openClose"].as_bool().unwrap_or(change != 0);
    let (method, params) = match input {
        Input::Sync {
            uri,
            language,
            version,
            text,
            unchanged_lines,
        } => {
            state.retryable_queries.clear();
            let previous = state.texts.insert(uri.clone(), text.clone());
            match previous {
                None if open_close => (
                    "textDocument/didOpen",
                    json!({
                        "textDocument":{"uri":uri,"languageId":language,"version":version,"text":text.to_string()}
                    }),
                ),
                Some(old) if change == 2 => (
                    "textDocument/didChange",
                    json!({
                        "textDocument":{"uri":uri,"version":version},
                        "contentChanges":[protocol::incremental_change(&old, &text, unchanged_lines)]
                    }),
                ),
                Some(_) if change == 1 => (
                    "textDocument/didChange",
                    json!({
                        "textDocument":{"uri":uri,"version":version},
                        "contentChanges":[{"text":text.to_string()}]
                    }),
                ),
                _ => return Ok(()),
            }
        }
        Input::Close(uri) => {
            if state.texts.remove(&uri).is_none() || !open_close {
                return Ok(());
            }
            state.retryable_queries.clear();
            ("textDocument/didClose", json!({"textDocument":{"uri":uri}}))
        }
        Input::Save { uri, text } => {
            let save = &sync["save"];
            if !state.texts.contains_key(&uri) || !(save == true || save.is_object()) {
                return Ok(());
            }
            let mut params = json!({"textDocument":{"uri":uri}});
            if save["includeText"] == true
                && let Some(text) = text
            {
                params["text"] = json!(text.to_string());
            }
            ("textDocument/didSave", params)
        }
        Input::Query {
            id,
            kind,
            uri,
            position,
            until,
        } => {
            let capability = &state.caps[kind.capability()];
            if capability != true && !capability.is_object() {
                publish(Output::Reply {
                    id,
                    result: Err("method unavailable".into()),
                });
                return Ok(());
            }
            let mut params = json!({"textDocument":{"uri":uri},"position":position.json()});
            match kind {
                Query::References => params["context"] = json!({"includeDeclaration":false}),
                Query::Completion => params["context"] = json!({"triggerKind":1}),
                _ => {}
            }
            let message = request(id, kind.method(), params);
            if state.analysis.is_some() {
                state.retryable_queries.insert(id, (until, message.clone()));
            }
            return send_message(out, message);
        }
        Input::Resolve { id, item, .. } => {
            if state.caps["completionProvider"]["resolveProvider"] == true {
                return send_message(out, request(id, "completionItem/resolve", item));
            }
            publish(Output::Reply {
                id,
                result: Ok(item),
            });
            return Ok(());
        }
        Input::Cancel(id) => {
            state.retryable_queries.remove(&id);
            let before = state.queries.len();
            state.queries.retain(|query| query.id != id);
            if before != state.queries.len() {
                return Ok(());
            }
            ("$/cancelRequest", json!({"id": id}))
        }
        Input::LinkedProjects(_) => return Ok(()),
        Input::Watched(paths) => {
            let changes = paths.iter().filter_map(|path| {
                Some((
                    file_uri(path)?,
                    if winc_backend::fs::exists(path) { 2 } else { 3 },
                ))
            });
            return send_watched(out, changes);
        }
    };
    send_message(out, notification(method, params))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_scan_completion_recovers_from_a_query_timeout() {
        let now = Instant::now();
        let mut state = State {
            caps: json!({"hoverProvider":true}),
            texts: HashMap::new(),
            analysis: Some(Analysis::probe(now, false)),
            queries: VecDeque::new(),
            retryable_queries: HashMap::new(),
            announced_ready: false,
        };
        let (out, wire) = mpsc::channel();
        let (reply, results) = mpsc::channel();
        let publish = |message| reply.send(message).unwrap();
        let query = |id| Input::Query {
            id,
            kind: Query::Hover,
            uri: "file:///main.rs".into(),
            position: Position {
                line: 0,
                character: 0,
            },
            until: now + Duration::from_secs(60),
        };

        state.rust_status(&json!({"quiescent":false}));
        apply(&out, &mut state, query(2), &publish).unwrap();
        apply(&out, &mut state, query(3), &publish).unwrap();
        apply(&out, &mut state, Input::Cancel(3), &publish).unwrap();
        let expired = now + ANALYSIS_TIMEOUT + Duration::from_secs(1);
        state.check_probe(expired);
        state.drive(&out, expired, &publish).unwrap();
        assert!(matches!(
            results.try_recv(),
            Ok(Output::Reply {
                id: 2,
                result: Err(_)
            })
        ));
        assert!(wire.try_recv().is_err());
        assert!(results.try_recv().is_err());

        state.rust_scan(&json!({"kind":"end"}));
        state.drive(&out, expired, &publish).unwrap();
        assert!(matches!(results.try_recv(), Ok(Output::Ready { .. })));
        apply(&out, &mut state, query(4), &publish).unwrap();
        let sent = wire.try_recv().unwrap();
        assert_eq!(sent["id"], 4);
        assert!(wire.try_recv().is_err());

        let obsolete = json!({"error":{"code":-32801}});
        assert!(
            state
                .retry_content_modified(&out, 4, expired, &obsolete)
                .unwrap()
        );
        assert_eq!(wire.try_recv().unwrap(), sent);
        assert!(
            !state
                .retry_content_modified(&out, 4, expired, &obsolete)
                .unwrap()
        );
    }
}
