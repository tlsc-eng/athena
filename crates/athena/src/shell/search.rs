use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use athena_editor::find::{self, FindOptions};
use athena_editor::{DiffView, ToggleMatchCase, ToggleRegex, ToggleWholeWord};
use athena_ui::{ActiveTheme, Button, ButtonKind, InputEvent, TextInput};
use athena_workspace::{DiffBase, ItemKind};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use gpui::{
    AnyElement, Context, Entity, Focusable, FontWeight, HighlightStyle, PromptLevel,
    ScrollStrategy, SharedString, StyledText, Subscription, Task, UniformListScrollHandle, Window,
    div, prelude::*, px, uniform_list,
};
use regex::Regex;

use super::Shell;
use super::drawer::DrawerTab;
use super::item::ItemView;

/// Typing pauses this long before a new search starts.
const DEBOUNCE: Duration = Duration::from_millis(150);
const MAX_FILE: u64 = 1024 * 1024;
/// A NUL byte this early marks a file as binary, as git and ripgrep decide.
const BINARY_PROBE: usize = 8 * 1024;
pub(super) const MAX_HITS: usize = 2000;
/// Results reach the list in groups this size, so long searches show progress without a redraw per line.
const BATCH: usize = 50;
/// Longest excerpt shown for a matching line.
const EXCERPT: usize = 200;
const ROW_HEIGHT: f32 = 24.;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Hit {
    /// Zero-based line and UTF-16 column of the first match, as editors are positioned.
    line: u32,
    column: u32,
    text: String,
    /// Byte ranges of the matches within `text`.
    ranges: Vec<Range<usize>>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct FileHits {
    path: PathBuf,
    hits: Vec<Hit>,
}

#[derive(Clone, Copy)]
enum Row {
    File(usize),
    Hit(usize, usize),
}

enum Found {
    Batch(Vec<FileHits>),
    Done { truncated: bool },
}

#[derive(Default)]
pub(super) struct SearchState {
    find: Option<Entity<TextInput>>,
    replace: Option<Entity<TextInput>>,
    include: Option<Entity<TextInput>>,
    exclude: Option<Entity<TextInput>>,
    options: FindOptions,
    /// The files to include and exclude fields are shown.
    details: bool,
    /// Why the query cannot run: an invalid regex or glob.
    error: Option<String>,
    /// The project the results belong to, which Replace All rewrites even after a switch.
    root: Option<PathBuf>,
    files: Rc<Vec<FileHits>>,
    rows: Rc<Vec<Row>>,
    collapsed: HashSet<PathBuf>,
    selected: Option<(usize, usize)>,
    matches: usize,
    /// The query the finished results answer, which Replace All replaces rather than the fields'.
    results_for: Option<Query>,
    /// The query last searched for or shown, so an unchanged one is not searched again.
    asked: Option<Query>,
    /// Other projects' searches, given back when the project is active again.
    saved: HashMap<PathBuf, Saved>,
    running: bool,
    truncated: bool,
    replacing: bool,
    cancel: Arc<AtomicBool>,
    task: Option<Task<()>>,
    scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

/// A project's search while another project is active.
struct Saved {
    query: Query,
    replace: String,
    files: Rc<Vec<FileHits>>,
    collapsed: HashSet<PathBuf>,
    selected: Option<(usize, usize)>,
    matches: usize,
    truncated: bool,
    results_for: Option<Query>,
}

/// What a project search asks for: the text, its toggles and the include and exclude globs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Query {
    text: String,
    options: FindOptions,
    include: String,
    exclude: String,
}

/// A query compiled to run.
#[derive(Clone)]
pub(super) struct Matcher {
    re: Regex,
    options: FindOptions,
    include: Option<GlobSet>,
    exclude: Option<GlobSet>,
}

impl Matcher {
    /// `None` for an empty query; an invalid regex or glob is the error to show.
    pub(super) fn new(query: &Query) -> Result<Option<Self>, String> {
        if query.text.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self {
            re: find::compile(&query.text, query.options)?,
            options: query.options,
            include: globs(&query.include)?,
            exclude: globs(&query.exclude)?,
        }))
    }

    fn find(&self, line: &str) -> Vec<Range<usize>> {
        find::find_matches(&self.re, line, self.options.word)
    }

    /// `text` with every match replaced, `$1` filled in only in regex mode; how many it replaced.
    fn replace(&self, text: &str, with: &str) -> (String, usize) {
        find::replace_lines(&self.re, text, with, self.options)
    }

    /// Whether the globs let the search read `path`, judged by its path from `root` or any
    /// folder on the way, so a folder stands for everything in it.
    fn wants(&self, root: &Path, path: &Path) -> bool {
        let Ok(rel) = path.strip_prefix(root) else {
            return true;
        };
        let hit = |set: &GlobSet| {
            rel.ancestors()
                .any(|p| !p.as_os_str().is_empty() && set.is_match(p))
        };
        self.include.as_ref().is_none_or(hit) && !self.exclude.as_ref().is_some_and(hit)
    }
}

/// VS Code's files to include/exclude syntax: comma-separated globs that match at any depth
/// unless they start with `./`.
fn globs(spec: &str) -> Result<Option<GlobSet>, String> {
    let mut set = GlobSetBuilder::new();
    let mut any = false;
    for pattern in split_globs(spec) {
        let pattern = pattern.trim().trim_end_matches('/');
        let pattern = match pattern.strip_prefix("./").or(pattern.strip_prefix('/')) {
            Some(anchored) => anchored.to_string(),
            None if pattern.starts_with("**") => pattern.to_string(),
            None => format!("**/{pattern}"),
        };
        if pattern.is_empty() || pattern == "**/" {
            continue;
        }
        let glob = GlobBuilder::new(&pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| e.to_string())?;
        set.add(glob);
        any = true;
    }
    if !any {
        return Ok(None);
    }
    set.build().map(Some).map_err(|e| e.to_string())
}

/// Splits at commas outside braces, where they separate a glob's alternatives.
fn split_globs(spec: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0usize, 0);
    for (i, c) in spec.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(&spec[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&spec[start..]);
    out
}

/// Every file a project search reads: .gitignore honoured, hidden files included, `.git` skipped,
/// and only what the include and exclude globs let through.
fn walk<'a>(root: &'a Path, m: &'a Matcher) -> impl Iterator<Item = PathBuf> + 'a {
    ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|e| e.file_name() != ".git" && !is_temp(e.path()))
        .sort_by_file_name(|a, b| a.cmp(b))
        .build()
        .flatten()
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(|e| e.into_path())
        .filter(move |p| m.wants(root, p))
}

/// Where a replacement is written before it takes the file's place.
fn temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.athena-tmp"))
}

fn is_temp(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| n.to_string_lossy().ends_with(".athena-tmp"))
}

/// Replaces `path`'s contents through a rename, so a failed write never leaves it half written.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = temp_path(path);
    let written = (|| {
        let mut out = std::fs::File::create(&tmp)?;
        out.write_all(bytes)?;
        out.sync_all()?;
        std::fs::set_permissions(&tmp, std::fs::metadata(path)?.permissions())?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// A file's text, unless it is large, binary or not UTF-8.
fn read_text(path: &Path) -> Option<String> {
    if std::fs::metadata(path).ok()?.len() > MAX_FILE {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes[..bytes.len().min(BINARY_PROBE)].contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// The line from its first non-blank character, cut to a window around the first match.
fn excerpt(line: &str, ranges: &[Range<usize>]) -> (String, Vec<Range<usize>>) {
    let mut start = line.len() - line.trim_start().len();
    let first = ranges.first().map_or(start, |r| r.start);
    let mut prefix = "";
    if first > start + EXCERPT / 2 {
        start = first - EXCERPT / 4;
        while !line.is_char_boundary(start) {
            start -= 1;
        }
        prefix = "…";
    }
    let mut end = (start + EXCERPT).min(line.len());
    while !line.is_char_boundary(end) {
        end += 1;
    }
    let shift = |at: usize| at - start + prefix.len();
    let shown = ranges
        .iter()
        .filter(|r| r.start >= start && r.start < end)
        .map(|r| shift(r.start)..shift(r.end.min(end)))
        .collect();
    (format!("{prefix}{}", &line[start..end]), shown)
}

/// Searches every file under `root`, handing results to `send` in batches; true when the cap cut it short.
fn search(
    root: &Path,
    m: &Matcher,
    cancel: &AtomicBool,
    mut send: impl FnMut(Vec<FileHits>) -> bool,
) -> bool {
    let mut batch = Vec::new();
    let mut pending = 0;
    let mut total = 0;
    for path in walk(root, m) {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let Some(text) = read_text(&path) else {
            continue;
        };
        let mut hits = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let ranges = m.find(line);
            let Some(first) = ranges.first() else {
                continue;
            };
            total += ranges.len();
            let column = line[..first.start].encode_utf16().count() as u32;
            let (text, ranges) = excerpt(line, &ranges);
            hits.push(Hit {
                line: i as u32,
                column,
                text,
                ranges,
            });
            if total >= MAX_HITS {
                break;
            }
        }
        if !hits.is_empty() {
            pending += hits.len();
            batch.push(FileHits { path, hits });
        }
        if total >= MAX_HITS {
            send(batch);
            return true;
        }
        if pending >= BATCH {
            pending = 0;
            if !send(std::mem::take(&mut batch)) {
                return false;
            }
        }
    }
    if !batch.is_empty() {
        send(batch);
    }
    false
}

/// What a disk replacement did, and which open files it left for their editors.
#[derive(Debug, Default)]
struct DiskReplace {
    files: usize,
    matches: usize,
    failed: Vec<String>,
    /// Paths in `open` that the search walk reaches, so their editors take the replacement.
    for_editors: HashSet<PathBuf>,
}

/// Replaces every match in the files a search under `root` reads, except those in `open`.
fn replace_on_disk(root: &Path, m: &Matcher, with: &str, open: &HashSet<PathBuf>) -> DiskReplace {
    let mut out = DiskReplace::default();
    for path in walk(root, m) {
        if open.contains(&path) {
            out.for_editors.insert(path);
            continue;
        }
        let Some(text) = read_text(&path) else {
            continue;
        };
        let (replaced, n) = m.replace(&text, with);
        if n == 0 {
            continue;
        }
        match write_atomic(&path, replaced.as_bytes()) {
            Ok(()) => {
                out.files += 1;
                out.matches += n;
            }
            Err(err) => out.failed.push(format!("{}: {err}", path.display())),
        }
    }
    out
}

impl SearchState {
    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        for (fi, file) in self.files.iter().enumerate() {
            rows.push(Row::File(fi));
            if !self.collapsed.contains(&file.path) {
                rows.extend((0..file.hits.len()).map(|hi| Row::Hit(fi, hi)));
            }
        }
        self.rows = Rc::new(rows);
    }

    fn query(&self, cx: &gpui::App) -> Query {
        let text = |input: &Option<Entity<TextInput>>| {
            input
                .as_ref()
                .map(|i| i.read(cx).text().to_string())
                .unwrap_or_default()
        };
        Query {
            text: text(&self.find),
            options: self.options,
            include: text(&self.include),
            exclude: text(&self.exclude),
        }
    }

    fn replacement(&self, cx: &gpui::App) -> String {
        self.replace
            .as_ref()
            .map(|i| i.read(cx).text().to_string())
            .unwrap_or_default()
    }

    /// Takes the shown search out, to keep while another project is active.
    fn take_saved(&mut self, cx: &gpui::App) -> Saved {
        Saved {
            query: self.query(cx),
            replace: self.replacement(cx),
            files: std::mem::take(&mut self.files),
            collapsed: std::mem::take(&mut self.collapsed),
            selected: self.selected.take(),
            matches: std::mem::take(&mut self.matches),
            truncated: std::mem::take(&mut self.truncated),
            results_for: self.results_for.take(),
        }
    }

    /// Replace All is offered only for finished results of the query now in the field.
    fn can_replace(&self, cx: &gpui::App) -> bool {
        replace_allowed(self, &self.query(cx))
    }
}

fn replace_allowed(s: &SearchState, query: &Query) -> bool {
    !s.files.is_empty() && !s.replacing && !s.running && s.results_for.as_ref() == Some(query)
}

impl Shell {
    fn ensure_search_inputs(&mut self, cx: &mut Context<Self>) -> Entity<TextInput> {
        if let Some(find) = &self.search.find {
            return find.clone();
        }
        let find = cx.new(|cx| TextInput::new("Search", cx));
        let replace = cx.new(|cx| TextInput::new("Replace", cx));
        let include = cx.new(|cx| TextInput::new("Files to include, e.g. *.ts, src", cx));
        let exclude = cx.new(|cx| TextInput::new("Files to exclude", cx));
        let filter = |this: &mut Self, event: &InputEvent, cx: &mut Context<Self>| match event {
            InputEvent::Changed => this.schedule_search(false, cx),
            InputEvent::Submit | InputEvent::SubmitBeside => this.open_selected_hit(cx),
            InputEvent::Cancel => this.close_search(cx),
            InputEvent::Up | InputEvent::Down => {}
        };
        let subscriptions = vec![
            cx.subscribe(&find, |this, _, event: &InputEvent, cx| match event {
                InputEvent::Changed => this.schedule_search(false, cx),
                InputEvent::Up => this.step_search(-1, cx),
                InputEvent::Down => this.step_search(1, cx),
                InputEvent::Submit | InputEvent::SubmitBeside => this.open_selected_hit(cx),
                InputEvent::Cancel => this.close_search(cx),
            }),
            cx.subscribe(&replace, |this, _, event: &InputEvent, cx| match event {
                InputEvent::Changed => this.refresh_replace_previews(cx),
                InputEvent::Cancel => this.close_search(cx),
                _ => {}
            }),
            cx.subscribe(&include, move |this, _, event: &InputEvent, cx| {
                filter(this, event, cx)
            }),
            cx.subscribe(&exclude, move |this, _, event: &InputEvent, cx| {
                filter(this, event, cx)
            }),
        ];
        self.search.find = Some(find.clone());
        self.search.replace = Some(replace);
        self.search.include = Some(include);
        self.search.exclude = Some(exclude);
        self.search._subscriptions = subscriptions;
        find
    }

    /// Cmd+Shift+F: shows the Search tab, seeded with the editor's one-line selection.
    pub(super) fn find_in_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let find = self.ensure_search_inputs(cx);
        let seed = self
            .focused_editor()
            .and_then(|e| e.read(cx).cursor())
            .and_then(|(_, _, selection)| selection)
            .filter(|s| !s.contains('\n'));
        if let Some(seed) = seed {
            find.update(cx, |i, cx| i.set_text(seed, cx));
        }
        self.show_drawer_tab(DrawerTab::Search, cx);
        window.focus(&find.focus_handle(cx));
    }

    fn close_search(&mut self, cx: &mut Context<Self>) {
        if self.drawer == Some(DrawerTab::Search) {
            self.toggle_drawer_tab(DrawerTab::Search, cx);
        }
        self.focus_pending = true;
    }

    fn stop_search(&mut self) {
        self.search.cancel.store(true, Ordering::Relaxed);
        self.search.cancel = Arc::default();
        self.search.task = None;
        self.search.running = false;
    }

    /// Keeps the search shown for the project left, and shows the one kept for `root`.
    fn switch_search_root(&mut self, root: Option<PathBuf>, cx: &mut Context<Self>) {
        self.stop_search();
        let saved = self.search.take_saved(cx);
        if let Some(old) = std::mem::replace(&mut self.search.root, root.clone()) {
            self.search.saved.insert(old, saved);
        }
        self.search.asked = None;
        self.search.error = None;
        let Some(saved) = root.and_then(|r| self.search.saved.remove(&r)) else {
            self.clear_results(cx);
            return;
        };
        let fields = [
            (self.search.find.clone(), saved.query.text.clone()),
            (self.search.replace.clone(), saved.replace),
            (self.search.include.clone(), saved.query.include.clone()),
            (self.search.exclude.clone(), saved.query.exclude.clone()),
        ];
        for (input, text) in fields {
            if let Some(input) = input.filter(|i| i.read(cx).text() != text) {
                input.update(cx, |i, cx| i.set_text(text, cx));
            }
        }
        let s = &mut self.search;
        s.details |= !saved.query.include.is_empty() || !saved.query.exclude.is_empty();
        s.options = saved.query.options;
        s.files = saved.files;
        s.collapsed = saved.collapsed;
        s.selected = saved.selected;
        s.matches = saved.matches;
        s.truncated = saved.truncated;
        s.asked = saved.results_for.clone();
        s.results_for = saved.results_for;
        s.rebuild_rows();
        cx.notify();
    }

    /// Searches for what the fields ask, unless that is already shown or running; `force` searches
    /// again anyway, as after a replacement.
    fn schedule_search(&mut self, force: bool, cx: &mut Context<Self>) {
        let root = self.workspace.active_project().map(|p| p.root.clone());
        if root != self.search.root {
            self.switch_search_root(root.clone(), cx);
        }
        let query = self.search.query(cx);
        if !force && self.search.asked.as_ref() == Some(&query) {
            return;
        }
        self.stop_search();
        self.search.asked = Some(query.clone());
        let matcher = Matcher::new(&query);
        self.search.error = matcher.as_ref().err().cloned();
        let (Ok(Some(m)), Some(root)) = (matcher, root) else {
            self.clear_results(cx);
            return;
        };
        let cancel = self.search.cancel.clone();
        self.search.running = true;
        cx.notify();
        self.search.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            tracing::debug!(query = query.text, root = %root.display(), "project search");
            let (tx, rx) = async_channel::unbounded();
            cx.background_executor()
                .spawn(async move {
                    let truncated = search(&root, &m, &cancel, |batch| {
                        tx.send_blocking(Found::Batch(batch)).is_ok()
                    });
                    let _ = tx.send_blocking(Found::Done { truncated });
                })
                .detach();
            let mut first = true;
            while let Ok(found) = rx.recv().await {
                let fresh = std::mem::take(&mut first);
                let alive = this.update(cx, |this, cx| {
                    if fresh {
                        this.clear_results(cx);
                    }
                    match found {
                        Found::Batch(batch) => this.add_results(batch, cx),
                        Found::Done { truncated } => {
                            this.search.running = false;
                            this.search.truncated = truncated;
                            this.search.results_for = Some(query.clone());
                            tracing::debug!(
                                matches = this.search.matches,
                                truncated,
                                "project search done"
                            );
                            this.refresh_replace_previews(cx);
                            cx.notify();
                        }
                    }
                });
                if alive.is_err() {
                    return;
                }
            }
        }));
    }

    fn clear_results(&mut self, cx: &mut Context<Self>) {
        self.search.files = Rc::default();
        self.search.rows = Rc::default();
        self.search.matches = 0;
        self.search.results_for = None;
        self.search.selected = None;
        self.search.truncated = false;
        self.search.collapsed.clear();
        cx.notify();
    }

    fn add_results(&mut self, batch: Vec<FileHits>, cx: &mut Context<Self>) {
        self.search.matches += batch
            .iter()
            .flat_map(|f| &f.hits)
            .map(|h| h.ranges.len().max(1))
            .sum::<usize>();
        Rc::make_mut(&mut self.search.files).extend(batch);
        self.search.rebuild_rows();
        cx.notify();
    }

    /// Up/Down in the search field walk the visible matches.
    fn step_search(&mut self, step: isize, cx: &mut Context<Self>) {
        let hits: Vec<(usize, (usize, usize))> = self
            .search
            .rows
            .iter()
            .enumerate()
            .filter_map(|(i, r)| match r {
                Row::Hit(f, h) => Some((i, (*f, *h))),
                Row::File(_) => None,
            })
            .collect();
        if hits.is_empty() {
            return;
        }
        let at = self
            .search
            .selected
            .and_then(|s| hits.iter().position(|(_, h)| *h == s));
        let next = match at {
            Some(i) => (i as isize + step).rem_euclid(hits.len() as isize) as usize,
            None if step < 0 => hits.len() - 1,
            None => 0,
        };
        self.search.selected = Some(hits[next].1);
        self.search
            .scroll
            .scroll_to_item(hits[next].0, ScrollStrategy::Center);
        cx.notify();
    }

    fn open_selected_hit(&mut self, cx: &mut Context<Self>) {
        let first = self.search.files.first().map(|_| (0, 0));
        if let Some((f, h)) = self.search.selected.or(first) {
            self.open_hit(f, h, cx);
        }
    }

    fn toggle_search_option(&mut self, flip: fn(&mut FindOptions), cx: &mut Context<Self>) {
        flip(&mut self.search.options);
        self.schedule_search(false, cx);
        cx.notify();
    }

    /// A click on a match previews the replacement while there is one, as VS Code does, and
    /// otherwise opens the file there.
    fn click_hit(&mut self, file: usize, hit: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.replacement(cx).is_empty() {
            return self.open_hit(file, hit, cx);
        }
        let Some(path) = self.search.files.get(file).map(|f| f.path.clone()) else {
            return;
        };
        self.search.selected = Some((file, hit));
        self.open_diff(path, DiffBase::SearchReplace, window, cx);
    }

    /// Fills a Replace Preview tab: the file as its editor or the disk has it, against what the
    /// finished search's Replace All would make of it.
    pub(super) fn load_replace_preview(
        &mut self,
        root: &Path,
        view: &Entity<DiffView>,
        cx: &mut Context<Self>,
    ) {
        let path = view.read(cx).path().to_path_buf();
        let with = self.search.replacement(cx);
        let matcher = (self.search.root.as_deref() == Some(root))
            .then_some(self.search.results_for.as_ref())
            .flatten()
            .and_then(|q| Matcher::new(q).ok().flatten());
        let Some(m) = matcher else {
            view.update(cx, |v, cx| {
                v.set_error("Search again to preview the replacement.", cx)
            });
            return;
        };
        let open = self.editors_under(root).into_iter().find_map(|e| {
            let e = e.read(cx);
            (e.path() == path).then(|| e.text()).flatten()
        });
        let weak = view.downgrade();
        cx.spawn(async move |_, cx| {
            let texts = cx
                .background_executor()
                .spawn(async move {
                    let old = open
                        .or_else(|| read_text(&path))
                        .ok_or("This file is too large, binary or not UTF-8 text.")?;
                    let new = m.replace(&old, &with).0;
                    Ok::<_, &str>((old, new))
                })
                .await;
            if let Some(view) = weak.upgrade() {
                let _ = view.update(cx, |v, cx| match texts {
                    Ok((old, new)) => v.set_texts(old, new, cx),
                    Err(e) => v.set_error(e, cx),
                });
            }
        })
        .detach();
    }

    /// Recomputes the open Replace Preview tabs of the searched project.
    fn refresh_replace_previews(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.search.root.clone() else {
            return;
        };
        let views: Vec<Entity<DiffView>> = self
            .workspace
            .projects
            .iter()
            .filter(|p| p.root == root)
            .filter_map(|p| p.layout.as_ref())
            .flat_map(|l| l.items())
            .filter(|i| {
                matches!(
                    &i.kind,
                    ItemKind::Diff {
                        base: DiffBase::SearchReplace,
                        ..
                    }
                )
            })
            .filter_map(|i| match self.items.get(&(root.clone(), i.id)) {
                Some(ItemView::Diff(view)) => Some(view.clone()),
                _ => None,
            })
            .collect();
        for view in views {
            self.load_replace_preview(&root, &view, cx);
        }
    }

    /// Opens a match through the same path go-to-definition uses, at the next frame.
    fn open_hit(&mut self, file: usize, hit: usize, cx: &mut Context<Self>) {
        let Some(f) = self.search.files.get(file) else {
            return;
        };
        let Some(h) = f.hits.get(hit) else {
            return;
        };
        self.search.selected = Some((file, hit));
        let at = athena_lsp::Position {
            line: h.line,
            character: h.column,
        };
        self.lsp.jump = Some((f.path.clone(), at));
        cx.notify();
    }

    fn toggle_search_file(&mut self, file: usize, cx: &mut Context<Self>) {
        let Some(path) = self.search.files.get(file).map(|f| f.path.clone()) else {
            return;
        };
        if !self.search.collapsed.remove(&path) {
            self.search.collapsed.insert(path);
        }
        self.search.rebuild_rows();
        cx.notify();
    }

    /// Asks first, then replaces in open editors' buffers (undoable there) and on disk elsewhere.
    fn replace_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search.can_replace(cx) {
            return;
        }
        let query = self.search.results_for.clone().unwrap_or_default();
        let (Ok(Some(m)), Some(root)) = (Matcher::new(&query), self.search.root.clone()) else {
            return;
        };
        let with = self.search.replacement(cx);
        let count = if self.search.truncated {
            format!("{}+", self.search.matches)
        } else {
            self.search.matches.to_string()
        };
        let files = self.search.files.len();
        let message = format!(
            "Replace {count} {} across {files} {} with “{with}”?",
            if self.search.matches == 1 {
                "occurrence"
            } else {
                "occurrences"
            },
            if files == 1 { "file" } else { "files" },
        );
        let answer = window.prompt(
            PromptLevel::Warning,
            &message,
            Some("Files open in an editor change there, where ⌘Z undoes it. Other files are written now."),
            &["Replace", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let Ok(open) = this.update(cx, |this, cx| {
                this.search.replacing = true;
                cx.notify();
                this.editor_paths_under(&root, cx)
            }) else {
                return;
            };
            let disk_root = root.clone();
            let disk_m = m.clone();
            let disk_with = with.clone();
            let disk = cx
                .background_executor()
                .spawn(async move { replace_on_disk(&disk_root, &disk_m, &disk_with, &open) })
                .await;
            let _ = this.update(cx, |this, cx| {
                let in_editors = this.replace_in_editors(&root, &disk.for_editors, &m, &with, cx);
                this.search.replacing = false;
                let files = disk.files + in_editors.0;
                let total = disk.matches + in_editors.1;
                tracing::info!(files, total, "replaced in project");
                let title = format!(
                    "Replaced {total} {} in {files} {}",
                    if total == 1 {
                        "occurrence"
                    } else {
                        "occurrences"
                    },
                    if files == 1 { "file" } else { "files" },
                );
                let body = if disk.failed.is_empty() {
                    String::new()
                } else {
                    format!("Could not write {}", disk.failed.join(", "))
                };
                this.transient_notice(&title, body, cx);
                this.git_kick(cx);
                this.schedule_search(true, cx);
            });
        })
        .detach();
    }

    /// Files open in a loaded editor, which take a replacement there instead of on disk.
    fn editor_paths_under(&self, root: &Path, cx: &gpui::App) -> HashSet<PathBuf> {
        self.editors_under(root)
            .iter()
            .map(|e| e.read(cx))
            .filter(|e| e.version().is_some())
            .map(|e| e.path().to_path_buf())
            .collect()
    }

    /// Applies the replacement to the editors on `paths`; returns (files, matches) changed.
    fn replace_in_editors(
        &mut self,
        root: &Path,
        paths: &HashSet<PathBuf>,
        m: &Matcher,
        with: &str,
        cx: &mut Context<Self>,
    ) -> (usize, usize) {
        let mut buffers = HashSet::new();
        let mut changed = HashSet::new();
        let mut count = 0;
        for editor in self.editors_under(root) {
            let path = editor.read(cx).path().to_path_buf();
            if !paths.contains(&path) {
                continue;
            }
            // Tabs on the same file share one buffer, which must be replaced in only once.
            if !buffers.insert(super::lsp::document_key(&path)) {
                continue;
            }
            let Some(text) = editor.read(cx).text() else {
                continue;
            };
            let (replaced, n) = m.replace(&text, with);
            if n == 0 {
                continue;
            }
            editor.update(cx, |e, cx| e.replace_text(&replaced, cx));
            if changed.insert(path) {
                count += n;
            }
        }
        (changed.len(), count)
    }

    pub(super) fn render_search_status(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = cx.theme();
        let s = &self.search;
        let text = match (s.running, s.matches) {
            (true, 0) => "Searching…".to_string(),
            (_, 0) => return None,
            (_, n) => {
                let results = if n == 1 { "result" } else { "results" };
                let files = s.files.len();
                let in_files = if files == 1 { "file" } else { "files" };
                let limited = if s.truncated {
                    format!(" (first {MAX_HITS} shown)")
                } else {
                    String::new()
                };
                format!("{n} {results} in {files} {in_files}{limited}")
            }
        };
        Some(
            div()
                .text_color(t.color.content_muted)
                .child(text)
                .into_any_element(),
        )
    }

    /// The Search tab: query and replacement fields above matches grouped by file.
    pub(super) fn render_search(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let find = self.ensure_search_inputs(cx);
        // Results from the project shown before a switch would be opened and replaced in the wrong one.
        if self.search.root.as_ref() != self.workspace.active_project().map(|p| &p.root) {
            self.schedule_search(false, cx);
        }
        let replace = self.search.replace.clone();
        let (include, exclude) = (self.search.include.clone(), self.search.exclude.clone());
        let t = cx.theme().clone();
        let error = self.search.error.clone();
        let field = |input: Entity<TextInput>, invalid: bool| {
            div()
                .flex_1()
                .min_w_0()
                .h(px(24.))
                .pl(px(8.))
                .pr(px(2.))
                .flex()
                .items_center()
                .gap(px(2.))
                .bg(t.color.surface)
                .border_1()
                .border_color(if invalid {
                    t.color.danger
                } else {
                    t.color.border
                })
                .rounded(t.shape.radius_control)
                .text_size(t.typography.caption)
                .child(div().flex_1().min_w_0().child(input))
        };
        let opts = self.search.options;
        let toggle = |id: &'static str,
                      label: &'static str,
                      tip: &'static str,
                      on: bool,
                      flip: fn(&mut FindOptions)| {
            div()
                .id(id)
                .flex_none()
                .h(px(18.))
                .px(px(4.))
                .flex()
                .items_center()
                .rounded(t.shape.radius_control)
                .cursor_pointer()
                .text_color(if on {
                    t.color.content
                } else {
                    t.color.content_muted
                })
                .when(on, |el| {
                    el.bg(t.color.surface_accent)
                        .border_1()
                        .border_color(t.color.accent)
                })
                .when(!on, |el| el.hover(|s| s.bg(t.color.surface_hover)))
                .tooltip(move |_, cx| athena_ui::Tooltip::view(tip, cx))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_search_option(flip, cx)))
                .child(label)
        };
        let toggles = [
            toggle(
                "search-match-case",
                "Aa",
                "Match Case  ⌥⌘C",
                opts.case,
                |o| o.case = !o.case,
            ),
            toggle(
                "search-whole-word",
                "ab",
                "Match Whole Word  ⌥⌘W",
                opts.word,
                |o| o.word = !o.word,
            ),
            toggle(
                "search-regex",
                ".*",
                "Use Regular Expression  ⌥⌘R",
                opts.regex,
                |o| o.regex = !o.regex,
            ),
        ];
        let details = self.search.details;
        let details_toggle = div()
            .id("search-details")
            .flex_none()
            .w(px(24.))
            .h(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(t.shape.radius_control)
            .cursor_pointer()
            .text_color(if details {
                t.color.content
            } else {
                t.color.content_muted
            })
            .hover(|s| s.bg(t.color.surface_hover))
            .tooltip(|_, cx| athena_ui::Tooltip::view("Toggle Search Details", cx))
            .on_click(cx.listener(|this, _, _, cx| {
                this.search.details = !this.search.details;
                cx.notify();
            }))
            .child("⋯");
        let can_replace = self.search.can_replace(cx);
        let row = || div().h(px(24.)).flex().items_center().gap(px(8.));
        let bar = div()
            .key_context("ProjectSearch")
            .on_action(cx.listener(|this, _: &ToggleMatchCase, _, cx| {
                this.toggle_search_option(|o| o.case = !o.case, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleWholeWord, _, cx| {
                this.toggle_search_option(|o| o.word = !o.word, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleRegex, _, cx| {
                this.toggle_search_option(|o| o.regex = !o.regex, cx)
            }))
            .flex_none()
            .py(px(6.))
            .px(px(12.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                row()
                    .child(field(find, error.is_some()).children(toggles))
                    .children(replace.map(|r| field(r, false)))
                    .children(can_replace.then(|| {
                        Button::new("search-replace-all", "Replace All", ButtonKind::Secondary)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.replace_all(window, cx)),
                            )
                    }))
                    .child(details_toggle),
            )
            .when(details, |bar| {
                bar.child(
                    row()
                        .children(include.map(|i| field(i, false)))
                        .children(exclude.map(|e| field(e, false))),
                )
            });
        let query = self.search.query(cx);
        let message = |text: &str, color| {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px(px(12.))
                .text_size(t.typography.caption)
                .text_color(color)
                .child(text.to_string())
                .into_any_element()
        };
        let muted = t.color.content_muted;
        let body = if let Some(error) = &error {
            message(error, t.color.danger)
        } else if query.text.is_empty() {
            message("Type to search every file in this project.", muted)
        } else if self.search.rows.is_empty() {
            message(
                if self.search.running {
                    "Searching…"
                } else {
                    "No results."
                },
                muted,
            )
        } else {
            self.render_search_rows(cx)
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(bar)
            .child(body)
            .into_any_element()
    }

    fn render_search_rows(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let files = self.search.files.clone();
        let rows = self.search.rows.clone();
        let collapsed = self.search.collapsed.clone();
        let selected = self.search.selected;
        let root = self.search.root.clone();
        uniform_list(
            "search-results",
            rows.len(),
            cx.processor(move |_this, range: Range<usize>, _window, cx| {
                range
                    .map(|i| match rows[i] {
                        Row::File(fi) => {
                            let file = &files[fi];
                            let rel = root
                                .as_ref()
                                .and_then(|r| file.path.strip_prefix(r).ok())
                                .unwrap_or(&file.path);
                            let name = rel
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            let dir = rel
                                .parent()
                                .map(|p| p.display().to_string())
                                .unwrap_or_default();
                            let count: usize =
                                file.hits.iter().map(|h| h.ranges.len().max(1)).sum();
                            let chevron = if collapsed.contains(&file.path) {
                                "▸"
                            } else {
                                "▾"
                            };
                            div()
                                .id(("search-file", fi))
                                .w_full()
                                .h(px(ROW_HEIGHT))
                                .px(px(12.))
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .cursor_pointer()
                                .text_size(t.typography.caption)
                                .hover(|s| s.bg(t.color.surface_hover))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.toggle_search_file(fi, cx)
                                }))
                                .child(
                                    div()
                                        .w(px(10.))
                                        .flex_none()
                                        .text_color(t.color.content_muted)
                                        .child(chevron),
                                )
                                .child(athena_ui::file_icon(&file.path, false, cx))
                                .child(
                                    div()
                                        .flex_none()
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(t.color.content)
                                        .child(name),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_color(t.color.content_muted)
                                        .child(dir),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .px(px(6.))
                                        .rounded(t.shape.radius_control)
                                        .bg(t.color.surface_active)
                                        .text_color(t.color.content_secondary)
                                        .child(count.to_string()),
                                )
                                .into_any_element()
                        }
                        Row::Hit(fi, hi) => {
                            let hit = &files[fi].hits[hi];
                            let active = selected == Some((fi, hi));
                            let marks: Vec<(Range<usize>, HighlightStyle)> = hit
                                .ranges
                                .iter()
                                .map(|r| {
                                    (
                                        r.clone(),
                                        HighlightStyle {
                                            color: Some(t.color.content),
                                            background_color: Some(t.color.surface_accent),
                                            ..Default::default()
                                        },
                                    )
                                })
                                .collect();
                            div()
                                .id(("search-hit", i))
                                .w_full()
                                .h(px(ROW_HEIGHT))
                                .pl(px(40.))
                                .pr(px(12.))
                                .flex()
                                .items_center()
                                .gap(px(10.))
                                .cursor_pointer()
                                .hover(|s| s.bg(t.color.surface_hover))
                                .when(active, |el| el.bg(t.color.surface_active))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.click_hit(fi, hi, window, cx)
                                }))
                                .child(
                                    div()
                                        .w(px(36.))
                                        .flex_none()
                                        .flex()
                                        .justify_end()
                                        .text_size(t.typography.caption)
                                        .text_color(if active {
                                            t.color.accent
                                        } else {
                                            t.color.content_disabled
                                        })
                                        .child((hit.line + 1).to_string()),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .font_family(t.typography.mono.clone())
                                        .text_size(t.typography.caption)
                                        .text_color(t.color.content_muted)
                                        .child(
                                            StyledText::new(SharedString::from(hit.text.clone()))
                                                .with_highlights(marks),
                                        ),
                                )
                                .into_any_element()
                        }
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(self.search.scroll.clone())
        .flex_1()
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("athena-search-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // The ignore crate only reads .gitignore inside a repository.
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        dir
    }

    fn query(text: &str) -> Query {
        Query {
            text: text.into(),
            ..Query::default()
        }
    }

    fn matcher(text: &str) -> Matcher {
        Matcher::new(&query(text)).unwrap().unwrap()
    }

    fn run_query(root: &Path, q: &Query) -> (Vec<FileHits>, bool) {
        let mut out = Vec::new();
        let m = Matcher::new(q).unwrap().unwrap();
        let truncated = search(root, &m, &AtomicBool::new(false), |b| {
            out.extend(b);
            true
        });
        (out, truncated)
    }

    fn run(root: &Path, text: &str) -> (Vec<FileHits>, bool) {
        run_query(root, &query(text))
    }

    #[test]
    fn replace_all_waits_for_finished_results_of_the_current_query() {
        let mut s = SearchState {
            files: Rc::new(vec![FileHits {
                path: "/p/a.rs".into(),
                hits: Vec::new(),
            }]),
            results_for: Some(query("foo")),
            ..Default::default()
        };
        assert!(replace_allowed(&s, &query("foo")));
        assert!(
            !replace_allowed(&s, &query("foob")),
            "typed past the results"
        );
        let mut regex = query("foo");
        regex.options.regex = true;
        assert!(!replace_allowed(&s, &regex), "a toggle changed since");
        let mut excluded = query("foo");
        excluded.exclude = "*.rs".into();
        assert!(!replace_allowed(&s, &excluded), "the globs changed since");
        s.running = true;
        assert!(
            !replace_allowed(&s, &query("foo")),
            "a newer search is running"
        );
        s.running = false;
        s.results_for = None;
        assert!(
            !replace_allowed(&s, &query("foo")),
            "results of no finished search"
        );
    }

    fn names(found: &[FileHits], root: &Path) -> Vec<String> {
        found
            .iter()
            .map(|f| f.path.strip_prefix(root).unwrap().display().to_string())
            .collect()
    }

    #[test]
    fn search_skips_ignored_binary_and_large_files() {
        let root = project("skip");
        std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(root.join("ignored.txt"), "needle\n").unwrap();
        std::fs::write(
            root.join("src/a.rs"),
            "let x = 1;\n  let needle = 2; // needle\n",
        )
        .unwrap();
        std::fs::write(root.join(".env"), "NEEDLE_KEY=needle\n").unwrap();
        std::fs::write(root.join("blob.bin"), b"needle\0needle").unwrap();
        std::fs::write(
            root.join("big.txt"),
            "needle\n".repeat(MAX_FILE as usize / 7 + 1),
        )
        .unwrap();
        std::fs::write(root.join(".git/config"), "needle\n").unwrap();

        let (found, truncated) = run(&root, "needle");
        assert!(!truncated);
        assert_eq!(names(&found, &root), vec![".env", "src/a.rs"]);
        let hit = &found[1].hits[0];
        assert_eq!((hit.line, hit.column), (1, 6));
        assert_eq!(hit.text, "let needle = 2; // needle");
        assert_eq!(hit.ranges, vec![4..10, 19..25]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn match_case_whole_word_and_regex_toggles_narrow_the_search() {
        let root = project("case");
        std::fs::write(
            root.join("src/a.rs"),
            "Parser\nparser\nPARSER\nparsers\nüber Über\n",
        )
        .unwrap();
        let lines = |text: &str, case: bool, word: bool, regex: bool| -> Vec<u32> {
            let q = Query {
                text: text.into(),
                options: FindOptions { case, word, regex },
                ..Query::default()
            };
            run_query(&root, &q)
                .0
                .iter()
                .flat_map(|f| f.hits.iter().map(|h| h.line))
                .collect()
        };
        assert_eq!(lines("Parser", false, false, false), [0, 1, 2, 3]);
        assert_eq!(lines("Parser", true, false, false), [0]);
        assert_eq!(lines("parser", false, true, false), [0, 1, 2]);
        assert_eq!(lines("pars.r", false, false, false), Vec::<u32>::new());
        assert_eq!(lines("^pars.rs?$", true, true, true), [1, 3]);
        assert_eq!(lines("über", false, true, false), [4]);
        let hits = run_query(
            &root,
            &Query {
                text: "über".into(),
                options: FindOptions {
                    word: true,
                    ..FindOptions::default()
                },
                ..Query::default()
            },
        )
        .0;
        assert_eq!(hits[0].hits[0].ranges.len(), 2);
        let invalid = Query {
            text: "(".into(),
            options: FindOptions {
                regex: true,
                ..FindOptions::default()
            },
            ..Query::default()
        };
        assert!(Matcher::new(&invalid).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn include_and_exclude_globs_pick_files_and_folders_as_vs_code_does() {
        let root = project("globs");
        for f in [
            "src/a.ts",
            "src/a.go",
            "src/web/b.ts",
            "web/c.ts",
            "docs/d.md",
            "src/gen/e.ts",
        ] {
            std::fs::create_dir_all(root.join(f).parent().unwrap()).unwrap();
            std::fs::write(root.join(f), "needle\n").unwrap();
        }
        let found = |include: &str, exclude: &str| {
            let q = Query {
                text: "needle".into(),
                include: include.into(),
                exclude: exclude.into(),
                ..Query::default()
            };
            names(&run_query(&root, &q).0, &root)
        };
        assert_eq!(
            found("*.ts", ""),
            ["src/a.ts", "src/gen/e.ts", "src/web/b.ts", "web/c.ts"]
        );
        assert_eq!(found("web", ""), ["src/web/b.ts", "web/c.ts"]);
        assert_eq!(found("./web", ""), ["web/c.ts"]);
        assert_eq!(found("src/*.ts", ""), ["src/a.ts"]);
        assert_eq!(
            found("*.{go,md}, web/", ""),
            ["docs/d.md", "src/a.go", "src/web/b.ts", "web/c.ts"]
        );
        assert_eq!(found("*.ts", "gen, ./web"), ["src/a.ts", "src/web/b.ts"]);
        assert_eq!(found("", "**/*.ts,*.md"), ["src/a.go"]);
        let bad = Query {
            text: "needle".into(),
            include: "src/[".into(),
            ..Query::default()
        };
        assert!(Matcher::new(&bad).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn search_stops_at_the_cap() {
        let root = project("cap");
        std::fs::write(root.join("src/a.txt"), "hit hit\n".repeat(MAX_HITS)).unwrap();
        std::fs::write(root.join("src/b.txt"), "hit\n").unwrap();
        let (found, truncated) = run(&root, "hit");
        assert!(truncated);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].hits.len(), MAX_HITS / 2);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn long_lines_show_a_window_around_the_match() {
        let line = format!("{}needle{}", "x".repeat(500), "y".repeat(500));
        let (text, ranges) = excerpt(&line, std::slice::from_ref(&(500..506)));
        assert!(text.starts_with('…'));
        assert_eq!(&text[ranges[0].clone()], "needle");
        assert!(text.len() <= EXCERPT + "…".len());
    }

    #[test]
    fn replace_writes_files_except_open_ones_and_keeps_dollars_literal() {
        let root = project("replace");
        std::fs::write(root.join("src/a.rs"), "old old\nkeep\n").unwrap();
        std::fs::write(root.join("src/b.rs"), "old\n").unwrap();
        std::fs::write(root.join("src/open.rs"), "old\n").unwrap();
        let open = HashSet::from([root.join("src/open.rs")]);
        let done = replace_on_disk(&root, &matcher("old"), "$new", &open);
        assert_eq!((done.files, done.matches), (2, 3));
        assert!(done.failed.is_empty());
        let read = |p: &str| std::fs::read_to_string(root.join(p)).unwrap();
        assert_eq!(read("src/a.rs"), "$new $new\nkeep\n");
        assert_eq!(read("src/b.rs"), "$new\n");
        assert_eq!(read("src/open.rs"), "old\n");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn replace_stays_inside_the_search_root() {
        let searched = project("replace-root");
        let other = project("replace-other");
        std::fs::write(searched.join("src/a.rs"), "old\n").unwrap();
        std::fs::write(other.join("src/a.rs"), "old\n").unwrap();
        let done = replace_on_disk(&searched, &matcher("old"), "new", &HashSet::new());
        assert_eq!(done.files, 1);
        assert_eq!(
            std::fs::read_to_string(searched.join("src/a.rs")).unwrap(),
            "new\n"
        );
        assert_eq!(
            std::fs::read_to_string(other.join("src/a.rs")).unwrap(),
            "old\n"
        );
        std::fs::remove_dir_all(&searched).unwrap();
        std::fs::remove_dir_all(&other).unwrap();
    }

    #[test]
    fn only_open_files_the_search_reads_are_left_for_their_editors() {
        let root = project("replace-open");
        let outside = project("replace-outside");
        std::fs::write(root.join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(root.join("ignored.rs"), "old\n").unwrap();
        std::fs::write(root.join("src/shown.rs"), "old\n").unwrap();
        std::fs::write(outside.join("src/far.rs"), "old\n").unwrap();
        let open = HashSet::from([
            root.join("ignored.rs"),
            root.join("src/shown.rs"),
            outside.join("src/far.rs"),
        ]);
        let done = replace_on_disk(&root, &matcher("old"), "new", &open);
        assert_eq!(done.for_editors, HashSet::from([root.join("src/shown.rs")]));
        assert_eq!(
            std::fs::read_to_string(root.join("ignored.rs")).unwrap(),
            "old\n"
        );
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn replacing_on_disk_keeps_the_mode_and_leaves_no_temp_file() {
        use std::os::unix::fs::PermissionsExt;
        let root = project("replace-mode");
        let script = root.join("src/run.sh");
        std::fs::write(&script, "echo old\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let done = replace_on_disk(&root, &matcher("old"), "new", &HashSet::new());
        assert_eq!(done.files, 1);
        assert_eq!(std::fs::read_to_string(&script).unwrap(), "echo new\n");
        let mode = std::fs::metadata(&script).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        assert!(!temp_path(&script).exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn regex_replace_fills_in_groups_only_in_files_the_globs_let_through() {
        let root = project("replace-regex");
        std::fs::write(root.join("src/a.go"), "x := f(a, b)\r\ny := f(c, d)\n").unwrap();
        std::fs::write(root.join("src/skip.go"), "f(a, b)\n").unwrap();
        let q = Query {
            text: r"f\((\w+), (\w+)\)".into(),
            options: FindOptions {
                regex: true,
                ..FindOptions::default()
            },
            exclude: "skip.go".into(),
            ..Query::default()
        };
        let m = Matcher::new(&q).unwrap().unwrap();
        let done = replace_on_disk(&root, &m, "g($2, $1)", &HashSet::new());
        assert_eq!((done.files, done.matches), (1, 2));
        assert_eq!(
            std::fs::read_to_string(root.join("src/a.go")).unwrap(),
            "x := g(b, a)\r\ny := g(d, c)\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("src/skip.go")).unwrap(),
            "f(a, b)\n"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn commas_inside_braces_stay_in_one_glob() {
        assert_eq!(split_globs("*.{ts,tsx}, src"), ["*.{ts,tsx}", " src"]);
        assert!(globs(" , ").unwrap().is_none());
    }
}
