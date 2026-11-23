//! Teal 专有的词法规则。注释、双引号定界符和跨行状态由通用词法器处理。

use super::{Lexer, TokenKind, ascii_word, scan};

/// 当前行中限定下一个名称类别的语法。
#[derive(Clone, Copy)]
pub(super) enum CodeContext {
    Plain,
    Declaration(TokenKind),
    Import,
}

impl Lexer<'_> {
    /// 着色一个代码 token，返回之后的字符偏移。
    pub(super) fn teal_token(&mut self, i: usize, context: CodeContext) -> usize {
        let chars = self.chars;
        let c = chars[i];
        if c.is_ascii_whitespace() {
            return i + 1;
        }
        if let Some((name, end)) = name_range(chars, i) {
            if self.tokens.is_some() {
                self.teal_word(i, name, end, context);
            }
            return end;
        }
        if matches!(context, CodeContext::Declaration(_)) || c == ';' {
            self.teal = Some(CodeContext::Plain);
        }
        match c {
            '@' if self.tokens.is_some() => {
                if let Some(end) = attribute_end(chars, i + 1, self.language.keywords) {
                    self.push(i, end, TokenKind::Decorator);
                    return end;
                }
            }
            '\'' => {
                if let Some(end) = character_end(chars, i) {
                    self.push(i, end, TokenKind::String);
                    return end;
                }
                if let Some(end) = bare_name_end(chars, i + 1) {
                    if self.tokens.is_some()
                        && !reserved(&chars[i + 1..end], self.language.keywords)
                    {
                        self.push(i, end, TokenKind::Variable);
                    }
                    return end;
                }
            }
            '.' if chars.get(i + 1) == Some(&'.') => return i + 2,
            '.' if chars.get(i + 1).is_some_and(char::is_ascii_digit) => {
                return self.teal_number(i);
            }
            '0'..='9' => return self.teal_number(i),
            _ => {}
        }
        i + 1
    }

    /// 用行内声明、路径和调用位置区分名称。`name` 跳过 `r#`，着色包含完整拼写。
    fn teal_word(&mut self, start: usize, name: usize, end: usize, context: CodeContext) {
        let mut buf = [0; 24];
        let chars = self.chars;
        let spelling = ascii_word(&chars[name..end], &mut buf, false);
        if start != name && spelling == Some("_") {
            return;
        }
        let word = spelling.filter(|_| start == name);
        let mut next_context = CodeContext::Plain;
        let next = skip_space(chars, end);
        let kind = if matches!(context, CodeContext::Import) {
            next_context = CodeContext::Import;
            if word == Some("as") {
                TokenKind::Keyword
            } else {
                TokenKind::Variable
            }
        } else if let Some(keyword) = word.filter(|word| self.language.keywords.contains(word)) {
            next_context = match keyword {
                "use" => CodeContext::Import,
                "fn" | "task" => CodeContext::Declaration(TokenKind::Function),
                "class" | "struct" | "enum" | "trait" | "agent" | "type" => {
                    CodeContext::Declaration(TokenKind::Type)
                }
                "mod" | "const" => CodeContext::Declaration(TokenKind::Variable),
                _ => CodeContext::Plain,
            };
            TokenKind::Keyword
        } else if matches!(word, Some("true" | "false" | "_")) {
            TokenKind::Builtin
        } else if let CodeContext::Declaration(kind) = context {
            kind
        } else if word.is_some_and(|word| self.language.builtins.contains(&word)) {
            TokenKind::Builtin
        } else if word == Some("self") {
            TokenKind::Variable
        } else if self.language.capitalized_types
            && chars[name].is_ascii_uppercase()
            && chars[name + 1..end].iter().any(char::is_ascii_lowercase)
        {
            TokenKind::Type
        } else if chars.get(next) == Some(&'(')
            || chars.get(next..next + 3) == Some(&[':', ':', '<'])
        {
            TokenKind::Function
        } else if chars.get(next..next + 2) == Some(&[':', ':']) {
            TokenKind::Variable
        } else {
            TokenKind::Text
        };
        self.teal = Some(next_context);
        if kind != TokenKind::Text {
            self.push(start, end, kind);
        }
    }

    /// 扫描十进制整数或小数。下划线仅能分隔相邻的整数字位。
    fn teal_number(&mut self, start: usize) -> usize {
        let chars = self.chars;
        let digits = |from| scan(chars, from, |c| c.is_ascii_digit() || c == '_');
        let mut end = if chars[start] == '.' {
            start
        } else {
            digits(start + 1)
        };
        let decimal =
            chars.get(end) == Some(&'.') && chars.get(end + 1).is_some_and(char::is_ascii_digit);
        if decimal {
            end = digits(end + 2);
        }
        if self.tokens.is_some() {
            let number = &chars[start..end];
            let valid = if decimal {
                !number.contains(&'_')
            } else {
                number.last() != Some(&'_') && !number.windows(2).any(|pair| pair == ['_', '_'])
            };
            if valid {
                self.push(start, end, TokenKind::Number);
            }
        }
        end
    }

    /// 从 `from` 扫描字符串，返回收尾引号之后的位置。没有收尾引号时由调用者续到下一行。
    pub(super) fn teal_string_close(&mut self, mut start: usize, mut from: usize) -> Option<usize> {
        let chars = self.chars;
        while let Some(&c) = chars.get(from) {
            let escape_end = match c {
                '"' => {
                    self.push(start, from + 1, TokenKind::String);
                    return Some(from + 1);
                }
                '\\' => {
                    if let Some(end) = escape_end(chars, from, false) {
                        end
                    } else {
                        // 未知转义保留原样。未完成的 Unicode 转义不会开始插值或吞掉后面的引号。
                        let len = if chars.get(from + 1..from + 3) == Some(&['u', '{']) {
                            3
                        } else {
                            2
                        };
                        from = (from + len).min(chars.len());
                        continue;
                    }
                }
                '{' | '}' if chars.get(from + 1) == Some(&c) => from + 2,
                '$' if chars.get(from + 1) == Some(&'{') && chars.get(from + 2) != Some(&'{') => {
                    // 旧式 `${…}` 保留字符串颜色。
                    from += 2;
                    continue;
                }
                '{' if self.tokens.is_some() => {
                    let keywords = self.language.keywords;
                    if let Some(end) = interpolation_end(chars, from + 1, keywords, |_, _, _| {}) {
                        self.push(start, from + 1, TokenKind::String);
                        interpolation_end(chars, from + 1, keywords, |start, end, kind| {
                            self.push(start, end, kind);
                        });
                        start = end - 1;
                        from = end;
                    } else {
                        from += 1;
                    }
                    continue;
                }
                _ => {
                    from += 1;
                    continue;
                }
            };
            self.push(start, from, TokenKind::String);
            self.push(from, escape_end, TokenKind::Escape);
            from = escape_end;
            start = escape_end;
        }
        self.push(start, chars.len(), TokenKind::String);
        None
    }
}

/// 读取 ASCII 标识符，返回名称起点和末尾。对原始标识符，起点跳过 `r#`。
fn name_range(chars: &[char], from: usize) -> Option<(usize, usize)> {
    if chars.get(from..from + 2) == Some(&['r', '#'])
        && let Some(end) = bare_name_end(chars, from + 2)
    {
        return Some((from + 2, end));
    }
    bare_name_end(chars, from).map(|end| (from, end))
}

/// 返回 ASCII 裸标识符之后的位置。
fn bare_name_end(chars: &[char], from: usize) -> Option<usize> {
    let first = *chars.get(from)?;
    (first.is_ascii_alphabetic() || first == '_')
        .then(|| scan(chars, from + 1, |c| c.is_ascii_alphanumeric() || c == '_'))
}

/// Teal 的关键字、布尔字面量和通配符不能作为裸名称。
fn reserved(chars: &[char], keywords: &[&str]) -> bool {
    let mut buf = [0; 24];
    ascii_word(chars, &mut buf, false)
        .is_some_and(|word| matches!(word, "true" | "false" | "_") || keywords.contains(&word))
}

/// 返回属性与插值路径的一个合法名称段之后的位置。
fn path_name_end(chars: &[char], from: usize, keywords: &[&str]) -> Option<usize> {
    let (name, end) = name_range(chars, from)?;
    let valid = if name == from {
        !reserved(&chars[name..end], keywords)
    } else {
        chars[name..end] != ['_']
    };
    valid.then_some(end)
}

/// 返回可带空格的 `@module::name` 路径末尾，点号开始的字段访问不属于属性。
fn attribute_end(chars: &[char], from: usize, keywords: &[&str]) -> Option<usize> {
    let mut end = path_name_end(chars, skip_space(chars, from), keywords)?;
    loop {
        let sep = skip_space(chars, end);
        if chars.get(sep..sep + 2) != Some(&[':', ':']) {
            break;
        }
        let Some(next) = path_name_end(chars, skip_space(chars, sep + 2), keywords) else {
            break;
        };
        end = next;
    }
    Some(end)
}

/// 读取同一行的 `{限定名称.字段}`。回调只在再次扫描已确认闭合的路径时记录 token。
fn interpolation_end(
    chars: &[char],
    from: usize,
    keywords: &[&str],
    mut mark: impl FnMut(usize, usize, TokenKind),
) -> Option<usize> {
    let mut start = skip_space(chars, from);
    let mut end = path_name_end(chars, start, keywords)?;
    mark(start, end, TokenKind::Variable);
    while chars.get(end..end + 2) == Some(&[':', ':']) {
        start = end + 2;
        end = path_name_end(chars, start, keywords)?;
        mark(start, end, TokenKind::Variable);
    }
    while chars.get(end) == Some(&'.') {
        start = end + 1;
        end = if chars.get(start).is_some_and(char::is_ascii_digit) {
            scan(chars, start + 1, |c| c.is_ascii_digit())
        } else {
            path_name_end(chars, start, keywords)?
        };
        mark(start, end, TokenKind::Property);
    }
    end = skip_space(chars, end);
    (chars.get(end) == Some(&'}')).then_some(end + 1)
}

/// 返回字符字面量的收尾引号之后的位置。
fn character_end(chars: &[char], start: usize) -> Option<usize> {
    let end = match chars.get(start + 1)? {
        '\\' => escape_end(chars, start + 1, true)?,
        '\'' | '\n' | '\r' => return None,
        _ => start + 2,
    };
    (chars.get(end) == Some(&'\'')).then_some(end + 1)
}

/// 返回合法转义序列之后的位置。Unicode 转义最多读取六位十六进制数字。
fn escape_end(chars: &[char], from: usize, character: bool) -> Option<usize> {
    match chars.get(from + 1)? {
        'n' | 't' | 'r' | '\\' | '"' | '0' => Some(from + 2),
        '\'' if character => Some(from + 2),
        'u' if chars.get(from + 2) == Some(&'{') => {
            let mut value = 0;
            let mut end = from + 3;
            while end - (from + 3) < 6 {
                let Some(digit) = chars.get(end).and_then(|c| c.to_digit(16)) else {
                    break;
                };
                value = value * 16 + digit;
                end += 1;
            }
            (end > from + 3 && chars.get(end) == Some(&'}') && char::from_u32(value).is_some())
                .then_some(end + 1)
        }
        _ => None,
    }
}

/// 跳过 Teal 名称或路径边缘的 ASCII 空白。
fn skip_space(chars: &[char], from: usize) -> usize {
    scan(chars, from, |c| c.is_ascii_whitespace())
}
