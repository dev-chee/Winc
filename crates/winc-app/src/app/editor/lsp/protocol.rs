//! LSP 的帧、文件地址和 UTF-16 位置。

use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};

use ropey::Rope;
use serde_json::{Value, json};
use winc_core::Lines;

const MAX_HEADER: u64 = 8 * 1024;
const MAX_MESSAGE: usize = 32 * 1024 * 1024;

pub(super) fn read(input: &mut impl BufRead) -> io::Result<Option<Value>> {
    let mut header = input.take(MAX_HEADER);
    let mut line = String::new();
    let mut length = None;
    loop {
        line.clear();
        if header.read_line(&mut line)? == 0 {
            return if header.limit() == MAX_HEADER {
                Ok(None)
            } else {
                Err(io::ErrorKind::UnexpectedEof.into())
            };
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("Content-Length")
        {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let length = length
        .filter(|&len| len <= MAX_MESSAGE)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid LSP Content-Length"))?;
    let mut body = vec![0; length];
    input.read_exact(&mut body)?;
    Ok(Some(serde_json::from_slice(&body)?))
}

pub(super) fn write(output: &mut impl Write, message: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(message)?;
    write!(output, "Content-Length: {}\r\n\r\n", body.len())?;
    output.write_all(&body)?;
    output.flush()
}

pub(super) fn file_uri(path: &Path) -> Option<String> {
    if !path.is_absolute() {
        return None;
    }
    #[cfg(windows)]
    let (prefix, bytes): (&str, Vec<u8>) = {
        let path = path.to_str()?.replace('\\', "/");
        let path = if let Some(unc) = path.strip_prefix("//?/UNC/") {
            format!("//{unc}")
        } else {
            path.strip_prefix("//?/").unwrap_or(&path).to_owned()
        };
        if let Some(unc) = path.strip_prefix("//") {
            ("file://", unc.as_bytes().to_vec())
        } else {
            let mut path = path.into_bytes();
            if path.get(1) == Some(&b':') {
                path[0].make_ascii_lowercase();
            }
            ("file:///", path)
        }
    };
    #[cfg(not(windows))]
    let (prefix, bytes) = ("file://", path.as_os_str().as_encoded_bytes());
    let mut uri = prefix.to_owned();
    let bytes: &[u8] = bytes.as_ref();
    for &byte in bytes {
        if byte.is_ascii_alphanumeric() || b"-_.~/:".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(uri, "%{byte:02X}");
        }
    }
    Some(uri)
}

pub(super) fn file_path(uri: &str) -> Option<PathBuf> {
    let (scheme, rest) = uri.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("file") || rest.contains(['#', '?']) {
        return None;
    }
    let (host, path) = rest.split_at(rest.find('/')?);
    let local = host.is_empty() || host.eq_ignore_ascii_case("localhost");
    #[cfg(not(windows))]
    if !local {
        return None;
    }
    let mut bytes = Vec::with_capacity(path.len());
    #[cfg(windows)]
    if !local {
        if !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
        {
            return None;
        }
        bytes.extend_from_slice(b"//");
        bytes.extend_from_slice(host.as_bytes());
    }
    let mut chunks = path.as_bytes().iter();
    while let Some(&byte) = chunks.next() {
        let byte = if byte == b'%' {
            let hi = char::from(*chunks.next()?).to_digit(16)?;
            let lo = char::from(*chunks.next()?).to_digit(16)?;
            (hi * 16 + lo) as u8
        } else {
            byte
        };
        if byte == 0 {
            return None;
        }
        bytes.push(byte);
    }
    #[cfg(windows)]
    let path = {
        let text = std::str::from_utf8(&bytes).ok()?;
        let text = if local { text.strip_prefix('/')? } else { text };
        PathBuf::from(text.replace('/', "\\"))
    };
    #[cfg(unix)]
    let path = {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_vec(bytes))
    };
    path.is_absolute().then_some(path)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Position {
    pub line: usize,
    pub character: usize,
}

impl Position {
    pub fn parse(value: &Value) -> Option<Self> {
        Some(Self {
            line: usize::try_from(value.get("line")?.as_u64()?).ok()?,
            character: usize::try_from(value.get("character")?.as_u64()?).ok()?,
        })
    }

    pub fn at(text: &impl Lines, offset: usize) -> Self {
        let (line, col) = text.pos_to_line_col(offset);
        let character = text
            .line_content(line)
            .slice()
            .chars()
            .take(col)
            .map(char::len_utf16)
            .sum();
        Self { line, character }
    }

    pub fn offset(self, text: &impl Lines) -> Option<usize> {
        if self.line >= text.line_count() {
            return None;
        }
        let mut units = 0;
        let line = text.line_content(self.line);
        for (column, ch) in line.slice().chars().enumerate() {
            if units == self.character {
                return Some(text.line_start(self.line) + column);
            }
            units += ch.len_utf16();
            if units > self.character {
                return None;
            }
        }
        // LSP uses the line length if the column exceeds it.
        Some(text.line_start(self.line) + line.slice().len_chars())
    }

    pub fn json(self) -> Value {
        json!({"line": self.line, "character": self.character})
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Range {
    pub start: Position,
    pub end: Position,
}

impl Range {
    pub fn parse(value: &Value) -> Option<Self> {
        let start = Position::parse(value.get("start")?)?;
        let end = Position::parse(value.get("end")?)?;
        (start <= end).then_some(Self { start, end })
    }
}

pub(super) fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0", "method":method, "params":params})
}

pub(super) fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params})
}

/// 比较对齐的块片段，编辑造成的 Rope 分块变化不影响匹配。
fn common_bytes<'a>(
    mut old: impl Iterator<Item = &'a str>,
    mut new: impl Iterator<Item = &'a str>,
    reverse: bool,
) -> usize {
    let mut a = &[][..];
    let mut b = &[][..];
    let mut matched = 0;
    loop {
        while a.is_empty() {
            let Some(chunk) = old.next() else {
                return matched;
            };
            a = chunk.as_bytes();
        }
        while b.is_empty() {
            let Some(chunk) = new.next() else {
                return matched;
            };
            b = chunk.as_bytes();
        }
        let len = a.len().min(b.len());
        let (left, right);
        if reverse {
            (a, left) = a.split_at(a.len() - len);
            (b, right) = b.split_at(b.len() - len);
        } else {
            (left, a) = a.split_at(len);
            (right, b) = b.split_at(len);
        }
        if left.as_ptr() != right.as_ptr() && left != right {
            let partial = if reverse {
                left.iter()
                    .rev()
                    .zip(right.iter().rev())
                    .take_while(|(a, b)| a == b)
                    .count()
            } else {
                left.iter().zip(right).take_while(|(a, b)| a == b).count()
            };
            return matched + partial;
        }
        matched += len;
    }
}

/// 以一处区间修改覆盖两个缓冲区的差异，CRLF 的两个字符必须一起替换。
pub(super) fn incremental_change(old: &Rope, new: &Rope, unchanged_lines: usize) -> Value {
    let old_len = old.len_chars();
    let new_len = new.len_chars();
    let line = unchanged_lines.min(old.len_lines().min(new.len_lines()) - 1);
    let start_of_line = old.line_to_char(line);
    let known = if start_of_line == new.line_to_char(line) {
        start_of_line
    } else {
        0
    };
    let prefix = common_bytes(
        old.slice(known..).chunks(),
        new.slice(known..).chunks(),
        false,
    );
    // 若仅匹配到一个 UTF-8 字符的开头，byte_to_char 回到该字符起点。
    let mut start = old.byte_to_char(old.char_to_byte(known) + prefix);
    let splits_crlf = |text: &Rope, at: usize| {
        at > 0 && text.get_char(at - 1) == Some('\r') && text.get_char(at) == Some('\n')
    };
    if splits_crlf(old, start) || splits_crlf(new, start) {
        start -= 1;
    }
    let old_tail = old.slice(start..);
    let new_tail = new.slice(start..);
    let suffix = common_bytes(
        old_tail.chunks_at_byte(old_tail.len_bytes()).0.reversed(),
        new_tail.chunks_at_byte(new_tail.len_bytes()).0.reversed(),
        true,
    );
    let suffix_byte = old.len_bytes() - suffix;
    let end = old.byte_to_char(suffix_byte);
    let mut suffix = old_len - end - usize::from(old.char_to_byte(end) < suffix_byte);
    if splits_crlf(old, old_len - suffix) || splits_crlf(new, new_len - suffix) {
        suffix -= 1;
    }
    json!({
        "range":{
            "start":Position::at(old, start).json(),
            "end":Position::at(old, old_len - suffix).json()
        },
        "text":new.slice(start..new_len - suffix).to_string()
    })
}
