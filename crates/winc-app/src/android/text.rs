use std::ops::Range;

use winc_core::{EditKind, Lines, Selection, edit};
use winc_editor::Document;

const CONTEXT_RADIUS: usize = 256;
const REFRESH_MARGIN: usize = 64;

/// The editor's original text and the LF view passed to Android.
pub(crate) struct InputText {
    start: usize,
    raw: String,
    pub(crate) text: String,
    boundaries: Vec<usize>,
}

impl InputText {
    fn new(start: usize, raw: String) -> Self {
        let mut text = String::new();
        let mut boundaries = vec![0];
        let mut chars = raw.chars().peekable();
        let mut offset = 0;
        while let Some(ch) = chars.next() {
            offset += 1;
            if ch == '\r' && chars.next_if_eq(&'\n').is_some() {
                offset += 1;
            }
            text.push(if ch == '\r' { '\n' } else { ch });
            boundaries.push(offset);
        }
        Self {
            start,
            raw,
            text,
            boundaries,
        }
    }

    pub(crate) fn capture(doc: &Document) -> Self {
        let buffer = doc.text();
        let mut start = doc.selection.from().saturating_sub(CONTEXT_RADIUS);
        let mut end = (doc.selection.to() + CONTEXT_RADIUS).min(buffer.end_pos());
        if start.checked_sub(1).and_then(|pos| buffer.char_at(pos)) == Some('\r') {
            start -= 1;
        }
        if end.checked_sub(1).and_then(|pos| buffer.char_at(pos)) == Some('\r')
            && buffer.char_at(end) == Some('\n')
        {
            end += 1;
        }
        Self::new(start, buffer.substring(start, end))
    }

    fn offset(&self, raw_position: usize) -> usize {
        self.boundaries
            .partition_point(|&boundary| boundary <= raw_position)
            - 1
    }

    pub(crate) fn utf16_at(&self, position: usize) -> i32 {
        self.text
            .chars()
            .take(self.offset(position - self.start))
            .map(char::len_utf16)
            .sum::<usize>() as i32
    }

    pub(crate) fn near_edge(&self, doc: &Document) -> bool {
        let end = self.start + self.boundaries[self.boundaries.len() - 1];
        (self.start != 0 && doc.selection.from().saturating_sub(self.start) < REFRESH_MARGIN)
            || (end != doc.text().end_pos()
                && end.saturating_sub(doc.selection.to()) < REFRESH_MARGIN)
    }

    /// Apply only the supplied replacement, preserving the document's line endings.
    pub(crate) fn apply(
        &self,
        doc: &mut Document,
        incoming: String,
        anchor: i32,
        head: i32,
    ) -> Option<Self> {
        let anchor = char_offset(&incoming, anchor)?;
        let head = char_offset(&incoming, head)?;
        let next = Self::new(0, incoming);
        let (range, replacement) = difference(&self.text, &next.text);
        let mut from = self.boundaries[range.start];
        let to = self.boundaries[range.end];
        let mut inserted = replacement.replace('\n', doc.line_ending.as_str());
        let before = doc.selection;

        if !range.is_empty() || !replacement.is_empty() {
            let left = (self.start + from)
                .checked_sub(1)
                .and_then(|pos| doc.text().char_at(pos));
            let right = doc.text().char_at(self.start + to);
            if inserted.ends_with('\r') && right == Some('\n') {
                inserted.push('\n');
            }
            // Keep a pre-existing lone CR distinct from an adjacent LF after the edit.
            if left == Some('\r')
                && (inserted.starts_with('\n') || (inserted.is_empty() && right == Some('\n')))
            {
                from = from.checked_sub(1)?;
                inserted.insert_str(0, "\r\n");
            }
        }

        let from_byte = byte_at(&self.raw, from);
        let to_byte = byte_at(&self.raw, to);
        let updated = Self::new(
            self.start,
            format!(
                "{}{inserted}{}",
                &self.raw[..from_byte],
                &self.raw[to_byte..]
            ),
        );
        if updated.text != next.text {
            return None;
        }
        let selection = Selection::new(
            self.start + updated.boundaries[next.offset(anchor)],
            self.start + updated.boundaries[next.offset(head)],
        );
        if self.raw != updated.raw {
            let range = Selection::new(self.start + from, self.start + to);
            let mut tx = edit::replace_selection(doc.text(), range, &inserted, EditKind::Other);
            tx.before = before;
            tx.after = selection;
            let revision = doc.revision;
            doc.apply(tx);
            if doc.revision == revision {
                return None;
            }
        } else {
            doc.selection = selection;
        }
        doc.goal_cell = None;
        Some(updated)
    }
}

fn byte_at(text: &str, offset: usize) -> usize {
    text.char_indices()
        .nth(offset)
        .map_or(text.len(), |(index, _)| index)
}

/// Converts a UTF-16 boundary to the corresponding Rust character offset.
pub(crate) fn char_offset(text: &str, offset: i32) -> Option<usize> {
    let mut remaining = usize::try_from(offset).ok()?;
    let mut count = 0;
    for ch in text.chars() {
        if remaining == 0 {
            return Some(count);
        }
        remaining = remaining.checked_sub(ch.len_utf16())?;
        count += 1;
    }
    (remaining == 0).then_some(count)
}

/// Returns the character range to replace and the replacement text.
pub(crate) fn difference<'a>(before: &str, after: &'a str) -> (Range<usize>, &'a str) {
    let mut prefix = 0;
    let mut prefix_bytes = 0;
    for (from, _) in before
        .chars()
        .zip(after.chars())
        .take_while(|(a, b)| a == b)
    {
        prefix += 1;
        prefix_bytes += from.len_utf8();
    }
    let before = &before[prefix_bytes..];
    let after = &after[prefix_bytes..];
    let mut suffix = 0;
    let mut suffix_bytes = 0;
    for (from, _) in before
        .chars()
        .rev()
        .zip(after.chars().rev())
        .take_while(|(a, b)| a == b)
    {
        suffix += 1;
        suffix_bytes += from.len_utf8();
    }
    (
        prefix..prefix + before.chars().count() - suffix,
        &after[..after.len() - suffix_bytes],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use winc_core::LineEnding;
    use winc_editor::{Command, Editor};

    #[test]
    fn converts_utf16_without_splitting_an_emoji() {
        assert_eq!(char_offset("甲😀乙", 1), Some(1));
        assert_eq!(char_offset("甲😀乙", 2), None);
        assert_eq!(char_offset("甲😀乙", 3), Some(2));
        assert_eq!(char_offset("甲😀乙", 4), Some(3));
        assert_eq!(char_offset("甲😀乙", 5), None);
    }

    #[test]
    fn finds_the_minimal_unicode_replacement() {
        assert_eq!(difference("甲😀乙", "甲你好乙"), (1..2, "你好"));
        assert_eq!(difference("aaa", "aa"), (2..3, ""));
        assert_eq!(difference("甲", "甲\n"), (1..1, "\n"));
        assert_eq!(difference("甲", "甲"), (1..1, ""));
    }

    #[test]
    fn replaces_literal_parentheses_and_supports_undo() {
        let mut editor = Editor::default();
        editor.new_untitled();
        let doc = editor.active_mut().unwrap();
        let before = InputText::capture(doc);
        let typed = before.apply(doc, "(".into(), 1, 1).unwrap();
        assert_eq!(doc.text().substring(0, doc.text().end_pos()), "(");
        typed.apply(doc, String::new(), 0, 0).unwrap();
        assert_eq!(doc.text().end_pos(), 0);
        doc.execute(Command::Undo, 25, 4);
        assert_eq!(doc.text().substring(0, doc.text().end_pos()), "(");
        assert_eq!(doc.selection, Selection::point(1));
    }

    #[test]
    fn maps_utf16_edits_back_to_the_documents_crlf_offsets() {
        let mut editor = Editor::default();
        editor.new_untitled();
        let doc = editor.active_mut().unwrap();
        doc.line_ending = LineEnding::CrLf;
        let start = InputText::capture(doc);
        let before = start.apply(doc, "😀\nB".into(), 2, 2).unwrap();
        assert_eq!(before.utf16_at(3), 3);
        let newline = before.apply(doc, "😀\n\nB".into(), 3, 3).unwrap();
        assert_eq!(doc.selection, Selection::point(3));
        newline.apply(doc, "😀\nX\nB".into(), 4, 4).unwrap();
        assert_eq!(
            doc.text().substring(0, doc.text().end_pos()),
            "😀\r\nX\r\nB"
        );
        assert_eq!(doc.selection, Selection::point(4));
    }

    #[test]
    fn requests_context_before_the_cursor_reaches_the_window_edge() {
        let mut editor = Editor::default();
        editor.new_untitled();
        let doc = editor.active_mut().unwrap();
        let initial = InputText::capture(doc);
        initial.apply(doc, "a".repeat(800), 400, 400).unwrap();
        let current = InputText::capture(doc);
        assert!(!current.near_edge(doc));
        current.apply(doc, current.text.clone(), 16, 16).unwrap();
        assert!(current.near_edge(doc));
    }
}
