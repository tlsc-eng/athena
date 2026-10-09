use std::collections::VecDeque;
use std::fs;
use std::io::Read;
use std::ops::Range;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use ropey::Rope;
use tree_sitter::{InputEdit, Point};

use crate::display::{Fold, TAB_WIDTH, indent_fold_at};
use crate::pairs::{self, AutoClosed};
use crate::syntax::{Lang, ParseJob, Parsed, Syntax, Token, bracket_pair};

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

/// One caret's selection and the column its vertical moves aim for; a buffer has none of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Cursor {
    pub selection: Selection,
    pub(crate) goal_column: Option<usize>,
    pub(crate) closed: AutoClosed,
}

impl Cursor {
    pub fn at(char: usize) -> Self {
        Self {
            selection: Selection::cursor(char),
            goal_column: None,
            closed: AutoClosed::default(),
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
            self.closed.clear();
        }
        self.selection.anchor = self.selection.anchor.min(len);
        self.selection.head = self.selection.head.min(len);
    }

    /// Follows an edit this view made at another caret, keeping the column goal and closers.
    fn map_edit(&mut self, edit: &Edit) {
        self.selection.anchor = edit.map(self.selection.anchor);
        self.selection.head = edit.map(self.selection.head);
        self.closed.retain_map(|at| Some(edit.map(at)));
    }

    fn shift(&mut self, by: isize) {
        let at = |p: usize| p.saturating_add_signed(by);
        self.selection.anchor = at(self.selection.anchor);
        self.selection.head = at(self.selection.head);
        self.closed.retain_map(|p| Some(at(p)));
    }
}

/// Every caret of a view, in text order and never overlapping, one of them primary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursors {
    all: Vec<Cursor>,
    primary: usize,
}

impl Default for Cursors {
    fn default() -> Self {
        Self::new(Cursor::default())
    }
}

impl From<Cursor> for Cursors {
    fn from(c: Cursor) -> Self {
        Self::new(c)
    }
}

impl Cursors {
    pub fn new(c: Cursor) -> Self {
        Self {
            all: vec![c],
            primary: 0,
        }
    }

    /// The caret that scrolling, the status bar and language features follow.
    pub fn primary(&self) -> &Cursor {
        &self.all[self.primary]
    }

    pub fn primary_mut(&mut self) -> &mut Cursor {
        &mut self.all[self.primary]
    }

    pub fn primary_index(&self) -> usize {
        self.primary
    }

    pub fn head(&self) -> usize {
        self.primary().head()
    }

    pub fn selection(&self) -> Selection {
        self.primary().selection
    }

    pub fn all(&self) -> &[Cursor] {
        &self.all
    }

    /// Every caret, to change in ways that keep their order and keep them apart.
    pub(crate) fn all_mut(&mut self) -> &mut [Cursor] {
        &mut self.all
    }

    pub fn len(&self) -> usize {
        self.all.len()
    }

    pub fn is_empty(&self) -> bool {
        self.all.is_empty()
    }

    pub fn is_multi(&self) -> bool {
        self.all.len() > 1
    }

    /// Drops every caret but the primary.
    pub fn collapse(&mut self) {
        let primary = *self.primary();
        *self = Self::new(primary);
    }

    /// Adds a caret, which becomes the primary, merging any it overlaps.
    pub fn add(&mut self, c: Cursor) {
        self.all.push(c);
        self.primary = self.all.len() - 1;
        self.normalize();
    }

    /// Removes the caret at `index` unless it is the last one.
    pub fn remove(&mut self, index: usize) {
        if self.all.len() < 2 || index >= self.all.len() {
            return;
        }
        self.all.remove(index);
        if self.primary > index || self.primary == self.all.len() {
            self.primary = self.primary.saturating_sub(1);
        }
    }

    /// Removes the caret at or around `at`, as Alt+click does, or adds one there; true if added.
    pub fn toggle(&mut self, at: usize) -> bool {
        let hit = self
            .all
            .iter()
            .position(|c| c.selection.range().contains(&at) || c.head() == at);
        match hit {
            Some(i) => {
                self.remove(i);
                false
            }
            None => {
                self.add(Cursor::at(at));
                true
            }
        }
    }

    /// Replaces every caret, `primary` indexing into `all`.
    pub fn set(&mut self, all: Vec<Cursor>, primary: usize) {
        if all.is_empty() {
            return;
        }
        self.primary = primary.min(all.len() - 1);
        self.all = all;
        self.normalize();
    }

    /// Restores carets saved as selections, as undo does.
    fn restore(&mut self, carets: &Carets) {
        let all = carets.selections.iter().map(|&selection| Cursor {
            selection,
            ..Cursor::default()
        });
        self.set(all.collect(), carets.primary);
    }

    fn carets(&self) -> Carets {
        Carets {
            selections: self.all.iter().map(|c| c.selection).collect(),
            primary: self.primary,
        }
    }

    /// Follows edits made through another view, and clamps to a text of `len` chars.
    pub fn follow<'a>(&mut self, edits: impl IntoIterator<Item = &'a Edit> + Clone, len: usize) {
        for c in &mut self.all {
            c.follow(edits.clone(), len);
        }
        self.normalize();
    }

    /// Sorts the carets and merges those that overlap, as VS Code does; touching selections stay
    /// apart but an empty caret joins a selection it touches.
    pub fn normalize(&mut self) {
        if self.all.len() < 2 {
            return;
        }
        let key = |c: &Cursor| (c.selection.range().start, c.selection.range().end);
        if !self.all.windows(2).all(|w| key(&w[0]) <= key(&w[1])) {
            let primary = self.all[self.primary];
            self.all.sort_by_key(key);
            self.primary = self.all.iter().position(|c| *c == primary).unwrap_or(0);
        }
        let mut merged: Vec<Cursor> = Vec::with_capacity(self.all.len());
        let mut primary = 0;
        for (i, c) in self.all.iter().enumerate() {
            if let Some(last) = merged.last_mut() {
                let (a, b) = (last.selection.range(), c.selection.range());
                let touch = b.start == a.end && (a.is_empty() || b.is_empty());
                if b.start < a.end || touch {
                    let end = a.end.max(b.end);
                    if last.selection.head < last.selection.anchor {
                        last.selection.anchor = end;
                    } else {
                        last.selection.head = end;
                    }
                    if i == self.primary {
                        primary = merged.len() - 1;
                    }
                    continue;
                }
            }
            if i == self.primary {
                primary = merged.len();
            }
            merged.push(*c);
        }
        self.all = merged;
        self.primary = primary;
    }
}

/// The selections of every caret, saved with an undo step.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Carets {
    selections: Vec<Selection>,
    primary: usize,
}

impl Carets {
    fn one(selection: Selection) -> Self {
        Self {
            selections: vec![selection],
            primary: 0,
        }
    }

    fn is(&self, selection: Selection) -> bool {
        self.selections.len() == 1 && self.selections[0] == selection
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

    /// Columns one level of indentation spans.
    pub fn size(self) -> usize {
        match self {
            Self::Tab => TAB_WIDTH,
            Self::Spaces(n) => n,
        }
    }

    /// Whitespace reaching column `col`, in this style, a tab spanning `tab` columns.
    fn fill(self, col: usize, tab: usize) -> String {
        match self {
            Self::Tab => "\t".repeat(col / tab) + &" ".repeat(col % tab),
            Self::Spaces(_) => " ".repeat(col),
        }
    }
}

/// The column leading blanks reach, a tab spanning `tab` columns, and how many chars they are.
fn indent_width(line: &str, tab: usize) -> (usize, usize) {
    let mut col = 0;
    let mut chars = 0;
    for c in line.chars() {
        match c {
            ' ' => col += 1,
            '\t' => col += tab - col % tab,
            _ => break,
        }
        chars += 1;
    }
    (col, chars)
}

/// How lines end, from the first line break in the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    CrLf,
}

impl LineEnding {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::CrLf => "\r\n",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditKind {
    Insert,
    Delete,
    Reload,
    Other,
}

impl EditKind {
    /// The kind of a step made of edits of both kinds.
    fn and(self, other: Self) -> Self {
        if self == other { self } else { Self::Other }
    }
}

/// Changes made at several carets, gathered into one undo step.
#[derive(Default)]
struct Batch {
    changes: Vec<Change>,
    kind: Option<EditKind>,
    /// Every edit applied in the batch, which the edit log may be too short to keep.
    edits: Vec<Edit>,
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
    before: Carets,
    after: Carets,
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
    /// The file as last read or written, to notice edits made elsewhere; `None` before it existed.
    disk: Option<Stamp>,
    /// Set while edits at several carets gather into one undo step.
    batch: Option<Batch>,
    /// Set while several changes share one parse, which is owed once they are all applied.
    defer_parse: bool,
    parse_owed: bool,
    /// Edits only move the tree; whoever owns the buffer runs [`Self::start_parse`] elsewhere.
    background_parse: bool,
}

/// What a file looked like on disk; tools that keep the modification time still change its size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    mtime: Option<SystemTime>,
    len: u64,
}

impl Stamp {
    fn of(meta: &fs::Metadata) -> Self {
        Self {
            mtime: meta.modified().ok(),
            len: meta.len(),
        }
    }
}

/// How the file on disk compares with the text this buffer last read or wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskState {
    Unchanged,
    Changed,
    /// The file is gone (deleted, moved away or checked out of existence).
    Deleted,
}

/// Why a checked save wrote nothing.
#[derive(Debug)]
pub enum SaveError {
    /// The file changed on disk since it was read or last saved.
    Conflict,
    /// The file was deleted on disk; a checked save must not bring it back.
    Deleted,
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
            disk: None,
            batch: None,
            defer_parse: false,
            parse_owed: false,
            background_parse: false,
        }
    }

    /// Opens a UTF-8 text file; binary and very large files are refused.
    pub fn open(path: &Path) -> Result<Self> {
        let (text, stamp) = read_text(path)?;
        let mut buffer = Self::new(&text, Some(path.to_path_buf()));
        buffer.disk = Some(stamp);
        Ok(buffer)
    }

    /// Writes through a temp file and rename, keeping the file's permissions; a symlink stays a
    /// link and its target gets the text, and a hard-linked file is rewritten in place.
    pub fn save(&mut self) -> Result<()> {
        let path = self.path.clone().context("buffer has no file")?;
        let target = link_target(&path);
        let meta = fs::metadata(&target).ok();
        if meta.as_ref().is_some_and(|m| m.is_file() && m.nlink() > 1) {
            // A rename would split it from its other links, so this write gives up atomicity.
            let mut out = fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&target)
                .with_context(|| format!("write {}", target.display()))?;
            self.rope.write_to(&mut out)?;
            out.sync_all()?;
        } else {
            self.replace_file(&target, meta.as_ref())?;
        }
        self.disk = stamp(&path);
        self.saved_at = Some(self.undo.len());
        // The next keystroke must start a new undo step, or it would fold into the saved one.
        self.last_edit = None;
        Ok(())
    }

    /// Swaps `target` for a fully written temp file, so a failed save never leaves it half written.
    fn replace_file(&self, target: &Path, meta: Option<&fs::Metadata>) -> Result<()> {
        let tmp = target.with_file_name(format!(
            ".{}.athena-tmp",
            target
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default()
        ));
        let written = (|| {
            let mut out =
                fs::File::create(&tmp).with_context(|| format!("write {}", tmp.display()))?;
            self.rope.write_to(&mut out)?;
            out.sync_all()?;
            if let Some(meta) = meta {
                fs::set_permissions(&tmp, meta.permissions())?;
            }
            fs::rename(&tmp, target).with_context(|| format!("replace {}", target.display()))
        })();
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        written
    }

    /// Whether another program wrote or removed the file since this buffer read or saved it.
    pub fn disk_state(&self) -> DiskState {
        let Some(path) = &self.path else {
            return DiskState::Unchanged;
        };
        match fs::metadata(path) {
            Ok(meta) if Some(Stamp::of(&meta)) != self.disk => DiskState::Changed,
            Ok(_) => DiskState::Unchanged,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && self.disk.is_some() => {
                DiskState::Deleted
            }
            Err(_) => DiskState::Unchanged,
        }
    }

    /// True when another program wrote the file since this buffer read or saved it.
    pub fn changed_on_disk(&self) -> bool {
        self.disk_state() == DiskState::Changed
    }

    /// Saves unless the file changed or vanished on disk, so another program's edit is never
    /// overwritten blindly and a deleted file is never quietly recreated.
    pub fn save_checked(&mut self) -> Result<(), SaveError> {
        match self.disk_state() {
            DiskState::Changed => Err(SaveError::Conflict),
            DiskState::Deleted => Err(SaveError::Deleted),
            DiskState::Unchanged => self.save().map_err(SaveError::Io),
        }
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
        self.disk = stamp(&path).or(self.disk);
        self.path = Some(path);
    }

    /// Takes the file's current text as an undoable edit and marks it saved.
    pub fn reload_from_disk(&mut self, c: &mut Cursor) -> Result<()> {
        let path = self.path.clone().context("buffer has no file")?;
        let disk = read_disk_text(&path, &self.rope)?;
        let mut cs = Cursors::new(*c);
        self.take_disk_text(&mut cs, disk);
        *c = *cs.primary();
        Ok(())
    }

    /// Applies a file read by [`read_disk_text`] against this buffer's current text and marks it
    /// saved; reloads with no edit between them undo as one step.
    pub(crate) fn take_disk_text(&mut self, cs: &mut Cursors, disk: DiskText) {
        self.disk = Some(disk.stamp);
        if let Some((range, inserted)) = disk.change {
            let (prefix, old_end) = (range.start, range.end);
            let new_end = prefix + inserted.chars().count();
            let map = |at: usize| match at {
                at if at <= prefix => at,
                at if at >= old_end => at - old_end + new_end,
                _ => new_end,
            };
            let before: Vec<Selection> = cs.all().iter().map(|c| c.selection).collect();
            self.edit_primary(cs, |b, c| b.replace(c, range, &inserted, EditKind::Reload));
            let all = before.into_iter().map(|s| Cursor {
                selection: Selection {
                    anchor: map(s.anchor),
                    head: map(s.head),
                },
                ..Cursor::default()
            });
            cs.set(all.collect(), cs.primary_index());
        } else if self.last_edit.is_some_and(|(k, _)| k != EditKind::Reload) {
            // The next keystroke must start a new undo step, or it would fold into the saved one.
            self.last_edit = None;
        }
        self.saved_at = Some(self.undo.len());
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
        let old_end_line = self.rope.char_to_line(old_end_char);
        self.rope.remove(change.start..old_end_char);
        self.rope.insert(change.start, &change.inserted);
        let new_end_char = change.start + change.inserted.chars().count();
        // Lines as the rope counts them (a lone CR breaks one); splitting or joining a CRLF
        // moves the start line too.
        let line = start_position.row.min(self.rope.char_to_line(change.start));
        if self.edits.len() == EDIT_LOG {
            self.edits.pop_front();
        }
        let edit = Edit {
            at: change.start,
            removed: old_end_char - change.start,
            inserted: new_end_char - change.start,
            line,
            lines_removed: old_end_line - line,
            lines_inserted: self.rope.char_to_line(new_end_char) - line,
        };
        self.edits.push_back(edit);
        if let Some(batch) = self.batch.as_mut() {
            batch.edits.push(edit);
        }
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
            if self.defer_parse || self.background_parse {
                syntax.edit_tree(edit);
                self.parse_owed |= self.defer_parse;
            } else {
                syntax.edit(edit, &self.rope);
            }
        }
        self.version += 1;
    }

    /// Leaves parsing after edits to [`Self::start_parse`], so typing never waits for a parse.
    pub(crate) fn parse_in_background(&mut self) {
        self.background_parse = true;
    }

    /// A parse of the current text to run off the UI thread, when the tree is behind the text
    /// and no parse is already running.
    pub(crate) fn start_parse(&mut self) -> Option<ParseJob> {
        self.syntax.as_mut()?.start_parse(&self.rope)
    }

    /// Swaps in a background parse's tree; false if it was superseded.
    pub(crate) fn finish_parse(&mut self, parsed: Parsed) -> bool {
        self.syntax
            .as_mut()
            .is_some_and(|syntax| syntax.finish_parse(parsed))
    }

    /// Parses now if the tree lags the text, for checks that must see the last keystroke's tokens.
    /// Within a batch the tree already lags its own edits, as it always has.
    fn settle_parse(&mut self) {
        if self.defer_parse && self.parse_owed {
            return;
        }
        if let Some(syntax) = self.syntax.as_mut()
            && !syntax.is_current()
        {
            syntax.reparse(&self.rope);
        }
    }

    /// Runs `f` with parsing put off until it returns, so its changes cost one parse.
    fn parse_once<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let nested = std::mem::replace(&mut self.defer_parse, true);
        let out = f(self);
        if !nested {
            self.defer_parse = false;
            if std::mem::take(&mut self.parse_owed)
                && !self.background_parse
                && let Some(syntax) = self.syntax.as_mut()
            {
                syntax.reparse(&self.rope);
            }
        }
        out
    }

    /// Files `changes` as an undo step, or into the open batch; typing and deleting within
    /// [`UNDO_GROUP`] of the same kind join the step before when it ended where they start.
    fn commit(&mut self, changes: Vec<Change>, before: Carets, after: Carets, kind: EditKind) {
        self.redo.clear();
        if let Some(batch) = self.batch.as_mut() {
            batch.changes.extend(changes);
            batch.kind = Some(batch.kind.map_or(kind, |k| k.and(kind)));
            return;
        }
        let now = Instant::now();
        let joins = match kind {
            EditKind::Other => false,
            EditKind::Reload => self.last_edit.is_some_and(|(k, _)| k == kind),
            _ => {
                self.last_edit
                    .is_some_and(|(k, t)| k == kind && now - t < UNDO_GROUP)
                    && self.undo.last().is_some_and(|t| t.after == before)
            }
        };
        self.last_edit = Some((kind, now));
        if joins && let Some(last) = self.undo.last_mut() {
            last.changes.extend(changes);
            last.after = after;
            return;
        }
        if self.saved_at.is_some_and(|at| at > self.undo.len()) {
            self.saved_at = None;
        }
        self.undo.push(Transaction {
            changes,
            before,
            after,
        });
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
        let (end, inserted) = (range.end, at - range.start);
        c.closed.retain_map(|p| match p {
            p if p < range.start => Some(p),
            p if p >= end => Some(p - (end - range.start) + inserted),
            _ => None,
        });
        self.commit(
            vec![change],
            Carets::one(before),
            Carets::one(c.selection),
            kind,
        );
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
        let mut order: Vec<(Range<usize>, &str, bool)> = vec![(main.clone(), main_text, true)];
        for (range, text) in &edits[1..] {
            let range = clamp(range);
            if range.end <= main.start || range.start >= main.end {
                order.push((range, text, false));
            }
        }
        // Later edits first, so each one's offsets still hold when it is applied.
        order.sort_by_key(|(r, _, _)| std::cmp::Reverse(r.start));
        // An edit reaching into one applied after it would remove text that is no longer there.
        let mut floor = usize::MAX;
        order.retain(|(r, _, is_main)| {
            let fits = r.end <= floor || *is_main;
            if fits {
                floor = r.start;
            }
            fits
        });
        // An insert at the main edit's start is applied after it, so it lands in front of it.
        let shift: isize = order
            .iter()
            .filter(|(r, _, is_main)| r.start < main.start || (r.start == main.start && !is_main))
            .map(|(r, t, _)| t.chars().count() as isize - r.len() as isize)
            .sum();
        let before = c.selection;
        let changes = self.parse_once(|b| {
            let mut changes = Vec::new();
            for (range, text, _) in order {
                let change = Change {
                    start: range.start,
                    deleted: b.rope.slice(range).to_string(),
                    inserted: text.to_string(),
                };
                let edit = b.apply(&change);
                b.reparse(&edit);
                changes.push(change);
            }
            changes
        });
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
            ..Cursor::default()
        };
        self.commit(
            changes,
            Carets::one(before),
            Carets::one(c.selection),
            EditKind::Other,
        );
    }

    pub fn newline(&mut self, c: &mut Cursor) {
        let range = c.selection.range();
        let line = self.line_of(range.start);
        let current = self.line(line);
        let mut indent: String = current
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let before_cursor = self.text(self.line_start(line)..range.start);
        let opener = before_cursor.trim_end().chars().next_back();
        let eol = self.line_ending().as_str();
        if !matches!(opener, Some('{' | '(' | '[')) {
            return self.replace(c, range, &format!("{eol}{indent}"), EditKind::Other);
        }
        let outer = indent.clone();
        indent.push_str(&self.indent.unit());
        let line_end = self.line_start(line) + self.line_len(line);
        let after_cursor = self.text(range.end.min(line_end)..line_end);
        let blanks = after_cursor
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .count();
        let closer = opener
            .and_then(|o| bracket_pair(&o.to_string()))
            .map(|(_, close)| close);
        if range.is_empty() && closer.is_some_and(|close| after_cursor[blanks..].starts_with(close))
        {
            // Enter between a bracket pair puts the closer on its own line below the cursor.
            let at = range.start + eol.len() + indent.chars().count();
            let text = format!("{eol}{indent}{eol}{outer}");
            return self.transact(
                c,
                vec![(range.start..range.end + blanks, text)],
                Selection::cursor(at),
            );
        }
        self.replace(c, range, &format!("{eol}{indent}"), EditKind::Other);
    }

    /// Indents the selected lines, or inserts indentation over the cursor or over a selection
    /// within one line, as VS Code does.
    pub fn tab(&mut self, c: &mut Cursor) {
        let range = c.selection.range();
        if self.tab_indents(c.selection) {
            return self.indent_lines(c, false);
        }
        let unit = match self.indent {
            Indent::Tab => "\t".to_string(),
            Indent::Spaces(n) => " ".repeat(n - self.column_of(range.start) % n),
        };
        self.replace(c, range, &unit, EditKind::Insert);
    }

    /// Whether Tab over `selection` indents its lines: it spans lines or one whole line.
    fn tab_indents(&self, selection: Selection) -> bool {
        let range = selection.range();
        if range.is_empty() {
            return false;
        }
        let (first, last) = (self.line_of(range.start), self.line_of(range.end));
        let line_end = self.line_start(first) + self.line_len(first);
        first != last || (range.start == self.line_start(first) && range.end == line_end)
    }

    /// Tab at every caret: indents all their lines if any caret would, else inserts at each.
    pub fn tab_all(&mut self, cs: &mut Cursors) {
        if cs.all().iter().any(|c| self.tab_indents(c.selection)) {
            self.indent_lines_all(cs, false);
        } else {
            self.edit_each(cs, |b, c| b.tab(c));
        }
    }

    pub fn backspace(&mut self, c: &mut Cursor) {
        let range = c.selection.range();
        if !range.is_empty() {
            return self.replace(c, range, "", EditKind::Delete);
        }
        let head = range.start;
        if head > 0 && head < self.len_chars() && c.closed.contains(head) {
            let pairs = pairs::pairs_for(self.lang());
            if pairs::closer_of(&pairs, self.rope.char(head - 1)) == Some(self.rope.char(head)) {
                return self.replace(c, head - 1..head + 1, "", EditKind::Delete);
            }
        }
        if head > 0 {
            self.replace(c, head - 1..head, "", EditKind::Delete);
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
        let mut cs = Cursors::new(*c);
        let undone = self.undo_all(&mut cs);
        *c = *cs.primary();
        undone
    }

    pub fn redo(&mut self, c: &mut Cursor) -> bool {
        let mut cs = Cursors::new(*c);
        let redone = self.redo_all(&mut cs);
        *c = *cs.primary();
        redone
    }

    /// Takes back the last change, whichever view made it, and puts every caret where it was.
    pub fn undo_all(&mut self, cs: &mut Cursors) -> bool {
        let Some(tx) = self.undo.pop() else {
            return false;
        };
        self.parse_once(|b| {
            for change in tx.changes.iter().rev() {
                let inverse = Change {
                    start: change.start,
                    deleted: change.inserted.clone(),
                    inserted: change.deleted.clone(),
                };
                let edit = b.apply(&inverse);
                b.reparse(&edit);
            }
        });
        cs.restore(&tx.before);
        self.redo.push(tx);
        self.last_edit = None;
        true
    }

    pub fn redo_all(&mut self, cs: &mut Cursors) -> bool {
        let Some(tx) = self.redo.pop() else {
            return false;
        };
        self.parse_once(|b| {
            for change in &tx.changes {
                let edit = b.apply(change);
                b.reparse(&edit);
            }
        });
        cs.restore(&tx.after);
        self.undo.push(tx);
        self.last_edit = None;
        true
    }

    /// Comments or uncomments every line the selection touches.
    pub fn toggle_comment(&mut self, c: &mut Cursor) {
        self.with_one(c, Self::toggle_comment_all);
    }

    /// Comments or uncomments the lines every caret touches, each block of lines on its own;
    /// one caret ends selecting its lines, several keep their places in the text.
    pub fn toggle_comment_all(&mut self, cs: &mut Cursors) {
        let Some(prefix) = self.lang().and_then(Lang::comment_prefix) else {
            return;
        };
        let code = |l: &String| !l.trim().is_empty();
        // Only ASCII blanks count as indent, so their byte length is their char count.
        let blank = |l: &str| l.len() - l.trim_start_matches([' ', '\t']).len();
        let mut changes = Vec::new();
        let mut span = 0..0;
        for (first, last, _) in self.line_blocks(cs, false) {
            let lines: Vec<String> = (first..=last).map(|l| self.line(l)).collect();
            let all_commented = lines
                .iter()
                .filter(|l| code(l))
                .all(|l| l[blank(l)..].starts_with(prefix.trim_end()));
            let indent = lines
                .iter()
                .filter(|l| code(l))
                .map(|l| blank(l))
                .min()
                .unwrap_or(0);
            for (line, text) in (first..=last).zip(&lines) {
                if !code(text) {
                    continue;
                }
                let start = self.line_start(line);
                if all_commented {
                    let at = start + blank(text);
                    let rest = &text[blank(text)..];
                    let strip = if rest.starts_with(prefix) {
                        prefix
                    } else {
                        prefix.trim_end()
                    };
                    changes.push((at..at + strip.chars().count(), String::new()));
                } else {
                    changes.push((start + indent..start + indent, prefix.to_string()));
                }
            }
            span = self.line_start(first)..self.line_start(last) + self.line_len(last);
        }
        let after = if cs.is_multi() {
            let at = |p: usize| through_indents(&changes, p, false);
            cs.all()
                .iter()
                .map(|c| Selection {
                    anchor: at(c.selection.anchor),
                    head: at(c.selection.head),
                })
                .collect()
        } else {
            let grown: isize = changes
                .iter()
                .map(|(r, t)| t.chars().count() as isize - r.len() as isize)
                .sum();
            vec![Selection {
                anchor: span.start,
                head: span.end.saturating_add_signed(grown),
            }]
        };
        self.transact_all(cs, changes, after);
    }

    /// Applies non-overlapping `changes`, in offsets of the current text, as one undo step that
    /// leaves `after` selected.
    fn transact(&mut self, c: &mut Cursor, changes: Vec<(Range<usize>, String)>, after: Selection) {
        let before = c.selection;
        *c = Cursor {
            selection: after,
            ..Cursor::default()
        };
        self.transact_carets(changes, Carets::one(before), Carets::one(after));
    }

    /// As [`Self::transact`], leaving each caret at the selection `after` lists for it.
    fn transact_all(
        &mut self,
        cs: &mut Cursors,
        changes: Vec<(Range<usize>, String)>,
        after: Vec<Selection>,
    ) {
        let before = cs.carets();
        let len = changes.iter().fold(self.len_chars(), |len, (r, t)| {
            len + t.chars().count() - r.len()
        });
        // A block moved below a last line without a break ends one char sooner than it began.
        let all = after
            .into_iter()
            .map(|s| Cursor {
                selection: Selection {
                    anchor: s.anchor.min(len),
                    head: s.head.min(len),
                },
                ..Cursor::default()
            })
            .collect();
        cs.set(all, cs.primary_index());
        self.transact_carets(changes, before, cs.carets());
    }

    fn transact_carets(
        &mut self,
        mut changes: Vec<(Range<usize>, String)>,
        before: Carets,
        after: Carets,
    ) {
        if changes.is_empty() {
            return;
        }
        // Later changes first, so each one's offsets still hold when it is applied.
        changes.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
        let applied = self.parse_once(|b| {
            let mut applied = Vec::with_capacity(changes.len());
            for (range, inserted) in changes {
                let change = Change {
                    start: range.start,
                    deleted: b.rope.slice(range).to_string(),
                    inserted,
                };
                let edit = b.apply(&change);
                b.reparse(&edit);
                applied.push(change);
            }
            applied
        });
        self.commit(applied, before, after, EditKind::Other);
    }

    /// The lines a selection covers; one ending at the start of a line leaves that line out.
    fn selected_lines(&self, selection: Selection) -> (usize, usize) {
        let range = selection.range();
        let first = self.line_of(range.start);
        let mut last = self.line_of(range.end);
        if last > first && range.end == self.line_start(last) {
            last -= 1;
        }
        (first, last)
    }

    /// The line break the file uses, judged by its first line.
    pub fn line_ending(&self) -> LineEnding {
        let first = self.rope.line(0);
        let n = first.len_chars();
        if n >= 2 && first.char(n - 1) == '\n' && first.char(n - 2) == '\r' {
            LineEnding::CrLf
        } else {
            LineEnding::Lf
        }
    }

    /// Highlights the text as `lang`, or as plain text, whatever its file name says.
    pub fn set_lang(&mut self, lang: Option<Lang>) {
        if lang != self.lang() {
            self.syntax = lang.map(|lang| Syntax::new(lang, &self.rope));
        }
    }

    /// Moves every selected line to the next indentation stop, or the previous one with
    /// `outdent`, rewriting its indentation in the buffer's style; one undo step.
    pub fn indent_lines(&mut self, c: &mut Cursor, outdent: bool) {
        self.with_one(c, |b, cs| b.indent_lines_all(cs, outdent));
    }

    /// As [`Self::indent_lines`] for every caret, each line moving once however many share it.
    pub fn indent_lines_all(&mut self, cs: &mut Cursors, outdent: bool) {
        let size = self.indent.size().max(1);
        let mut changes = Vec::new();
        for (first, last, _) in self.line_blocks(cs, false) {
            for line in first..=last {
                let text = self.line(line);
                if text.is_empty() && (first != last || outdent) {
                    continue;
                }
                let (col, chars) = indent_width(&text, TAB_WIDTH);
                let target = match (outdent, col) {
                    (true, 0) => continue,
                    (true, _) => (col - 1) / size * size,
                    (false, _) => (col / size + 1) * size,
                };
                let fill = self.indent.fill(target, TAB_WIDTH);
                // Indentation is ASCII, so its char count is its byte length.
                if fill != text[..chars] {
                    let start = self.line_start(line);
                    changes.push((start..start + chars, fill));
                }
            }
        }
        let after = cs
            .all()
            .iter()
            .map(|c| {
                let s = c.selection;
                if s.is_empty() {
                    Selection::cursor(through_indents(&changes, s.head, false))
                } else {
                    Selection {
                        anchor: through_indents(&changes, s.anchor, s.anchor < s.head),
                        head: through_indents(&changes, s.head, s.head < s.anchor),
                    }
                }
            })
            .collect();
        self.transact_all(cs, changes, after);
    }

    /// Rewrites every line's indentation in `to`'s style and keeps using it; one undo step.
    pub fn convert_indentation(&mut self, c: &mut Cursor, to: Indent) {
        self.with_one(c, |b, cs| b.convert_indentation_all(cs, to));
    }

    /// As [`Self::convert_indentation`], every caret keeping its place.
    pub fn convert_indentation_all(&mut self, cs: &mut Cursors, to: Indent) {
        // As in VS Code, a tab is as wide as one level of the file's current indentation.
        let tab = self.indent.size().max(1);
        let mut changes = Vec::new();
        for line in 0..self.len_lines() {
            let text = self.line(line);
            let (col, chars) = indent_width(&text, tab);
            let fill = to.fill(col, tab);
            if fill != text[..chars] {
                let start = self.line_start(line);
                changes.push((start..start + chars, fill));
            }
        }
        let after = cs
            .all()
            .iter()
            .map(|c| Selection {
                anchor: through_indents(&changes, c.selection.anchor, false),
                head: through_indents(&changes, c.selection.head, false),
            })
            .collect();
        self.indent = to;
        self.transact_all(cs, changes, after);
    }

    /// Swaps the selected lines with the line above or below; the selection moves with them.
    pub fn move_lines(&mut self, c: &mut Cursor, down: bool) {
        self.with_one(c, |b, cs| b.move_lines_all(cs, down));
    }

    /// As [`Self::move_lines`] for every caret; carets on neighbouring lines move as one block.
    pub fn move_lines_all(&mut self, cs: &mut Cursors, down: bool) {
        let eol = self.line_ending().as_str();
        let mut changes = Vec::new();
        let mut after: Vec<Selection> = cs.all().iter().map(|c| c.selection).collect();
        for (first, last, carets) in self.line_blocks(cs, true) {
            if (!down && first == 0) || (down && last + 1 >= self.len_lines()) {
                continue;
            }
            let (top, bottom, other) = if down {
                (first, last + 1, last + 1)
            } else {
                (first - 1, last, first - 1)
            };
            let other_text = self.line(other);
            let mut rows: Vec<String> = (first..=last).map(|l| self.line(l)).collect();
            if down {
                rows.insert(0, other_text.clone());
            } else {
                rows.push(other_text.clone());
            }
            let start = self.line_start(top);
            let end = self.line_start(bottom) + self.line_len(bottom);
            let step = other_text.chars().count() + eol.len();
            let shift = |at: usize| if down { at + step } else { at - step };
            for s in &mut after[carets] {
                *s = Selection {
                    anchor: shift(s.anchor),
                    head: shift(s.head),
                };
            }
            changes.push((start..end, rows.join(eol)));
        }
        if !changes.is_empty() {
            self.transact_all(cs, changes, after);
        }
    }

    /// Duplicates the selected lines; the selection follows the copy below, or stays on the one above.
    pub fn copy_lines(&mut self, c: &mut Cursor, down: bool) {
        self.with_one(c, |b, cs| b.copy_lines_all(cs, down));
    }

    /// As [`Self::copy_lines`] for every caret, lines shared by carets copied once.
    pub fn copy_lines_all(&mut self, cs: &mut Cursors, down: bool) {
        let eol = self.line_ending().as_str();
        let mut changes = Vec::new();
        let mut after: Vec<Selection> = cs.all().iter().map(|c| c.selection).collect();
        let mut below = 0;
        for (first, last, carets) in self.line_blocks(cs, false) {
            let block = (first..=last)
                .map(|l| self.line(l))
                .collect::<Vec<_>>()
                .join(eol);
            let end = self.line_start(last) + self.line_len(last);
            let inserted = format!("{eol}{block}");
            let len = inserted.chars().count();
            let step = below + if down { len } else { 0 };
            for s in &mut after[carets] {
                *s = Selection {
                    anchor: s.anchor + step,
                    head: s.head + step,
                };
            }
            below += len;
            changes.push((end..end, inserted));
        }
        self.transact_all(cs, changes, after);
    }

    /// Deletes the selected lines, leaving the cursor in the same column of the line that follows.
    pub fn delete_lines(&mut self, c: &mut Cursor) {
        self.with_one(c, Self::delete_lines_all);
    }

    /// As [`Self::delete_lines`] for every caret; carets whose lines go together merge.
    pub fn delete_lines_all(&mut self, cs: &mut Cursors) {
        let mut changes = Vec::new();
        let mut after: Vec<Selection> = cs.all().iter().map(|c| c.selection).collect();
        let mut removed = 0;
        for (first, last, carets) in self.line_blocks(cs, true) {
            let col = self.column_of(cs.all()[carets.start].selection.head);
            let (range, landing) = if last + 1 < self.len_lines() {
                let start = self.line_start(first);
                let next = self.line_len(last + 1);
                (start..self.line_start(last + 1), start + col.min(next))
            } else if first > 0 {
                let above = self.line_start(first - 1);
                let len = self.line_len(first - 1);
                (above + len..self.len_chars(), above + col.min(len))
            } else {
                (0..self.len_chars(), 0)
            };
            for s in &mut after[carets] {
                *s = Selection::cursor(landing - removed);
            }
            removed += range.len();
            changes.push((range, String::new()));
        }
        self.transact_all(cs, changes, after);
    }

    /// The runs of lines the carets touch, in order, with the indexes of the carets in each; runs
    /// sharing a line merge, and with `adjacent` so do runs on neighbouring lines.
    fn line_blocks(&self, cs: &Cursors, adjacent: bool) -> Vec<(usize, usize, Range<usize>)> {
        let mut blocks: Vec<(usize, usize, Range<usize>)> = Vec::new();
        for (i, c) in cs.all().iter().enumerate() {
            let (first, last) = self.selected_lines(c.selection);
            match blocks.last_mut() {
                Some((_, end, carets)) if first <= *end + usize::from(adjacent) => {
                    *end = (*end).max(last);
                    carets.end = i + 1;
                }
                _ => blocks.push((first, last, i..i + 1)),
            }
        }
        blocks
    }

    /// Runs a many-caret operation for one caret.
    fn with_one(&mut self, c: &mut Cursor, f: impl FnOnce(&mut Self, &mut Cursors)) {
        let mut cs = Cursors::new(*c);
        f(self, &mut cs);
        *c = *cs.primary();
    }

    /// Runs `op` at every caret, the last in the text first, as one undo step; carets the edits
    /// pass over follow them and carets that end up overlapping merge.
    pub fn edit_each(&mut self, cs: &mut Cursors, mut op: impl FnMut(&mut Self, &mut Cursor)) {
        self.edit_carets(cs, false, |b, c, _| op(b, c));
    }

    /// Pastes `text` at every caret; with as many lines as carets each caret takes one line, as
    /// VS Code spreads a multi-caret copy.
    pub fn paste_all(&mut self, cs: &mut Cursors, text: &str) {
        let lines: Vec<&str> = text
            .strip_suffix('\n')
            .unwrap_or(text)
            .split('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l))
            .collect();
        if cs.is_multi() && lines.len() == cs.len() {
            self.edit_carets(cs, false, |b, c, i| b.insert(c, lines[i]));
        } else {
            self.edit_each(cs, |b, c| b.insert(c, text));
        }
    }

    /// Runs `op` at the primary caret alone, as one undo step the other carets follow.
    pub fn edit_primary(&mut self, cs: &mut Cursors, op: impl FnOnce(&mut Self, &mut Cursor)) {
        let mut op = Some(op);
        self.edit_carets(cs, true, |b, c, _| {
            if let Some(op) = op.take() {
                op(b, c);
            }
        });
    }

    /// Runs `op` at each caret (or the primary alone), handing it the caret's index; see
    /// [`Self::edit_each`].
    pub(crate) fn edit_carets(
        &mut self,
        cs: &mut Cursors,
        only_primary: bool,
        mut op: impl FnMut(&mut Self, &mut Cursor, usize),
    ) {
        if !cs.is_multi() {
            return op(self, &mut cs.all[0], 0);
        }
        let before = cs.carets();
        self.batch = Some(Batch::default());
        let mut edits: Vec<Edit> = Vec::new();
        // How many of `edits` each caret has followed.
        let mut seen = vec![0; cs.len()];
        let mut lowest = usize::MAX;
        self.parse_once(|b| {
            for i in (0..cs.all.len()).rev() {
                if only_primary && i != cs.primary {
                    continue;
                }
                let c = &mut cs.all[i];
                // Edits so far were made after this caret, unless one reached back to it.
                if c.selection.range().end > lowest {
                    for e in &edits {
                        c.map_edit(e);
                    }
                }
                let done = b.batch.as_ref().map_or(0, |batch| batch.edits.len());
                op(b, c, i);
                if let Some(batch) = b.batch.as_ref() {
                    for e in &batch.edits[done..] {
                        lowest = lowest.min(e.at);
                        edits.push(*e);
                    }
                }
                seen[i] = edits.len();
            }
        });
        follow_later_edits(&mut cs.all, &seen, &edits);
        cs.normalize();
        let batch = self.batch.take().unwrap_or_default();
        let after = cs.carets();
        match batch.kind {
            Some(kind) => self.commit(batch.changes, before, after, kind),
            // Carets that typed over closers without editing still let the next keystroke join.
            None => {
                if let Some(last) = self.undo.last_mut()
                    && last.after == before
                {
                    last.after = after;
                }
            }
        }
    }

    /// Runs a move at every caret, merging carets that meet.
    pub fn move_each(&self, cs: &mut Cursors, mut op: impl FnMut(&Self, &mut Cursor)) {
        for c in &mut cs.all {
            op(self, c);
        }
        cs.normalize();
    }

    /// Opens an indented line below the cursor's line, or above it, without splitting it.
    pub fn insert_line(&mut self, c: &mut Cursor, below: bool) {
        let line = self.line_of(c.selection.head);
        let text = self.line(line);
        let mut indent: String = text
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let eol = self.line_ending().as_str();
        let start = self.line_start(line);
        if !below {
            let at = start + indent.chars().count();
            return self.transact(
                c,
                vec![(start..start, format!("{indent}{eol}"))],
                Selection::cursor(at),
            );
        }
        if text.trim_end().ends_with(['{', '(', '[']) {
            indent.push_str(&self.indent.unit());
        }
        let end = start + self.line_len(line);
        let at = end + eol.len() + indent.chars().count();
        self.transact(
            c,
            vec![(end..end, format!("{eol}{indent}"))],
            Selection::cursor(at),
        );
    }

    /// Types `ch` as VS Code does: closing brackets and quotes, typing over closers it inserted,
    /// and wrapping a selection in a pair.
    pub fn type_char(&mut self, c: &mut Cursor, ch: char) {
        let pairs = pairs::pairs_for(self.lang());
        let range = c.selection.range();
        let close = pairs::closer_of(&pairs, ch);
        if !range.is_empty() {
            return match close {
                Some(close) => self.surround(c, ch, close),
                None => self.insert(c, &ch.to_string()),
            };
        }
        let head = range.start;
        let next = (head < self.len_chars()).then(|| self.rope.char(head));
        if next == Some(ch) && c.closed.contains(head) {
            c.closed.retain_map(|at| (at != head).then_some(at));
            c.selection = Selection::cursor(head + 1);
            c.goal_column = None;
            self.extend_last_step(head, c.selection);
            return;
        }
        if let Some(close) = close {
            self.settle_parse();
            let prev = (head > 0).then(|| self.rope.char(head - 1));
            if pairs::should_close(ch, prev, next, self.in_string_or_comment(head)) {
                self.replace(c, range, &format!("{ch}{close}"), EditKind::Insert);
                c.selection = Selection::cursor(head + 1);
                self.extend_last_step(head + 2, c.selection);
                c.closed.push(head + 1);
                return;
            }
        }
        self.insert(c, &ch.to_string());
    }

    /// Lets the next keystroke join the undo step that ended at `was`, now that the cursor moved.
    fn extend_last_step(&mut self, was: usize, now: Selection) {
        if self.batch.is_none()
            && let Some(last) = self.undo.last_mut()
            && last.after.is(Selection::cursor(was))
        {
            last.after = Carets::one(now);
        }
    }

    fn surround(&mut self, c: &mut Cursor, open: char, close: char) {
        let s = c.selection;
        let range = s.range();
        let (start, end) = (range.start + 1, range.end + 1);
        let after = if s.anchor <= s.head {
            Selection {
                anchor: start,
                head: end,
            }
        } else {
            Selection {
                anchor: end,
                head: start,
            }
        };
        let changes = vec![
            (range.start..range.start, open.to_string()),
            (range.end..range.end, close.to_string()),
        ];
        self.transact(c, changes, after);
    }

    /// Whether `at` sits inside a string or comment, where pairs are not auto-closed.
    pub(crate) fn in_string_or_comment(&self, at: usize) -> bool {
        let line = self.line_of(at);
        let byte = self.rope.char_to_byte(at);
        let line_end = self
            .rope
            .char_to_byte(self.line_start(line) + self.line_len(line));
        self.highlights(line..line + 1)
            .iter()
            .any(|(r, token)| match token {
                Token::String | Token::StringSpecial | Token::Escape => {
                    r.start < byte && byte < r.end
                }
                // A line comment runs to the line's end; a block comment ends at its `*/`.
                Token::Comment => {
                    r.start < byte
                        && (byte < r.end
                            || (byte == r.end
                                && r.end >= line_end
                                && !self.rope.byte_slice(r.clone()).to_string().ends_with("*/")))
                }
                _ => false,
            })
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
        self.forget_closers_behind(c);
    }

    /// Auto-inserted closers stop being typed over once the cursor leaves the text between them.
    fn forget_closers_behind(&self, c: &mut Cursor) {
        let head = c.selection.head;
        let line_end = {
            let line = self.line_of(head);
            self.line_start(line) + self.line_len(line)
        };
        c.closed
            .retain_map(|at| (at >= head && at < line_end).then_some(at));
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
        let close = self.syntax.as_ref().and_then(|_| {
            let start = self.rope.line_to_byte(line);
            let end = self.rope.line_to_byte(line + 1);
            let mut best = None;
            for (i, b) in self.rope.byte_slice(start..end).bytes().enumerate() {
                if matches!(b, b'{' | b'[' | b'(')
                    && let Some(partner) = self.bracket_partner(start + i)
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
        if let Some(partner) = self.bracket_partner(byte) {
            return Some((at, self.rope.byte_to_char(partner)));
        }
        self.scan_bracket(at).map(|partner| (at, partner))
    }

    /// The tree's partner for the bracket at `byte`, if the text still holds both brackets there;
    /// a tree waiting for a background parse can point at text that has moved.
    fn bracket_partner(&self, byte: usize) -> Option<usize> {
        let partner = self.syntax.as_ref()?.bracket_partner(byte)?;
        let len = self.rope.len_bytes();
        if byte >= len || partner >= len {
            return None;
        }
        let here = self.rope.byte(byte);
        let (open, close) = bracket_pair(std::str::from_utf8(&[here]).ok()?)?;
        let want = if open.as_bytes()[0] == here {
            close
        } else {
            open
        };
        (self.rope.byte(partner) == want.as_bytes()[0]).then_some(partner)
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
        self.find(query, false, false)
    }

    /// Occurrences of `query` as char ranges, matching case only with `case` and only whole
    /// identifiers with `word`; a match never starts or ends inside a char that lowercases to several.
    pub fn find(&self, query: &str, case: bool, word: bool) -> Vec<Range<usize>> {
        // Each folded char with the index of the char it came from.
        let fold = |chars: &mut dyn Iterator<Item = char>| {
            let mut out = Vec::new();
            for (i, c) in chars.enumerate() {
                if case {
                    out.push((c, i));
                } else {
                    out.extend(c.to_lowercase().map(|f| (f, i)));
                }
            }
            out
        };
        let needle: Vec<char> = fold(&mut query.chars())
            .into_iter()
            .map(|(c, _)| c)
            .collect();
        if needle.is_empty() {
            return Vec::new();
        }
        let hay = fold(&mut self.rope.chars());
        let from = |k: usize| hay.get(k).map_or(self.len_chars(), |&(_, i)| i);
        let mut out = Vec::new();
        let mut k = 0;
        while k + needle.len() <= hay.len() {
            let end = k + needle.len();
            let found = hay[k..end].iter().map(|(c, _)| c).eq(needle.iter())
                && (k == 0 || from(k - 1) != from(k))
                && from(end) != from(end - 1);
            let range = from(k)..from(end - 1) + 1;
            let whole = !word
                || ((range.start == 0 || !is_word(self.rope.char(range.start - 1)))
                    && (range.end == self.len_chars() || !is_word(self.rope.char(range.end))));
            if found && whole {
                out.push(range);
                k = end;
            } else {
                k += 1;
            }
        }
        out
    }

    /// The identifier at `at` or ending there, as a cursor touching a word picks it.
    pub fn word_around(&self, at: usize) -> Option<Range<usize>> {
        self.word_at(at)
            .or_else(|| at.checked_sub(1).and_then(|a| self.word_at(a)))
    }
}

/// The file a save to `path` must replace: symlinks are followed, so the link itself survives.
fn link_target(path: &Path) -> PathBuf {
    if let Ok(real) = fs::canonicalize(path) {
        return real;
    }
    // A dangling link is followed by hand, so saving creates the file it points at.
    let mut at = path.to_path_buf();
    for _ in 0..40 {
        match fs::read_link(&at) {
            Ok(next) => {
                at = at
                    .parent()
                    .map_or_else(|| next.clone(), |dir| dir.join(&next))
            }
            Err(_) => break,
        }
    }
    at
}

fn stamp(path: &Path) -> Option<Stamp> {
    fs::metadata(path).ok().map(|m| Stamp::of(&m))
}

/// A UTF-8 text file's contents and modification time; binary and very large files are refused.
fn read_text(path: &Path) -> Result<(String, Stamp)> {
    // Non-blocking, so a FIFO without a writer fails the checks below instead of hanging here.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    let too_large = || anyhow::anyhow!("{} is larger than 50 MB", path.display());
    if meta.len() > MAX_FILE {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE {
        return Err(too_large());
    }
    if bytes.iter().take(8192).any(|b| *b == 0) {
        bail!("{} looks like a binary file", path.display());
    }
    let text =
        String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))?;
    Ok((text, Stamp::of(&meta)))
}

/// A file's text as read from disk, as the one edit that turns a buffer's text into it.
pub(crate) struct DiskText {
    stamp: Stamp,
    /// The chars replaced and their replacement; `None` when the text is the same.
    change: Option<(Range<usize>, String)>,
}

/// Reads `path` and compares it with `rope`; it leaves the buffer alone, so it can run off the UI thread.
pub(crate) fn read_disk_text(path: &Path, rope: &Rope) -> Result<DiskText> {
    let (text, stamp) = read_text(path)?;
    Ok(DiskText {
        stamp,
        change: differing_span(rope, &text),
    })
}

/// The chars of `rope` between its common prefix and suffix with `text`, and what replaces them.
fn differing_span(rope: &Rope, text: &str) -> Option<(Range<usize>, String)> {
    let new = text.as_bytes();
    let old_len = rope.len_bytes();
    let mut prefix = 0;
    for chunk in rope.chunks() {
        let same = chunk
            .bytes()
            .zip(&new[prefix..])
            .take_while(|(a, b)| a == *b)
            .count();
        prefix += same;
        if same < chunk.len() {
            break;
        }
    }
    if prefix == old_len && prefix == new.len() {
        return None;
    }
    // Equal bytes split chars at the same places, so a boundary in `text` is one in `rope` too.
    while !text.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let mut suffix = rope
        .bytes_at(old_len)
        .reversed()
        .zip(new.iter().rev())
        .take(old_len.min(new.len()) - prefix)
        .take_while(|(a, b)| a == *b)
        .count();
    while !text.is_char_boundary(new.len() - suffix) {
        suffix -= 1;
    }
    let range = rope.byte_to_char(prefix)..rope.byte_to_char(old_len - suffix);
    Some((range, text[prefix..new.len() - suffix].to_string()))
}

/// Moves each caret past the edits made after its own, `seen[i]` being how many caret `i` has
/// followed; edits that all lie below a caret, last first, only shift it.
fn follow_later_edits(carets: &mut [Cursor], seen: &[usize], edits: &[Edit]) {
    let n = edits.len();
    let mut shift = vec![0isize; n + 1];
    let mut downward = vec![true; n + 1];
    for k in (0..n).rev() {
        let e = &edits[k];
        shift[k] = shift[k + 1] + e.inserted as isize - e.removed as isize;
        downward[k] = downward[k + 1]
            && edits
                .get(k + 1)
                .is_none_or(|next| next.at + next.removed <= e.at);
    }
    for (c, &k) in carets.iter_mut().zip(seen) {
        if k == n {
            continue;
        }
        if downward[k] && edits[k].at + edits[k].removed < c.selection.range().start {
            c.shift(shift[k]);
        } else {
            for e in &edits[k..] {
                c.map_edit(e);
            }
        }
    }
}

/// Where `at` lands after indentation `changes` (ascending, one per line); with `stay`, a point in
/// the indentation keeps its column, as the start of a selection does in VS Code.
fn through_indents(changes: &[(Range<usize>, String)], at: usize, stay: bool) -> usize {
    let mut shift = 0isize;
    for (r, text) in changes {
        let len = text.chars().count();
        if at > r.end || (at == r.end && !stay) {
            shift += len as isize - r.len() as isize;
        } else if at >= r.start {
            return (r.start as isize + shift) as usize + (at - r.start).min(len);
        } else {
            break;
        }
    }
    (at as isize + shift) as usize
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
    fn toggling_comments_keeps_crlf_line_breaks() {
        let mut b = buf("a := 1\r\nb := 2\r\nc := 3\r\n", "/x/a.go");
        let mut c = select(0, b.line_start(2));
        b.toggle_comment(&mut c);
        assert_eq!(b.full_text(), "// a := 1\r\n// b := 2\r\nc := 3\r\n");
        assert_eq!(c.selection, select(0, b.line_start(1) + 9).selection);
        b.toggle_comment(&mut c);
        assert_eq!(b.full_text(), "a := 1\r\nb := 2\r\nc := 3\r\n");
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
    fn comments_lines_indented_with_wide_blanks_without_panicking() {
        let text = "\u{a0}\u{a0}x := 1\n  y := 2\n\u{3000}z := 3\n";
        let mut b = buf(text, "/x/a.go");
        let mut c = Cursor {
            selection: Selection {
                anchor: 0,
                head: b.len_chars(),
            },
            ..Default::default()
        };
        b.toggle_comment(&mut c);
        assert_eq!(
            b.rope().to_string(),
            "// \u{a0}\u{a0}x := 1\n//   y := 2\n// \u{3000}z := 3\n"
        );
        b.toggle_comment(&mut c);
        assert_eq!(b.rope().to_string(), text);
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

    #[test]
    fn only_regular_files_of_a_sane_size_are_opened() {
        let file = temp_file("special", "x\n");
        let dir = file.parent().unwrap().to_path_buf();
        let fifo = dir.join("pipe");
        let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) }, 0);
        let err = Buffer::open(&fifo).err().unwrap();
        assert!(err.to_string().contains("not a regular file"), "{err}");
        assert!(Buffer::open(Path::new("/dev/zero")).is_err());
        assert!(Buffer::open(&dir).is_err());
        let huge = dir.join("huge.txt");
        fs::File::create(&huge)
            .unwrap()
            .set_len(MAX_FILE + 1)
            .unwrap();
        assert!(Buffer::open(&huge).is_err());
        assert_eq!(Buffer::open(&file).unwrap().full_text(), "x\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    fn temp_file(name: &str, text: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("athena-buf-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("main.go");
        fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn saving_through_a_symlink_updates_the_target_and_keeps_the_link() {
        let target = temp_file("symlink", "old\n");
        let dir = target.parent().unwrap().to_path_buf();
        let link = dir.join("CLAUDE.md");
        std::os::unix::fs::symlink("main.go", &link).unwrap();
        let mut b = Buffer::open(&link).unwrap();
        b.insert(&mut Cursor::default(), "new ");
        b.save_checked().unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "new old\n");
        assert_eq!(b.path.as_deref(), Some(link.as_path()));
        assert!(!b.changed_on_disk());

        fs::remove_file(&target).unwrap();
        b.insert(&mut Cursor::default(), "again ");
        b.save().unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "again new old\n");
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names.len(), 2, "no temp file left: {names:?}");
        fs::remove_dir_all(&dir).unwrap();
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
    fn a_failed_save_leaves_no_temp_file_and_hard_links_stay_linked() {
        let path = temp_file("links", "a\n");
        let dir = path.parent().unwrap().to_path_buf();
        let twin = dir.join("twin.go");
        fs::hard_link(&path, &twin).unwrap();
        let mut b = Buffer::open(&path).unwrap();
        b.insert(&mut Cursor::default(), "x");
        b.save().unwrap();
        assert_eq!(fs::read_to_string(&twin).unwrap(), "xa\n");

        let folder = dir.join("folder");
        fs::create_dir_all(folder.join("inside")).unwrap();
        let mut b = Buffer::new("text", Some(folder.clone()));
        assert!(b.save().is_err());
        let mut names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["folder", "main.go", "twin.go"], "no temp file left");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_checked_save_never_recreates_a_deleted_file() {
        let path = temp_file("deleted", "a\n");
        let mut b = Buffer::open(&path).unwrap();
        b.insert(&mut Cursor::default(), "x");
        fs::remove_file(&path).unwrap();
        assert_eq!(b.disk_state(), DiskState::Deleted);
        assert!(matches!(b.save_checked(), Err(SaveError::Deleted)));
        assert!(!path.exists(), "the deleted file stays deleted");
        b.save().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "xa\n");
        assert_eq!(b.disk_state(), DiskState::Unchanged);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_rewrite_that_keeps_the_modification_time_still_counts_as_a_change() {
        let path = temp_file("same-mtime", "a\n");
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        let b = Buffer::open(&path).unwrap();
        fs::write(&path, "longer\n").unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert_eq!(b.disk_state(), DiskState::Changed);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
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
    fn reload_diffs_on_char_boundaries() {
        let cases = [
            ("aé\n", "aè\n"),
            ("aéb", "a©b"),
            ("aaa", "aaaa"),
            ("aaaa", "aaa"),
            ("日本語", "日本人語"),
            ("ab€cd", "ab€€cd"),
            ("", "x"),
            ("x", ""),
        ];
        for (old, new) in cases {
            let mut rope = Rope::from_str(old);
            let (range, inserted) = differing_span(&rope, new).unwrap();
            rope.remove(range.clone());
            rope.insert(range.start, &inserted);
            assert_eq!(rope.to_string(), new, "{old:?} -> {new:?}");
        }
        assert_eq!(
            differing_span(&Rope::from_str("aé\n"), "aè\n"),
            Some((1..2, "è".to_string()))
        );
        assert_eq!(differing_span(&Rope::from_str("héllo"), "héllo"), None);
    }

    #[test]
    fn consecutive_reloads_undo_as_one_step() {
        let path = temp_file("reload-twice", "one\n");
        let mut b = Buffer::open(&path).unwrap();
        let mut c = Cursor::default();
        write_elsewhere(&path, "two\n");
        b.reload_from_disk(&mut c).unwrap();
        write_elsewhere(&path, "three é\n");
        b.reload_from_disk(&mut c).unwrap();
        assert_eq!(b.full_text(), "three é\n");
        assert!(!b.is_dirty());
        assert!(b.undo(&mut c));
        assert_eq!(b.full_text(), "one\n");
        assert!(!b.undo(&mut c), "both reloads were one step");
        b.redo(&mut c);
        b.insert(&mut c, "x");
        write_elsewhere(&path, "four\n");
        b.reload_from_disk(&mut c).unwrap();
        b.undo(&mut c);
        assert_eq!(
            b.full_text(),
            "three éx\n",
            "an edit between reloads splits them"
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn typing_after_reloading_identical_text_is_unsaved() {
        let path = temp_file("reload-same", "one\n");
        let mut b = Buffer::open(&path).unwrap();
        let mut c = Cursor::at(3);
        b.insert(&mut c, "!");
        write_elsewhere(&path, "one!\n");
        b.reload_from_disk(&mut c).unwrap();
        assert!(!b.is_dirty());
        b.insert(&mut c, "?");
        assert!(b.is_dirty(), "the keystroke is its own undo step");
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

        let mut b = buf("ab", "/x/a.txt");
        let mut c = Cursor::at(1);
        b.insert(&mut c, "\r");
        assert_eq!(b.len_lines(), 2);
        assert_eq!(lines(&b, 0), vec![(0, 0, 1)], "a lone CR breaks a line");
        let mut b = buf("a\rX\nb", "/x/a.txt");
        let mut c = select(2, 3);
        b.backspace(&mut c);
        assert_eq!(b.len_lines(), 2);
        assert_eq!(
            lines(&b, 0),
            vec![(0, 1, 0)],
            "joining CR and LF into one break merges lines"
        );
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
    fn overlapping_extra_edits_are_dropped_instead_of_panicking() {
        let mut b = buf("abcdefghij", "/x/a.txt");
        let mut c = Cursor::at(10);
        b.apply_edits(
            &mut c,
            &[
                (9..10, "J".into()),
                (5..8, "".into()),
                (2..7, "X".into()),
                (0..1, "A".into()),
                (40..50, "end".into()),
            ],
            None,
        );
        assert_eq!(b.full_text(), "AbcdeiJend");
        assert_eq!(c.head(), 7, "after the main edit's text");
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

    fn select(anchor: usize, head: usize) -> Cursor {
        Cursor {
            selection: Selection { anchor, head },
            ..Default::default()
        }
    }

    /// Runs `op` and checks it undoes in one step back to the original text and selection.
    fn one_step(b: &mut Buffer, c: &mut Cursor, op: impl FnOnce(&mut Buffer, &mut Cursor)) {
        let (text, before) = (b.full_text(), c.selection);
        op(b, c);
        let (after_text, after) = (b.full_text(), c.selection);
        assert!(b.undo(c));
        assert_eq!(b.full_text(), text, "undo restores the text");
        assert_eq!(c.selection, before, "undo restores the selection");
        b.redo(c);
        assert_eq!((b.full_text(), c.selection), (after_text, after));
    }

    #[test]
    fn tab_indents_selected_lines_instead_of_replacing_them() {
        let mut b = buf("a\n  b\n\nc\n", "/x/a.ts");
        assert_eq!(b.indent, Indent::Spaces(2));
        let mut c = select(0, b.line_start(3) + 1);
        one_step(&mut b, &mut c, |b, c| b.tab(c));
        assert_eq!(
            b.full_text(),
            "  a\n    b\n\n  c\n",
            "the empty line stays empty"
        );
        assert_eq!(c.selection, select(0, b.line_start(3) + 3).selection);
        b.indent_lines(&mut c, true);
        b.indent_lines(&mut c, true);
        assert_eq!(b.full_text(), "a\nb\n\nc\n");
    }

    #[test]
    fn tab_replaces_a_selection_inside_one_line() {
        let mut b = buf("let abc = 1", "/x/a.ts");
        let mut c = select(4, 7);
        b.tab(&mut c);
        assert_eq!(b.full_text(), "let    = 1", "two spaces to the next stop");
        let mut b = buf("let abc = 1", "/x/a.ts");
        let mut c = select(0, 11);
        b.tab(&mut c);
        assert_eq!(b.full_text(), "  let abc = 1", "a whole line indents");
    }

    #[test]
    fn indenting_snaps_to_stops_and_rewrites_mixed_blanks() {
        let mut b = buf("   x\n\t  y\n", "/x/a.py");
        b.indent = Indent::Spaces(4);
        let mut c = select(0, b.len_chars());
        b.indent_lines(&mut c, false);
        assert_eq!(b.full_text(), "    x\n        y\n");
        b.indent_lines(&mut c, true);
        assert_eq!(b.full_text(), "x\n    y\n");

        let mut b = buf("  \tz\n", "/x/main.go");
        let mut c = Cursor::at(b.line_start(0) + 3);
        b.indent_lines(&mut c, false);
        assert_eq!(b.full_text(), "\t\tz\n", "tabs in a tab-indented file");
        assert_eq!(c.head(), 2, "the cursor stays before the code");
    }

    #[test]
    fn outdent_keeps_the_cursor_with_its_text_including_wide_chars() {
        let mut b = buf("    héllo 😀\n", "/x/a.py");
        let mut c = Cursor::at(9);
        one_step(&mut b, &mut c, |b, c| b.indent_lines(c, true));
        assert_eq!(b.full_text(), "héllo 😀\n");
        assert_eq!(c.head(), 5);
        let mut c = Cursor::at(2);
        b.indent_lines(&mut c, true);
        assert_eq!(b.full_text(), "héllo 😀\n", "nothing to outdent");
    }

    #[test]
    fn moves_lines_up_and_down_with_the_selection() {
        let mut b = buf("one\ntwo\nthree\n", "/x/a.txt");
        let mut c = Cursor::at(5);
        one_step(&mut b, &mut c, |b, c| b.move_lines(c, true));
        assert_eq!(b.full_text(), "one\nthree\ntwo\n");
        assert_eq!(b.line_of(c.head()), 2);
        assert_eq!(b.column_of(c.head()), 1);
        b.move_lines(&mut c, false);
        b.move_lines(&mut c, false);
        assert_eq!(b.full_text(), "two\none\nthree\n");
        b.move_lines(&mut c, false);
        assert_eq!(
            b.full_text(),
            "two\none\nthree\n",
            "the first line stays put"
        );

        let mut b = buf("a\nb\nc\nd", "/x/a.txt");
        let mut c = select(b.line_start(1), b.line_start(3));
        b.move_lines(&mut c, false);
        assert_eq!(
            b.full_text(),
            "b\nc\na\nd",
            "a selection ending at a line start leaves it out"
        );
        assert_eq!(c.selection, select(0, b.line_start(2)).selection);
    }

    #[test]
    fn moving_a_line_below_the_last_keeps_the_selection_inside_the_text() {
        let mut b = buf("a\nb", "/x/a.txt");
        let mut c = select(0, 2);
        one_step(&mut b, &mut c, |b, c| b.move_lines(c, true));
        assert_eq!(b.full_text(), "b\na");
        assert_eq!(c.selection, select(2, 3).selection);
        b.type_char(&mut c, 'x');
        assert_eq!(
            b.full_text(),
            "b\nx",
            "the next key replaces the moved line"
        );
    }

    #[test]
    fn moving_keeps_crlf_line_breaks() {
        let mut b = buf("a\r\nb\r\nc\r\n", "/x/a.txt");
        assert_eq!(b.line_ending(), LineEnding::CrLf);
        let mut c = Cursor::at(0);
        b.move_lines(&mut c, true);
        assert_eq!(b.full_text(), "b\r\na\r\nc\r\n");
        assert_eq!(c.head(), 3);
    }

    #[test]
    fn lines_break_only_where_language_servers_break_them() {
        let b = buf(
            "a\u{b}b\u{c}c\u{85}d\u{2028}e\u{2029}f\ng\r\nh\ri",
            "/x/a.txt",
        );
        assert_eq!(b.len_lines(), 4, "LF, CRLF and a lone CR break lines");
        assert_eq!(b.line(0), "a\u{b}b\u{c}c\u{85}d\u{2028}e\u{2029}f");
        assert_eq!(b.utf16_position(b.len_chars()), (3, 1));
        assert_eq!(b.char_at_utf16(1, 1), b.line_start(1) + 1);
    }

    #[test]
    fn copies_lines_and_follows_the_copy_down() {
        let mut b = buf("x\ny\n", "/x/a.txt");
        let mut c = Cursor::at(1);
        one_step(&mut b, &mut c, |b, c| b.copy_lines(c, true));
        assert_eq!(b.full_text(), "x\nx\ny\n");
        assert_eq!(c.head(), 3);
        let mut c = Cursor::at(b.line_start(2));
        b.copy_lines(&mut c, false);
        assert_eq!(b.full_text(), "x\nx\ny\ny\n");
        assert_eq!(c.head(), b.line_start(2), "copy up stays on the upper line");
    }

    #[test]
    fn deletes_lines_keeping_the_column() {
        let mut b = buf("first\nsecond\nthird", "/x/a.txt");
        let mut c = Cursor::at(4);
        one_step(&mut b, &mut c, |b, c| b.delete_lines(c));
        assert_eq!(b.full_text(), "second\nthird");
        assert_eq!(c.head(), 4);
        let mut c = Cursor::at(b.len_chars());
        b.delete_lines(&mut c);
        assert_eq!(
            b.full_text(),
            "second",
            "the last line takes the break before it"
        );
        assert_eq!(c.head(), 5);
        b.delete_lines(&mut c);
        assert_eq!(b.full_text(), "");
    }

    #[test]
    fn inserts_lines_below_and_above_without_splitting() {
        let mut b = buf("\tif x {\n\t}\n", "/x/main.go");
        let mut c = Cursor::at(3);
        one_step(&mut b, &mut c, |b, c| b.insert_line(c, true));
        assert_eq!(b.full_text(), "\tif x {\n\t\t\n\t}\n");
        assert_eq!(c.head(), b.line_start(1) + 2);
        let mut c = Cursor::at(b.line_start(2) + 1);
        b.insert_line(&mut c, false);
        assert_eq!(b.full_text(), "\tif x {\n\t\t\n\t\n\t}\n");
        assert_eq!(c.head(), b.line_start(2) + 1);
    }

    #[test]
    fn enter_over_a_downward_multi_line_selection_replaces_it() {
        let mut b = buf("  ab\n  cd\n  ef", "/x/a.ts");
        let mut c = select(3, b.line_start(2) + 3);
        b.newline(&mut c);
        assert_eq!(b.full_text(), "  a\n  f");
    }

    #[test]
    fn enter_between_brackets_opens_an_indented_line() {
        let mut b = buf("  f({})", "/x/a.ts");
        let mut c = Cursor::at(5);
        b.newline(&mut c);
        assert_eq!(b.full_text(), "  f({\n    \n  })");
        assert_eq!(c.head(), 10);
        assert!(b.undo(&mut c));
        assert_eq!(b.full_text(), "  f({})");
    }

    #[test]
    fn enter_keeps_a_closer_behind_non_ascii_blanks_and_the_file_line_break() {
        let mut b = buf("f(\u{a0})", "/x/a.ts");
        let mut c = Cursor::at(2);
        b.newline(&mut c);
        assert_eq!(b.full_text(), "f(\n  \u{a0})", "the closer is not deleted");

        let mut b = buf("a\r\nf({})\r\n", "/x/a.ts");
        let mut c = Cursor::at(b.line_start(1) + 3);
        b.newline(&mut c);
        assert_eq!(b.full_text(), "a\r\nf({\r\n  \r\n})\r\n");
        assert_eq!(c.head(), b.line_start(2) + 2);
        b.newline(&mut c);
        assert_eq!(b.full_text(), "a\r\nf({\r\n  \r\n  \r\n})\r\n");
    }

    fn typed(b: &mut Buffer, c: &mut Cursor, keys: &str) {
        for ch in keys.chars() {
            b.type_char(c, ch);
        }
    }

    #[test]
    fn closes_pairs_and_types_over_what_it_closed() {
        let mut b = buf("", "/x/a.ts");
        let mut c = Cursor::default();
        typed(&mut b, &mut c, "f(\"a");
        assert_eq!(b.full_text(), "f(\"a\")");
        typed(&mut b, &mut c, "\")");
        assert_eq!(b.full_text(), "f(\"a\")", "typed over, not doubled");
        assert_eq!(c.head(), 6);
        typed(&mut b, &mut c, ";");
        assert!(b.undo(&mut c));
        assert_eq!(b.full_text(), "", "typing undoes as one step");
    }

    #[test]
    fn only_closers_it_inserted_are_typed_over() {
        let mut b = buf("x)", "/x/a.ts");
        let mut c = Cursor::at(1);
        typed(&mut b, &mut c, ")");
        assert_eq!(b.full_text(), "x))");
        let mut b = buf("", "/x/a.ts");
        let mut c = Cursor::default();
        typed(&mut b, &mut c, "(");
        b.move_left(&mut c, false);
        b.move_right(&mut c, false);
        b.move_right(&mut c, false);
        b.move_left(&mut c, false);
        typed(&mut b, &mut c, ")");
        assert_eq!(b.full_text(), "())", "leaving the pair forgets its closer");
    }

    #[test]
    fn pairs_are_not_closed_before_words_after_words_or_in_strings_and_comments() {
        let mut b = buf("foo", "/x/a.ts");
        let mut c = Cursor::at(0);
        typed(&mut b, &mut c, "(");
        assert_eq!(b.full_text(), "(foo");

        let mut b = buf("const s = \"ab\"; // note\n", "/x/a.ts");
        let mut c = Cursor::at(12);
        assert!(b.in_string_or_comment(12));
        typed(&mut b, &mut c, "(");
        assert_eq!(b.full_text(), "const s = \"a(b\"; // note\n");
        let end = b.line_len(0);
        assert!(b.in_string_or_comment(end));
        let mut c = Cursor::at(end);
        typed(&mut b, &mut c, "'");
        assert_eq!(b.full_text(), "const s = \"a(b\"; // note'\n");
        assert!(!b.in_string_or_comment(15), "after the closing quote");

        let mut b = buf("let it = ", "/x/a.rs");
        let mut c = Cursor::at(9);
        typed(&mut b, &mut c, "'");
        assert_eq!(
            b.full_text(),
            "let it = '",
            "Rust lifetimes keep a lone quote"
        );
    }

    #[test]
    fn backspace_deletes_an_empty_pair_it_inserted() {
        let mut b = buf("", "/x/main.go");
        let mut c = Cursor::default();
        typed(&mut b, &mut c, "[");
        b.backspace(&mut c);
        assert_eq!(b.full_text(), "");
        let mut b = buf("()", "/x/main.go");
        let mut c = Cursor::at(1);
        b.backspace(&mut c);
        assert_eq!(
            b.full_text(),
            ")",
            "a pair typed by hand loses only the opener"
        );
    }

    #[test]
    fn surrounds_a_selection_keeping_it_selected() {
        let mut b = buf("a héllo b", "/x/a.ts");
        let mut c = select(7, 2);
        one_step(&mut b, &mut c, |b, c| b.type_char(c, '"'));
        assert_eq!(b.full_text(), "a \"héllo\" b");
        assert_eq!(c.selection, select(8, 3).selection);
        b.type_char(&mut c, 'x');
        assert_eq!(
            b.full_text(),
            "a \"x\" b",
            "other keys replace the selection"
        );
    }

    #[test]
    fn converts_indentation_between_tabs_and_spaces() {
        let mut b = buf("a\n\tb\n\t\tc\n", "/x/a.py");
        let mut c = Cursor::at(b.line_start(2) + 2);
        one_step(&mut b, &mut c, |b, c| {
            b.convert_indentation(c, Indent::Spaces(4))
        });
        assert_eq!(b.full_text(), "a\n    b\n        c\n");
        assert_eq!(c.head(), b.line_start(2) + 8);
        assert_eq!(b.indent, Indent::Spaces(4));
    }

    #[test]
    fn converting_to_tabs_turns_each_indent_level_into_one_tab() {
        let mut b = buf("a\n  b\n     c\n", "/x/a.ts");
        assert_eq!(b.indent, Indent::Spaces(2));
        let mut c = Cursor::at(0);
        one_step(&mut b, &mut c, |b, c| b.convert_indentation(c, Indent::Tab));
        assert_eq!(b.full_text(), "a\n\tb\n\t\t c\n");
    }

    fn carets(at: &[(usize, usize)]) -> Cursors {
        let mut cs = Cursors::default();
        let all = at
            .iter()
            .map(|&(anchor, head)| Cursor {
                selection: Selection { anchor, head },
                ..Cursor::default()
            })
            .collect();
        cs.set(all, 0);
        cs
    }

    fn spots(cs: &Cursors) -> Vec<(usize, usize)> {
        cs.all()
            .iter()
            .map(|c| (c.selection.anchor, c.selection.head))
            .collect()
    }

    #[test]
    fn typing_at_every_caret_undoes_as_one_step() {
        let mut b = buf("ab\nab\nab", "/x/a.txt");
        let mut cs = carets(&[(2, 2), (5, 5), (8, 8)]);
        for ch in "xy".chars() {
            b.edit_each(&mut cs, |b, c| b.type_char(c, ch));
        }
        assert_eq!(b.full_text(), "abxy\nabxy\nabxy");
        assert_eq!(spots(&cs), [(4, 4), (9, 9), (14, 14)]);
        assert!(b.undo_all(&mut cs));
        assert_eq!(b.full_text(), "ab\nab\nab");
        assert_eq!(spots(&cs), [(2, 2), (5, 5), (8, 8)]);
        assert!(!b.undo_all(&mut cs), "the keystrokes joined one step");
        assert!(b.redo_all(&mut cs));
        assert_eq!(b.full_text(), "abxy\nabxy\nabxy");
        assert_eq!(spots(&cs), [(4, 4), (9, 9), (14, 14)]);
    }

    #[test]
    fn deleting_at_every_caret_merges_carets_that_meet() {
        let mut b = buf("abc abc", "/x/a.txt");
        let mut cs = carets(&[(1, 1), (2, 2), (5, 5)]);
        b.edit_each(&mut cs, Buffer::backspace);
        assert_eq!(b.full_text(), "c bc");
        assert_eq!(spots(&cs), [(0, 0), (2, 2)], "the first two carets met");
        b.edit_each(&mut cs, Buffer::delete_forward);
        assert_eq!(b.full_text(), " c");
        let mut cs = carets(&[(0, 1), (2, 2)]);
        b.edit_each(&mut cs, Buffer::backspace);
        assert_eq!(b.full_text(), "");
        assert_eq!(spots(&cs), [(0, 0)]);
    }

    #[test]
    fn newline_at_every_caret_keeps_each_line_indent() {
        let mut b = buf("  a\n\tb {", "/x/a.ts");
        let mut cs = carets(&[(3, 3), (b.len_chars(), b.len_chars())]);
        b.edit_each(&mut cs, Buffer::newline);
        assert_eq!(b.full_text(), "  a\n  \n\tb {\n\t\t");
        let ends: Vec<usize> = cs.all().iter().map(Cursor::head).collect();
        assert_eq!(ends, [6, b.len_chars()]);
        assert!(b.undo_all(&mut cs));
        assert_eq!(b.full_text(), "  a\n\tb {");
    }

    #[test]
    fn pasting_as_many_lines_as_carets_spreads_them() {
        let mut b = buf("a\nb\nc", "/x/a.txt");
        let mut cs = carets(&[(1, 1), (3, 3), (5, 5)]);
        b.paste_all(&mut cs, "1\r\n2\r\n3\r\n");
        assert_eq!(b.full_text(), "a1\nb2\nc3");
        assert_eq!(spots(&cs), [(2, 2), (5, 5), (8, 8)]);
        b.paste_all(&mut cs, "-+");
        assert_eq!(
            b.full_text(),
            "a1-+\nb2-+\nc3-+",
            "other text goes to every caret"
        );
        assert!(b.undo_all(&mut cs) && b.undo_all(&mut cs));
        assert_eq!(b.full_text(), "a\nb\nc");
    }

    #[test]
    fn pairs_close_and_type_over_at_every_caret() {
        let mut b = buf("f\ng", "/x/a.ts");
        let mut cs = carets(&[(1, 1), (3, 3)]);
        b.edit_each(&mut cs, |b, c| b.type_char(c, '('));
        assert_eq!(b.full_text(), "f()\ng()");
        assert_eq!(spots(&cs), [(2, 2), (6, 6)]);
        b.edit_each(&mut cs, |b, c| b.type_char(c, ')'));
        assert_eq!(b.full_text(), "f()\ng()", "typed over the closers");
        assert_eq!(spots(&cs), [(3, 3), (7, 7)]);
        let mut cs = carets(&[(0, 1), (4, 5)]);
        b.edit_each(&mut cs, |b, c| b.type_char(c, '['));
        assert_eq!(b.full_text(), "[f]()\n[g]()", "selections are wrapped");
        assert_eq!(spots(&cs), [(1, 2), (7, 8)]);
    }

    #[test]
    fn line_operations_touch_a_shared_line_once() {
        let mut b = buf("a\nb\nc\n", "/x/a.ts");
        let mut cs = carets(&[(0, 0), (1, 1), (4, 4)]);
        b.indent_lines_all(&mut cs, false);
        assert_eq!(b.full_text(), "  a\nb\n  c\n");
        assert_eq!(spots(&cs), [(2, 2), (3, 3), (8, 8)]);
        b.toggle_comment_all(&mut cs);
        assert_eq!(b.full_text(), "  // a\nb\n  // c\n");
        assert_eq!(
            spots(&cs),
            [(5, 5), (6, 6)]
                .into_iter()
                .chain([(14, 14)])
                .collect::<Vec<_>>()
        );
        b.toggle_comment_all(&mut cs);
        assert_eq!(b.full_text(), "  a\nb\n  c\n");
        b.copy_lines_all(&mut cs, true);
        assert_eq!(b.full_text(), "  a\n  a\nb\n  c\n  c\n");
        assert_eq!(spots(&cs), [(6, 6), (7, 7), (16, 16)]);
        b.delete_lines_all(&mut cs);
        assert_eq!(b.full_text(), "  a\nb\n  c\n");
        assert!(b.undo_all(&mut cs));
        assert_eq!(b.full_text(), "  a\n  a\nb\n  c\n  c\n");
    }

    #[test]
    fn moving_lines_carries_neighbouring_carets_as_one_block() {
        let mut b = buf("a\nb\nc\nd\ne", "/x/a.txt");
        let mut cs = carets(&[(0, 0), (2, 2), (6, 6)]);
        b.move_lines_all(&mut cs, true);
        assert_eq!(
            b.full_text(),
            "c\na\nb\nd\ne".replace("d\ne", "e\nd").as_str()
        );
        assert_eq!(spots(&cs), [(2, 2), (4, 4), (8, 8)]);
        b.move_lines_all(&mut cs, false);
        assert_eq!(b.full_text(), "a\nb\nc\nd\ne");
        assert_eq!(spots(&cs), [(0, 0), (2, 2), (6, 6)]);
        let mut top = carets(&[(0, 0), (6, 6)]);
        b.move_lines_all(&mut top, false);
        assert_eq!(b.full_text(), "a\nb\nd\nc\ne", "a block at the top stays");
        assert_eq!(spots(&top), [(0, 0), (4, 4)]);
    }

    #[test]
    fn an_edit_at_the_primary_moves_the_other_carets() {
        let mut b = buf("x\nfoo\nfoo", "/x/a.ts");
        let mut cs = carets(&[(5, 5), (9, 9)]);
        let edits = vec![(5..5, "d".to_string()), (0..0, "use y;\n".to_string())];
        b.edit_primary(&mut cs, |b, c| b.apply_edits(c, &edits, None));
        assert_eq!(b.full_text(), "use y;\nx\nfood\nfoo");
        assert_eq!(spots(&cs), [(13, 13), (17, 17)]);
        assert!(b.undo_all(&mut cs));
        assert_eq!(spots(&cs), [(5, 5), (9, 9)]);
    }

    #[test]
    fn overlapping_carets_merge_and_keep_the_primary() {
        let mut cs = carets(&[(4, 8), (0, 2), (6, 10), (2, 2), (12, 12)]);
        assert_eq!(spots(&cs), [(0, 2), (4, 10), (12, 12)]);
        assert_eq!(cs.primary_index(), 1, "the merged caret holds the primary");
        cs.add(Cursor::at(3));
        assert_eq!(spots(&cs), [(0, 2), (3, 3), (4, 10), (12, 12)]);
        assert_eq!(cs.primary().head(), 3);
        cs.add(Cursor {
            selection: Selection { anchor: 3, head: 2 },
            ..Cursor::default()
        });
        assert_eq!(
            spots(&cs),
            [(0, 2), (3, 2), (4, 10), (12, 12)],
            "an empty caret joins the selection it touches, touching selections stay apart"
        );
        cs.collapse();
        assert_eq!(spots(&cs), [(3, 2)]);
    }

    #[test]
    fn many_carets_cost_one_parse() {
        let text = "fn f() { let a = 1; }\n".repeat(200);
        let mut b = buf(&text, "/x/a.rs");
        let mut cs = carets(
            &(0..200)
                .map(|l| (l * 22 + 9, l * 22 + 9))
                .collect::<Vec<_>>(),
        );
        let version = b.version();
        b.edit_each(&mut cs, |b, c| b.type_char(c, 'x'));
        assert_eq!(b.version(), version + 200, "one version per edit");
        assert!(b.full_text().starts_with("fn f() { xlet a"));
        assert_eq!(
            b.highlights(199..200).len(),
            b.highlights(0..1).len(),
            "the tree was parsed after the last edit"
        );
        assert_eq!(cs.len(), 200);
    }

    /// Runs background parses inline until the tree is current, as the editor's executor would.
    fn parse_all(b: &mut Buffer) {
        while let Some(job) = b.start_parse() {
            assert!(b.finish_parse(job.run()));
        }
    }

    #[test]
    fn a_background_parse_ends_with_the_highlights_of_a_fresh_parse() {
        let text = "fn f() {\n    let a = 1;\n}\n".repeat(50);
        let mut b = buf(&text, "/x/a.rs");
        b.parse_in_background();
        let mut c = Cursor::at(b.line_start(20) + 4);
        typed(&mut b, &mut c, "let s = \"x\"; // done");
        b.newline(&mut c);
        assert!(
            !b.highlights(0..b.len_lines()).is_empty(),
            "the edited old tree still paints"
        );
        parse_all(&mut b);
        let fresh = buf(&b.full_text(), "/x/a.rs");
        assert_eq!(
            b.highlights(0..b.len_lines()),
            fresh.highlights(0..fresh.len_lines())
        );
    }

    #[test]
    fn edits_made_while_a_parse_runs_are_replayed_on_its_tree() {
        let mut b = buf("fn f() {}\nfn g() {}\n", "/x/a.rs");
        b.parse_in_background();
        assert!(b.start_parse().is_none(), "a current tree needs no parse");
        let mut c = Cursor::at(0);
        b.insert(&mut c, "pub ");
        let job = b.start_parse().expect("the tree lags the text");
        assert!(b.start_parse().is_none(), "one parse at a time");
        let mut tail = Cursor::at(b.len_chars());
        b.insert(&mut tail, "const N: u8 = 1;\n");
        assert!(b.finish_parse(job.run()));
        assert!(
            b.highlights(0..1).contains(&(0..3, Token::Keyword)),
            "the landed tree knows the first edit"
        );
        parse_all(&mut b);
        let fresh = buf(&b.full_text(), "/x/a.rs");
        assert_eq!(
            b.highlights(0..b.len_lines()),
            fresh.highlights(0..fresh.len_lines())
        );
    }

    #[test]
    fn a_pair_typed_in_a_comment_not_yet_parsed_is_not_closed() {
        let mut b = buf("fn f() {\n    let s = ;\n}\n", "/x/a.rs");
        b.parse_in_background();
        let mut c = Cursor::at(b.line_start(1) + 12);
        typed(&mut b, &mut c, "// ");
        let job = b.start_parse().expect("the tree lags the text");
        typed(&mut b, &mut c, "(");
        assert_eq!(b.line(1).trim_end(), "    let s = // (;");
        assert!(
            !b.finish_parse(job.run()),
            "the parse the pair needed superseded the one in flight"
        );
    }

    #[test]
    fn bracket_matches_from_a_lagging_tree_point_only_at_brackets() {
        let mut b = buf("fn f() { g(h[1]); }\n", "/x/a.rs");
        b.parse_in_background();
        let mut c = Cursor::default();
        b.replace(&mut c, 10..11, "", EditKind::Delete);
        b.replace(&mut c, 3..3, "]{", EditKind::Insert);
        b.replace(&mut c, 0..2, "", EditKind::Delete);
        let is_bracket =
            |b: &Buffer, at: usize| bracket_pair(&b.rope.char(at).to_string()).is_some();
        for at in 0..=b.len_chars() {
            if let Some((here, partner)) = b.matching_bracket(at) {
                assert!(is_bracket(&b, here) && is_bracket(&b, partner), "{at}");
            }
        }
    }

    /// `cargo test -p athena-editor --release --lib -- --ignored keystroke_cost --nocapture`
    #[test]
    #[ignore]
    fn keystroke_cost_on_a_2k_line_rust_file() {
        let text: String = (0..400)
            .map(|i| format!("fn f{i}(x: u32) -> u32 {{\n    let y = x * {i};\n    // note {i}\n    y + 1\n}}\n"))
            .collect();
        let micros = |d: Duration| d.as_secs_f64() * 1e6;
        let report = |name: &str, mut times: Vec<f64>| {
            if times.is_empty() {
                return;
            }
            times.sort_by(f64::total_cmp);
            let avg = times.iter().sum::<f64>() / times.len() as f64;
            let at = |q: usize| times[(times.len() * q / 100).min(times.len() - 1)];
            println!(
                "{name}: n={} avg={avg:.0}us p50={:.0}us p99={:.0}us max={:.0}us",
                times.len(),
                at(50),
                at(99),
                at(100)
            );
        };
        // Inline parsing; parses landing between keys; parses that never land before the next key.
        for (mode, background, land) in [
            ("sync", false, false),
            ("background", true, true),
            ("background, typing faster than parses", true, false),
        ] {
            let mut b = buf(&text, "/x/a.rs");
            assert!(b.len_lines() >= 2000);
            if background {
                b.parse_in_background();
            }
            let mut c = Cursor::at(b.line_start(1000) + 4);
            let (mut keys, mut parses) = (Vec::new(), Vec::new());
            for ch in "let value = compute(a, b);\n".repeat(40).chars() {
                let t = Instant::now();
                match ch {
                    '\n' => b.newline(&mut c),
                    ch => b.type_char(&mut c, ch),
                }
                let first = b.line_of(c.head()).saturating_sub(30);
                std::hint::black_box(b.highlights(first..first + 60));
                keys.push(micros(t.elapsed()));
                if land && let Some(job) = b.start_parse() {
                    let t = Instant::now();
                    let parsed = job.run();
                    parses.push(micros(t.elapsed()));
                    b.finish_parse(parsed);
                }
            }
            report(&format!("{mode}, UI thread per key"), keys);
            report(&format!("{mode}, background parse"), parses);
        }
    }

    #[test]
    fn occurrences_respect_case_words_and_unicode_folding() {
        let b = buf("İstanbul istanbul ISTANBUL", "/x/a.txt");
        assert_eq!(b.find_all("istanbul"), [9..17, 18..26]);
        assert_eq!(b.find("istanbul", true, false), vec![9..17]);
        assert_eq!(b.find_all("İ"), vec![0..1]);
        assert_eq!(
            b.find_all("i̇stanbul"),
            vec![0..8],
            "a folded İ matches as a whole"
        );
        let b = buf("foo foobar _foo foo(Foo)", "/x/a.txt");
        assert_eq!(b.find("foo", true, true), [0..3, 16..19]);
        assert_eq!(b.find("foo", false, true), [0..3, 16..19, 20..23]);
        assert_eq!(b.find("foo", true, false).len(), 4);
        assert_eq!(
            b.word_around(3),
            Some(0..3),
            "a caret just after a word picks it"
        );
        assert_eq!(b.word_around(20), Some(20..23));
    }

    #[test]
    fn an_insert_at_the_main_edit_start_keeps_the_caret_after_the_main_text() {
        let mut b = buf("ab", "/x/a.txt");
        let mut c = Cursor::at(2);
        let edits = vec![(2..2, "cd".to_string()), (2..2, "X".to_string())];
        b.apply_edits(&mut c, &edits, None);
        assert_eq!(b.full_text(), "abXcd");
        assert_eq!(c.head(), 5);
    }

    #[test]
    fn carets_follow_a_primary_edit_longer_than_the_edit_log() {
        let mut b = buf(&"a".repeat(5000), "/x/a.txt");
        let len = b.len_chars();
        let mut cs = carets(&[(0, 0), (len, len)]);
        cs.add(Cursor::at(0));
        let edits: Vec<_> = (0..5000)
            .rev()
            .map(|i| (i..i + 1, "bb".to_string()))
            .collect();
        b.edit_primary(&mut cs, |b, c| b.apply_edits(c, &edits, None));
        assert_eq!(b.len_chars(), 10_000);
        assert_eq!(
            cs.all().last().unwrap().head(),
            10_000,
            "the far caret followed every edit"
        );
    }
}
