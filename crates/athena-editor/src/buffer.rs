use std::collections::VecDeque;
use std::fs;
use std::io::Read;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use ropey::Rope;
use tree_sitter::{InputEdit, Point};

use crate::display::{Fold, indent_fold_at};
use crate::syntax::{Lang, Syntax, Token, bracket_pair};

/// Typing within this window joins the previous undo step.
pub(crate) const UNDO_GROUP: Duration = Duration::from_millis(500);
const MAX_FILE: u64 = 50 * 1024 * 1024;
/// Changes kept for views that have not caught up; one further behind starts over.
const EDIT_LOG: usize = 4096;

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

/// One view's selection and the column its vertical moves aim for; a buffer has none of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Cursor {
    pub selection: Selection,
    goal_column: Option<usize>,
}

impl Cursor {
    pub fn at(char: usize) -> Self {
        Self {
            selection: Selection::cursor(char),
            goal_column: None,
        }
    }

    pub fn head(&self) -> usize {
        self.selection.head
    }

    /// Follows edits made through another view, and clamps to a text of `len` chars.
    pub fn follow<'a>(&mut self, edits: impl IntoIterator<Item = &'a Edit>, len: usize) {
        for edit in edits {
            self.selection.anchor = edit.map(self.selection.anchor);
            self.selection.head = edit.map(self.selection.head);
            self.goal_column = None;
        }
        self.selection.anchor = self.selection.anchor.min(len);
        self.selection.head = self.selection.head.min(len);
    }
}

/// One applied change, in chars and in lines, so every view of the buffer can follow it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edit {
    pub at: usize,
    pub removed: usize,
    pub inserted: usize,
    pub line: usize,
    pub lines_removed: usize,
    pub lines_inserted: usize,
}

impl Edit {
    /// Where an offset lands after this edit; one inside the removed text moves to its start.
    pub fn map(&self, at: usize) -> usize {
        if at <= self.at {
            at
        } else if at >= self.at + self.removed {
            at - self.removed + self.inserted
        } else {
            self.at
        }
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
    pub indent: Indent,
    syntax: Option<Syntax>,
    version: u64,
    /// Undo depth matching the file on disk; `None` once that state can no longer be reached.
    saved_at: Option<usize>,
    undo: Vec<Transaction>,
    redo: Vec<Transaction>,
    last_edit: Option<(EditKind, Instant)>,
    /// The latest changes, the last one made at `version`.
    edits: VecDeque<Edit>,
    /// Modification time of the file as last read or written, to notice edits made elsewhere.
    disk_mtime: Option<SystemTime>,
}

/// Why a checked save wrote nothing.
#[derive(Debug)]
pub enum SaveError {
    /// The file changed on disk since it was read or last saved.
    Conflict,
    Io(anyhow::Error),
}

impl Buffer {
    pub fn new(text: &str, path: Option<PathBuf>) -> Self {
        let rope = Rope::from_str(text);
        let lang = path
            .as_deref()
            .and_then(Lang::for_path)
            .or_else(|| Lang::for_shebang(text.lines().next().unwrap_or_default()));
        let syntax = lang.map(|lang| Syntax::new(lang, &rope));
        let indent = detect_indent(text, lang);
        Self {
            rope,
            path,
            indent,
            syntax,
            version: 0,
            saved_at: Some(0),
            undo: Vec::new(),
            redo: Vec::new(),
            last_edit: None,
            edits: VecDeque::new(),
            disk_mtime: None,
        }
    }

    /// Opens a UTF-8 text file; binary and very large files are refused.
    pub fn open(path: &Path) -> Result<Self> {
        let (text, mtime) = read_text(path)?;
        let mut buffer = Self::new(&text, Some(path.to_path_buf()));
        buffer.disk_mtime = mtime;
        Ok(buffer)
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
        self.disk_mtime = modified(&path);
        self.saved_at = Some(self.undo.len());
        // The next keystroke must start a new undo step, or it would fold into the saved one.
        self.last_edit = None;
        Ok(())
    }

    /// True when another program wrote the file since this buffer read or saved it.
    pub fn changed_on_disk(&self) -> bool {
        let Some(path) = &self.path else {
            return false;
        };
        modified(path).is_some_and(|m| Some(m) != self.disk_mtime)
    }

    /// Saves unless the file changed on disk, so another program's edit is never overwritten blindly.
    pub fn save_checked(&mut self) -> Result<(), SaveError> {
        if self.changed_on_disk() {
            return Err(SaveError::Conflict);
        }
        self.save().map_err(SaveError::Io)
    }

    /// Writes to `path` and keeps editing it there, highlighting by its new extension.
    pub fn save_as(&mut self, path: PathBuf) -> Result<()> {
        let old = self.path.replace(path.clone());
        if let Err(e) = self.save() {
            self.path = old;
            return Err(e);
        }
        self.syntax = Lang::for_path(&path).map(|lang| Syntax::new(lang, &self.rope));
        Ok(())
    }

    /// Follows the file to `path` after it was renamed or moved, highlighting by its new name.
    pub fn set_path(&mut self, path: PathBuf) {
        let lang = Lang::for_path(&path).or_else(|| Lang::for_shebang(&self.line(0)));
        if lang != self.lang() {
            self.syntax = lang.map(|lang| Syntax::new(lang, &self.rope));
        }
        self.disk_mtime = modified(&path).or(self.disk_mtime);
        self.path = Some(path);
    }

    /// Takes the file's current text as one undoable edit and marks it saved.
    pub fn reload_from_disk(&mut self, c: &mut Cursor) -> Result<()> {
        let path = self.path.clone().context("buffer has no file")?;
        let (text, mtime) = read_text(&path)?;
        self.disk_mtime = mtime;
        let old: Vec<char> = self.rope.chars().collect();
        let new: Vec<char> = text.chars().collect();
        let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
        let suffix = old[prefix..]
            .iter()
            .rev()
            .zip(new[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        if prefix + suffix < old.len() || old.len() != new.len() {
            let inserted: String = new[prefix..new.len() - suffix].iter().collect();
            let (old_end, new_end) = (old.len() - suffix, new.len() - suffix);
            let map = |at: usize| match at {
                at if at <= prefix => at,
                at if at >= old_end => at - old_end + new_end,
                _ => new_end,
            };
            let selection = c.selection;
            self.replace(c, prefix..old_end, &inserted, EditKind::Other);
            c.selection = Selection {
                anchor: map(selection.anchor),
                head: map(selection.head),
            };
        }
        self.saved_at = Some(self.undo.len());
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

    pub fn selected_text(&self, c: &Cursor) -> String {
        let r = c.selection.range();
        let len = self.len_chars();
        self.rope
            .slice(r.start.min(len)..r.end.min(len))
            .to_string()
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
        if self.edits.len() == EDIT_LOG {
            self.edits.pop_front();
        }
        self.edits.push_back(Edit {
            at: change.start,
            removed: old_end_char - change.start,
            inserted: change.inserted.chars().count(),
            line: start_position.row,
            lines_removed: change.deleted.matches('\n').count(),
            lines_inserted: change.inserted.matches('\n').count(),
        });
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
    fn replace(&mut self, c: &mut Cursor, range: Range<usize>, text: &str, kind: EditKind) {
        let before = c.selection;
        let change = Change {
            start: range.start,
            deleted: self.rope.slice(range.clone()).to_string(),
            inserted: text.to_string(),
        };
        let edit = self.apply(&change);
        self.reparse(&edit);
        let at = range.start + text.chars().count();
        c.selection = Selection::cursor(at);
        c.goal_column = None;
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
            last.after = c.selection;
            return;
        }
        if self.saved_at.is_some_and(|at| at > self.undo.len()) {
            self.saved_at = None;
        }
        self.undo.push(Transaction {
            changes: vec![change],
            before,
            after: c.selection,
        });
    }

    pub fn insert(&mut self, c: &mut Cursor, text: &str) {
        let kind = if text.contains('\n') || text.chars().count() > 1 {
            EditKind::Other
        } else {
            EditKind::Insert
        };
        self.replace(c, c.selection.range(), text, kind);
    }

    /// Replaces `range` with `text` as its own undo step, leaving the cursor after it.
    pub fn replace_range(&mut self, c: &mut Cursor, range: Range<usize>, text: &str) {
        let len = self.len_chars();
        let range = range.start.min(len)..range.end.min(len);
        self.replace(c, range, text, EditKind::Other);
    }

    /// Applies non-overlapping edits, in offsets of the current text, as one undo step, then
    /// selects `select` within the text the first edit inserts (or puts the cursor after it).
    pub fn apply_edits(
        &mut self,
        c: &mut Cursor,
        edits: &[(Range<usize>, String)],
        select: Option<Range<usize>>,
    ) {
        let Some((main, main_text)) = edits.first() else {
            return;
        };
        let len = self.len_chars();
        let clamp = |r: &Range<usize>| r.start.min(len)..r.end.min(len).max(r.start.min(len));
        let main = clamp(main);
        let mut order: Vec<(Range<usize>, &str)> = vec![(main.clone(), main_text)];
        for (range, text) in &edits[1..] {
            let range = clamp(range);
            if range.end <= main.start || range.start >= main.end {
                order.push((range, text));
            }
        }
        // Later edits first, so each one's offsets still hold when it is applied.
        order.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
        let shift: isize = order
            .iter()
            .filter(|(r, _)| r.start < main.start)
            .map(|(r, t)| t.chars().count() as isize - r.len() as isize)
            .sum();
        let before = c.selection;
        let mut changes = Vec::new();
        for (range, text) in order {
            let change = Change {
                start: range.start,
                deleted: self.rope.slice(range).to_string(),
                inserted: text.to_string(),
            };
            let edit = self.apply(&change);
            self.reparse(&edit);
            changes.push(change);
        }
        let start = (main.start as isize + shift) as usize;
        let select = select.unwrap_or_else(|| {
            let end = main_text.chars().count();
            end..end
        });
        *c = Cursor {
            selection: Selection {
                anchor: start + select.start,
                head: start + select.end,
            },
            goal_column: None,
        };
        self.redo.clear();
        self.last_edit = None;
        if self.saved_at.is_some_and(|at| at > self.undo.len()) {
            self.saved_at = None;
        }
        self.undo.push(Transaction {
            changes,
            before,
            after: c.selection,
        });
    }

    pub fn newline(&mut self, c: &mut Cursor) {
        let line = self.line_of(c.selection.head);
        let current = self.line(line);
        let mut indent: String = current
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let before_cursor = self.text(self.line_start(line)..c.selection.range().start);
        if before_cursor.trim_end().ends_with(['{', '(', '[']) {
            indent.push_str(&self.indent.unit());
        }
        self.replace(
            c,
            c.selection.range(),
            &format!("\n{indent}"),
            EditKind::Other,
        );
    }

    pub fn tab(&mut self, c: &mut Cursor) {
        let unit = match self.indent {
            Indent::Tab => "\t".to_string(),
            Indent::Spaces(n) => " ".repeat(n - self.column_of(c.selection.head) % n),
        };
        self.replace(c, c.selection.range(), &unit, EditKind::Insert);
    }

    pub fn backspace(&mut self, c: &mut Cursor) {
        let range = c.selection.range();
        if !range.is_empty() {
            return self.replace(c, range, "", EditKind::Delete);
        }
        if range.start > 0 {
            self.replace(c, range.start - 1..range.start, "", EditKind::Delete);
        }
    }

    pub fn delete_forward(&mut self, c: &mut Cursor) {
        let range = c.selection.range();
        if !range.is_empty() {
            return self.replace(c, range, "", EditKind::Delete);
        }
        if range.end < self.len_chars() {
            self.replace(c, range.start..range.end + 1, "", EditKind::Delete);
        }
    }

    pub fn delete_word_back(&mut self, c: &mut Cursor) {
        let end = c.selection.head;
        let start = if c.selection.is_empty() {
            self.word_left(end)
        } else {
            c.selection.range().start
        };
        self.replace(
            c,
            start..end.max(c.selection.range().end),
            "",
            EditKind::Other,
        );
    }

    pub fn delete_to_line_start(&mut self, c: &mut Cursor) {
        let head = c.selection.head;
        let start = self.line_start(self.line_of(head));
        let start = if start == head && head > 0 {
            head - 1
        } else {
            start
        };
        self.replace(c, start..head, "", EditKind::Other);
    }

    /// Takes back the last change, whichever view made it, and puts `c` where it was before.
    pub fn undo(&mut self, c: &mut Cursor) -> bool {
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
        *c = Cursor {
            selection: tx.before,
            goal_column: None,
        };
        self.redo.push(tx);
        self.last_edit = None;
        true
    }

    pub fn redo(&mut self, c: &mut Cursor) -> bool {
        let Some(tx) = self.redo.pop() else {
            return false;
        };
        for change in &tx.changes {
            let edit = self.apply(change);
            self.reparse(&edit);
        }
        *c = Cursor {
            selection: tx.after,
            goal_column: None,
        };
        self.undo.push(tx);
        self.last_edit = None;
        true
    }

    /// Comments or uncomments every line the selection touches.
    pub fn toggle_comment(&mut self, c: &mut Cursor) {
        let Some(prefix) = self.lang().and_then(Lang::comment_prefix) else {
            return;
        };
        let range = c.selection.range();
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
        self.replace(c, start..end, &rewritten.join("\n"), EditKind::Other);
        let new_end =
            start + rewritten.iter().map(|l| l.chars().count()).sum::<usize>() + (last - first);
        c.selection = Selection {
            anchor: start,
            head: new_end,
        };
    }

    fn set_head(&self, c: &mut Cursor, head: usize, extend: bool) {
        let head = head.min(self.len_chars());
        c.selection = if extend {
            Selection {
                anchor: c.selection.anchor,
                head,
            }
        } else {
            Selection::cursor(head)
        };
    }

    pub fn move_left(&self, c: &mut Cursor, extend: bool) {
        c.goal_column = None;
        let r = c.selection.range();
        let head = if !extend && !r.is_empty() {
            r.start
        } else {
            c.selection.head.saturating_sub(1)
        };
        self.set_head(c, head, extend);
    }

    pub fn move_right(&self, c: &mut Cursor, extend: bool) {
        c.goal_column = None;
        let r = c.selection.range();
        let head = if !extend && !r.is_empty() {
            r.end
        } else {
            c.selection.head + 1
        };
        self.set_head(c, head, extend);
    }

    pub fn move_vertical(&self, c: &mut Cursor, lines: isize, extend: bool) {
        let head = c.selection.head;
        let line = self.line_of(head) as isize;
        let goal = *c.goal_column.get_or_insert(self.column_of(head));
        let target = line + lines;
        let head = if target < 0 {
            0
        } else if target as usize >= self.len_lines() {
            self.len_chars()
        } else {
            self.char_at(target as usize, goal)
        };
        self.set_head(c, head, extend);
        c.goal_column = Some(goal);
    }

    /// Moves to `line`, keeping the column vertical moves aim for.
    pub fn move_to_line(&self, c: &mut Cursor, line: usize, extend: bool) {
        let goal = *c
            .goal_column
            .get_or_insert(self.column_of(c.selection.head));
        let head = self.char_at(line.min(self.len_lines() - 1), goal);
        self.set_head(c, head, extend);
        c.goal_column = Some(goal);
    }

    pub fn move_line_start(&self, c: &mut Cursor, extend: bool) {
        c.goal_column = None;
        let line = self.line_of(c.selection.head);
        let text = self.line(line);
        let first_code = text.chars().take_while(|c| c.is_whitespace()).count();
        let col = self.column_of(c.selection.head);
        // Toggles between the first non-blank character and column 0, as most editors do.
        let target = if col == first_code { 0 } else { first_code };
        self.set_head(c, self.line_start(line) + target, extend);
    }

    pub fn move_line_end(&self, c: &mut Cursor, extend: bool) {
        c.goal_column = None;
        let line = self.line_of(c.selection.head);
        self.set_head(c, self.line_start(line) + self.line_len(line), extend);
    }

    pub fn move_word(&self, c: &mut Cursor, forward: bool, extend: bool) {
        c.goal_column = None;
        let head = c.selection.head;
        let target = if forward {
            self.word_right(head)
        } else {
            self.word_left(head)
        };
        self.set_head(c, target, extend);
    }

    pub fn move_to(&self, c: &mut Cursor, char: usize, extend: bool) {
        c.goal_column = None;
        self.set_head(c, char, extend);
    }

    pub fn select_all(&self, c: &mut Cursor) {
        c.selection = Selection {
            anchor: 0,
            head: self.len_chars(),
        };
    }

    pub fn select_word_at(&self, c: &mut Cursor, char: usize) {
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
        c.selection = Selection {
            anchor: start,
            head: end,
        };
    }

    pub fn select_line_at(&self, c: &mut Cursor, char: usize) {
        let line = self.line_of(char);
        let end = if line + 1 < self.len_lines() {
            self.line_start(line + 1)
        } else {
            self.len_chars()
        };
        c.selection = Selection {
            anchor: self.line_start(line),
            head: end,
        };
    }

    /// The identifier the char at `at` belongs to, if it is part of one.
    pub fn word_at(&self, at: usize) -> Option<Range<usize>> {
        let len = self.len_chars();
        if at >= len || !is_word(self.rope.char(at)) {
            return None;
        }
        let mut end = at;
        while end < len && is_word(self.rope.char(end)) {
            end += 1;
        }
        Some(self.word_start(at)..end)
    }

    /// Where the identifier ending at `at` starts; `at` itself when none ends there.
    pub fn word_start(&self, at: usize) -> usize {
        let mut i = at.min(self.len_chars());
        while i > 0 && is_word(self.rope.char(i - 1)) {
            i -= 1;
        }
        i
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

    /// The changes made after `version`, oldest first; `None` once they are no longer all kept.
    pub fn edits_since(&self, version: u64) -> Option<impl Iterator<Item = &Edit>> {
        let behind = usize::try_from(self.version.checked_sub(version)?).ok()?;
        (behind <= self.edits.len()).then(|| self.edits.iter().skip(self.edits.len() - behind))
    }

    /// The region `line` can fold: up to the line before the furthest closing bracket of a bracket
    /// opened on it, else the block indented under it.
    pub fn fold_at(&self, line: usize) -> Option<Fold> {
        if line + 1 >= self.len_lines() {
            return None;
        }
        let close = self.syntax.as_ref().and_then(|syntax| {
            let start = self.rope.line_to_byte(line);
            let end = self.rope.line_to_byte(line + 1);
            let mut best = None;
            for (i, b) in self.rope.byte_slice(start..end).bytes().enumerate() {
                if matches!(b, b'{' | b'[' | b'(')
                    && let Some(partner) = syntax.bracket_partner(start + i)
                {
                    let partner_line = self.rope.byte_to_line(partner);
                    if partner_line > line && best.is_none_or(|b| partner_line > b) {
                        best = Some(partner_line);
                    }
                }
            }
            best
        });
        match close {
            Some(close) if close >= line + 2 => Some(Fold {
                start: line + 1,
                end: close - 1,
            }),
            Some(_) => None,
            None => indent_fold_at(line, self.len_lines(), |l| self.line(l)),
        }
    }

    /// The bracket at or just before `head` and its partner, as char offsets.
    pub fn matching_bracket(&self, head: usize) -> Option<(usize, usize)> {
        let at = [head, head.wrapping_sub(1)].into_iter().find(|&i| {
            i < self.len_chars() && bracket_pair(&self.rope.char(i).to_string()).is_some()
        })?;
        let byte = self.rope.char_to_byte(at);
        if let Some(syntax) = &self.syntax
            && let Some(partner) = syntax.bracket_partner(byte)
        {
            return Some((at, self.rope.byte_to_char(partner)));
        }
        self.scan_bracket(at).map(|partner| (at, partner))
    }

    /// Plain nesting count, for text without a parse tree or brackets the tree leaves unpaired.
    fn scan_bracket(&self, at: usize) -> Option<usize> {
        const LIMIT: usize = 100_000;
        let c = self.rope.char(at).to_string();
        let (open, close) = bracket_pair(&c)?;
        let (open, close) = (open.chars().next()?, close.chars().next()?);
        let forward = c.starts_with(open);
        let mut depth = 0usize;
        if forward {
            for (i, ch) in self.rope.chars_at(at).enumerate().take(LIMIT) {
                depth = match ch {
                    _ if ch == open => depth + 1,
                    _ if ch == close => depth - 1,
                    _ => depth,
                };
                if depth == 0 {
                    return Some(at + i);
                }
            }
        } else {
            let mut chars = self.rope.chars_at(at + 1);
            for i in 0..LIMIT.min(at + 1) {
                let ch = chars.prev()?;
                depth = match ch {
                    _ if ch == close => depth + 1,
                    _ if ch == open => depth - 1,
                    _ => depth,
                };
                if depth == 0 {
                    return Some(at - i);
                }
            }
        }
        None
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

fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// A UTF-8 text file's contents and modification time; binary and very large files are refused.
fn read_text(path: &Path) -> Result<(String, Option<SystemTime>)> {
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
    Ok((text, meta.modified().ok()))
}

pub(crate) fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Tabs if any line starts with one (Go's gofmt style), else the smallest space step in use.
fn detect_indent(text: &str, lang: Option<Lang>) -> Indent {
    if lang == Some(Lang::Go) {
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
        let mut c = Cursor::default();
        for ch in "hello".chars() {
            b.insert(&mut c, &ch.to_string());
        }
        assert_eq!(b.rope().to_string(), "hello");
        assert!(b.undo(&mut c));
        assert_eq!(b.rope().to_string(), "");
        assert!(b.redo(&mut c));
        assert_eq!(b.rope().to_string(), "hello");
        assert_eq!(c.selection, Selection::cursor(5));
    }

    #[test]
    fn undoing_to_the_saved_state_is_clean() {
        let mut b = buf("x", "/x/a.ts");
        let mut c = Cursor::at(1);
        b.insert(&mut c, "y");
        assert!(b.is_dirty());
        b.undo(&mut c);
        assert!(!b.is_dirty());
        b.redo(&mut c);
        assert!(b.is_dirty());
        b.undo(&mut c);
        b.insert(&mut c, "z");
        b.undo(&mut c);
        assert!(!b.is_dirty(), "back at the original text");
        b.redo(&mut c);
        b.undo(&mut c);
        b.undo(&mut c);
        assert!(!b.is_dirty());
    }

    #[test]
    fn newline_keeps_indent_and_opens_blocks() {
        let mut b = buf("func main() {", "/x/main.go");
        b.newline(&mut Cursor::at(13));
        assert_eq!(b.rope().to_string(), "func main() {\n\t");
        let mut b = buf("  if (x) {", "/x/a.ts");
        b.newline(&mut Cursor::at(10));
        assert_eq!(b.rope().to_string(), "  if (x) {\n    ");
    }

    #[test]
    fn vertical_moves_remember_the_column() {
        let b = buf("abcdef\nab\nabcdef", "/x/a.ts");
        let mut c = Cursor::at(5);
        b.move_vertical(&mut c, 1, false);
        assert_eq!(c.head(), 9);
        b.move_vertical(&mut c, 1, false);
        assert_eq!(c.head(), 15);
    }

    #[test]
    fn word_motion_and_delete() {
        let mut b = buf("let foo_bar = 1", "/x/a.ts");
        let mut c = Cursor::at(11);
        b.move_word(&mut c, false, false);
        assert_eq!(c.head(), 4);
        b.move_word(&mut c, true, false);
        assert_eq!(c.head(), 11);
        b.delete_word_back(&mut c);
        assert_eq!(b.rope().to_string(), "let  = 1");
    }

    #[test]
    fn toggles_line_comments() {
        let mut b = buf("\tx := 1\n\ty := 2\n", "/x/a.go");
        let mut c = Cursor {
            selection: Selection {
                anchor: 0,
                head: 14,
            },
            ..Default::default()
        };
        b.toggle_comment(&mut c);
        assert_eq!(b.rope().to_string(), "\t// x := 1\n\t// y := 2\n");
        b.toggle_comment(&mut c);
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
        assert_eq!(detect_indent("", Some(Lang::Go)), Indent::Tab);
    }

    #[test]
    fn save_is_atomic_and_clears_dirty() {
        let dir = std::env::temp_dir().join(format!("athena-buf-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("main.go");
        fs::write(&path, "package main\n").unwrap();
        let mut b = Buffer::open(&path).unwrap();
        b.insert(&mut Cursor::at(b.len_chars()), "x");
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

    fn temp_file(name: &str, text: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("athena-buf-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("main.go");
        fs::write(&path, text).unwrap();
        path
    }

    /// Writes as another program would, with a modification time that differs from ours.
    fn write_elsewhere(path: &Path, text: &str) {
        fs::write(path, text).unwrap();
        let later = SystemTime::now() + Duration::from_secs(5);
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(later)
            .unwrap();
    }

    #[test]
    fn save_detects_external_change() {
        let path = temp_file("conflict", "a\n");
        let mut b = Buffer::open(&path).unwrap();
        let mut c = Cursor::default();
        assert!(!b.changed_on_disk());
        b.insert(&mut c, "x");
        b.save_checked().unwrap();
        assert!(
            !b.changed_on_disk(),
            "our own save is not an outside change"
        );
        write_elsewhere(&path, "theirs\n");
        assert!(b.changed_on_disk());
        b.insert(&mut c, "y");
        assert!(matches!(b.save_checked(), Err(SaveError::Conflict)));
        assert_eq!(fs::read_to_string(&path).unwrap(), "theirs\n");
        b.save().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "xya\n");
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn reload_keeps_the_cursor_and_can_be_undone() {
        let path = temp_file("reload", "one\ntwo\nthree\n");
        let mut b = Buffer::open(&path).unwrap();
        let mut c = Cursor::at(b.line_start(2) + 2);
        write_elsewhere(&path, "one\nTWO!\nthree\n");
        b.reload_from_disk(&mut c).unwrap();
        assert_eq!(b.full_text(), "one\nTWO!\nthree\n");
        assert_eq!(
            c.head(),
            b.line_start(2) + 2,
            "cursor after the change moves with it"
        );
        assert!(!b.is_dirty());
        assert!(!b.changed_on_disk());
        b.undo(&mut c);
        assert_eq!(b.full_text(), "one\ntwo\nthree\n");
        assert!(b.is_dirty());
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn save_as_moves_the_buffer() {
        let path = temp_file("save-as", "x = 1\n");
        let mut b = Buffer::new("const x = 1\n", None);
        assert!(b.lang().is_none());
        let to = path.with_file_name("x.ts");
        b.save_as(to.clone()).unwrap();
        assert_eq!(b.path.as_deref(), Some(to.as_path()));
        assert_eq!(b.lang(), Some(Lang::TypeScript));
        assert_eq!(fs::read_to_string(&to).unwrap(), "const x = 1\n");
        assert!(!b.is_dirty());
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_renamed_file_keeps_its_edits_and_takes_the_new_language() {
        let mut b = buf("const x = 1\n", "/x/a.txt");
        b.insert(&mut Cursor::default(), "export ");
        b.set_path(PathBuf::from("/x/a.ts"));
        assert_eq!(b.lang(), Some(Lang::TypeScript));
        assert!(b.is_dirty(), "unsaved edits stay unsaved");
        assert_eq!(b.full_text(), "export const x = 1\n");
    }

    #[test]
    fn matching_bracket_pairs_nested() {
        let b = buf("func f() {\n\tif x { g(\"}\") }\n}\n", "/x/a.go");
        assert_eq!(b.matching_bracket(9), Some((9, b.len_chars() - 2)));
        assert_eq!(
            b.matching_bracket(b.len_chars() - 1),
            Some((b.len_chars() - 2, 9))
        );
        let inner = b.full_text().find("{ g").unwrap();
        let close = b.full_text().rfind(") }").unwrap() + 2;
        assert_eq!(b.matching_bracket(inner + 1), Some((inner, close)));
        assert_eq!(b.matching_bracket(3), None);

        let plain = buf("a (b [c] d) e", "/x/notes.txt");
        assert_eq!(plain.matching_bracket(2), Some((2, 10)));
        assert_eq!(plain.matching_bracket(8), Some((7, 5)));
    }

    #[test]
    fn folds_start_at_brackets_and_fall_back_to_indent() {
        let go = "func f() {\n\tx := []int{\n\t\t1,\n\t}\n\treturn\n}\n";
        let b = buf(go, "/x/a.go");
        assert_eq!(b.fold_at(0), Some(Fold { start: 1, end: 4 }));
        assert_eq!(b.fold_at(1), Some(Fold { start: 2, end: 2 }));
        assert_eq!(b.fold_at(2), None);
        let py = "def f():\n    if x:\n        pass\n    return 1\n";
        let b = buf(py, "/x/a.py");
        assert_eq!(b.fold_at(0), Some(Fold { start: 1, end: 3 }));
        assert_eq!(b.fold_at(1), Some(Fold { start: 2, end: 2 }));
        let one_line = buf("f(a, {\n})\n", "/x/a.ts");
        assert_eq!(one_line.fold_at(0), None);
    }

    #[test]
    fn edits_report_line_deltas() {
        let lines = |b: &Buffer, since: u64| -> Vec<(usize, usize, usize)> {
            b.edits_since(since)
                .unwrap()
                .map(|e| (e.line, e.lines_removed, e.lines_inserted))
                .collect()
        };
        let mut b = buf("a\nb\nc", "/x/a.ts");
        let mut c = Cursor::at(2);
        b.insert(&mut c, "x\ny\n");
        b.select_all(&mut c);
        b.backspace(&mut c);
        assert_eq!(lines(&b, 0), vec![(1, 0, 2), (0, 4, 0)]);
        let seen = b.version();
        b.undo(&mut c);
        assert_eq!(lines(&b, seen), vec![(0, 0, 4)]);
        assert!(b.edits_since(b.version() + 1).is_none());
    }

    #[test]
    fn two_cursors_share_one_buffer_and_its_undo_history() {
        let mut b = buf("one\ntwo\n", "/x/a.go");
        let mut left = Cursor::at(0);
        let mut right = Cursor::at(b.line_start(1) + 1);
        let seen = b.version();
        b.insert(&mut left, "zero\n");
        assert_eq!(b.full_text(), "zero\none\ntwo\n");
        right.follow(b.edits_since(seen).unwrap(), b.len_chars());
        assert_eq!(
            b.line_of(right.head()),
            2,
            "the other cursor moves down with the text"
        );
        assert_eq!(b.column_of(right.head()), 1);
        assert_eq!(left.head(), 5, "each view keeps its own cursor");

        let seen = b.version();
        assert!(b.undo(&mut right), "undo is the buffer's, not the view's");
        assert_eq!(b.full_text(), "one\ntwo\n");
        assert_eq!(
            right.head(),
            0,
            "the undoing view's cursor goes where the edit was"
        );
        left.follow(b.edits_since(seen).unwrap(), b.len_chars());
        assert_eq!(left.head(), 0);
    }

    #[test]
    fn versions_only_grow_whichever_cursor_edits() {
        let mut b = buf("", "/x/a.go");
        let (mut one, mut two) = (Cursor::default(), Cursor::default());
        let mut versions = vec![b.version()];
        for i in 0..6 {
            let c = if i % 2 == 0 { &mut one } else { &mut two };
            if i == 4 {
                b.undo(c);
            } else {
                b.insert(c, "x");
            }
            two.follow(
                b.edits_since(*versions.last().unwrap()).unwrap(),
                b.len_chars(),
            );
            versions.push(b.version());
        }
        assert!(versions.windows(2).all(|w| w[0] < w[1]), "{versions:?}");
    }

    #[test]
    fn several_edits_apply_as_one_undo_step() {
        let text = "package main\n\nfunc main() { fmt.Pri }\n";
        let mut b = buf(text, "/x/main.go");
        let pri = text.find("Pri").unwrap();
        let mut c = Cursor::at(pri + 3);
        let import = b.line_start(1);
        b.apply_edits(
            &mut c,
            &[
                (pri..pri + 3, "Println()".into()),
                (import..import, "\nimport \"fmt\"\n".into()),
            ],
            Some(8..8),
        );
        let want = "package main\n\nimport \"fmt\"\n\nfunc main() { fmt.Println() }\n";
        assert_eq!(b.full_text(), want);
        assert_eq!(
            c.head(),
            want.rfind("()").unwrap() + 1,
            "inside the parentheses"
        );
        assert!(b.undo(&mut c));
        assert_eq!(b.full_text(), text);
        assert_eq!(c.head(), pri + 3);
        assert!(b.redo(&mut c));
        assert_eq!(b.full_text(), want);
    }

    #[test]
    fn a_cursor_inside_removed_text_moves_to_where_it_was() {
        let mut b = buf("abcdef", "/x/a.txt");
        let mut other = Cursor::at(3);
        let mut c = Cursor {
            selection: Selection { anchor: 1, head: 5 },
            ..Default::default()
        };
        let seen = b.version();
        b.insert(&mut c, "XY");
        other.follow(b.edits_since(seen).unwrap(), b.len_chars());
        assert_eq!(b.full_text(), "aXYf");
        assert_eq!(other.head(), 1);
    }

    #[test]
    fn highlights_follow_edits() {
        let mut b = buf("package main\n", "/x/main.go");
        b.insert(&mut Cursor::at(b.len_chars()), "func f() {}\n");
        let tokens = b.highlights(0..b.len_lines());
        let text = b.rope().to_string();
        assert!(
            tokens
                .iter()
                .any(|(r, t)| &text[r.clone()] == "func" && *t == Token::Keyword)
        );
    }
}
