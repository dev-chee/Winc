use std::io::{BufReader, Cursor};
use std::time::SystemTime;

use ropey::Rope;
use serde_json::json;
use winc_editor::shell::Shell;

use super::*;

#[test]
fn frames_use_utf8_byte_lengths_and_keep_message_boundaries() {
    let first = json!({"result":"中😀"});
    let second = json!({"id":2,"result":null});
    let mut wire = Vec::new();
    protocol::write(&mut wire, &first).unwrap();
    protocol::write(&mut wire, &second).unwrap();
    let mut reader = BufReader::with_capacity(4, Cursor::new(wire));
    assert_eq!(protocol::read(&mut reader).unwrap(), Some(first));
    assert_eq!(protocol::read(&mut reader).unwrap(), Some(second));
    assert_eq!(protocol::read(&mut reader).unwrap(), None);
    let truncated = b"Content-Length: 8\r\n\r\n{}";
    assert!(protocol::read(&mut &truncated[..]).is_err());
}

#[test]
fn positions_count_utf16_without_counting_crlf_in_the_column() {
    let text = Rope::from_str("中😀f\r\nlast");
    let point = Position {
        line: 0,
        character: 3,
    };
    assert_eq!(Position::at(&text, 2), point);
    assert_eq!(point.offset(&text), Some(2));
    assert_eq!(
        Position {
            character: 2,
            ..point
        }
        .offset(&text),
        None
    );
    assert_eq!(
        Position {
            character: 20,
            ..point
        }
        .offset(&text),
        Some(3)
    );
    assert_eq!(
        Position::at(&text, 5),
        Position {
            line: 1,
            character: 0
        }
    );
    assert_eq!(
        Position::at(&text, text.len_chars()),
        Position {
            line: 1,
            character: 4
        }
    );
}

#[test]
fn incremental_edits_preserve_crlf_and_astral_characters() {
    for (old, new) in [
        ("前😀test\r\nend", "前😀next\r\nend"),
        ("a\r\nb", "a\nb"),
        ("a\r\nb", "a\rX\nb"),
        ("a\nb", "a\r\nb"),
        ("same\r\n", "same\r\n"),
        ("abc", ""),
    ] {
        let mut text = Rope::from_str(old);
        let delta = protocol::incremental_change(&text, &Rope::from_str(new), 0);
        let range = LocationRange::parse(&delta["range"]).unwrap();
        let start = range.start.offset(&text).unwrap();
        let end = range.end.offset(&text).unwrap();
        text.remove(start..end);
        text.insert(start, delta["text"].as_str().unwrap());
        assert_eq!(text, new, "{old:?} -> {new:?}: {delta}");
    }
    let old = Rope::from_str("前😀\r\nline two\r\nold suffix");
    let new = Rope::from_str("前😀\r\nline two\r\nnew suffix");
    assert_eq!(
        protocol::incremental_change(&old, &new, 2),
        protocol::incremental_change(&old, &new, 0)
    );
}

#[test]
fn incremental_edits_span_rope_chunks_with_different_layouts() {
    let original = Rope::from_str(&"α😀abc\r\n这是一行\n".repeat(2000));
    for start in [0, original.len_chars() / 2 + 3, original.len_chars() - 9] {
        let mut updated = original.clone();
        updated.remove(start..start + 7);
        updated.insert(start, "🤀\r\nβ");
        for updated in [updated.clone(), Rope::from_str(&updated.to_string())] {
            let unchanged = original.char_to_line(start).saturating_sub(1);
            let edit = protocol::incremental_change(&original, &updated, unchanged);
            assert_eq!(edit, protocol::incremental_change(&original, &updated, 0));
            let range = LocationRange::parse(&edit["range"]).unwrap();
            let mut actual = original.clone();
            let start = range.start.offset(&actual).unwrap();
            let end = range.end.offset(&actual).unwrap();
            actual.remove(start..end);
            actual.insert(start, edit["text"].as_str().unwrap());
            assert_eq!(actual, updated);
        }
    }
}

#[test]
fn file_uris_round_trip_unicode_and_reserved_characters() {
    let file = std::env::temp_dir().join("some # % 中 😀.tl");
    let uri = file_uri(&file).unwrap();
    assert!(uri.contains("some%20%23%20%25%20%E4%B8%AD%20%F0%9F%98%80.tl"));
    assert_eq!(file_path(&uri).as_deref().and_then(file_uri), Some(uri));
    assert!(file_path("https://localhost/file.tl").is_none());
    assert!(file_path("file:///bad%ZZ").is_none());
}

#[test]
fn stale_diagnostic_versions_do_not_replace_current_diagnostics() {
    let path = std::env::temp_dir().join("lsp-diagnostic.tl");
    let uri = file_uri(&path).unwrap();
    let mut editor = Editor::default();
    let mut doc = Document::from_text(DocumentId(1), "fn main() {}");
    doc.path = Some(path.clone());
    let id = doc.id;
    editor.documents.push(doc);
    let mut app = App::new(editor, None, Bench::from_env(), Config::default());
    let project = projects::teal_project(&[], &path);
    app.language_servers.documents.insert(
        id,
        Tracked {
            path,
            uri: uri.clone(),
            project: project.clone(),
            revision: Some(0),
            version: 5,
            due: None,
            diagnostics: None,
            seen: 0,
        },
    );
    let diagnostic = json!({
        "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":2}},
        "message":"test error", "severity":1
    });
    app.lsp_diagnostics(
        &project,
        true,
        &json!({"uri":uri,"version":5,"diagnostics":[diagnostic]}),
    );
    app.lsp_diagnostics(
        &project,
        true,
        &json!({"uri":uri,"version":4,"diagnostics":[]}),
    );
    app.lsp_diagnostics(&project, true, &json!({"uri":uri,"diagnostics":[]}));
    assert_eq!(
        app.language_servers
            .diagnostic_severity(app.editor.document(id).unwrap(), 0),
        Some(1)
    );
    app.lsp_diagnostics(&project, false, &json!({"uri":uri,"diagnostics":[]}));
    assert_eq!(
        app.language_servers
            .diagnostic_severity(app.editor.document(id).unwrap(), 0),
        None
    );
    app.lsp_diagnostics(
        &project,
        true,
        &json!({"uri":uri,"version":5,"diagnostics":[diagnostic]}),
    );
    let doc = app.editor.document_mut(id).unwrap();
    assert_eq!(app.language_servers.diagnostic_severity(doc, 0), Some(1));
    doc.revision += 1;
    assert_eq!(app.language_servers.diagnostic_severity(doc, 0), None);
}

#[test]
fn selecting_a_completion_applies_all_edits_and_undoes_them_together() {
    let text = "// 😀\nfn main() { va }";
    let mut editor = Editor::default();
    editor.new_untitled();
    let doc = editor.active_mut().unwrap();
    *doc = Document::from_text(doc.id, text);
    doc.path = Some(std::env::temp_dir().join("completion.rs"));
    doc.selection = Selection::point(doc.text().line_start(1) + 14);
    let origin = Origin::new(doc).unwrap();
    let mut app = App::new(editor, None, Bench::from_env(), Config::default());
    app.focus = Focus::Editor;
    app.lsp_result(Query::Completion, origin, &json!({"items":[{
        "label":"value",
        "textEdit":{"range":{"start":{"line":1,"character":12},"end":{"line":1,"character":14}},"newText":"value"},
        "additionalTextEdits":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}},"newText":"use crate::value;\n"}]
    }]}));
    assert_eq!(app.focus, Focus::Picker);
    app.places_accept();
    assert_eq!(app.focus, Focus::Editor);
    let doc = app.editor.active().unwrap();
    assert_eq!(
        doc.text().rope().unwrap(),
        &Rope::from_str("use crate::value;\n// 😀\nfn main() { value }")
    );
    assert_eq!(doc.selection.head, doc.text().line_start(2) + 17);
    app.run(Command::Undo);
    assert_eq!(
        app.editor.active().unwrap().text().rope().unwrap(),
        &Rope::from_str(text)
    );
}

// With both executables on PATH, run with: cargo test -p winc-app real_language_servers -- --ignored
#[test]
#[ignore = "requires rust-analyzer and teal on PATH"]
fn real_language_servers() {
    for server in [Server::Rust, Server::Teal] {
        exercise_server(server);
    }
}

fn exercise_server(server: Server) {
    let root = scratch(server.program());
    let (path, language, source, changed, incomplete, diagnostic) = match server {
        Server::Rust => {
            std::fs::create_dir(root.join("src")).unwrap();
            std::fs::write(
                root.join("Cargo.toml"),
                "[package]\nname = \"winc_lsp_smoke\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            )
            .unwrap();
            (
                root.join("src/main.rs"),
                "rust",
                "fn value() -> i32 { 1 }\r\nfn main() { let _ = \"中😀\"; let _ = value(); }\r\n",
                "fn value() -> bool { true }\r\nfn main() { let _ = \"中😀\"; let _ = value(); }\r\n",
                "fn value() -> bool { true }\r\nfn main() { let _ = \"中😀\"; let _ = valu; }\r\n",
                "fn value() -> bool { true }\r\nfn main() { let _ = \"中😀\"; let _ = value(,); }\r\n",
            )
        }
        Server::Teal => (
            root.join("main.tl"),
            "teal",
            "fn value() -> i32 { return 1; }\r\nfn main() -> i32 { let marker = \"中😀\"; let x = value(); return 0; }\r\n",
            "fn value() -> bool { return true; }\r\nfn main() -> i32 { let marker = \"中😀\"; let x = value(); return 0; }\r\n",
            "use std::num::UInt;\r\nfn unsigned<T: UInt>(value: T) -> u32 { _=value; return value. }\r\nfn main() -> i32 { return 0; }",
            "fn value() -> bool { return true; }\r\nfn main() -> i32 { let marker = \"中😀\"; let x: i32 = value(); return 0; }\r\n",
        ),
    };
    std::fs::write(&path, source).unwrap();
    let uri = file_uri(&path).unwrap();
    let (send, receive) = mpsc::channel();
    let project = match server {
        Server::Rust => Project::rust(root.clone(), root.join("Cargo.toml")),
        Server::Teal => projects::teal_project(&[root], &path),
    };
    let connection = Connection::start(project, Vec::new(), Shell::default(), move |output| {
        let _ = send.send(output);
    })
    .unwrap();
    let versioned_diagnostics = server == Server::Teal
        && wait_for(&receive, |output| match output {
            Output::Ready {
                versioned_diagnostics,
            } => Some(versioned_diagnostics),
            _ => None,
        });
    let sync = |version, source: &str| {
        assert!(connection.send(Input::Sync {
            uri: uri.clone(),
            language,
            version,
            text: Rope::from_str(source),
            unchanged_lines: 0,
        }));
    };
    let query = |id, kind, source: &str, needle: &str, chars| {
        let byte = source.rfind(needle).unwrap();
        let offset = source[..byte].chars().count() + chars;
        let position = Position::at(&Rope::from_str(source), offset);
        assert!(connection.send(Input::Query {
            id,
            kind,
            uri: uri.clone(),
            position,
            until: Instant::now() + QUERY_TIMEOUT,
        }));
        wait_for(&receive, |output| match output {
            Output::Reply {
                id: received,
                result,
            } if id == received => Some(result.unwrap()),
            _ => None,
        })
    };
    sync(1, source);
    let id = 2;
    let hover = query(id, Query::Hover, source, "value()", 2);
    assert!(
        hover.to_string().contains("i32"),
        "{server:?} first query: {hover}"
    );
    let definition = query(id + 1, Query::Definition, source, "value()", 2);
    assert!(
        definition
            .to_string()
            .contains(path.file_name().unwrap().to_str().unwrap()),
        "{server:?}: {definition}"
    );
    let references = query(id + 2, Query::References, source, "value()", 2);
    assert!(
        references.as_array().is_some_and(|items| !items.is_empty()),
        "{server:?}: {references}"
    );
    sync(2, changed);
    assert!(
        query(id + 3, Query::Hover, changed, "value()", 2)
            .to_string()
            .contains("bool"),
        "{server:?}"
    );
    sync(3, incomplete);
    let (needle, offset, label) = match server {
        Server::Rust => ("valu;", 4, "value"),
        Server::Teal => ("value.", 6, "checked_rem"),
    };
    let completion = query(id + 4, Query::Completion, incomplete, needle, offset);
    assert!(
        completion.to_string().contains(label),
        "{server:?}: {completion}"
    );
    sync(4, diagnostic);
    wait_for(&receive, |output| match output {
        Output::Diagnostics(value)
            if value["version"]
                .as_u64()
                .map_or(!versioned_diagnostics, |version| version == 4)
                && value["diagnostics"]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|item| item["severity"] == 1)) =>
        {
            Some(())
        }
        _ => None,
    });
    assert_eq!(std::fs::read_to_string(path).unwrap(), source);
    assert!(connection.send(Input::Close(uri)));
    drop(connection);
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        match receive.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => panic!("{server:?} did not shut down"),
            Ok(Output::Stopped(error)) => panic!("{server:?} failed to shut down: {error}"),
            Ok(_) => {}
        }
    }
}

fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "winc-lsp-{name}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    std::fs::create_dir_all(&root).unwrap();
    winc_editor::canonical_path(&root)
}

fn fixture(path: PathBuf, text: &str) -> PathBuf {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, text).unwrap();
    path
}

fn rust_package(name: &str) -> String {
    format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n")
}

fn teal_package(name: &str) -> String {
    format!("format=11\nname=\"{name}\"\nversion=\"1.0.0\"\nlanguage_edition=\"2026\"\n")
}

fn query_input(
    id: u64,
    kind: Query,
    path: &Path,
    source: &str,
    needle: &str,
    offset: usize,
) -> Input {
    let byte = source.rfind(needle).unwrap();
    let position = Position::at(
        &Rope::from_str(source),
        source[..byte].chars().count() + offset,
    );
    Input::Query {
        id,
        kind,
        uri: file_uri(path).unwrap(),
        position,
        until: Instant::now() + QUERY_TIMEOUT,
    }
}

fn read_reply(receive: &Receiver<Output>, request: u64) -> Value {
    wait_for(receive, |output| match output {
        Output::Reply { id, result } if id == request => Some(result.unwrap()),
        _ => None,
    })
}

fn app_reply(app: &App, project: &Project, request: u64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (key, _, output) = app
            .language_servers
            .messages
            .1
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        assert_eq!(&key, project);
        match output {
            Output::Reply { id, result } if id == request => {
                return result.unwrap_or_else(|error| panic!("query {request}: {error}"));
            }
            Output::Stopped(error) => panic!("{error}"),
            _ => {}
        }
    }
}

fn wait_for_documents(app: &mut App, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        app.sync_lsp_with(Instant::now(), None, || {});
        if app.language_servers.documents.len() == count {
            break;
        }
        assert!(Instant::now() < deadline, "project discovery timed out");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn connect(project: Project) -> (Connection, Receiver<Output>) {
    connect_with_entries(project, Vec::new())
}

fn app_with_real_tools(editor: Editor) -> App {
    let mut app = App::new(editor, None, Bench::from_env(), Config::default());
    app.format.formatter = winc_editor::format::Formatter::with_vars(std::env::vars_os().collect());
    app
}

fn connect_with_entries(project: Project, entries: Vec<PathBuf>) -> (Connection, Receiver<Output>) {
    let (send, receive) = mpsc::channel();
    let connection = Connection::start(project, entries, Shell::default(), move |output| {
        let _ = send.send(output);
    })
    .unwrap();
    (connection, receive)
}

fn open_buffer(connection: &Connection, path: &Path, language: &'static str, source: &str) {
    assert!(connection.send(Input::Sync {
        uri: file_uri(path).unwrap(),
        language,
        version: 1,
        text: Rope::from_str(source),
        unchanged_lines: 0,
    }));
}

#[test]
#[ignore = "requires rust-analyzer, cargo and teal on PATH"]
fn project_queries_see_other_documents_before_the_edit_delay() {
    for server in [Server::Rust, Server::Teal] {
        let outer = scratch(server.program());
        let (editor_root, attached, from, dependency, source, original, changed) = match server {
            Server::Rust => {
                let workspace = outer.join("nested");
                fixture(
                    workspace.join("Cargo.toml"),
                    "[workspace]\nresolver=\"3\"\nmembers=[\"app\",\"dep\"]\n",
                );
                fixture(
                    workspace.join("app/Cargo.toml"),
                    &(rust_package("app") + "[dependencies]\ndep={path=\"../dep\"}\n"),
                );
                fixture(workspace.join("dep/Cargo.toml"), &rust_package("dep"));
                (
                    outer,
                    None,
                    workspace.join("app/src/main.rs"),
                    workspace.join("dep/src/lib.rs"),
                    "fn main() { let _ = dep::value(); }\n",
                    "pub fn value() -> i32 { 1 }\n",
                    "pub fn value() -> bool { true }\n",
                )
            }
            Server::Teal => {
                let app = outer.join("app");
                let dep = outer.join("dep");
                fixture(
                    app.join("package.toml"),
                    &(teal_package("app")
                        + "[dependencies]\ndep={path=\"../dep\",version=\"1.0.0\"}\n"),
                );
                fixture(dep.join("package.toml"), &teal_package("dep"));
                (
                    app.clone(),
                    Some(dep.clone()),
                    app.join("src/main.tl"),
                    dep.join("src/lib.tl"),
                    "use ::dep as lib;\nfn main() -> i32 { let result = lib::value(); return 0; }\n",
                    "pub fn value() -> i32 { return 1; }\n",
                    "pub fn value() -> bool { return true; }\n",
                )
            }
        };
        let from = fixture(from, source);
        let dependency = fixture(dependency, original);
        let mut editor = Editor::default();
        editor.open_folder(&editor_root).unwrap();
        if let Some(attached) = attached {
            editor
                .workspace
                .as_mut()
                .unwrap()
                .add_folder(&attached)
                .unwrap();
        }
        let dependency_index = editor.open(&dependency);
        let dependency_id = editor.documents[dependency_index].id;
        let from_index = editor.open(&from);
        let from_id = editor.documents[from_index].id;
        let mut app = app_with_real_tools(editor);
        wait_for_documents(&mut app, 2);
        let project = app.language_servers.documents[&from_id].project.clone();
        assert_eq!(
            app.language_servers.documents[&dependency_id].project,
            project
        );
        assert_eq!(app.language_servers.sessions.len(), 1);
        let query = |app: &App, request| {
            let servers = &app.language_servers;
            assert!(servers.send(
                &project,
                query_input(request, Query::Hover, &from, source, "value()", 2)
            ));
            app_reply(app, &project, request)
        };
        assert!(query(&app, 20).to_string().contains("i32"), "{server:?}");
        let doc = app.editor.document_mut(dependency_id).unwrap();
        doc.selection = Selection::new(0, doc.text().end_pos());
        doc.execute(Command::Paste(changed.to_owned()), 10, 4);
        let now = Instant::now();
        app.sync_lsp_with(now, None, || {});
        assert_eq!(
            app.language_servers.documents[&dependency_id].due,
            Some(now + EDIT_DELAY)
        );
        app.sync_lsp_with(now, Some(from_id), || {});
        assert!(query(&app, 21).to_string().contains("bool"), "{server:?}");
        assert_eq!(std::fs::read_to_string(dependency).unwrap(), original);
    }
}

#[test]
#[ignore = "requires rust-analyzer on PATH"]
fn rust_analyzes_standalone_files_and_resolves_auto_imports() {
    let root = scratch("rust-standalone");
    let standalone = "fn value() -> i32 { 1 }\nfn main() { let _ = value(); }\n";
    let file = fixture(root.join("standalone.rs"), standalone);
    let project = projects::rust_project(&file, &Shell::default(), &mut HashMap::new());
    assert!(project.is_standalone());
    let (client, results) = connect_with_entries(project, vec![file.clone()]);
    open_buffer(&client, &file, "rust", standalone);
    assert!(client.send(query_input(
        2,
        Query::Definition,
        &file,
        standalone,
        "value()",
        2
    )));
    assert!(!read_reply(&results, 2).as_array().unwrap().is_empty());
    drop(client);

    let root = scratch("rust-complete");
    fixture(root.join("Cargo.toml"), &rust_package("complete"));
    let source = "mod helper { pub struct UniqueProbeType; }\nfn main() { let _ = UniqueProbe; }\n";
    let file = fixture(root.join("src/main.rs"), source);
    let mut editor = Editor::default();
    editor.open_folder(&root).unwrap();
    editor.open(&file);
    let doc = editor.active_mut().unwrap();
    doc.selection = Selection::point(source.rfind("UniqueProbe;").unwrap() + "UniqueProbe".len());
    let origin = Origin::new(doc).unwrap();
    let mut app = app_with_real_tools(editor);
    app.focus = Focus::Editor;
    wait_for_documents(&mut app, 1);
    let project = app.language_servers.documents[&origin.doc].project.clone();
    assert!(app.language_servers.send(
        &project,
        query_input(
            3,
            Query::Completion,
            &file,
            source,
            "UniqueProbe;",
            "UniqueProbe".len()
        )
    ));
    let list = app_reply(&app, &project, 3);
    app.lsp_result(Query::Completion, origin, &list);
    let picker = &mut app.overlays.picker;
    let places = picker.places.as_ref().expect("completion picker");
    picker.selected = picker
        .hits
        .iter()
        .position(|&index| places.items[index].relative.contains("UniqueProbeType"))
        .expect("rust-analyzer did not return an auto-import completion");
    app.places_accept();
    assert!(app.language_servers.pending.is_some());
    let deadline = Instant::now() + Duration::from_secs(15);
    while app.language_servers.pending.is_some() {
        app.poll_language_servers();
        assert!(Instant::now() < deadline, "completion resolution timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
    let doc = app.editor.active().unwrap();
    let applied = doc.text().rope().unwrap().to_string();
    assert!(
        applied.contains("use crate::helper::UniqueProbeType;"),
        "{applied}"
    );
    assert!(applied.contains("let _ = UniqueProbeType;"), "{applied}");
    app.run(Command::Undo);
    assert_eq!(
        app.editor.active().unwrap().text().rope().unwrap(),
        &Rope::from_str(source)
    );
}

#[test]
#[ignore = "requires rust-analyzer on PATH"]
fn rust_standalone_modules_share_overlays_while_membership_changes() {
    let root = scratch("rust-standalone-modules");
    let source = "mod helper;\nfn main() { let _ = helper::value(); }\n";
    let main = fixture(root.join("main.rs"), source);
    let helper = fixture(root.join("helper/mod.rs"), "pub fn value() -> i32 { 1 }\n");
    let mut editor = Editor::default();
    editor.open_folder(&root).unwrap();
    editor.open(&main);
    let main_id = editor.active().unwrap().id;
    let mut app = app_with_real_tools(editor);
    wait_for_documents(&mut app, 1);
    let project = app.language_servers.documents[&main_id].project.clone();
    assert_eq!(project, Project::standalone(root));
    let generation = app.language_servers.sessions[&project].generation;
    let query = |app: &App, id| {
        assert!(app.language_servers.send(
            &project,
            query_input(id, Query::Hover, &main, source, "value()", 2)
        ));
        app_reply(app, &project, id).to_string()
    };
    assert!(query(&app, 21).contains("i32"));

    let index = app.editor.open(&helper);
    let doc = &mut app.editor.documents[index];
    let helper_id = doc.id;
    doc.selection = Selection::new(0, doc.text().end_pos());
    doc.execute(
        Command::Paste("pub fn value() -> bool { true }\n".into()),
        10,
        4,
    );
    wait_for_documents(&mut app, 2);
    assert_eq!(app.language_servers.documents[&helper_id].project, project);
    app.sync_lsp_with(Instant::now(), Some(main_id), || {});
    assert!(query(&app, 22).contains("bool"));

    app.editor.close(index).unwrap();
    app.sync_lsp_with(Instant::now(), Some(main_id), || {});
    assert!(query(&app, 23).contains("i32"));
    assert_eq!(
        app.language_servers.sessions[&project].generation,
        generation
    );
}

#[test]
#[ignore = "requires a Teal server with dynamic file watches on PATH"]
fn teal_watches_closed_path_dependencies_outside_the_root() {
    let outer = scratch("teal-watched");
    let app = outer.join("app");
    let dep = outer.join("dep");
    fixture(
        app.join("package.toml"),
        &(teal_package("app") + "[dependencies]\ndep={path=\"../dep\",version=\"1.0.0\"}\n"),
    );
    fixture(dep.join("package.toml"), &teal_package("dep"));
    let source =
        "use ::dep as lib;\nfn main() -> i32 { let result: i32 = lib::value(); return result; }\n";
    let file = fixture(app.join("src/main.tl"), source);
    let original = "pub fn value() -> i32 { return 1; }\n";
    let dependent = fixture(dep.join("src/lib.tl"), original);
    let (client, results) = connect(projects::teal_project(&[app], &file));
    open_buffer(&client, &file, "teal", source);
    assert!(client.send(query_input(2, Query::Hover, &file, source, "value()", 2)));
    assert!(read_reply(&results, 2).to_string().contains("i32"));
    std::fs::write(&dependent, "pub fn value() -> bool { return true; }\n").unwrap();
    wait_for(&results, |output| match output {
        Output::Diagnostics(value)
            if value["diagnostics"]
                .to_string()
                .contains("Expected i32, found bool") =>
        {
            Some(())
        }
        _ => None,
    });
    std::fs::write(&dependent, original).unwrap();
    wait_for(&results, |output| match output {
        Output::Diagnostics(value)
            if value["diagnostics"].as_array().is_some_and(Vec::is_empty) =>
        {
            Some(())
        }
        _ => None,
    });

    let replacement = outer.join("replacement");
    let incompatible = "pub fn value() -> bool { return true; }\n";
    fixture(replacement.join("lib.tl"), incompatible);
    std::fs::rename(dep.join("src"), dep.join("src-old")).unwrap();
    std::fs::rename(replacement, dep.join("src")).unwrap();
    wait_for(&results, |output| match output {
        Output::Diagnostics(value)
            if value["diagnostics"]
                .to_string()
                .contains("Expected i32, found bool") =>
        {
            Some(())
        }
        _ => None,
    });
    std::fs::write(&dependent, original).unwrap();
    wait_for(&results, |output| match output {
        Output::Diagnostics(value)
            if value["diagnostics"].as_array().is_some_and(Vec::is_empty) =>
        {
            Some(())
        }
        _ => None,
    });
}

fn wait_for<T>(receive: &Receiver<Output>, mut check: impl FnMut(Output) -> Option<T>) -> T {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let output = receive
            .recv_timeout(until.saturating_duration_since(Instant::now()))
            .expect("language server did not produce a result");
        if let Output::Stopped(error) = output {
            panic!("language server stopped: {error}");
        }
        if let Some(value) = check(output) {
            return value;
        }
    }
}
