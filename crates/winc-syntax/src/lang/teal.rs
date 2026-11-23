//! Teal 源码和生成接口的语言规则。

use super::{BRACKETS, CODE, Language, concat_keywords};

/// 关键字对应 `teal-syntax` 的 `syntax/lexer.rs`，类型取自语言手册和标准包接口。
pub(super) static TEAL: Language = Language {
    id: "teal",
    name: "Teal",
    extensions: &["tl"],
    keywords: &[
        "agent", "as", "await", "break", "class", "const", "continue", "dylib", "else", "enum",
        "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "pub", "return", "static",
        "struct", "task", "trait", "type", "use",
    ],
    builtins: &[
        "true",
        "false",
        "_",
        "i8",
        "u8",
        "i16",
        "u16",
        "i32",
        "u32",
        "i64",
        "u64",
        "f32",
        "f64",
        "bool",
        "char",
        "string",
        "buffer",
        "vec",
        "mat",
        "Option",
        "Result",
        "Task",
        "RuntimeError",
        "Error",
    ],
    line_comments: &["//"],
    multiline_strings: true,
    capitalized_types: true,
    indent_after: BRACKETS,
    ..CODE
};

/// `builtin.tli` 生成签名中的内部类型也着色。
static INTERFACE_BUILTINS: [&str; TEAL.builtins.len() + 1] =
    concat_keywords(TEAL.builtins, &["any"]);

/// Teal 的生成接口，使用与 VS Code 扩展相同的语言标识。
pub(super) static TEAL_INTERFACE: Language = Language {
    id: "teal-interface",
    name: "Teal Interface",
    extensions: &["tli"],
    builtins: &INTERFACE_BUILTINS,
    ..TEAL
};
