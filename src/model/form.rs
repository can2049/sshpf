//! State of the create/edit form: its field order, its mode, and one text
//! buffer with a cursor.
//!
//! This lives in the model (not in `app`) so `ui` can render the form without
//! depending on the `app` module. The form's *rules* (validation, port planning)
//! are `app::form`; what is here is the shape the user edits.
//!
//! Belongs here: field metadata/order, create-vs-edit identity, text editing.
//! Does not belong: validation, ssh probing, rendering.

use super::TunnelId;

/// Editable fields of the create/edit form, in tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormField {
    Name,
    Kind,
    SshHost,
    LocalAddr,
    LocalPort,
    RemoteHost,
    RemotePort,
    AutoRestart,
}

impl FormField {
    /// Field order, chosen so the form reads top-to-bottom in the order a user
    /// actually thinks about a forward: pick the machine, name it, then say
    /// *where* the traffic goes (remote port) before *where it lands locally*
    /// (local port) — because the remote port is what the user knows, and the
    /// local port is derived from it as a suggestion.
    pub const ORDER: [FormField; 8] = [
        FormField::SshHost,
        FormField::Name,
        FormField::Kind,
        FormField::RemoteHost,
        FormField::RemotePort,
        FormField::LocalAddr,
        FormField::LocalPort,
        FormField::AutoRestart,
    ];

    /// Fields the user types into (as opposed to toggles/selects).
    pub fn is_text(self) -> bool {
        matches!(
            self,
            FormField::Name
                | FormField::SshHost
                | FormField::LocalAddr
                | FormField::LocalPort
                | FormField::RemoteHost
                | FormField::RemotePort
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            FormField::Name => "名称",
            FormField::Kind => "类型",
            FormField::SshHost => "SSH 主机",
            FormField::LocalAddr => "本地地址",
            FormField::LocalPort => "本地端口",
            FormField::RemoteHost => "远端地址",
            FormField::RemotePort => "远端端口",
            FormField::AutoRestart => "自动重连",
        }
    }

    /// English half of [`FormField::label`], for the bilingual UI.
    pub fn label_en(self) -> &'static str {
        match self {
            FormField::Name => "Name",
            FormField::Kind => "Type",
            FormField::SshHost => "SSH host",
            FormField::LocalAddr => "Local addr",
            FormField::LocalPort => "Local port",
            FormField::RemoteHost => "Remote host",
            FormField::RemotePort => "Remote port",
            FormField::AutoRestart => "Auto-retry",
        }
    }

    pub fn next(self) -> FormField {
        // `ORDER` is a fixed array, but the "position may be missing" case is
        // real: a field added without being listed would make `position` return
        // `None`. Fall back to the first field instead of indexing blind.
        let i = FormField::ORDER.iter().position(|f| *f == self).unwrap_or(0);
        FormField::ORDER.get(i + 1).or_else(|| FormField::ORDER.first()).copied().unwrap_or(self)
    }

    pub fn prev(self) -> FormField {
        let i = FormField::ORDER.iter().position(|f| *f == self).unwrap_or(0);
        match i.checked_sub(1) {
            Some(prev) => FormField::ORDER.get(prev).copied().unwrap_or(self),
            None => FormField::ORDER.last().copied().unwrap_or(self),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormMode {
    Create,
    Edit(TunnelId),
}

/// A text input buffer with a cursor position.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputBuf {
    pub text: String,
    pub cursor: usize,
}

impl InputBuf {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.chars().count();
        InputBuf { text, cursor }
    }

    pub fn value(&self) -> &str {
        &self.text
    }

    pub fn set(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.chars().count();
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn insert(&mut self, c: char) {
        // `cursor` is a public field and can be stale (past the end of the text).
        // Clamping first keeps `cursor += 1` from overflowing and puts the new
        // character where the user is looking rather than after a `usize::MAX`.
        let len = self.text.chars().count();
        if self.cursor > len {
            self.cursor = len;
        }
        let byte = self.byte_index(self.cursor);
        self.text.insert(byte, c);
        self.cursor = self.cursor.saturating_add(1);
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        // A stale `cursor` past the end means "the caret is after the last
        // character", so clamp and delete that character rather than bailing
        // out — backspace at the end is the common case.
        let len = self.text.chars().count();
        if self.cursor > len {
            self.cursor = len;
        }
        if self.cursor == 0 {
            return;
        }
        let end = self.byte_index(self.cursor);
        let start = self.byte_index(self.cursor - 1);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
    }

    pub fn delete(&mut self) {
        let len = self.text.chars().count();
        if self.cursor >= len {
            return;
        }
        let start = self.byte_index(self.cursor);
        let end = self.byte_index(self.cursor + 1);
        self.text.replace_range(start..end, "");
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        let len = self.text.chars().count();
        if self.cursor < len {
            self.cursor += 1;
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.text.chars().count();
    }

    fn byte_index(&self, char_idx: usize) -> usize {
        self.text.char_indices().nth(char_idx).map(|(i, _)| i).unwrap_or(self.text.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_buf_edits_multibyte() {
        let mut b = InputBuf::new("中文");
        assert_eq!(b.cursor, 2);
        b.left();
        b.insert('X');
        assert_eq!(b.value(), "中X文");
        b.backspace();
        assert_eq!(b.value(), "中文");
        b.delete();
        assert_eq!(b.value(), "中");
    }

    #[test]
    fn input_buf_cursor_navigation() {
        let mut b = InputBuf::new("abc");
        assert_eq!(b.cursor, 3, "cursor starts at the end");
        b.home();
        assert_eq!(b.cursor, 0);
        b.insert('X');
        assert_eq!(b.value(), "Xabc");
        // Currently "Xabc" with the cursor after "X".
        b.end();
        assert_eq!(b.cursor, 4);
        b.left();
        b.left();
        assert_eq!(b.cursor, 2); // between 'a' and 'b'
        b.delete(); // delete the char under the cursor
        assert_eq!(b.value(), "Xac");
        b.right();
        b.right();
        b.right(); // no-op past the end
        assert_eq!(b.cursor, 3);
        b.clear();
        assert!(b.value().is_empty());
        assert_eq!(b.cursor, 0);
    }

    #[test]
    fn input_buf_backspace_at_start_is_a_noop() {
        let mut b = InputBuf::new("ab");
        b.home();
        b.backspace();
        assert_eq!(b.value(), "ab");
        assert_eq!(b.cursor, 0);
    }

    #[test]
    fn input_buf_set_moves_cursor_to_end() {
        let mut b = InputBuf::new("abc");
        b.home();
        b.set("xyz");
        assert_eq!(b.value(), "xyz");
        assert_eq!(b.cursor, 3);
    }

    #[test]
    fn an_input_buffer_with_a_stale_cursor_does_not_panic() {
        // `cursor` is a public field: a value past the end must clamp, not slice
        // at a byte offset the text does not have (and not overflow on `+= 1`).
        let mut b = InputBuf::new("abc");
        b.cursor = usize::MAX;
        b.insert('X');
        assert!(b.value().ends_with('X'), "the character must land at the end: {:?}", b.value());
        assert_eq!(b.value(), "abcX");

        b.cursor = usize::MAX;
        b.backspace();
        assert_eq!(b.value(), "abc");

        b.cursor = usize::MAX;
        b.delete();
        assert_eq!(b.value(), "abc", "delete at the end is a no-op");

        b.cursor = usize::MAX;
        b.right();
        b.left();
        b.home();
        b.end();
        assert_eq!(b.cursor, b.value().chars().count());
    }

    #[test]
    fn an_input_buffer_with_a_stale_cursor_edits_multibyte_text_safely() {
        let mut b = InputBuf::new("中文");
        b.cursor = 99;
        b.insert('X');
        assert_eq!(b.value(), "中文X");
        b.cursor = 1;
        b.insert('Y');
        assert_eq!(b.value(), "中Y文X");
    }

    #[test]
    fn form_field_metadata() {
        // Every field is reachable by cycling, and text fields are flagged.
        for f in FormField::ORDER {
            assert!(!f.label().is_empty());
        }
        assert!(FormField::Name.is_text());
        assert!(FormField::SshHost.is_text());
        assert!(FormField::LocalPort.is_text());
        assert!(!FormField::Kind.is_text());
        assert!(!FormField::AutoRestart.is_text());
        // Host comes first: it is the field the user fills first.
        assert_eq!(FormField::ORDER[0], FormField::SshHost);
    }

    #[test]
    fn form_field_cycles() {
        let mut f = FormField::SshHost;
        for _ in 0..FormField::ORDER.len() {
            f = f.next();
        }
        assert_eq!(f, FormField::SshHost);
        assert_eq!(FormField::SshHost.prev(), FormField::AutoRestart);
    }

    #[test]
    fn form_field_navigation_wraps_in_both_directions() {
        // `next`/`prev` index a fixed array; a field missing from ORDER would
        // make `position` return `None`, and the old code then indexed blind.
        for field in FormField::ORDER {
            let next = field.next();
            let prev = field.prev();
            assert!(FormField::ORDER.contains(&next), "{field:?}.next() = {next:?} is not in ORDER");
            assert!(FormField::ORDER.contains(&prev), "{field:?}.prev() = {prev:?} is not in ORDER");
            assert_eq!(next.prev(), field, "next/prev must be inverses for {field:?}");
        }
        assert_eq!(FormField::ORDER.first().copied().unwrap().prev(), FormField::AutoRestart);
        assert_eq!(FormField::ORDER.last().copied().unwrap().next(), FormField::SshHost);
    }
}
