use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use arboard::Clipboard;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthChar;

use crate::config::Config;
use crate::highlight::{HlSpan, Highlighter, LineState};

const MAX_UNDO: usize = 500;
const DISK_CHECK_INTERVAL: Duration = Duration::from_millis(1500);
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(500);

type Pos = (usize, usize);

#[derive(Clone, Copy)]
struct LastClick {
    row: usize,
    col: usize,
    at: Instant,
}

#[derive(Clone, Copy, Debug)]
struct Fold {
    start: usize,
    end: usize,
}

#[derive(Clone)]
struct Snapshot {
    text: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
    dirty: bool,
    had_trailing_newline: bool,
    folds: Vec<Fold>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditKind {
    InsertChar,
    Other,
}

enum Mode {
    Normal,
    Prompt(PromptState),
}

struct PromptState {
    kind: PromptKind,
    input: String,
}

#[derive(Clone, Copy)]
enum PromptKind {
    Search,
    Goto,
    SaveAs,
}

impl PromptKind {
    fn label(&self) -> &'static str {
        match self {
            PromptKind::Search => "search",
            PromptKind::Goto => "goto",
            PromptKind::SaveAs => "save as",
        }
    }
}

// ---------------------------------------------------------------------------
// Buffer: per-file state
// ---------------------------------------------------------------------------

struct Buffer {
    text: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
    scroll_row: usize,
    scroll_col: usize,
    path: Option<PathBuf>,
    dirty: bool,
    had_trailing_newline: bool,
    selection_anchor: Option<Pos>,
    undo_stack: Vec<Snapshot>,
    redo_stack: Vec<Snapshot>,
    last_edit: Option<EditKind>,
    known_mtime: Option<SystemTime>,
    external_change: bool,
    last_disk_check: Option<Instant>,
    syntax_name: Option<String>,
    line_state_before: Vec<Option<LineState>>,
    highlights: Vec<Option<Vec<HlSpan>>>,
    folds: Vec<Fold>,
    foldable_at: Vec<Option<usize>>,
    foldable_dirty: bool,
}

impl Buffer {
    fn empty() -> Self {
        Self {
            text: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
            scroll_row: 0,
            scroll_col: 0,
            path: None,
            dirty: false,
            had_trailing_newline: true,
            selection_anchor: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            last_edit: None,
            known_mtime: None,
            external_change: false,
            last_disk_check: None,
            syntax_name: None,
            line_state_before: vec![None],
            highlights: vec![None],
            folds: Vec::new(),
            foldable_at: vec![None],
            foldable_dirty: true,
        }
    }

    fn load(&mut self, path: &Path) -> Result<()> {
        let text = fs::read_to_string(path).unwrap_or_default();
        let had_trailing_newline = text.ends_with('\n');
        let mut lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
        if had_trailing_newline {
            lines.pop();
        }
        if lines.is_empty() {
            lines.push(String::new());
        }
        let n = lines.len();
        self.text = lines;
        self.had_trailing_newline = had_trailing_newline;
        self.path = Some(path.to_path_buf());
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.scroll_row = 0;
        self.scroll_col = 0;
        self.dirty = false;
        self.selection_anchor = None;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.last_edit = None;
        self.known_mtime = read_mtime(path);
        self.external_change = false;
        self.last_disk_check = Some(Instant::now());
        self.syntax_name = None;
        self.line_state_before = vec![None; n];
        self.highlights = vec![None; n];
        self.folds = Vec::new();
        self.foldable_at = vec![None; n];
        self.foldable_dirty = true;
        Ok(())
    }

    fn seed_syntax(&mut self, hl: &Highlighter) {
        let Some(path) = &self.path else {
            return;
        };
        let syntax = hl.syntax_for_path(path);
        if let Some(syntax) = syntax {
            self.syntax_name = Some(syntax.name.clone());
            self.line_state_before[0] = Some(hl.initial_state(syntax));
        } else {
            self.syntax_name = None;
            self.line_state_before[0] = None;
        }
        for slot in self.highlights.iter_mut() {
            *slot = None;
        }
        for slot in self.line_state_before.iter_mut().skip(1) {
            *slot = None;
        }
    }

    fn invalidate_highlights_from(&mut self, row: usize) {
        for i in row..self.highlights.len() {
            self.highlights[i] = None;
        }
        for i in (row + 1)..self.line_state_before.len() {
            self.line_state_before[i] = None;
        }
    }

    fn ensure_foldable(&mut self, tab_width: usize) {
        if !self.foldable_dirty && self.foldable_at.len() == self.text.len() {
            return;
        }
        self.foldable_at = vec![None; self.text.len()];
        for row in 0..self.text.len() {
            self.foldable_at[row] = compute_fold_end(&self.text, row, tab_width);
        }
        self.foldable_dirty = false;
    }

    fn mark_foldable_dirty(&mut self) {
        self.foldable_dirty = true;
    }

    fn adjust_folds_for_edit(
        &mut self,
        at: usize,
        old_lines_removed: usize,
        new_lines_added: usize,
    ) {
        let delta = new_lines_added as isize - old_lines_removed as isize;
        let removed_end = at + old_lines_removed;
        self.folds.retain_mut(|f| {
            if f.end < at {
                return true;
            }
            if f.start >= removed_end {
                let ns = f.start as isize + delta;
                let ne = f.end as isize + delta;
                if ns < 0 || ne < 0 {
                    return false;
                }
                f.start = ns as usize;
                f.end = ne as usize;
                return true;
            }
            false
        });
        self.foldable_dirty = true;
    }

    fn is_hidden(&self, row: usize) -> bool {
        self.folds
            .iter()
            .any(|f| row > f.start && row <= f.end)
    }

    fn collapsed_fold_at(&self, row: usize) -> Option<Fold> {
        self.folds.iter().find(|f| f.start == row).copied()
    }

    fn next_visible_row(&self, from: usize) -> Option<usize> {
        let mut r = from + 1;
        while r < self.text.len() {
            if !self.is_hidden(r) {
                return Some(r);
            }
            r += 1;
        }
        None
    }

    fn prev_visible_row(&self, from: usize) -> Option<usize> {
        if from == 0 {
            return None;
        }
        let mut r = from - 1;
        loop {
            if !self.is_hidden(r) {
                return Some(r);
            }
            if r == 0 {
                return None;
            }
            r -= 1;
        }
    }

    /// Walk visible rows starting from `from` (inclusive) and return the
    /// `n`-th visible row. If we run out, return the last visible row.
    fn nth_visible_row_from(&self, from: usize, n: usize) -> usize {
        let mut count = 0usize;
        let mut r = from;
        let mut last = from.min(self.text.len().saturating_sub(1));
        while r < self.text.len() {
            if !self.is_hidden(r) {
                last = r;
                if count == n {
                    return r;
                }
                count += 1;
                r = match self.collapsed_fold_at(r) {
                    Some(f) => f.end + 1,
                    None => r + 1,
                };
            } else {
                r += 1;
            }
        }
        last
    }

    /// Count visible rows from `from` up to but not including `to`.
    /// Returns the visible distance (0 if `to == from` and `from` visible).
    fn visible_offset(&self, from: usize, to: usize) -> usize {
        if to <= from {
            return 0;
        }
        let mut count = 0usize;
        let mut r = from;
        while r < to && r < self.text.len() {
            if !self.is_hidden(r) {
                count += 1;
                r = match self.collapsed_fold_at(r) {
                    Some(f) => f.end + 1,
                    None => r + 1,
                };
            } else {
                r += 1;
            }
        }
        count
    }

    fn ensure_highlights(&mut self, up_to: usize, hl: &Highlighter) {
        if self.syntax_name.is_none() {
            return;
        }
        let cap = (up_to + 1).min(self.text.len());
        for i in 0..cap {
            if self.highlights[i].is_some() {
                continue;
            }
            let base = (0..=i)
                .rev()
                .find(|&j| self.line_state_before[j].is_some())
                .unwrap_or(0);
            let Some(seed) = self.line_state_before[base].clone() else {
                return;
            };
            let mut state = seed;
            for j in base..=i {
                let spans = hl.highlight_line(&self.text[j], &mut state);
                self.highlights[j] = Some(spans);
                if j + 1 < self.line_state_before.len() {
                    self.line_state_before[j + 1] = Some(state.clone());
                }
            }
        }
    }

    fn save(&mut self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let mut content = self.text.join("\n");
        if self.had_trailing_newline {
            content.push('\n');
        }
        fs::write(path, content)?;
        self.dirty = false;
        self.known_mtime = self.path.as_deref().and_then(read_mtime);
        self.external_change = false;
        self.last_disk_check = Some(Instant::now());
        Ok(())
    }

    fn save_as(&mut self, new_path: &Path) -> Result<()> {
        self.path = Some(new_path.to_path_buf());
        self.save()
    }

    fn check_disk(&mut self) {
        let Some(path) = &self.path else {
            return;
        };
        let should_check = self
            .last_disk_check
            .map(|t| t.elapsed() >= DISK_CHECK_INTERVAL)
            .unwrap_or(true);
        if !should_check {
            return;
        }
        self.last_disk_check = Some(Instant::now());
        let current = read_mtime(path);
        match (current, self.known_mtime) {
            (Some(a), Some(b)) if a != b => self.external_change = true,
            (Some(_), None) => self.external_change = true,
            _ => {}
        }
    }

    fn is_fresh_and_clean(&self) -> bool {
        self.path.is_none()
            && !self.dirty
            && self.text.len() == 1
            && self.text[0].is_empty()
            && self.undo_stack.is_empty()
    }

    fn goto_line(&mut self, n: usize) {
        let target = n.saturating_sub(1).min(self.text.len().saturating_sub(1));
        self.cursor_row = target;
        self.cursor_col = 0;
        self.selection_anchor = None;
        self.reset_coalesce();
    }

    fn current_line_len(&self) -> usize {
        self.text
            .get(self.cursor_row)
            .map(|l| l.chars().count())
            .unwrap_or(0)
    }

    fn update_selection(&mut self, extend: bool) {
        if extend {
            if self.selection_anchor.is_none() {
                self.selection_anchor = Some((self.cursor_row, self.cursor_col));
            }
        } else {
            self.selection_anchor = None;
        }
    }

    fn selection_range(&self) -> Option<(Pos, Pos)> {
        let anchor = self.selection_anchor?;
        let cursor = (self.cursor_row, self.cursor_col);
        if anchor == cursor {
            return None;
        }
        if anchor < cursor {
            Some((anchor, cursor))
        } else {
            Some((cursor, anchor))
        }
    }

    fn line_sel_range(&self, row: usize) -> Option<(usize, usize)> {
        let ((sr, sc), (er, ec)) = self.selection_range()?;
        if row < sr || row > er {
            return None;
        }
        let line_len = self.text[row].chars().count();
        let start = if row == sr { sc } else { 0 };
        let end = if row == er { ec } else { line_len };
        Some((start, end))
    }

    fn move_left(&mut self, extend: bool) {
        self.update_selection(extend);
        self.reset_coalesce();
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if let Some(r) = self.prev_visible_row(self.cursor_row) {
            self.cursor_row = r;
            self.cursor_col = self.current_line_len();
        }
    }

    fn move_right(&mut self, extend: bool) {
        self.update_selection(extend);
        self.reset_coalesce();
        let len = self.current_line_len();
        if self.cursor_col < len {
            self.cursor_col += 1;
        } else if let Some(r) = self.next_visible_row(self.cursor_row) {
            self.cursor_row = r;
            self.cursor_col = 0;
        }
    }

    fn move_up(&mut self, extend: bool) {
        self.update_selection(extend);
        self.reset_coalesce();
        if let Some(r) = self.prev_visible_row(self.cursor_row) {
            self.cursor_row = r;
            self.cursor_col = self.cursor_col.min(self.current_line_len());
        }
    }

    fn move_down(&mut self, extend: bool) {
        self.update_selection(extend);
        self.reset_coalesce();
        if let Some(r) = self.next_visible_row(self.cursor_row) {
            self.cursor_row = r;
            self.cursor_col = self.cursor_col.min(self.current_line_len());
        }
    }

    fn indent_selection(&mut self, tab_width: usize, use_tabs: bool) {
        let (first, last) = self.indent_row_range();
        let indent: String = if use_tabs {
            "\t".to_string()
        } else {
            " ".repeat(tab_width.max(1))
        };
        let add = indent.chars().count();
        self.checkpoint(EditKind::Other);
        for row in first..=last {
            self.text[row].insert_str(0, &indent);
        }
        if self.cursor_row >= first && self.cursor_row <= last {
            self.cursor_col += add;
        }
        if let Some((ar, ac)) = self.selection_anchor.as_mut() {
            if *ar >= first && *ar <= last {
                *ac += add;
            }
        }
        self.invalidate_highlights_from(first);
        self.mark_foldable_dirty();
        self.dirty = true;
    }

    fn outdent_selection(&mut self, tab_width: usize) {
        let (first, last) = self.indent_row_range();
        let max_remove = tab_width.max(1);
        self.checkpoint(EditKind::Other);
        let mut per_row_removed: Vec<usize> = Vec::with_capacity(last - first + 1);
        for row in first..=last {
            let line = &mut self.text[row];
            let removed = if line.starts_with('\t') {
                line.drain(0..1);
                1
            } else {
                let spaces = line
                    .chars()
                    .take(max_remove)
                    .take_while(|c| *c == ' ')
                    .count();
                if spaces > 0 {
                    line.drain(0..spaces);
                }
                spaces
            };
            per_row_removed.push(removed);
        }
        if self.cursor_row >= first && self.cursor_row <= last {
            let idx = self.cursor_row - first;
            self.cursor_col = self.cursor_col.saturating_sub(per_row_removed[idx]);
        }
        if let Some((ar, ac)) = self.selection_anchor.as_mut() {
            if *ar >= first && *ar <= last {
                let idx = *ar - first;
                *ac = ac.saturating_sub(per_row_removed[idx]);
            }
        }
        self.invalidate_highlights_from(first);
        self.mark_foldable_dirty();
        if per_row_removed.iter().any(|&n| n > 0) {
            self.dirty = true;
        }
    }

    fn indent_row_range(&self) -> (usize, usize) {
        match self.selection_range() {
            Some(((sr, _), (er, ec))) => {
                let last = if er > sr && ec == 0 { er - 1 } else { er };
                (sr, last)
            }
            None => (self.cursor_row, self.cursor_row),
        }
    }

    fn select_word_at(&mut self, row: usize, col: usize) -> bool {
        let Some(line) = self.text.get(row) else {
            return false;
        };
        let chars: Vec<char> = line.chars().collect();
        let clamped = col.min(chars.len());
        if clamped >= chars.len() || !is_word_char(chars[clamped]) {
            return false;
        }
        let mut start = clamped;
        let mut end = clamped;
        while start > 0 && is_word_char(chars[start - 1]) {
            start -= 1;
        }
        while end < chars.len() && is_word_char(chars[end]) {
            end += 1;
        }
        self.cursor_row = row;
        self.cursor_col = end;
        self.selection_anchor = Some((row, start));
        self.reset_coalesce();
        true
    }

    fn move_word_left(&mut self, extend: bool) {
        self.update_selection(extend);
        self.reset_coalesce();
        if self.cursor_col == 0 {
            if let Some(r) = self.prev_visible_row(self.cursor_row) {
                self.cursor_row = r;
                self.cursor_col = self.current_line_len();
            }
            return;
        }
        let chars: Vec<char> = self.text[self.cursor_row].chars().collect();
        let mut i = self.cursor_col;
        while i > 0 && !is_word_char(chars[i - 1]) {
            i -= 1;
        }
        while i > 0 && is_word_char(chars[i - 1]) {
            i -= 1;
        }
        self.cursor_col = i;
    }

    fn move_word_right(&mut self, extend: bool) {
        self.update_selection(extend);
        self.reset_coalesce();
        let line_len = self.current_line_len();
        if self.cursor_col >= line_len {
            if let Some(r) = self.next_visible_row(self.cursor_row) {
                self.cursor_row = r;
                self.cursor_col = 0;
            }
            return;
        }
        let chars: Vec<char> = self.text[self.cursor_row].chars().collect();
        let mut i = self.cursor_col;
        while i < chars.len() && is_word_char(chars[i]) {
            i += 1;
        }
        while i < chars.len() && !is_word_char(chars[i]) {
            i += 1;
        }
        self.cursor_col = i;
    }

    fn move_home(&mut self, extend: bool) {
        self.update_selection(extend);
        self.reset_coalesce();
        self.cursor_col = 0;
    }

    fn move_end(&mut self, extend: bool) {
        self.update_selection(extend);
        self.reset_coalesce();
        self.cursor_col = self.current_line_len();
    }

    fn insert_char(&mut self, c: char) {
        if self.selection_anchor.is_some() {
            self.checkpoint(EditKind::Other);
            if let Some(((sr, sc), (er, ec))) = self.selection_range() {
                self.delete_range(sr, sc, er, ec);
            }
            self.selection_anchor = None;
        } else {
            self.checkpoint(EditKind::InsertChar);
        }
        let line = &mut self.text[self.cursor_row];
        let byte_idx = char_idx_to_byte(line, self.cursor_col);
        line.insert(byte_idx, c);
        self.cursor_col += 1;
        self.dirty = true;
        self.invalidate_highlights_from(self.cursor_row);
        self.mark_foldable_dirty();
    }

    fn insert_newline(&mut self) {
        self.checkpoint(EditKind::Other);
        if let Some(((sr, sc), (er, ec))) = self.selection_range() {
            self.delete_range(sr, sc, er, ec);
            self.selection_anchor = None;
        }
        let line = &mut self.text[self.cursor_row];
        let byte_idx = char_idx_to_byte(line, self.cursor_col);
        let tail = line.split_off(byte_idx);
        let insert_at = self.cursor_row + 1;
        self.text.insert(insert_at, tail);
        self.highlights.insert(insert_at, None);
        self.line_state_before.insert(insert_at, None);
        self.adjust_folds_for_edit(insert_at, 0, 1);
        self.cursor_row += 1;
        self.cursor_col = 0;
        self.dirty = true;
        self.invalidate_highlights_from(self.cursor_row - 1);
    }

    fn backspace(&mut self) {
        if let Some(((sr, sc), (er, ec))) = self.selection_range() {
            self.checkpoint(EditKind::Other);
            self.delete_range(sr, sc, er, ec);
            self.selection_anchor = None;
            self.dirty = true;
            return;
        }
        self.checkpoint(EditKind::Other);
        if self.cursor_col > 0 {
            let line = &mut self.text[self.cursor_row];
            let byte_idx = char_idx_to_byte(line, self.cursor_col - 1);
            line.remove(byte_idx);
            self.cursor_col -= 1;
            self.dirty = true;
            self.invalidate_highlights_from(self.cursor_row);
            self.mark_foldable_dirty();
        } else if self.cursor_row > 0 {
            let row = self.cursor_row;
            let line = self.text.remove(row);
            self.highlights.remove(row);
            self.line_state_before.remove(row);
            self.adjust_folds_for_edit(row, 1, 0);
            self.cursor_row -= 1;
            self.cursor_col = self.current_line_len();
            self.text[self.cursor_row].push_str(&line);
            self.dirty = true;
            self.invalidate_highlights_from(self.cursor_row);
        }
    }

    fn delete_range(&mut self, sr: usize, sc: usize, er: usize, ec: usize) {
        if sr == er {
            let line = &mut self.text[sr];
            let start_byte = char_idx_to_byte(line, sc);
            let end_byte = char_idx_to_byte(line, ec);
            line.drain(start_byte..end_byte);
            self.mark_foldable_dirty();
        } else {
            let suffix = {
                let last = &self.text[er];
                let byte_idx = char_idx_to_byte(last, ec);
                last[byte_idx..].to_string()
            };
            {
                let first = &mut self.text[sr];
                let byte_idx = char_idx_to_byte(first, sc);
                first.truncate(byte_idx);
                first.push_str(&suffix);
            }
            self.text.drain((sr + 1)..=er);
            self.highlights.drain((sr + 1)..=er);
            self.line_state_before.drain((sr + 1)..=er);
            self.adjust_folds_for_edit(sr + 1, er - sr, 0);
        }
        self.cursor_row = sr;
        self.cursor_col = sc;
        self.invalidate_highlights_from(sr);
    }

    fn insert_text(&mut self, text: &str) {
        let parts: Vec<&str> = text.split('\n').collect();
        let start_row = self.cursor_row;
        if parts.len() == 1 {
            let line = &mut self.text[self.cursor_row];
            let byte_idx = char_idx_to_byte(line, self.cursor_col);
            line.insert_str(byte_idx, parts[0]);
            self.cursor_col += parts[0].chars().count();
            self.mark_foldable_dirty();
        } else {
            let first = parts[0];
            let last = parts[parts.len() - 1];
            let middle = &parts[1..parts.len() - 1];

            let tail = {
                let line = &mut self.text[self.cursor_row];
                let byte_idx = char_idx_to_byte(line, self.cursor_col);
                let t = line.split_off(byte_idx);
                line.push_str(first);
                t
            };

            let mut new_lines: Vec<String> = middle.iter().map(|s| (*s).to_string()).collect();
            new_lines.push(format!("{last}{tail}"));

            let added = new_lines.len();
            let insert_at = self.cursor_row + 1;
            for (i, l) in new_lines.into_iter().enumerate() {
                self.text.insert(insert_at + i, l);
                self.highlights.insert(insert_at + i, None);
                self.line_state_before.insert(insert_at + i, None);
            }
            self.adjust_folds_for_edit(insert_at, 0, added);
            self.cursor_row += parts.len() - 1;
            self.cursor_col = last.chars().count();
        }
        self.invalidate_highlights_from(start_row);
    }

    fn checkpoint(&mut self, kind: EditKind) {
        let can_merge = matches!(
            (self.last_edit, kind),
            (Some(EditKind::InsertChar), EditKind::InsertChar)
        );
        if !can_merge {
            self.undo_stack.push(self.snapshot());
            self.redo_stack.clear();
            if self.undo_stack.len() > MAX_UNDO {
                self.undo_stack.remove(0);
            }
        }
        self.last_edit = Some(kind);
    }

    fn reset_coalesce(&mut self) {
        self.last_edit = None;
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.text.clone(),
            cursor_row: self.cursor_row,
            cursor_col: self.cursor_col,
            dirty: self.dirty,
            had_trailing_newline: self.had_trailing_newline,
            folds: self.folds.clone(),
        }
    }

    fn restore(&mut self, snap: Snapshot) {
        self.text = snap.text;
        self.cursor_row = snap.cursor_row;
        self.cursor_col = snap.cursor_col;
        self.dirty = snap.dirty;
        self.had_trailing_newline = snap.had_trailing_newline;
        self.selection_anchor = None;
        let n = self.text.len();
        self.highlights = vec![None; n];
        let seed = self.line_state_before.first().cloned().flatten();
        self.line_state_before = vec![None; n];
        if n > 0 {
            self.line_state_before[0] = seed;
        }
        self.folds = snap
            .folds
            .into_iter()
            .filter(|f| f.start < n && f.end < n && f.end >= f.start)
            .collect();
        self.foldable_at = vec![None; n];
        self.foldable_dirty = true;
    }

    fn undo(&mut self) -> bool {
        let Some(snap) = self.undo_stack.pop() else {
            return false;
        };
        let current = self.snapshot();
        self.restore(snap);
        self.redo_stack.push(current);
        self.last_edit = None;
        true
    }

    fn redo(&mut self) -> bool {
        let Some(snap) = self.redo_stack.pop() else {
            return false;
        };
        let current = self.snapshot();
        self.restore(snap);
        self.undo_stack.push(current);
        self.last_edit = None;
        true
    }

    fn adjust_scroll(&mut self, viewport_height: usize, viewport_width: usize, tab_width: usize) {
        if viewport_height > 0 {
            // If cursor landed inside a collapsed fold, hoist it to the header.
            if self.is_hidden(self.cursor_row) {
                if let Some(fold) = self
                    .folds
                    .iter()
                    .find(|f| self.cursor_row > f.start && self.cursor_row <= f.end)
                {
                    self.cursor_row = fold.start;
                }
            }
            if self.is_hidden(self.scroll_row) {
                self.scroll_row = self
                    .folds
                    .iter()
                    .find(|f| self.scroll_row > f.start && self.scroll_row <= f.end)
                    .map(|f| f.end + 1)
                    .unwrap_or(self.scroll_row)
                    .min(self.text.len().saturating_sub(1));
            }
            if self.cursor_row < self.scroll_row {
                self.scroll_row = self.cursor_row;
            } else {
                // Walk visible rows from scroll_row; if cursor is beyond
                // the viewport, advance scroll_row.
                let mut visible = 0usize;
                let mut found = false;
                let mut r = self.scroll_row;
                while r < self.text.len() && visible < viewport_height {
                    if !self.is_hidden(r) {
                        if r == self.cursor_row {
                            found = true;
                            break;
                        }
                        visible += 1;
                        r = match self.collapsed_fold_at(r) {
                            Some(f) => f.end + 1,
                            None => r + 1,
                        };
                    } else {
                        r += 1;
                    }
                }
                if !found {
                    // Advance scroll until cursor is the last visible row.
                    let mut visible_rows: Vec<usize> = Vec::new();
                    let mut rr = 0usize;
                    while rr < self.text.len() {
                        if !self.is_hidden(rr) {
                            visible_rows.push(rr);
                            rr = match self.collapsed_fold_at(rr) {
                                Some(f) => f.end + 1,
                                None => rr + 1,
                            };
                        } else {
                            rr += 1;
                        }
                    }
                    if let Some(idx) = visible_rows.iter().position(|&x| x == self.cursor_row) {
                        let start_idx = idx + 1 - viewport_height.min(idx + 1);
                        self.scroll_row = visible_rows[start_idx];
                    }
                }
            }
        }
        if viewport_width > 0 {
            let line = self
                .text
                .get(self.cursor_row)
                .map(String::as_str)
                .unwrap_or("");
            let vis = char_idx_to_vis_col(line, self.cursor_col, tab_width);
            if vis < self.scroll_col {
                self.scroll_col = vis;
            } else if vis >= self.scroll_col + viewport_width {
                self.scroll_col = vis + 1 - viewport_width;
            }
        }
    }

    fn display_name(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "[no file]".to_string())
    }
}

// ---------------------------------------------------------------------------
// Editor: shared state + buffer collection
// ---------------------------------------------------------------------------

struct LayoutRects {
    tab_area: Rect,
    text_area: Rect,
    gutter: Rect,
}

pub struct Editor {
    buffers: Vec<Buffer>,
    active: usize,
    status: String,
    status_shown_at: Option<Instant>,
    mode: Mode,
    clipboard: Option<Clipboard>,
    last_search: Option<String>,
    quit_pending: bool,
    close_pending: bool,
    pub should_quit: bool,
    config: Config,
    highlighter: Option<Highlighter>,
    layout: Option<LayoutRects>,
    watcher: Option<RecommendedWatcher>,
    fs_rx: mpsc::Receiver<PathBuf>,
    watched_paths: HashSet<PathBuf>,
    last_click: Option<LastClick>,
    tabs_scroll: usize,
}

impl Editor {
    pub fn new(config: Config) -> Self {
        let highlighter = if config.syntax_highlighting {
            Some(Highlighter::new())
        } else {
            None
        };
        let (fs_tx, fs_rx) = mpsc::channel::<PathBuf>();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                if matches!(
                    event.kind,
                    notify::EventKind::Modify(_)
                        | notify::EventKind::Create(_)
                        | notify::EventKind::Remove(_)
                ) {
                    for p in event.paths {
                        let _ = fs_tx.send(p);
                    }
                }
            }
        })
        .ok();
        Self {
            buffers: vec![Buffer::empty()],
            active: 0,
            status: String::new(),
            status_shown_at: None,
            mode: Mode::Normal,
            clipboard: Clipboard::new().ok(),
            last_search: None,
            quit_pending: false,
            close_pending: false,
            should_quit: false,
            config,
            highlighter,
            layout: None,
            watcher,
            fs_rx,
            watched_paths: HashSet::new(),
            last_click: None,
            tabs_scroll: 0,
        }
    }

    pub fn load(&mut self, path: &Path) -> Result<()> {
        self.active_mut().load(path)?;
        if let Some(hl) = self.highlighter.as_ref() {
            let idx = self.active;
            self.buffers[idx].seed_syntax(hl);
        }
        self.watch(path);
        self.set_status(format!("opened {}", path.display()));
        Ok(())
    }

    pub fn try_load_remote(&mut self, path: &Path) -> Result<()> {
        if self.active().is_fresh_and_clean() {
            self.active_mut().load(path)?;
        } else {
            let mut b = Buffer::empty();
            b.load(path)?;
            self.buffers.push(b);
            self.active = self.buffers.len() - 1;
        }
        if let Some(hl) = self.highlighter.as_ref() {
            let idx = self.active;
            self.buffers[idx].seed_syntax(hl);
        }
        self.watch(path);
        self.set_status(format!("opened {}", path.display()));
        Ok(())
    }

    pub fn capture_session(&self) -> crate::session::Session {
        let buffers: Vec<_> = self
            .buffers
            .iter()
            .filter_map(|b| {
                b.path.as_ref().map(|p| crate::session::SessionBuffer {
                    path: p.clone(),
                    cursor_row: b.cursor_row,
                    cursor_col: b.cursor_col,
                    scroll_row: b.scroll_row,
                    scroll_col: b.scroll_col,
                    folds: b.folds.iter().map(|f| (f.start, f.end)).collect(),
                })
            })
            .collect();

        // Map active index past any untitled buffers we filtered out.
        let mut active = 0usize;
        let mut seen = 0usize;
        for (i, b) in self.buffers.iter().enumerate() {
            if b.path.is_none() {
                continue;
            }
            if i == self.active {
                active = seen;
                break;
            }
            seen += 1;
        }

        crate::session::Session { active, buffers }
    }

    pub fn restore_session(&mut self, session: crate::session::Session) {
        let mut loaded: Vec<Buffer> = Vec::new();
        for sb in &session.buffers {
            if !sb.path.exists() {
                continue;
            }
            let mut b = Buffer::empty();
            if b.load(&sb.path).is_err() {
                continue;
            }
            let max_row = b.text.len().saturating_sub(1);
            b.cursor_row = sb.cursor_row.min(max_row);
            let line_len = b
                .text
                .get(b.cursor_row)
                .map(|l| l.chars().count())
                .unwrap_or(0);
            b.cursor_col = sb.cursor_col.min(line_len);
            b.scroll_row = sb.scroll_row.min(max_row);
            b.scroll_col = sb.scroll_col;
            let n = b.text.len();
            b.folds = sb
                .folds
                .iter()
                .filter_map(|&(s, e)| {
                    if s < n && e < n && e >= s {
                        Some(Fold { start: s, end: e })
                    } else {
                        None
                    }
                })
                .collect();
            b.folds.sort_by_key(|f| f.start);
            b.foldable_dirty = true;
            if let Some(hl) = self.highlighter.as_ref() {
                b.seed_syntax(hl);
            }
            loaded.push(b);
        }
        if loaded.is_empty() {
            return;
        }
        let count = loaded.len();
        self.buffers = loaded;
        self.active = session.active.min(count - 1);
        let paths: Vec<PathBuf> = self.buffers.iter().filter_map(|b| b.path.clone()).collect();
        for p in paths {
            self.watch(&p);
        }
        self.set_status(format!("restored {count} buffers"));
    }

    fn watch(&mut self, path: &Path) {
        let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if self.watched_paths.contains(&canonical) {
            return;
        }
        if let Some(w) = self.watcher.as_mut() {
            if w.watch(&canonical, RecursiveMode::NonRecursive).is_ok() {
                self.watched_paths.insert(canonical);
            }
        }
    }

    fn drain_fs_events(&mut self) {
        let mut paths: HashSet<PathBuf> = HashSet::new();
        while let Ok(p) = self.fs_rx.try_recv() {
            paths.insert(p);
        }
        for path in paths {
            self.reload_if_changed(&path);
        }
    }

    fn reload_if_changed(&mut self, path: &Path) {
        let new_mtime = read_mtime(path);
        if new_mtime.is_none() {
            return;
        }
        let indices: Vec<usize> = self
            .buffers
            .iter()
            .enumerate()
            .filter(|(_, b)| {
                b.path
                    .as_deref()
                    .map(|p| paths_equal(p, path))
                    .unwrap_or(false)
            })
            .map(|(i, _)| i)
            .collect();
        let mut reloaded_any = false;
        for idx in indices {
            let buf = &mut self.buffers[idx];
            if new_mtime == buf.known_mtime {
                continue;
            }
            let saved_row = buf.cursor_row;
            let saved_col = buf.cursor_col;
            let saved_scroll_row = buf.scroll_row;
            let saved_scroll_col = buf.scroll_col;
            if buf.load(path).is_ok() {
                let max_row = buf.text.len().saturating_sub(1);
                buf.cursor_row = saved_row.min(max_row);
                let line_len = buf
                    .text
                    .get(buf.cursor_row)
                    .map(|l| l.chars().count())
                    .unwrap_or(0);
                buf.cursor_col = saved_col.min(line_len);
                buf.scroll_row = saved_scroll_row.min(max_row);
                buf.scroll_col = saved_scroll_col;
                reloaded_any = true;
            }
        }
        if reloaded_any {
            if let Some(hl) = self.highlighter.as_ref() {
                for idx in 0..self.buffers.len() {
                    let same = self.buffers[idx]
                        .path
                        .as_deref()
                        .map(|p| paths_equal(p, path))
                        .unwrap_or(false);
                    if same {
                        self.buffers[idx].seed_syntax(hl);
                    }
                }
            }
            self.set_status(format!("reloaded {}", path.display()));
        }
    }

    pub fn goto_line(&mut self, n: usize) {
        self.active_mut().goto_line(n);
    }

    pub fn set_status(&mut self, s: impl Into<String>) {
        self.status = s.into();
        self.status_shown_at = Some(Instant::now());
    }

    fn active(&self) -> &Buffer {
        &self.buffers[self.active]
    }

    fn active_mut(&mut self) -> &mut Buffer {
        &mut self.buffers[self.active]
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        match self.mode {
            Mode::Normal => self.handle_key_normal(key),
            Mode::Prompt(_) => self.handle_key_prompt(key),
        }
    }

    fn handle_key_normal(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::SUPER);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        if !(ctrl && matches!(key.code, KeyCode::Char('q'))) {
            self.quit_pending = false;
        }
        if !(ctrl && matches!(key.code, KeyCode::Char('w'))) {
            self.close_pending = false;
        }

        if alt {
            match key.code {
                KeyCode::Char(c) => {
                    if let Some(d) = c.to_digit(10) {
                        if (1..=9).contains(&d) {
                            self.switch_to((d - 1) as usize);
                            return;
                        }
                    }
                    match c.to_ascii_lowercase() {
                        's' => {
                            self.open_prompt(PromptKind::SaveAs);
                            return;
                        }
                        'b' => {
                            self.active_mut().move_word_left(shift);
                            return;
                        }
                        'f' => {
                            self.active_mut().move_word_right(shift);
                            return;
                        }
                        _ => {}
                    }
                }
                KeyCode::Left => {
                    self.active_mut().move_word_left(shift);
                    return;
                }
                KeyCode::Right => {
                    self.active_mut().move_word_right(shift);
                    return;
                }
                _ => {}
            }
        }

        match (ctrl, key.code) {
            (true, KeyCode::Char('q')) => {
                let any_dirty = self.buffers.iter().any(|b| b.dirty);
                if any_dirty && !self.quit_pending {
                    self.quit_pending = true;
                    self.set_status("unsaved changes — Ctrl-Q again to quit");
                } else {
                    self.should_quit = true;
                }
            }
            (true, KeyCode::Char('w')) => self.close_active(),
            (true, KeyCode::Tab) => self.next_buffer(),
            (_, KeyCode::BackTab) => self.prev_buffer(),
            (true, KeyCode::Char('s')) if shift => {
                self.open_prompt(PromptKind::SaveAs);
            }
            (true, KeyCode::Char('s')) => {
                if self.active().path.is_none() {
                    self.open_prompt(PromptKind::SaveAs);
                } else {
                    match self.active_mut().save() {
                        Ok(()) => {
                            let name = self.active().display_name();
                            self.set_status(format!("saved {name}"));
                        }
                        Err(e) => self.set_status(format!("save failed: {e}")),
                    }
                }
            }
            (true, KeyCode::Char('z')) if shift => {
                if !self.active_mut().redo() {
                    self.set_status("nothing to redo");
                }
            }
            (true, KeyCode::Char('z')) => {
                if !self.active_mut().undo() {
                    self.set_status("nothing to undo");
                }
            }
            (true, KeyCode::Char('y')) => {
                if !self.active_mut().redo() {
                    self.set_status("nothing to redo");
                }
            }
            (true, KeyCode::Char('c')) => self.copy_selection(),
            (true, KeyCode::Char('x')) => self.cut_selection(),
            (true, KeyCode::Char('v')) => self.paste(),
            (true, KeyCode::Char('f')) => self.open_prompt(PromptKind::Search),
            (true, KeyCode::Char('g')) => self.open_prompt(PromptKind::Goto),
            (true, KeyCode::Char('[')) if alt && shift => self.fold_all(),
            (true, KeyCode::Char(']')) if alt && shift => self.unfold_all(),
            (true, KeyCode::Char('[')) if alt => self.fold_at_cursor(),
            (true, KeyCode::Char(']')) if alt => self.unfold_at_cursor(),
            (true, KeyCode::Char(']')) => {
                let tw = self.config.tab_width;
                let tabs = self.config.indent_with_tabs;
                self.active_mut().indent_selection(tw, tabs);
            }
            (true, KeyCode::Char('[')) => {
                let tw = self.config.tab_width;
                self.active_mut().outdent_selection(tw);
            }
            (_, KeyCode::Esc) => {
                self.active_mut().selection_anchor = None;
                self.active_mut().reset_coalesce();
            }
            (_, KeyCode::Left) => self.active_mut().move_left(shift),
            (_, KeyCode::Right) => self.active_mut().move_right(shift),
            (_, KeyCode::Up) => self.active_mut().move_up(shift),
            (_, KeyCode::Down) => self.active_mut().move_down(shift),
            (_, KeyCode::Home) => self.active_mut().move_home(shift),
            (_, KeyCode::End) => self.active_mut().move_end(shift),
            (_, KeyCode::Enter) => self.active_mut().insert_newline(),
            (_, KeyCode::Backspace) => self.active_mut().backspace(),
            (_, KeyCode::Tab) => {
                if self.config.indent_with_tabs {
                    self.active_mut().insert_char('\t');
                } else {
                    for _ in 0..self.config.tab_width {
                        self.active_mut().insert_char(' ');
                    }
                }
            }
            (_, KeyCode::Char(c)) if !ctrl => self.active_mut().insert_char(c),
            _ => {}
        }
    }

    fn handle_key_prompt(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::SUPER);
        match (ctrl, key.code) {
            (_, KeyCode::Esc) => self.mode = Mode::Normal,
            (_, KeyCode::Enter) => self.submit_prompt(),
            (_, KeyCode::Backspace) => {
                if let Mode::Prompt(state) = &mut self.mode {
                    state.input.pop();
                }
            }
            (false, KeyCode::Char(c)) => {
                if let Mode::Prompt(state) = &mut self.mode {
                    state.input.push(c);
                }
            }
            _ => {}
        }
    }

    pub fn handle_mouse(&mut self, ev: MouseEvent) {
        if !matches!(self.mode, Mode::Normal) {
            return;
        }
        let Some(layout) = self.layout.as_ref() else {
            return;
        };
        let tab_area = layout.tab_area;
        let text_area = layout.text_area;
        let tab_width = self.config.tab_width;

        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if point_in(ev.column, ev.row, tab_area) {
                    if let Some(idx) = self.tab_at_row(ev.row, tab_area) {
                        self.switch_to(idx);
                    }
                } else if layout.gutter.width > 0 && point_in(ev.column, ev.row, layout.gutter) {
                    // Click in the gutter. Chevron sits in the second-to-last
                    // column (there's a trailing space for breathing room).
                    let gutter = layout.gutter;
                    let chevron_x = gutter.x + gutter.width - 2;
                    if ev.column == chevron_x {
                        let screen_row = ev.row.saturating_sub(gutter.y) as usize;
                        let b = self.active();
                        let doc_row = b.nth_visible_row_from(b.scroll_row, screen_row);
                        self.toggle_fold_at_row(doc_row);
                    }
                } else if point_in(ev.column, ev.row, text_area) {
                    let (row, col) = self.screen_to_doc(ev.column, ev.row, text_area, tab_width);
                    let now = Instant::now();
                    let is_double = self.last_click.is_some_and(|lc| {
                        lc.row == row
                            && lc.col == col
                            && now.duration_since(lc.at) < DOUBLE_CLICK_WINDOW
                    });
                    self.last_click = Some(LastClick { row, col, at: now });
                    let selected_word = is_double && self.active_mut().select_word_at(row, col);
                    if !selected_word {
                        let b = self.active_mut();
                        b.cursor_row = row;
                        b.cursor_col = col;
                        b.selection_anchor = None;
                        b.reset_coalesce();
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if point_in(ev.column, ev.row, text_area) {
                    let (row, col) = self.screen_to_doc(ev.column, ev.row, text_area, tab_width);
                    let b = self.active_mut();
                    if b.selection_anchor.is_none() {
                        b.selection_anchor = Some((b.cursor_row, b.cursor_col));
                    }
                    b.cursor_row = row;
                    b.cursor_col = col;
                    b.reset_coalesce();
                }
            }
            MouseEventKind::ScrollUp if point_in(ev.column, ev.row, tab_area) => {
                self.tabs_scroll = self.tabs_scroll.saturating_sub(1);
            }
            MouseEventKind::ScrollDown if point_in(ev.column, ev.row, tab_area) => {
                let h = tab_area.height as usize;
                let max = self.buffers.len().saturating_sub(h);
                if self.tabs_scroll < max {
                    self.tabs_scroll += 1;
                }
            }
            MouseEventKind::ScrollUp => {
                let b = self.active_mut();
                if let Some(r) = b.prev_visible_row(b.cursor_row) {
                    b.cursor_row = r;
                    b.cursor_col = b.cursor_col.min(b.current_line_len());
                }
                b.reset_coalesce();
            }
            MouseEventKind::ScrollDown => {
                let b = self.active_mut();
                if let Some(r) = b.next_visible_row(b.cursor_row) {
                    b.cursor_row = r;
                    b.cursor_col = b.cursor_col.min(b.current_line_len());
                }
                b.reset_coalesce();
            }
            _ => {}
        }
    }

    fn screen_to_doc(&self, sx: u16, sy: u16, text_area: Rect, tab_width: usize) -> (usize, usize) {
        let b = self.active();
        let screen_row = sy.saturating_sub(text_area.y) as usize;
        let doc_row = b.nth_visible_row_from(b.scroll_row, screen_row);
        let line = b.text.get(doc_row).map(String::as_str).unwrap_or("");
        let target_vis = b.scroll_col + sx.saturating_sub(text_area.x) as usize;
        let doc_col = vis_col_to_char_idx(line, target_vis, tab_width);
        (doc_row, doc_col)
    }

    fn tab_at_row(&self, row: u16, tab_area: Rect) -> Option<usize> {
        if row < tab_area.y || row >= tab_area.y + tab_area.height {
            return None;
        }
        let screen_row = (row - tab_area.y) as usize;
        let idx = self.tabs_scroll + screen_row;
        (idx < self.buffers.len()).then_some(idx)
    }

    fn switch_to(&mut self, idx: usize) {
        if idx < self.buffers.len() {
            self.active = idx;
        }
    }

    fn next_buffer(&mut self) {
        if self.buffers.len() > 1 {
            self.active = (self.active + 1) % self.buffers.len();
        }
    }

    fn prev_buffer(&mut self) {
        if self.buffers.len() > 1 {
            self.active = (self.active + self.buffers.len() - 1) % self.buffers.len();
        }
    }

    fn close_active(&mut self) {
        if self.active().dirty && !self.close_pending {
            self.close_pending = true;
            self.set_status("unsaved changes — Ctrl-W again to close");
            return;
        }
        self.close_pending = false;
        self.buffers.remove(self.active);
        if self.buffers.is_empty() {
            self.should_quit = true;
            return;
        }
        if self.active >= self.buffers.len() {
            self.active = self.buffers.len() - 1;
        }
    }

    fn open_prompt(&mut self, kind: PromptKind) {
        self.active_mut().reset_coalesce();
        let input = match kind {
            PromptKind::Search => self.last_search.clone().unwrap_or_default(),
            PromptKind::Goto => String::new(),
            PromptKind::SaveAs => self
                .active()
                .path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        };
        self.mode = Mode::Prompt(PromptState { kind, input });
    }

    fn submit_prompt(&mut self) {
        let Mode::Prompt(state) = std::mem::replace(&mut self.mode, Mode::Normal) else {
            return;
        };
        match state.kind {
            PromptKind::Search => {
                if !state.input.is_empty() {
                    self.last_search = Some(state.input.clone());
                    self.search_next(&state.input);
                }
            }
            PromptKind::Goto => match state.input.trim().parse::<usize>() {
                Ok(n) if n > 0 => self.goto_line(n),
                _ => self.set_status("invalid line number"),
            },
            PromptKind::SaveAs => {
                let raw = state.input.trim();
                if raw.is_empty() {
                    self.set_status("save as: no path");
                    return;
                }
                let path = expand_path(raw);
                match self.active_mut().save_as(&path) {
                    Ok(()) => {
                        let name = self.active().display_name();
                        self.set_status(format!("saved {name}"));
                    }
                    Err(e) => self.set_status(format!("save failed: {e}")),
                }
            }
        }
    }

    fn search_next(&mut self, needle: &str) {
        let needle_lc = needle.to_lowercase();
        let start = {
            let b = self.active();
            (b.cursor_row, b.cursor_col + 1)
        };
        let hit = find_from(&self.active().text, start, &needle_lc);
        let hit = hit.or_else(|| find_from(&self.active().text, (0, 0), &needle_lc));
        match hit {
            Some((r, c)) => {
                let wrapped = {
                    let b = self.active();
                    (r, c) < start && (r, c) != (b.cursor_row, b.cursor_col)
                };
                let b = self.active_mut();
                b.cursor_row = r;
                b.cursor_col = c;
                b.selection_anchor = None;
                b.reset_coalesce();
                if wrapped {
                    self.set_status("wrapped");
                }
            }
            None => self.set_status(format!("not found: {needle}")),
        }
    }

    fn fold_at_cursor(&mut self) {
        let tw = self.config.tab_width;
        let b = self.active_mut();
        b.ensure_foldable(tw);
        let row = b.cursor_row;
        if b.collapsed_fold_at(row).is_some() {
            return;
        }
        let Some(end) = b.foldable_at.get(row).copied().flatten() else {
            return;
        };
        b.folds.push(Fold { start: row, end });
        b.folds.sort_by_key(|f| f.start);
    }

    fn unfold_at_cursor(&mut self) {
        let b = self.active_mut();
        let row = b.cursor_row;
        b.folds.retain(|f| f.start != row);
    }

    fn fold_all(&mut self) {
        let tw = self.config.tab_width;
        let b = self.active_mut();
        b.ensure_foldable(tw);
        let existing_starts: std::collections::HashSet<usize> =
            b.folds.iter().map(|f| f.start).collect();
        for row in 0..b.text.len() {
            if existing_starts.contains(&row) {
                continue;
            }
            if let Some(end) = b.foldable_at.get(row).copied().flatten() {
                b.folds.push(Fold { start: row, end });
            }
        }
        b.folds.sort_by_key(|f| f.start);
        // If cursor got hidden, hoist to enclosing fold's header.
        if b.is_hidden(b.cursor_row) {
            if let Some(fold) = b
                .folds
                .iter()
                .find(|f| b.cursor_row > f.start && b.cursor_row <= f.end)
            {
                b.cursor_row = fold.start;
                b.cursor_col = b.cursor_col.min(b.current_line_len());
            }
        }
    }

    fn unfold_all(&mut self) {
        self.active_mut().folds.clear();
    }

    fn toggle_fold_at_row(&mut self, row: usize) {
        let tw = self.config.tab_width;
        let b = self.active_mut();
        b.ensure_foldable(tw);
        if b.collapsed_fold_at(row).is_some() {
            b.folds.retain(|f| f.start != row);
            return;
        }
        if let Some(end) = b.foldable_at.get(row).copied().flatten() {
            b.folds.push(Fold { start: row, end });
            b.folds.sort_by_key(|f| f.start);
        }
    }

    fn copy_selection(&mut self) {
        let sel = self.active().selection_range();
        let Some(((sr, sc), (er, ec))) = sel else {
            self.set_status("no selection");
            return;
        };
        let text = extract_range(&self.active().text, sr, sc, er, ec);
        if let Err(msg) = self.write_clipboard(&text) {
            self.set_status(msg);
        }
    }

    fn cut_selection(&mut self) {
        let sel = self.active().selection_range();
        let Some(((sr, sc), (er, ec))) = sel else {
            self.set_status("no selection");
            return;
        };
        let text = extract_range(&self.active().text, sr, sc, er, ec);
        if let Err(msg) = self.write_clipboard(&text) {
            self.set_status(msg);
            return;
        }
        let b = self.active_mut();
        b.checkpoint(EditKind::Other);
        b.delete_range(sr, sc, er, ec);
        b.selection_anchor = None;
        b.dirty = true;
    }

    fn paste(&mut self) {
        let text = match self.read_clipboard() {
            Ok(s) => s,
            Err(msg) => {
                self.set_status(msg);
                return;
            }
        };
        self.insert_paste_text(&text);
    }

    pub fn handle_paste(&mut self, text: String) {
        if !matches!(self.mode, Mode::Normal) {
            return;
        }
        if text.is_empty() {
            return;
        }
        self.insert_paste_text(&text);
    }

    fn insert_paste_text(&mut self, text: &str) {
        let b = self.active_mut();
        b.checkpoint(EditKind::Other);
        if let Some(((sr, sc), (er, ec))) = b.selection_range() {
            b.delete_range(sr, sc, er, ec);
            b.selection_anchor = None;
        }
        b.insert_text(text);
        b.dirty = true;
    }

    fn read_clipboard(&mut self) -> std::result::Result<String, String> {
        let cb = self
            .clipboard
            .as_mut()
            .ok_or_else(|| "clipboard unavailable".to_string())?;
        cb.get_text().map_err(|e| format!("paste failed: {e}"))
    }

    fn write_clipboard(&mut self, text: &str) -> std::result::Result<(), String> {
        let cb = self
            .clipboard
            .as_mut()
            .ok_or_else(|| "clipboard unavailable".to_string())?;
        cb.set_text(text.to_string())
            .map_err(|e| format!("copy failed: {e}"))
    }

    // -----------------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------------

    pub fn render(&mut self, frame: &mut Frame) {
        self.drain_fs_events();
        self.active_mut().check_disk();
        self.expire_status();
        let tw = self.config.tab_width;
        self.active_mut().ensure_foldable(tw);

        let area = frame.area();
        let show_bottom = matches!(self.mode, Mode::Prompt(_)) || !self.status.is_empty();

        let (upper, bottom) = if show_bottom {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(area);
            (chunks[0], Some(chunks[1]))
        } else {
            (area, None)
        };

        self.ensure_active_tab_visible(upper.height);
        let tab_col_w = self.tab_column_width();
        let h_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(tab_col_w),
                Constraint::Length(1),
                Constraint::Min(1),
            ])
            .split(upper);
        let tab_area = h_chunks[0];
        let sep_area = h_chunks[1];
        let body = h_chunks[2];

        let gw = self.gutter_width();
        let gutter = Rect::new(body.x, body.y, gw.min(body.width), body.height);
        let text_area = Rect::new(
            body.x + gw.min(body.width),
            body.y,
            body.width.saturating_sub(gw),
            body.height,
        );

        let tw = self.config.tab_width;
        self.active_mut()
            .adjust_scroll(text_area.height as usize, text_area.width as usize, tw);

        self.render_tab_bar(frame, tab_area);
        self.render_tab_separator(frame, sep_area);
        if gw > 0 {
            self.render_gutter(frame, gutter);
        }
        self.render_body(frame, text_area);

        if let Some(area) = bottom {
            match &self.mode {
                Mode::Prompt(state) => self.render_prompt(frame, area, state),
                Mode::Normal => self.render_status_line(frame, area),
            }
        }

        let cursor_overlay = bottom.unwrap_or(Rect::new(0, 0, 0, 0));
        self.place_cursor(frame, text_area, cursor_overlay);
        self.layout = Some(LayoutRects {
            tab_area,
            text_area,
            gutter,
        });
    }

    fn render_gutter(&self, frame: &mut Frame, area: Rect) {
        let b = self.active();
        let digits = (area.width as usize).saturating_sub(4);
        let mut lines: Vec<Line> = Vec::with_capacity(area.height as usize);
        let mut visible: Vec<usize> = Vec::with_capacity(area.height as usize);
        let mut r = b.scroll_row;
        while visible.len() < area.height as usize && r < b.text.len() {
            if !b.is_hidden(r) {
                visible.push(r);
                r = match b.collapsed_fold_at(r) {
                    Some(f) => f.end + 1,
                    None => r + 1,
                };
            } else {
                r += 1;
            }
        }
        for i in 0..area.height as usize {
            let Some(&doc_row) = visible.get(i) else {
                lines.push(Line::from(" ".repeat(area.width as usize)));
                continue;
            };
            let chevron = if b.collapsed_fold_at(doc_row).is_some() {
                '▸'
            } else if b.foldable_at.get(doc_row).copied().flatten().is_some() {
                '▾'
            } else {
                ' '
            };
            let label = format!(" {:>width$} {} ", doc_row + 1, chevron, width = digits);
            let style = if doc_row == b.cursor_row {
                Style::default()
            } else {
                Style::default().add_modifier(Modifier::DIM)
            };
            lines.push(Line::from(Span::styled(label, style)));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn expire_status(&mut self) {
        let timeout = Duration::from_millis(self.config.status_timeout_ms);
        if let Some(t) = self.status_shown_at {
            if t.elapsed() >= timeout {
                self.status.clear();
                self.status_shown_at = None;
            }
        }
    }

    fn gutter_width(&self) -> u16 {
        if !self.config.line_numbers {
            return 0;
        }
        let len = self.active().text.len().max(1);
        let digits = digits_in(len);
        (digits + 4) as u16
    }

    fn tab_column_width(&self) -> u16 {
        let widest = self
            .buffers
            .iter()
            .map(tab_label_inner_width)
            .max()
            .unwrap_or(1);
        // +1 trailing pad so the bullet doesn't kiss the separator.
        ((widest + 1) as u16).min(TAB_COL_MAX).max(1)
    }

    fn ensure_active_tab_visible(&mut self, height: u16) {
        let h = height as usize;
        if h == 0 {
            return;
        }
        if self.active < self.tabs_scroll {
            self.tabs_scroll = self.active;
        } else if self.active >= self.tabs_scroll + h {
            self.tabs_scroll = self.active + 1 - h;
        }
        let max_scroll = self.buffers.len().saturating_sub(h);
        if self.tabs_scroll > max_scroll {
            self.tabs_scroll = max_scroll;
        }
    }

    fn render_tab_bar(&self, frame: &mut Frame, area: Rect) {
        let n = self.buffers.len();
        let height = area.height as usize;
        let col_w = area.width as usize;
        let scroll = self.tabs_scroll;
        let max_shown = height.min(n.saturating_sub(scroll));

        let mut lines: Vec<Line> = Vec::with_capacity(height);
        for row in 0..max_shown {
            let i = scroll + row;
            let buf = &self.buffers[i];
            let active = i == self.active;

            // Reserve: leading space + name + at-least-1 gap + bullet slot + trailing pad.
            let name_budget = col_w.saturating_sub(4).max(1);
            let name = truncate_with_ellipsis(&buf.display_name(), name_budget);
            let name_len = name.chars().count();

            let name_style = if active {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::Gray)
            };

            let mut spans: Vec<Span> = Vec::new();
            spans.push(Span::raw(" "));
            spans.push(Span::styled(name, name_style));
            // Pad so the bullet slot lands at col_w - 2 (trailing space at col_w - 1).
            let before_bullet = 1 + name_len;
            let bullet_col = col_w.saturating_sub(2);
            if before_bullet < bullet_col {
                spans.push(Span::raw(" ".repeat(bullet_col - before_bullet)));
            }
            if buf.dirty {
                spans.push(Span::styled(
                    "•",
                    Style::default().fg(Color::LightYellow),
                ));
            } else {
                spans.push(Span::raw(" "));
            }
            spans.push(Span::raw(" "));
            lines.push(Line::from(spans));
        }
        while lines.len() < height {
            lines.push(Line::from(" ".repeat(col_w)));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_tab_separator(&self, frame: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let style = Style::default().fg(Color::DarkGray);
        let lines: Vec<Line> = (0..area.height)
            .map(|_| Line::from(Span::styled("│", style)))
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_body(&mut self, frame: &mut Frame, text_area: Rect) {
        let tab_width = self.config.tab_width;
        let height = text_area.height as usize;
        let width = text_area.width as usize;

        let b = self.active();
        let mut visible_rows: Vec<usize> = Vec::with_capacity(height);
        let mut r = b.scroll_row;
        while visible_rows.len() < height && r < b.text.len() {
            if !b.is_hidden(r) {
                visible_rows.push(r);
                r = match b.collapsed_fold_at(r) {
                    Some(f) => f.end + 1,
                    None => r + 1,
                };
            } else {
                r += 1;
            }
        }
        let last_doc = visible_rows.last().copied().unwrap_or(b.scroll_row);

        if let Some(hl) = self.highlighter.as_ref() {
            let idx = self.active;
            let b = &mut self.buffers[idx];
            b.ensure_highlights(last_doc, hl);
        }

        let b = self.active();
        let mut lines: Vec<Line> = Vec::with_capacity(visible_rows.len());
        for &row in &visible_rows {
            let line = &b.text[row];
            let mut display = build_display_line(
                line,
                b.highlights.get(row).and_then(|o| o.as_deref()),
                b.scroll_col,
                width,
                b.line_sel_range(row),
                tab_width,
            );
            if b.collapsed_fold_at(row).is_some() {
                display.spans.push(Span::styled(
                    " …",
                    Style::default().fg(Color::DarkGray),
                ));
            }
            lines.push(display);
        }
        frame.render_widget(Paragraph::new(lines), text_area);
    }

    fn render_status_line(&self, frame: &mut Frame, area: Rect) {
        if self.status.is_empty() {
            return;
        }
        frame.render_widget(Paragraph::new(self.status.clone()), area);
    }

    fn render_prompt(&self, frame: &mut Frame, area: Rect, state: &PromptState) {
        let text = format!("{}: {}", state.kind.label(), state.input);
        frame.render_widget(Paragraph::new(text), area);
    }

    fn place_cursor(&self, frame: &mut Frame, text_area: Rect, status_bar: Rect) {
        match &self.mode {
            Mode::Normal => {
                let b = self.active();
                let line = b.text.get(b.cursor_row).map(String::as_str).unwrap_or("");
                let vis = char_idx_to_vis_col(line, b.cursor_col, self.config.tab_width);
                if vis < b.scroll_col
                    || b.cursor_row < b.scroll_row
                    || b.is_hidden(b.cursor_row)
                {
                    return;
                }
                let screen_row = b.visible_offset(b.scroll_row, b.cursor_row) as u16;
                let screen_col = (vis - b.scroll_col) as u16;
                if screen_row < text_area.height && screen_col < text_area.width {
                    frame.set_cursor_position((
                        text_area.x + screen_col,
                        text_area.y + screen_row,
                    ));
                }
            }
            Mode::Prompt(state) => {
                let prefix_len = state.kind.label().chars().count() + 2;
                let col = (prefix_len + state.input.chars().count()) as u16;
                if col < status_bar.width {
                    frame.set_cursor_position((status_bar.x + col, status_bar.y));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// free helpers
// ---------------------------------------------------------------------------

fn char_idx_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn indent_cols(line: &str, tab_width: usize) -> Option<usize> {
    let mut col = 0usize;
    for c in line.chars() {
        match c {
            ' ' => col += 1,
            '\t' => col += char_display_width('\t', col, tab_width),
            _ => return Some(col),
        }
    }
    None
}

fn compute_fold_end(text: &[String], row: usize, tab_width: usize) -> Option<usize> {
    let base = indent_cols(&text[row], tab_width)?;
    let mut end: Option<usize> = None;
    for i in (row + 1)..text.len() {
        match indent_cols(&text[i], tab_width) {
            None => continue,
            Some(n) if n > base => end = Some(i),
            Some(_) => break,
        }
    }
    end
}

fn read_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

const TAB_COL_MAX: u16 = 30;

fn tab_label_inner_width(buf: &Buffer) -> usize {
    // Leading space + name + at-least-one gap + bullet slot (rightmost col).
    1 + buf.display_name().chars().count() + 2
}

fn truncate_with_ellipsis(s: &str, budget: usize) -> String {
    if s.chars().count() <= budget {
        return s.to_string();
    }
    if budget == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(budget - 1).collect();
    out.push('…');
    out
}

fn digits_in(n: usize) -> usize {
    let mut n = n.max(1);
    let mut d = 0;
    while n > 0 {
        n /= 10;
        d += 1;
    }
    d
}

fn expand_path(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(s)
}

fn char_display_width(c: char, vis_col: usize, tab_width: usize) -> usize {
    if c == '\t' {
        let tw = tab_width.max(1);
        tw - (vis_col % tw)
    } else {
        UnicodeWidthChar::width(c).unwrap_or(0)
    }
}

fn char_idx_to_vis_col(s: &str, char_idx: usize, tab_width: usize) -> usize {
    let mut vis = 0usize;
    for c in s.chars().take(char_idx) {
        vis += char_display_width(c, vis, tab_width);
    }
    vis
}

fn vis_col_to_char_idx(s: &str, target_vis: usize, tab_width: usize) -> usize {
    let mut vis = 0usize;
    for (i, c) in s.chars().enumerate() {
        let w = char_display_width(c, vis, tab_width);
        if vis + w > target_vis {
            return i;
        }
        vis += w;
    }
    s.chars().count()
}

fn point_in(x: u16, y: u16, r: Rect) -> bool {
    x >= r.x && x < r.x.saturating_add(r.width) && y >= r.y && y < r.y.saturating_add(r.height)
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    let ca = fs::canonicalize(a).ok();
    let cb = fs::canonicalize(b).ok();
    match (ca, cb) {
        (Some(pa), Some(pb)) => pa == pb,
        _ => a == b,
    }
}

fn extract_range(text: &[String], sr: usize, sc: usize, er: usize, ec: usize) -> String {
    if sr == er {
        let line = &text[sr];
        let start_byte = char_idx_to_byte(line, sc);
        let end_byte = char_idx_to_byte(line, ec);
        line[start_byte..end_byte].to_string()
    } else {
        let mut s = String::new();
        let first = &text[sr];
        let start_byte = char_idx_to_byte(first, sc);
        s.push_str(&first[start_byte..]);
        s.push('\n');
        for r in (sr + 1)..er {
            s.push_str(&text[r]);
            s.push('\n');
        }
        let last = &text[er];
        let end_byte = char_idx_to_byte(last, ec);
        s.push_str(&last[..end_byte]);
        s
    }
}

fn find_from(text: &[String], start: Pos, needle_lc: &str) -> Option<Pos> {
    if needle_lc.is_empty() {
        return None;
    }
    let (start_row, start_col) = start;
    for (i, line) in text.iter().enumerate().skip(start_row) {
        let haystack = line.to_lowercase();
        let from_byte = if i == start_row {
            char_idx_to_byte(line, start_col).min(haystack.len())
        } else {
            0
        };
        if let Some(byte_idx) = haystack[from_byte..].find(needle_lc) {
            let abs_byte = from_byte + byte_idx;
            let char_idx = line[..abs_byte].chars().count();
            return Some((i, char_idx));
        }
    }
    None
}

fn build_display_line(
    line: &str,
    hl: Option<&[HlSpan]>,
    start_vis: usize,
    max_width: usize,
    sel_range: Option<(usize, usize)>,
    tab_width: usize,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut current_text = String::new();
    let mut current_key: Option<(Option<usize>, bool)> = None;

    let mut current_vis = 0usize;
    let mut char_idx = 0usize;
    let mut hl_cursor = 0usize;
    let viewport_end = start_vis.saturating_add(max_width);

    for (byte_offset, c) in line.char_indices() {
        if let Some(spans_ref) = hl {
            while hl_cursor < spans_ref.len() && byte_offset >= spans_ref[hl_cursor].byte_end {
                hl_cursor += 1;
            }
        }
        let active_hl = match hl {
            Some(spans_ref) if hl_cursor < spans_ref.len()
                && byte_offset >= spans_ref[hl_cursor].byte_start =>
            {
                Some(hl_cursor)
            }
            _ => None,
        };

        let w = char_display_width(c, current_vis, tab_width);
        let char_vis_start = current_vis;
        let char_vis_end = current_vis + w;
        current_vis = char_vis_end;

        if char_vis_end <= start_vis {
            char_idx += 1;
            continue;
        }
        if char_vis_start >= viewport_end {
            break;
        }

        let effective_start = char_vis_start.max(start_vis);
        let effective_end = char_vis_end.min(viewport_end);
        let visible_width = effective_end.saturating_sub(effective_start);
        if visible_width == 0 {
            char_idx += 1;
            continue;
        }

        let selected = sel_range.is_some_and(|(a, b)| char_idx >= a && char_idx < b);
        let key = if selected {
            (None, true)
        } else {
            (active_hl, false)
        };
        if current_key != Some(key) {
            if !current_text.is_empty() {
                let text = std::mem::take(&mut current_text);
                let (hi, sel) = current_key.unwrap();
                let hl_ref = hi.and_then(|i| hl.and_then(|arr| arr.get(i)));
                spans.push(make_span(text, hl_ref, sel));
            }
            current_key = Some(key);
        }

        if c == '\t' {
            for _ in 0..visible_width {
                current_text.push(' ');
            }
        } else if visible_width == w {
            current_text.push(c);
        } else {
            for _ in 0..visible_width {
                current_text.push(' ');
            }
        }
        char_idx += 1;
    }

    if !current_text.is_empty() {
        if let Some((hi, sel)) = current_key {
            let hl_ref = hi.and_then(|i| hl.and_then(|arr| arr.get(i)));
            spans.push(make_span(current_text, hl_ref, sel));
        }
    }
    Line::from(spans)
}

fn make_span(text: String, hl: Option<&HlSpan>, selected: bool) -> Span<'static> {
    let mut style = Style::default();
    if selected {
        style = style.fg(Color::White).bg(Color::DarkGray);
    } else if let Some(h) = hl {
        if let Some(c) = h.color {
            style = style.fg(c);
        }
        style = style.add_modifier(h.modifier);
    }
    Span::styled(text, style)
}
