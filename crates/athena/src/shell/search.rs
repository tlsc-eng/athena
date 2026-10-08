use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use athena_editor::EditorView;
use athena_ui::{ActiveTheme, Button, ButtonKind, InputEvent, TextInput};
use gpui::{
    AnyElement, Context, Entity, Focusable, FontWeight, HighlightStyle, PromptLevel,
    ScrollStrategy, SharedString, StyledText, Subscription, Task, UniformListScrollHandle, Window,
    div, prelude::*, px, uniform_list,
};
use regex::{NoExpand, Regex, RegexBuilder};

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
    files: Rc<Vec<FileHits>>,
    rows: Rc<Vec<Row>>,
    collapsed: HashSet<PathBuf>,
    selected: Option<(usize, usize)>,
    matches: usize,
    running: bool,
    truncated: bool,
    replacing: bool,
    cancel: Arc<AtomicBool>,
    task: Option<Task<()>>,
    scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

/// A literal, smart-case matcher: case matters only once the query has a capital letter.
pub(super) fn matcher(query: &str) -> Option<Regex> {
    if query.is_empty() {
        return None;
    }
    RegexBuilder::new(&regex::escape(query))
        .case_insensitive(!query.chars().any(char::is_uppercase))
        .build()
        .ok()
}

/// Every file a project search reads: .gitignore honoured, hidden files included, `.git` skipped.
fn walk(root: &Path) -> impl Iterator<Item = PathBuf> {
    ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|e| e.file_name() != ".git")
        .sort_by_file_name(|a, b| a.cmp(b))
        .build()
        .flatten()
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(|e| e.into_path())
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
    re: &Regex,
    cancel: &AtomicBool,
    mut send: impl FnMut(Vec<FileHits>) -> bool,
) -> bool {
    let mut batch = Vec::new();
    let mut pending = 0;
    let mut total = 0;
    for path in walk(root) {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let Some(text) = read_text(&path) else {
            continue;
        };
        let mut hits = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let ranges: Vec<Range<usize>> = re.find_iter(line).map(|m| m.range()).collect();
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

/// Replaces every match in files under `root` not in `skip`; returns (files, matches, failures).
fn replace_on_disk(
    root: &Path,
    re: &Regex,
    with: &str,
    skip: &HashSet<PathBuf>,
) -> (usize, usize, Vec<String>) {
    let (mut files, mut count, mut failed) = (0, 0, Vec::new());
    for path in walk(root).filter(|p| !skip.contains(p)) {
        let Some(text) = read_text(&path) else {
            continue;
        };
        let n = re.find_iter(&text).count();
        if n == 0 {
            continue;
        }
        let replaced = re.replace_all(&text, NoExpand(with));
        match std::fs::write(&path, replaced.as_bytes()) {
            Ok(()) => {
                files += 1;
                count += n;
            }
            Err(err) => failed.push(format!("{}: {err}", path.display())),
        }
    }
    (files, count, failed)
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

    fn query(&self, cx: &gpui::App) -> String {
        self.find
            .as_ref()
            .map(|i| i.read(cx).text().to_string())
            .unwrap_or_default()
    }
}

impl Shell {
    fn ensure_search_inputs(&mut self, cx: &mut Context<Self>) -> Entity<TextInput> {
        if let Some(find) = &self.search.find {
            return find.clone();
        }
        let find = cx.new(|cx| TextInput::new("Search", cx));
        let replace = cx.new(|cx| TextInput::new("Replace", cx));
        let subscriptions = vec![
            cx.subscribe(&find, |this, _, event: &InputEvent, cx| match event {
                InputEvent::Changed => this.schedule_search(cx),
                InputEvent::Up => this.step_search(-1, cx),
                InputEvent::Down => this.step_search(1, cx),
                InputEvent::Submit | InputEvent::SubmitBeside => this.open_selected_hit(cx),
                InputEvent::Cancel => this.close_search(cx),
            }),
            cx.subscribe(&replace, |this, _, event: &InputEvent, cx| {
                if *event == InputEvent::Cancel {
                    this.close_search(cx);
                }
            }),
        ];
        self.search.find = Some(find.clone());
        self.search.replace = Some(replace);
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

    fn schedule_search(&mut self, cx: &mut Context<Self>) {
        self.search.cancel.store(true, Ordering::Relaxed);
        self.search.cancel = Arc::default();
        let query = self.search.query(cx);
        let root = self.workspace.active_project().map(|p| p.root.clone());
        let (Some(re), Some(root)) = (matcher(&query), root) else {
            self.search.task = None;
            self.search.running = false;
            self.clear_results(cx);
            return;
        };
        let cancel = self.search.cancel.clone();
        self.search.running = true;
        cx.notify();
        self.search.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            tracing::debug!(query, root = %root.display(), "project search");
            let (tx, rx) = async_channel::unbounded();
            cx.background_executor()
                .spawn(async move {
                    let truncated = search(&root, &re, &cancel, |batch| {
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
                            tracing::debug!(
                                matches = this.search.matches,
                                truncated,
                                "project search done"
                            );
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
        let query = self.search.query(cx);
        let (Some(re), Some(root)) = (
            matcher(&query),
            self.workspace.active_project().map(|p| p.root.clone()),
        ) else {
            return;
        };
        if self.search.files.is_empty() || self.search.replacing {
            return;
        }
        let with = self
            .search
            .replace
            .as_ref()
            .map(|i| i.read(cx).text().to_string())
            .unwrap_or_default();
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
            let Ok((open, in_editors)) = this.update(cx, |this, cx| {
                this.replace_in_editors(&root, &re, &with, cx)
            }) else {
                return;
            };
            let disk_root = root.clone();
            let (files, count, failed) = cx
                .background_executor()
                .spawn(async move { replace_on_disk(&disk_root, &re, &with, &open) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.search.replacing = false;
                let total = count + in_editors.1;
                tracing::info!(files = files + in_editors.0, total, "replaced in project");
                let title = format!(
                    "Replaced {total} {} in {} {}",
                    if total == 1 {
                        "occurrence"
                    } else {
                        "occurrences"
                    },
                    files + in_editors.0,
                    if files + in_editors.0 == 1 {
                        "file"
                    } else {
                        "files"
                    },
                );
                let body = if failed.is_empty() {
                    String::new()
                } else {
                    format!("Could not write {}", failed.join(", "))
                };
                this.transient_notice(&title, body, cx);
                this.git_kick(cx);
                this.schedule_search(cx);
            });
        })
        .detach();
        self.search.replacing = true;
    }

    /// Applies the replacement to every editor under `root`; returns their paths and (files, matches).
    fn replace_in_editors(
        &mut self,
        root: &Path,
        re: &Regex,
        with: &str,
        cx: &mut Context<Self>,
    ) -> (HashSet<PathBuf>, (usize, usize)) {
        let editors: Vec<Entity<EditorView>> = self
            .items
            .iter()
            .filter(|((r, _), _)| r == root)
            .filter_map(|(_, v)| match v {
                ItemView::Editor(e) => Some(e.clone()),
                _ => None,
            })
            .collect();
        let mut open = HashSet::new();
        let mut buffers = HashSet::new();
        let mut changed = HashSet::new();
        let mut count = 0;
        for editor in editors {
            let path = editor.read(cx).path().to_path_buf();
            // Tabs on the same file share one buffer, which must be replaced in only once.
            if !buffers.insert(super::lsp::document_key(&path)) {
                continue;
            }
            let Some(text) = editor.read(cx).text() else {
                continue;
            };
            open.insert(path.clone());
            let n = re.find_iter(&text).count();
            if n == 0 {
                continue;
            }
            let replaced = re.replace_all(&text, NoExpand(with)).into_owned();
            editor.update(cx, |e, cx| e.replace_text(&replaced, cx));
            if changed.insert(path) {
                count += n;
            }
        }
        (open, (changed.len(), count))
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
        let replace = self.search.replace.clone();
        let t = cx.theme().clone();
        let field = |input: Entity<TextInput>| {
            div()
                .flex_1()
                .min_w_0()
                .h(px(24.))
                .px(px(8.))
                .flex()
                .items_center()
                .bg(t.color.surface)
                .border_1()
                .border_color(t.color.border)
                .rounded(t.shape.radius_control)
                .text_size(t.typography.caption)
                .child(input)
        };
        let can_replace = !self.search.files.is_empty() && !self.search.replacing;
        let bar = div()
            .h(px(36.))
            .flex_none()
            .px(px(12.))
            .flex()
            .items_center()
            .gap(px(8.))
            .child(field(find))
            .children(replace.map(field))
            .children(can_replace.then(|| {
                Button::new("search-replace-all", "Replace All", ButtonKind::Secondary)
                    .on_click(cx.listener(|this, _, window, cx| this.replace_all(window, cx)))
            }));
        let query = self.search.query(cx);
        let message = |text: &str| {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(t.typography.caption)
                .text_color(t.color.content_muted)
                .child(text.to_string())
                .into_any_element()
        };
        let body = if query.is_empty() {
            message("Type to search every file in this project.")
        } else if self.search.rows.is_empty() {
            message(if self.search.running {
                "Searching…"
            } else {
                "No results."
            })
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
        let root = self.workspace.active_project().map(|p| p.root.clone());
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
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.open_hit(fi, hi, cx)),
                                )
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

    fn run(root: &Path, query: &str) -> (Vec<FileHits>, bool) {
        let mut out = Vec::new();
        let truncated = search(
            root,
            &matcher(query).unwrap(),
            &AtomicBool::new(false),
            |b| {
                out.extend(b);
                true
            },
        );
        (out, truncated)
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
    fn search_is_smart_case() {
        let root = project("case");
        std::fs::write(root.join("src/a.rs"), "Parser\nparser\nPARSER\n").unwrap();
        let lines = |q: &str| -> Vec<u32> {
            run(&root, q)
                .0
                .iter()
                .flat_map(|f| f.hits.iter().map(|h| h.line))
                .collect()
        };
        assert_eq!(lines("parser"), vec![0, 1, 2]);
        assert_eq!(lines("Parser"), vec![0]);
        assert_eq!(lines("a.b"), Vec::<u32>::new());
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
        let skip = HashSet::from([root.join("src/open.rs")]);
        let (files, count, failed) =
            replace_on_disk(&root, &matcher("old").unwrap(), "$new", &skip);
        assert_eq!((files, count), (2, 3));
        assert!(failed.is_empty());
        let read = |p: &str| std::fs::read_to_string(root.join(p)).unwrap();
        assert_eq!(read("src/a.rs"), "$new $new\nkeep\n");
        assert_eq!(read("src/b.rs"), "$new\n");
        assert_eq!(read("src/open.rs"), "old\n");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
