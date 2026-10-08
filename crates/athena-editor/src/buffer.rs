use std::fs;
use std::io::Read;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ropey::Rope;
use tree_sitter::{InputEdit, Point};

use crate::syntax::{Lang, Syntax, Token};

/// Typing within this window joins the previous undo step.
const UNDO_GROUP: Duration = Duration::from_millis(500);
const MAX_FILE: u64 = 50 * 1024 * 1024;

/// A selection in char offsets; `head` is where the cursor is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
}

impl Selection {
    pub fn cursor(at: usize) -> Self {
        Self {
            anchor: at,
            head: at,
        }
    }

    pub fn range(&self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Indent {
    Tab,
    Spaces(usize),
}

impl Indent {
    fn unit(self) -> String {
        match self {
            Self::Tab => "\t".into(),
            Self::Spaces(n) => " ".repeat(n),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditKind {
    Insert,
    Delete,
    Other,
}

#[derive(Clone, Debug)]
struct Change {
    start: usize,
    deleted: String,
    inserted: String,
}

#[derive(Clone, Debug)]
struct Transaction {
    changes: Vec<Change>,
    before: Selection,
    after: Selection,
}

pub struct Buffer {
    rope: Rope,
    pub path: Option<PathBuf>,
    pub selection: Selection,
    pub indent: Indent,
    syntax: Option<Syntax>,
    version: u64,
    /// Undo depth matching the file on disk; `None` once that state can no longer be reached.
    saved_at: Option<usize>,
    undo: Vec<Transaction>,
    redo: Vec<Transaction>,
    last_edit: Option<(EditKind, Instant)>,
    /// Column the cursor tries to return to on vertical moves across shorter lines.
    goal_column: Option<usize>,
}

impl Buffer {
    pub fn new(text: &str, path: Option<PathBuf>) -> Self {
        let rope = Rope::from_str(text);
        let syntax = path
            .as_deref()
            .and_then(Lang::for_path)
            .map(|lang| Syntax::new(lang, &rope));
        let indent = detect_indent(text, path.as_deref());
        Self {
            rope,
            path,
            selection: Selection::default(),
            indent,
            syntax,
            version: 0,
            saved_at: Some(0),
            undo: Vec::new(),
            redo: Vec::new(),
            last_edit: None,
            goal_column: None,
        }
    }

    /// Opens a UTF-8 text file; binary and very large files are refused.
    pub fn open(path: &Path) -> Result<Self> {
        let meta = fs::metadata(path).with_context(|| format!("open {}", path.display()))?;
        if meta.len() > MAX_FILE {
            bail!("{} is larger than 50 MB", path.display());
        }
        let mut bytes = Vec::new();
        fs::File::open(path)?.read_to_end(&mut bytes)?;
        if bytes.iter().take(8192).any(|b| *b == 0) {
            bail!("{} looks like a binary file", path.display());
        }
        let text =
            String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))?;
        Ok(Self::new(&text, Some(path.to_path_buf())))
    }

    /// Writes through a temp file and rename, keeping the file's permissions.
    pub fn save(&mut self) -> Result<()> {
        let path = self.path.clone().context("buffer has no file")?;
        let tmp = path.with_file_name(format!(
            ".{}.athena-tmp",
            path.file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default()
        ));
        let mut out = fs::File::create(&tmp).with_context(|| format!("write {}", tmp.display()))?;
        self.rope.write_to(&mut out)?;
        out.sync_all()?;
        if let Ok(meta) = fs::metadata(&path) {
            fs::set_permissions(&tmp, meta.permissions())?;
        }
        fs::rename(&tmp, &path).with_context(|| format!("replace {}", path.display()))?;
        self.saved_at = Some(self.undo.len());
        // The next keystroke must start a new undo step, or it would fold into the saved one.
        self.last_edit = None;
        Ok(())
    }

    pub fn is_dirty(&self) -> bool {
        self.saved_at != Some(self.undo.len())
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    pub fn lang(&self) -> Option<Lang> {
        self.syntax.as_ref().map(Syntax::lang)
    }

    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    pub fn len_lines(&self) -> usize {
        self.rope.len_lines()
    }

    /// Line text without its line break.
    pub fn line(&self, line: usize) -> String {
        let mut s = self.rope.line(line).to_string();
        while s.ends_with('\n') || s.ends_with('\r') {
            s.pop();
        }
        s
    }

    pub fn line_of(&self, char: usize) -> usize {
        self.rope.char_to_line(char.min(self.rope.len_chars()))
    }

    pub fn line_start(&self, line: usize) -> usize {
        self.rope.line_to_char(line)
    }

    pub fn line_len(&self, line: usize) -> usize {
        self.line(line).chars().count()
    }

    pub fn column_of(&self, char: usize) -> usize {
        char - self.line_start(self.line_of(char))
    }

    pub fn char_at(&self, line: usize, col: usize) -> usize {
        self.line_start(line) + col.min(self.line_len(line))
    }

    /// Zero-based line and UTF-16 column of a char index, as language servers count them.
    pub fn utf16_position(&self, char: usize) -> (u32, u32) {
        let char = char.min(self.rope.len_chars());
        let line = self.line_of(char);
        let start = self.line_start(line);
        let col: usize = self
            .rope
            .slice(start..char)
            .chars()
            .map(char::len_utf16)
            .sum();
        (line as u32, col as u32)
    }

    /// The char index at a zero-based line and UTF-16 column, clamped to the document.
    pub fn char_at_utf16(&self, line: u32, col: u32) -> usize {
        let line = (line as usize).min(self.len_lines().saturating_sub(1));
        let start = self.line_start(line);
        let mut units = 0;
        let mut chars = 0;
        for c in self.line(line).chars() {
            if units >= col as usize {
                break;
            }
            units += c.len_utf16();
            chars += 1;
        }
        start + chars
    }

    pub fn full_text(&self) -> String {
        self.rope.to_string()
    }

    pub fn highlights(&self, lines: Range<usize>) -> Vec<(Range<usize>, Token)> {
        let Some(syntax) = &self.syntax else {
            return Vec::new();
        };
        let last = self.rope.len_lines();
        let start = self.rope.line_to_byte(lines.start.min(last));
        let end = self.rope.line_to_byte(lines.end.min(last));
        syntax.highlights(&self.rope, start..end)
    }

    pub fn char_to_byte(&self, char: usize) -> usize {
        self.rope.char_to_byte(char)
    }

    pub fn selected_text(&self) -> String {
        self.rope.slice(self.selection.range()).to_string()
    }

    pub fn text(&self, range: Range<usize>) -> String {
        self.rope.slice(range).to_string()
    }

    fn point(&self, char: usize) -> Point {
        let line = self.rope.char_to_line(char);
        let col = self.rope.char_to_byte(char) - self.rope.line_to_byte(line);
        Point::new(line, col)
    }

    fn apply(&mut self, change: &Change) -> InputEdit {
        let start_byte = self.rope.char_to_byte(change.start);
        let start_position = self.point(change.start);
        let old_end_char = change.start + change.deleted.chars().count();
        let old_end_byte = self.rope.char_to_byte(old_end_char);
        let old_end_position = self.point(old_end_char);
        self.rope.remove(change.start..old_end_char);
        self.rope.insert(change.start, &change.inserted);
        let new_end_char = change.start + change.inserted.chars().count();
        InputEdit {
            start_byte,
            old_end_byte,
            new_end_byte: self.rope.char_to_byte(new_end_char),
            start_position,
            old_end_position,
            new_end_position: self.point(new_end_char),
        }
    }

    fn reparse(&mut self, edit: &InputEdit) {
        if let Some(syntax) = self.syntax.as_mut() {
            syntax.edit(edit, &self.rope);
        }
        self.version += 1;
    }

    /// Replaces `range` with `text` and leaves the cursor after it, as one undo step.
    fn replace(&mut self, range: Range<usize>, text: &str, kind: EditKind) {
        let before = self.selection;
        let change = Change {
            start: range.start,
            deleted: self.rope.slice(range.clone()).to_string(),
            inserted: text.to_string(),
        };
        let edit = self.apply(&change);
        self.reparse(&edit);
        let at = range.start + text.chars().count();
        self.selection = Selection::cursor(at);
        self.goal_column = None;
        self.redo.clear();

        let now = Instant::now();
        let joins = kind != EditKind::Other
            && self
                .last_edit
                .is_some_and(|(k, t)| k == kind && now - t < UNDO_GROUP)
            && self.undo.last().is_some_and(|t| t.after == before);
        self.last_edit = Some((kind, now));
        if joins && let Some(last) = self.undo.last_mut() {
            last.changes.push(change);
            last.after = self.selection;
            return;
        }
        if self.saved_at.is_some_and(|at| at > self.undo.len()) {
            self.saved_at = None;
        }
        self.undo.push(Transaction {
            changes: vec![change],
            before,
            after: self.selection,
        });
    }

    pub fn insert(&mut self, text: &str) {
        let kind = if text.contains('\n') || text.chars().count() > 1 {
            EditKind::Other
        } else {
            EditKind::Insert
        };
        self.replace(self.selection.range(), text, kind);
    }

    pub fn newline(&mut self) {
        let line = self.line_of(self.selection.head);
        let current = self.line(line);
        let mut indent: String = current
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let before_cursor = self.text(self.line_start(line)..self.selection.range().start);
        if before_cursor.trim_end().ends_with(['{', '(', '[']) {
            indent.push_str(&self.indent.unit());
        }
        self.replace(
            self.selection.range(),
            &format!("\n{indent}"),
            EditKind::Other,
        );
    }

    pub fn tab(&mut self) {
        let unit = match self.indent {
            Indent::Tab => "\t".to_string(),
            Indent::Spaces(n) => " ".repeat(n - self.column_of(self.selection.head) % n),
        };
        self.replace(self.selection.range(), &unit, EditKind::Insert);
    }

    pub fn backspace(&mut self) {
        let range = self.selection.range();
        if !range.is_empty() {
            return self.replace(range, "", EditKind::Delete);
        }
        if range.start > 0 {
            self.replace(range.start - 1..range.start, "", EditKind::Delete);
        }
    }

    pub fn delete_forward(&mut self) {
        let range = self.selection.range();
        if !range.is_empty() {
            return self.replace(range, "", EditKind::Delete);
        }
        if range.end < self.len_chars() {
            self.replace(range.start..range.end + 1, "", EditKind::Delete);
        }
    }

    pub fn delete_word_back(&mut self) {
        let end = self.selection.head;
        let start = if self.selection.is_empty() {
            self.word_left(end)
        } else {
            self.selection.range().start
        };
        self.replace(
            start..end.max(self.selection.range().end),
            "",
            EditKind::Other,
        );
    }

    pub fn delete_to_line_start(&mut self) {
        let head = self.selection.head;
        let start = self.line_start(self.line_of(head));
        let start = if start == head && head > 0 {
            head - 1
        } else {
            start
        };
        self.replace(start..head, "", EditKind::Other);
    }

    pub fn undo(&mut self) -> bool {
        let Some(tx) = self.undo.pop() else {
            return false;
        };
        for change in tx.changes.iter().rev() {
            let inverse = Change {
                start: change.start,
                deleted: change.inserted.clone(),
                inserted: change.deleted.clone(),
            };
            let edit = self.apply(&inverse);
            self.reparse(&edit);
        }
        self.selection = tx.before;
        self.redo.push(tx);
        self.last_edit = None;
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(tx) = self.redo.pop() else {
            return false;
        };
        for change in &tx.changes {
            let edit = self.apply(change);
            self.reparse(&edit);
        }
        self.selection = tx.after;
        self.undo.push(tx);
        self.last_edit = None;
        true
    }

    /// Comments or uncomments every line the selection touches.
    pub fn toggle_comment(&mut self) {
        let Some(prefix) = self.lang().map(Lang::comment_prefix) else {
            return;
        };
        let range = self.selection.range();
        let first = self.line_of(range.start);
        let mut last = self.line_of(range.end);
        if last > first && range.end == self.line_start(last) {
            last -= 1;
        }
        let lines: Vec<String> = (first..=last).map(|l| self.line(l)).collect();
        let code = |l: &String| !l.trim().is_empty();
        let all_commented = lines
            .iter()
            .filter(|l| code(l))
            .all(|l| l.trim_start().starts_with(prefix.trim_end()));
        let indent = lines
            .iter()
            .filter(|l| code(l))
            .map(|l| l.len() - l.trim_start().len())
            .min()
            .unwrap_or(0);
        let rewritten: Vec<String> = lines
            .iter()
            .map(|l| {
                if !code(l) {
                    l.clone()
                } else if all_commented {
                    let at = l.len() - l.trim_start().len();
                    let rest = &l[at..];
                    let rest = rest
                        .strip_prefix(prefix)
                        .or_else(|| rest.strip_prefix(prefix.trim_end()))
                        .unwrap_or(rest);
                    format!("{}{rest}", &l[..at])
                } else {
                    format!("{}{prefix}{}", &l[..indent], &l[indent..])
                }
            })
            .collect();
        let start = self.line_start(first);
        let end = start + lines.iter().map(|l| l.chars().count()).sum::<usize>() + (last - first);
        self.replace(start..end, &rewritten.join("\n"), EditKind::Other);
        let new_end =
            start + rewritten.iter().map(|l| l.chars().count()).sum::<usize>() + (last - first);
        self.selection = Selection {
            anchor: start,
            head: new_end,
        };
    }

    fn set_head(&mut self, head: usize, extend: bool) {
        let head = head.min(self.len_chars());
        self.selection = if extend {
            Selection {
                anchor: self.selection.anchor,
                head,
            }
        } else {
            Selection::cursor(head)
        };
    }

    pub fn move_left(&mut self, extend: bool) {
        self.goal_column = None;
        let r = self.selection.range();
        let head = if !extend && !r.is_empty() {
            r.start
        } else {
            self.selection.head.saturating_sub(1)
        };
        self.set_head(head, extend);
    }

    pub fn move_right(&mut self, extend: bool) {
        self.goal_column = None;
        let r = self.selection.range();
        let head = if !extend && !r.is_empty() {
            r.end
        } else {
            self.selection.head + 1
        };
        self.set_head(head, extend);
    }

    pub fn move_vertical(&mut self, lines: isize, extend: bool) {
        let head = self.selection.head;
        let line = self.line_of(head) as isize;
        let goal = *self.goal_column.get_or_insert(self.column_of(head));
        let target = line + lines;
        let head = if target < 0 {
            0
        } else if target as usize >= self.len_lines() {
            self.len_chars()
        } else {
            self.char_at(target as usize, goal)
        };
        self.set_head(head, extend);
        self.goal_column = Some(goal);
    }

    pub fn move_line_start(&mut self, extend: bool) {
        self.goal_column = None;
        let line = self.line_of(self.selection.head);
        let text = self.line(line);
        let first_code = text.chars().take_while(|c| c.is_whitespace()).count();
        let col = self.column_of(self.selection.head);
        // Toggles between the first non-blank character and column 0, as most editors do.
        let target = if col == first_code { 0 } else { first_code };
        self.set_head(self.line_start(line) + target, extend);
    }

    pub fn move_line_end(&mut self, extend: bool) {
        self.goal_column = None;
        let line = self.line_of(self.selection.head);
        self.set_head(self.line_start(line) + self.line_len(line), extend);
    }

    pub fn move_word(&mut self, forward: bool, extend: bool) {
        self.goal_column = None;
        let head = self.selection.head;
        let target = if forward {
            self.word_right(head)
        } else {
            self.word_left(head)
        };
        self.set_head(target, extend);
    }

    pub fn move_to(&mut self, char: usize, extend: bool) {
        self.goal_column = None;
        self.set_head(char, extend);
    }

    pub fn select_all(&mut self) {
        self.selection = Selection {
            anchor: 0,
            head: self.len_chars(),
        };
    }

    pub fn select_word_at(&mut self, char: usize) {
        let at = char.min(self.len_chars());
        let class = |c: char| {
            if is_word(c) {
                1
            } else if c.is_whitespace() {
                0
            } else {
                2
            }
        };
        let chars: Vec<char> = self.rope.chars().collect();
        if chars.is_empty() {
            return;
        }
        let probe = at.min(chars.len() - 1);
        let k = class(chars[probe]);
        let mut start = probe;
        while start > 0 && class(chars[start - 1]) == k {
            start -= 1;
        }
        let mut end = probe + 1;
        while end < chars.len() && class(chars[end]) == k {
            end += 1;
        }
        self.selection = Selection {
            anchor: start,
            head: end,
        };
    }

    pub fn select_line_at(&mut self, char: usize) {
        let line = self.line_of(char);
        let end = if line + 1 < self.len_lines() {
            self.line_start(line + 1)
        } else {
            self.len_chars()
        };
        self.selection = Selection {
            anchor: self.line_start(line),
            head: end,
        };
    }

    fn word_left(&self, from: usize) -> usize {
        let mut i = from;
        let at = |i: usize| self.rope.char(i);
        while i > 0 && !is_word(at(i - 1)) {
            i -= 1;
        }
        while i > 0 && is_word(at(i - 1)) {
            i -= 1;
        }
        i
    }

    fn word_right(&self, from: usize) -> usize {
        let len = self.len_chars();
        let mut i = from;
        let at = |i: usize| self.rope.char(i);
        while i < len && !is_word(at(i)) {
            i += 1;
        }
        while i < len && is_word(at(i)) {
            i += 1;
        }
        i
    }

    /// Case-insensitive occurrences of `query`, as char ranges.
    pub fn find_all(&self, query: &str) -> Vec<Range<usize>> {
        if query.is_empty() {
            return Vec::new();
        }
        let needle: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
        let hay: Vec<char> = self.rope.chars().flat_map(char::to_lowercase).collect();
        // Lowercasing can change length for a few scripts; such text simply won't match here.
        if hay.len() != self.len_chars() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut i = 0;
        while i + needle.len() <= hay.len() {
            if hay[i..i + needle.len()] == needle[..] {
                out.push(i..i + needle.len());
                i += needle.len();
            } else {
                i += 1;
            }
        }
        out
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Tabs if any line starts with one (Go's gofmt style), else the smallest space step in use.
fn detect_indent(text: &str, path: Option<&Path>) -> Indent {
    if path.and_then(|p| p.extension()).is_some_and(|e| e == "go") {
        return Indent::Tab;
    }
    let mut smallest = usize::MAX;
    for line in text.lines().take(2000) {
        if line.starts_with('\t') {
            return Indent::Tab;
        }
        let n = line.len() - line.trim_start_matches(' ').len();
        if n > 0 && line.len() > n {
            smallest = smallest.min(n);
        }
    }
    match smallest {
        2 | 4 | 8 => Indent::Spaces(smallest),
        _ => Indent::Spaces(2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_positions_round_trip() {
        let b = Buffer::new("a\nx😀y = 1\n", None);
        let y = b.line_start(1) + 2;
        assert_eq!(b.utf16_position(y), (1, 3));
        assert_eq!(b.char_at_utf16(1, 3), y);
        assert_eq!(b.char_at_utf16(1, 99), b.line_start(1) + b.line_len(1));
        assert_eq!(b.char_at_utf16(99, 0), b.line_start(2));
    }

    fn buf(text: &str, path: &str) -> Buffer {
        Buffer::new(text, Some(PathBuf::from(path)))
    }

    #[test]
    fn typing_groups_into_one_undo_step() {
        let mut b = buf("", "/x/a.ts");
        for c in "hello".chars() {
            b.insert(&c.to_string());
        }
        assert_eq!(b.rope().to_string(), "hello");
        assert!(b.undo());
        assert_eq!(b.rope().to_string(), "");
        assert!(b.redo());
        assert_eq!(b.rope().to_string(), "hello");
        assert_eq!(b.selection, Selection::cursor(5));
    }

    #[test]
    fn undoing_to_the_saved_state_is_clean() {
        let mut b = buf("x", "/x/a.ts");
        b.move_to(1, false);
        b.insert("y");
        assert!(b.is_dirty());
        b.undo();
        assert!(!b.is_dirty());
        b.redo();
        assert!(b.is_dirty());
        b.undo();
        b.insert("z");
        b.undo();
        assert!(!b.is_dirty(), "back at the original text");
        b.redo();
        b.undo();
        b.undo();
        assert!(!b.is_dirty());
    }

    #[test]
    fn newline_keeps_indent_and_opens_blocks() {
        let mut b = buf("func main() {", "/x/main.go");
        b.move_to(13, false);
        b.newline();
        assert_eq!(b.rope().to_string(), "func main() {\n\t");
        let mut b = buf("  if (x) {", "/x/a.ts");
        b.move_to(10, false);
        b.newline();
        assert_eq!(b.rope().to_string(), "  if (x) {\n    ");
    }

    #[test]
    fn vertical_moves_remember_the_column() {
        let mut b = buf("abcdef\nab\nabcdef", "/x/a.ts");
        b.move_to(5, false);
        b.move_vertical(1, false);
        assert_eq!(b.selection.head, 9);
        b.move_vertical(1, false);
        assert_eq!(b.selection.head, 15);
    }

    #[test]
    fn word_motion_and_delete() {
        let mut b = buf("let foo_bar = 1", "/x/a.ts");
        b.move_to(11, false);
        b.move_word(false, false);
        assert_eq!(b.selection.head, 4);
        b.move_word(true, false);
        assert_eq!(b.selection.head, 11);
        b.delete_word_back();
        assert_eq!(b.rope().to_string(), "let  = 1");
    }

    #[test]
    fn toggles_line_comments() {
        let mut b = buf("\tx := 1\n\ty := 2\n", "/x/a.go");
        b.selection = Selection {
            anchor: 0,
            head: 14,
        };
        b.toggle_comment();
        assert_eq!(b.rope().to_string(), "\t// x := 1\n\t// y := 2\n");
        b.toggle_comment();
        assert_eq!(b.rope().to_string(), "\tx := 1\n\ty := 2\n");
    }

    #[test]
    fn finds_case_insensitively() {
        let b = buf("Foo foo FOO", "/x/a.ts");
        assert_eq!(b.find_all("foo"), vec![0..3, 4..7, 8..11]);
    }

    #[test]
    fn detects_indent_style() {
        assert_eq!(
            detect_indent("a\n    b\n        c\n", None),
            Indent::Spaces(4)
        );
        assert_eq!(detect_indent("a\n\tb\n", None), Indent::Tab);
        assert_eq!(detect_indent("", Some(Path::new("x.go"))), Indent::Tab);
    }

    #[test]
    fn save_is_atomic_and_clears_dirty() {
        let dir = std::env::temp_dir().join(format!("athena-buf-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("main.go");
        fs::write(&path, "package main\n").unwrap();
        let mut b = Buffer::open(&path).unwrap();
        b.move_to(b.len_chars(), false);
        b.insert("x");
        assert!(b.is_dirty());
        b.save().unwrap();
        assert!(!b.is_dirty());
        assert_eq!(fs::read_to_string(&path).unwrap(), "package main\nx");
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            1,
            "no temp file left behind"
        );
        fs::write(dir.join("bin"), [0u8, 1, 2]).unwrap();
        assert!(Buffer::open(&dir.join("bin")).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn highlights_follow_edits() {
        let mut b = buf("package main\n", "/x/main.go");
        b.move_to(b.len_chars(), false);
        b.insert("func f() {}\n");
        let tokens = b.highlights(0..b.len_lines());
        let text = b.rope().to_string();
        assert!(
            tokens
                .iter()
                .any(|(r, t)| &text[r.clone()] == "func" && *t == Token::Keyword)
        );
    }
}
