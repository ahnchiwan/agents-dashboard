//! Layout of the read-only chat history shown under the selected session.
//!
//! The sessions panel renders it and `App` scrolls it; both need the same
//! wrapped rows, so the pure layout lives here instead of in either of them.

use crate::model::{ChatMessage, ChatRole};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use unicode_width::UnicodeWidthChar;

/// Columns left of the text: a space, the role label and a space.
pub(crate) const LABEL_COLS: usize = 3;
/// Columns the text never uses: the label plus the scrollbar column.
pub(crate) const CHROME_COLS: usize = LABEL_COLS + 1;
/// Extra indent of tool results under their call.
const RESULT_INDENT: usize = 2;
/// Narrowest text column that still gets the result indent.
const MIN_INDENTED_WIDTH: usize = 12;

/// Geometry of the chat view in the last drawn frame. All zero when no chat
/// view is on screen, which turns the chat scroll keys into no-ops.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ChatViewMetrics {
    pub text_width: usize,
    pub visible_rows: usize,
}

/// One wrapped terminal row of a chat message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChatRow {
    /// Index of the message in `chat_messages`.
    pub message: usize,
    /// True for the first row of a message, which carries the role label.
    pub first: bool,
    pub text: String,
}

/// Remembers which message row the viewport top points at. Messages are
/// dropped from the front once the collector's window is full, so a plain row
/// number would drift; the message content identifies the position instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChatAnchor {
    hash: u64,
    /// Index of the message when the anchor was taken; breaks ties between
    /// messages with identical text.
    index: usize,
    /// Row inside that message.
    row: usize,
}

/// All messages wrapped to one width, oldest first.
#[derive(Debug)]
pub(crate) struct ChatLayout {
    pub rows: Vec<ChatRow>,
    /// First row of each message.
    starts: Vec<usize>,
    hashes: Vec<u64>,
}

fn message_hash(message: &ChatMessage) -> u64 {
    let mut hasher = DefaultHasher::new();
    let role: u8 = match message.role {
        ChatRole::User => 0,
        ChatRole::Assistant => 1,
        ChatRole::ToolCall => 2,
        ChatRole::ToolResult => 3,
        ChatRole::ToolError => 4,
    };
    role.hash(&mut hasher);
    message.text.hash(&mut hasher);
    hasher.finish()
}

/// Columns a message's rows are indented by: tool results sit under their call
/// unless the view is too narrow to spare them.
fn text_indent(role: &ChatRole, text_width: usize) -> usize {
    match role {
        ChatRole::ToolResult | ChatRole::ToolError if text_width >= MIN_INDENTED_WIDTH => {
            RESULT_INDENT
        }
        _ => 0,
    }
}

impl ChatLayout {
    pub(crate) fn new(messages: &[ChatMessage], text_width: usize) -> Self {
        let mut rows = Vec::new();
        let mut starts = Vec::with_capacity(messages.len());
        let mut hashes = Vec::with_capacity(messages.len());
        for (index, message) in messages.iter().enumerate() {
            starts.push(rows.len());
            hashes.push(message_hash(message));
            let indent = text_indent(&message.role, text_width);
            for (n, text) in wrap_to_width(&message.text, text_width - indent)
                .into_iter()
                .enumerate()
            {
                rows.push(ChatRow {
                    message: index,
                    first: n == 0,
                    text: if indent == 0 || text.is_empty() {
                        text
                    } else {
                        format!("{}{text}", " ".repeat(indent))
                    },
                });
            }
        }
        Self {
            rows,
            starts,
            hashes,
        }
    }

    pub(crate) fn total_rows(&self) -> usize {
        self.rows.len()
    }

    /// Anchor for the viewport top at row `offset`.
    pub(crate) fn anchor_at(&self, offset: usize) -> Option<ChatAnchor> {
        let index = self.starts.partition_point(|&start| start <= offset);
        let index = index.checked_sub(1)?;
        Some(ChatAnchor {
            hash: self.hashes[index],
            index,
            row: offset - self.starts[index],
        })
    }

    /// Row of the viewport top for `anchor` in this layout. A message that is
    /// no longer retained falls back to the oldest retained row.
    pub(crate) fn resolve(&self, anchor: &ChatAnchor) -> usize {
        let matches = || {
            self.hashes
                .iter()
                .enumerate()
                .filter(|(_, hash)| **hash == anchor.hash)
                .map(|(index, _)| index)
        };
        // Messages only ever shift to a lower index when old ones are dropped.
        let Some(index) = matches()
            .rfind(|index| *index <= anchor.index)
            .or_else(|| matches().next())
        else {
            return 0;
        };
        let end = self
            .starts
            .get(index + 1)
            .copied()
            .unwrap_or(self.rows.len());
        let last_row = end.saturating_sub(self.starts[index]).saturating_sub(1);
        self.starts[index] + anchor.row.min(last_row)
    }
}

/// Split text into rows of at most `width` terminal columns, keeping line breaks.
pub(crate) fn wrap_to_width(text: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    for line in text.split('\n') {
        let first = rows.len();
        let mut row = String::new();
        let mut row_w = 0;
        for ch in line.chars() {
            let ch_w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if row_w + ch_w > width && !row.is_empty() {
                rows.push(std::mem::take(&mut row));
                row_w = 0;
            }
            // Don't start a wrapped row with whitespace.
            if row.is_empty() && rows.len() > first && ch.is_whitespace() {
                continue;
            }
            row.push(ch);
            row_w += ch_w;
        }
        // Keep blank lines as blank rows.
        if !row.is_empty() || rows.len() == first {
            rows.push(row);
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn message(role: ChatRole, text: &str) -> ChatMessage {
        ChatMessage {
            role,
            text: text.into(),
        }
    }

    #[test]
    fn wrap_to_width_respects_display_width() {
        assert_eq!(wrap_to_width("가나다라마", 4), vec!["가나", "다라", "마"]);
        assert_eq!(wrap_to_width("abc def", 3), vec!["abc", "def"]);
        assert_eq!(wrap_to_width("a가", 2), vec!["a", "가"]);
        assert_eq!(wrap_to_width("", 4), vec![""]);
        assert_eq!(
            wrap_to_width("abcd\n\n  ef", 3),
            vec!["abc", "d", "", "  e", "f"]
        );
    }

    #[test]
    fn layout_keeps_order_and_labels_first_rows() {
        let messages = [
            message(ChatRole::User, "abcdef"),
            message(ChatRole::Assistant, "x\ny"),
            message(ChatRole::User, "z"),
        ];
        let layout = ChatLayout::new(&messages, 4);
        let rows: Vec<_> = layout
            .rows
            .iter()
            .map(|row| (row.message, row.first, row.text.as_str()))
            .collect();
        assert_eq!(
            rows,
            vec![
                (0, true, "abcd"),
                (0, false, "ef"),
                (1, true, "x"),
                (1, false, "y"),
                (2, true, "z"),
            ]
        );
        assert_eq!(layout.total_rows(), 5);
        for row in &layout.rows {
            assert!(UnicodeWidthStr::width(row.text.as_str()) <= 4);
        }
    }

    #[test]
    fn anchor_survives_dropped_and_appended_messages() {
        let texts = ["one", "two", "three", "four"];
        let messages: Vec<_> = texts
            .iter()
            .map(|text| message(ChatRole::User, &format!("{text}\n{text}")))
            .collect();
        let layout = ChatLayout::new(&messages, 10);
        // Viewport top on the second row of "three".
        let anchor = layout.anchor_at(5).unwrap();
        assert_eq!(layout.resolve(&anchor), 5);

        // The oldest message is dropped and a new one is appended.
        let mut shifted: Vec<_> = messages[1..].to_vec();
        shifted.push(message(ChatRole::Assistant, "five\nfive"));
        let shifted_layout = ChatLayout::new(&shifted, 10);
        assert_eq!(shifted_layout.resolve(&anchor), 3);
        assert_eq!(shifted_layout.rows[3].text, "three");

        // The anchored message itself is gone: fall back to the oldest row.
        let gone = ChatLayout::new(&messages[3..], 10);
        assert_eq!(gone.resolve(&anchor), 0);
        assert!(ChatLayout::new(&[], 10).anchor_at(0).is_none());
    }

    #[test]
    fn anchor_row_is_clamped_after_a_resize() {
        let long = message(ChatRole::Assistant, &"a".repeat(30));
        let narrow = ChatLayout::new(std::slice::from_ref(&long), 5);
        let anchor = narrow.anchor_at(4).unwrap();
        let wide = ChatLayout::new(std::slice::from_ref(&long), 20);
        assert_eq!(wide.resolve(&anchor), 1);
    }

    #[test]
    fn tool_results_are_indented_and_still_fit_the_width() {
        let messages = [
            message(ChatRole::ToolCall, "Bash ls"),
            message(ChatRole::ToolResult, "abcdefghij\nk"),
            message(ChatRole::ToolError, "가나다라마"),
        ];
        let layout = ChatLayout::new(&messages, 12);
        let rows: Vec<_> = layout.rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(rows, vec!["Bash ls", "  abcdefghij", "  k", "  가나다라마"]);
        for row in &layout.rows {
            assert!(UnicodeWidthStr::width(row.text.as_str()) <= 12);
        }
        // Too narrow to spare the indent: results use the full width instead.
        let narrow = ChatLayout::new(&messages[1..2], 8);
        let rows: Vec<_> = narrow.rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(rows, vec!["abcdefgh", "ij", "k"]);
        // Blank result lines stay blank rows without trailing indent.
        let blank = ChatLayout::new(&[message(ChatRole::ToolResult, "a\n\nb")], 20);
        let rows: Vec<_> = blank.rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(rows, vec!["  a", "", "  b"]);
    }

    #[test]
    fn anchors_tell_apart_tool_items_and_messages_with_the_same_text() {
        let messages = [
            message(ChatRole::Assistant, "ls"),
            message(ChatRole::ToolCall, "ls"),
            message(ChatRole::ToolResult, "ls"),
            message(ChatRole::ToolError, "ls"),
        ];
        let layout = ChatLayout::new(&messages, 20);
        for (offset, expected) in [(0, 0), (1, 1), (2, 2), (3, 3)] {
            let anchor = layout.anchor_at(offset).unwrap();
            assert_eq!(layout.resolve(&anchor), expected);
        }
        // The first three items drop out: only the matching role is found.
        let tail = ChatLayout::new(&messages[3..], 20);
        let anchor = layout.anchor_at(3).unwrap();
        assert_eq!(tail.resolve(&anchor), 0);
        let call_anchor = layout.anchor_at(1).unwrap();
        // The tool call is gone from the tail; fall back to the oldest row.
        assert_eq!(tail.resolve(&call_anchor), 0);
        assert_ne!(
            message_hash(&messages[1]),
            message_hash(&messages[2]),
            "role is part of the identity"
        );
    }

    #[test]
    fn anchor_prefers_the_nearest_identical_message_at_or_before_its_index() {
        let same = message(ChatRole::User, "continue");
        let messages = vec![same.clone(), message(ChatRole::Assistant, "ok"), same];
        let layout = ChatLayout::new(&messages, 20);
        let second = layout.anchor_at(2).unwrap();
        assert_eq!(layout.resolve(&second), 2);
        let first = layout.anchor_at(0).unwrap();
        assert_eq!(layout.resolve(&first), 0);
    }
}
