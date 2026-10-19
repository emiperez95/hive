//! A small multi-line text editor for the notes pane, and a one-line variant
//! for inputs (add-todo, branch name).
//!
//! Hand-rolled rather than a crate: the notes pane needs insert, delete, cursor
//! movement across soft-wrapped lines, paste and click-to-place — about 150
//! lines — and a widget crate would pin hive's ratatui version to its own.

use unicode_width::UnicodeWidthChar;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Editor {
    chars: Vec<char>,
    /// Char index the next insert lands at.
    cursor: usize,
}

impl Editor {
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn set(&mut self, text: &str) {
        self.chars = text.chars().collect();
        self.cursor = self.chars.len();
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn insert(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    /// Pasted text; carriage returns become newlines (terminals paste `\r`).
    pub fn insert_str(&mut self, s: &str) {
        for c in s.replace("\r\n", "\n").replace('\r', "\n").chars() {
            self.insert(c);
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.chars.len());
    }

    /// Start / end of the current hard line.
    pub fn home(&mut self) {
        while self.cursor > 0 && self.chars[self.cursor - 1] != '\n' {
            self.cursor -= 1;
        }
    }

    pub fn end(&mut self) {
        while self.cursor < self.chars.len() && self.chars[self.cursor] != '\n' {
            self.cursor += 1;
        }
    }

    /// Soft-wrap into visual lines of at most `width` cells: `(start, end)` char
    /// ranges, `end` exclusive and never including the `\n`. An empty buffer is
    /// one empty line, so there is always somewhere for the cursor to be.
    pub fn wrap(&self, width: usize) -> Vec<(usize, usize)> {
        let width = width.max(1);
        let mut lines = Vec::new();
        let (mut start, mut w) = (0, 0);
        for (i, &c) in self.chars.iter().enumerate() {
            if c == '\n' {
                lines.push((start, i));
                start = i + 1;
                w = 0;
                continue;
            }
            let cw = c.width().unwrap_or(0);
            if w + cw > width && i > start {
                lines.push((start, i));
                start = i;
                w = 0;
            }
            w += cw;
        }
        lines.push((start, self.chars.len()));
        lines
    }

    pub fn line_text(&self, (s, e): (usize, usize)) -> String {
        self.chars[s..e].iter().collect()
    }

    /// Visual (row, col-in-cells) of the cursor.
    pub fn cursor_pos(&self, width: usize) -> (usize, usize) {
        let lines = self.wrap(width);
        // The cursor sits at the end of a wrapped line only if it's the last one
        // or a hard break follows; otherwise it belongs at the next line's start.
        for (row, &(s, e)) in lines.iter().enumerate() {
            let last = row + 1 == lines.len();
            let next_starts_here = lines.get(row + 1).is_some_and(|n| n.0 == e);
            if self.cursor >= s
                && (self.cursor < e || (self.cursor == e && (last || !next_starts_here)))
            {
                return (row, self.cells(s, self.cursor));
            }
        }
        (lines.len() - 1, 0)
    }

    fn cells(&self, from: usize, to: usize) -> usize {
        self.chars[from..to]
            .iter()
            .map(|c| c.width().unwrap_or(0))
            .sum()
    }

    /// Place the cursor at a visual (row, col) — a click, or up/down.
    pub fn place(&mut self, row: usize, col: usize, width: usize) {
        let lines = self.wrap(width);
        let (s, e) = lines[row.min(lines.len() - 1)];
        let mut i = s;
        let mut w = 0;
        while i < e {
            let cw = self.chars[i].width().unwrap_or(0);
            if w + cw > col {
                break;
            }
            w += cw;
            i += 1;
        }
        self.cursor = i;
    }

    pub fn up(&mut self, width: usize) {
        let (row, col) = self.cursor_pos(width);
        if row == 0 {
            self.cursor = 0;
        } else {
            self.place(row - 1, col, width);
        }
    }

    pub fn down(&mut self, width: usize) {
        let (row, col) = self.cursor_pos(width);
        if row + 1 >= self.wrap(width).len() {
            self.cursor = self.chars.len();
        } else {
            self.place(row + 1, col, width);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ed(s: &str) -> Editor {
        let mut e = Editor::default();
        e.set(s);
        e
    }

    #[test]
    fn wraps_on_width_and_hard_breaks() {
        let e = ed("abcdef\nxy");
        let lines: Vec<String> = e.wrap(4).into_iter().map(|l| e.line_text(l)).collect();
        assert_eq!(lines, ["abcd", "ef", "xy"]);
        assert_eq!(ed("").wrap(4), [(0, 0)]);
        // A trailing newline opens an empty last line for the cursor.
        assert_eq!(ed("ab\n").wrap(4).len(), 2);
    }

    // Emoji are two cells; wrapping by chars would overflow the pane.
    #[test]
    fn wraps_by_display_width() {
        let e = ed("🐝🐝🐝");
        assert_eq!(e.wrap(4).len(), 2);
    }

    #[test]
    fn cursor_position_tracks_soft_and_hard_lines() {
        let mut e = ed("abcdef\nxy");
        assert_eq!(e.cursor_pos(4), (2, 2));
        e.place(0, 4, 4);
        // End of a soft-wrapped line is the start of the next one.
        assert_eq!(e.cursor_pos(4), (1, 0));
        e.place(1, 9, 4);
        assert_eq!(e.cursor_pos(4), (1, 2), "clamped to the line's end");
    }

    #[test]
    fn up_and_down_keep_the_column() {
        let mut e = ed("abcd\nef\nghij");
        e.place(0, 3, 10);
        e.down(10);
        assert_eq!(e.cursor_pos(10), (1, 2));
        e.down(10);
        assert_eq!(e.cursor_pos(10), (2, 2));
        e.up(10);
        e.up(10);
        e.up(10);
        assert_eq!(e.cursor_pos(10), (0, 0));
    }

    #[test]
    fn editing_and_paste() {
        let mut e = ed("ac");
        e.left();
        e.insert('b');
        assert_eq!(e.text(), "abc");
        e.home();
        e.delete();
        e.end();
        e.backspace();
        assert_eq!(e.text(), "b");
        e.insert_str("\r\nx\ry");
        assert_eq!(e.text(), "b\nx\ny");
    }
}
