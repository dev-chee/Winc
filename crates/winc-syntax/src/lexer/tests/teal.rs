//! Teal 源文件和生成接口的词法高亮测试。

use super::*;
use TokenKind::{
    Builtin, Comment, Decorator, Escape, Function, Keyword, Number, Property, String, Type,
    Variable,
};

#[test]
fn declarations_paths_and_attributes() {
    let tokens = |file, line| lex(file, line, LineState::Normal).0;
    let source = "@ tools :: requires(\"gpu\") pub task r#type<T>(x: i32) -> T; // signature";
    let expected = [
        ("@ tools :: requires", Decorator),
        ("\"gpu\"", String),
        ("pub", Keyword),
        ("task", Keyword),
        ("r#type", Function),
        ("i32", Builtin),
        ("// signature", Comment),
    ];
    assert_eq!(
        tokens("main.tl", source),
        expected.map(|(text, kind)| (text.into(), kind))
    );
    assert_eq!(
        tokens(
            "main.tl",
            "use std::agent::{read as load}; let attrs = item.@web::Route.path;"
        ),
        [
            ("use", Keyword),
            ("std", Variable),
            ("agent", Variable),
            ("read", Variable),
            ("as", Keyword),
            ("load", Variable),
            ("let", Keyword),
            ("@web::Route", Decorator)
        ]
        .map(|(text, kind)| (text.into(), kind))
    );
    assert_eq!(
        tokens(
            "main.tl",
            "trait lower_case { type Item; } const VALUE = 10_000;"
        ),
        [
            ("trait", Keyword),
            ("lower_case", Type),
            ("type", Keyword),
            ("Item", Type),
            ("const", Keyword),
            ("VALUE", Variable),
            ("10_000", Number)
        ]
        .map(|(text, kind)| (text.into(), kind))
    );
    assert_eq!(
        tokens(
            "host.tli",
            "pub mod r#computer { pub task list_windows() -> Result<[r#Window], HostError>;"
        ),
        [
            ("pub", Keyword),
            ("mod", Keyword),
            ("r#computer", Variable),
            ("pub", Keyword),
            ("task", Keyword),
            ("list_windows", Function),
            ("Result", Builtin),
            ("r#Window", Type),
            ("HostError", Type)
        ]
        .map(|(text, kind)| (text.into(), kind))
    );
    assert_eq!(
        tokens("builtin.tli", "pub fn typeof(value: any) -> string;")[3],
        ("any".into(), Builtin)
    );
    assert!(
        tokens(
            "main.tl",
            "while import int float host catch none r#fn r#true _private r#_()"
        )
        .is_empty()
    );
}

#[test]
fn labels_characters_and_decimal_numbers() {
    let (tokens, _) = lex(
        "main.tl",
        r"'outer: loop { let chars = ['中', '😀', '\u{1F600}', '\'']; if true { break 'outer; } }",
        LineState::Normal,
    );
    assert_eq!(
        tokens
            .into_iter()
            .filter(|(_, kind)| matches!(kind, String | Variable))
            .collect::<Vec<_>>(),
        [
            ("'outer", Variable),
            ("'中'", String),
            ("'😀'", String),
            (r"'\u{1F600}'", String),
            (r"'\''", String),
            ("'outer", Variable)
        ]
        .map(|(text, kind)| (text.into(), kind))
    );

    let (tokens, _) = lex(
        "main.tl",
        "1..10; .5; 1.25; 1.; 10_000; 10__000; 10_; 1_000.5; .0_5",
        LineState::Normal,
    );
    assert_eq!(
        tokens,
        ["1", "10", ".5", "1.25", "1", "10_000"].map(|text| (text.into(), Number))
    );
}

#[test]
fn strings_preserve_line_state_and_only_highlight_path_interpolation() {
    let (_, next) = lex(
        "main.tl",
        "let text = \"第一行 // 字符串",
        LineState::Normal,
    );
    assert_eq!(next, LineState::DoubleQuote);
    let (tokens, next) = lex(
        "main.tl",
        r#"{{ok}} { pkg::r#type.field.0 } \u{1F600}\q {f()} ${old} {unfinished"; // 注释"#,
        next,
    );
    assert_eq!(next, LineState::Normal);
    assert_eq!(tokens.last(), Some(&("// 注释".into(), Comment)));
    assert_eq!(
        tokens
            .into_iter()
            .filter(|(_, kind)| matches!(kind, Escape | Variable | Property))
            .collect::<Vec<_>>(),
        [
            ("{{", Escape),
            ("}}", Escape),
            ("pkg", Variable),
            ("r#type", Variable),
            ("field", Property),
            ("0", Property),
            (r"\u{1F600}", Escape)
        ]
        .map(|(text, kind)| (text.into(), kind))
    );
    let (tokens, next) = lex(
        "main.tl",
        r#""\u{broken"; let value = 1; /* let plain = 2;"#,
        LineState::Normal,
    );
    assert_eq!(next, LineState::Normal);
    assert_eq!(
        tokens.iter().filter(|(_, kind)| *kind == Keyword).count(),
        2
    );
    assert!(tokens.iter().all(|(_, kind)| *kind != Comment));
}

#[test]
fn markdown_fences_share_teal_highlighting_and_string_state() {
    for info in ["teal", "tl", "teal-interface", "tli"] {
        let (_, next) = lex("guide.md", &format!("```{info}"), LineState::Normal);
        let (tokens, next) = lex("guide.md", "let text = \"first", next);
        assert_eq!(tokens[0], ("let".into(), Keyword), "{info}");
        let (tokens, next) = lex("guide.md", "// string\"; return 1;", next);
        assert_eq!(tokens[0], ("// string\"".into(), String), "{info}");
        assert_eq!(tokens[1], ("return".into(), Keyword), "{info}");
        assert_eq!(lex("guide.md", "```", next).1, LineState::Normal);
    }
}
