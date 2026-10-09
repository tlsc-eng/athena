use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

use athena_ui::{ActiveTheme, Button, ButtonKind, ContextMenu, InputEvent, MenuItem, TextInput};
use gpui::{
    AnyElement, App, ClickEvent, ClipboardItem, Context, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, FontWeight, KeyBinding, Keystroke, MouseButton, MouseDownEvent, Pixels,
    Point, ScrollStrategy, SharedString, Subscription, UniformListScrollHandle, Window, div,
    prelude::*, uniform_list,
};

use crate::keymap::{self, NewEntry, Recorder, Step};

/// The namespaces whose commands the table lists; the rest belong to text fields and menus.
const NAMESPACES: &[&str] = &[
    "athena",
    "editor",
    "terminal",
    "diff",
    "image_view",
    "large_file",
];

const ROW: f32 = 28.;

/// A change to keymap.json.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum KeymapEdit {
    Append(Vec<NewEntry>),
    Remove(Vec<usize>),
    Rebind { entry: usize, key: String },
}

impl KeymapEdit {
    pub(super) fn apply(&self, text: &str) -> Result<String, String> {
        match self {
            Self::Append(entries) => keymap::append_entries(text, entries),
            Self::Remove(entries) => keymap::remove_entries(text, entries),
            Self::Rebind { entry, key } => keymap::rebind_entry(text, *entry, key),
        }
    }
}

pub(super) enum ShortcutsEvent {
    Edit(KeymapEdit),
    OpenJson,
}

/// A binding as the table shows it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Bound {
    /// keymap.json's spelling.
    pub key: String,
    pub symbols: String,
    pub when: Option<String>,
    /// The keymap.json entry it comes from; `None` for Athena's own.
    pub entry: Option<usize>,
}

/// One line of the table: a command with one of its bindings, or with none.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Row {
    pub label: String,
    pub command: &'static str,
    pub binding: Option<Bound>,
    /// keymap.json can name it, so it can be given keys; commands that take arguments cannot.
    pub bindable: bool,
}

impl Bound {
    fn of(b: &KeyBinding) -> Self {
        Self {
            key: keymap::key_text(b),
            symbols: super::palette::keystrokes(b),
            when: b.predicate().map(|p| p.to_string()),
            entry: b.meta().map(|m| m.0 as usize),
        }
    }
}

/// `editor::MoveLinesUp` as `Editor: Move lines up`; Athena's own commands go unprefixed.
fn humanize(name: &str) -> String {
    let (namespace, action) = name.split_once("::").unwrap_or(("", name));
    let mut words = String::new();
    for (i, c) in action.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            words.push(' ');
            words.extend(c.to_lowercase());
        } else {
            words.push(c);
        }
    }
    match namespace {
        "athena" | "" => words,
        ns => {
            let ns = ns.replace('_', " ");
            let mut chars = ns.chars();
            let first: String = chars
                .next()
                .into_iter()
                .flat_map(char::to_uppercase)
                .collect();
            format!("{first}{}: {words}", chars.as_str())
        }
    }
}

fn shown(name: &str) -> bool {
    name.split_once("::")
        .is_some_and(|(ns, _)| NAMESPACES.contains(&ns))
}

/// The table: each listed binding, then each listed command without one, both by label.
pub(super) fn build_rows(
    bindings: &[KeyBinding],
    commands: &[&'static str],
    labels: &HashMap<&'static str, String>,
    bindable: impl Fn(&str) -> bool,
) -> Vec<Row> {
    let label = |name: &str| labels.get(name).cloned().unwrap_or_else(|| humanize(name));
    let mut bound: Vec<Row> = bindings
        .iter()
        .filter(|b| shown(b.action().name()))
        .map(|b| Row {
            label: label(b.action().name()),
            command: b.action().name(),
            binding: Some(Bound::of(b)),
            bindable: bindable(b.action().name()),
        })
        .collect();
    let mut unbound: Vec<Row> = commands
        .iter()
        .filter(|name| shown(name) && bindable(name))
        .filter(|name| !bound.iter().any(|r| r.command == **name))
        .map(|name| Row {
            label: label(name),
            command: name,
            binding: None,
            bindable: true,
        })
        .collect();
    bound.sort_by_key(|r| r.label.to_lowercase());
    unbound.sort_by_key(|r| r.label.to_lowercase());
    unbound.dedup_by(|a, b| a.command == b.command);
    bound.extend(unbound);
    bound
}

/// Gives `row`'s command `key` instead of the binding it has, as VS Code does: a user binding
/// is changed where it is, and a default one is replaced by a new binding and a removal.
pub(super) fn change(row: &Row, key: &str) -> Option<KeymapEdit> {
    let command = row.command.to_string();
    match &row.binding {
        None => add(row, key),
        Some(b) if b.key == key => None,
        Some(Bound {
            entry: Some(entry), ..
        }) => Some(KeymapEdit::Rebind {
            entry: *entry,
            key: key.to_string(),
        }),
        Some(b) => Some(KeymapEdit::Append(vec![
            NewEntry {
                key: key.to_string(),
                command: command.clone(),
                when: b.when.clone(),
            },
            NewEntry {
                key: b.key.clone(),
                command: format!("-{command}"),
                when: b.when.clone(),
            },
        ])),
    }
}

/// Another binding for `row`'s command, beside any it has.
pub(super) fn add(row: &Row, key: &str) -> Option<KeymapEdit> {
    Some(KeymapEdit::Append(vec![NewEntry {
        key: key.to_string(),
        command: row.command.to_string(),
        when: None,
    }]))
}

/// Takes `row`'s binding away: a user one is deleted, a default one removed by a `-` entry.
pub(super) fn remove(row: &Row) -> Option<KeymapEdit> {
    match row.binding.as_ref()? {
        Bound {
            entry: Some(entry), ..
        } => Some(KeymapEdit::Remove(vec![*entry])),
        b => Some(KeymapEdit::Append(vec![NewEntry {
            key: b.key.clone(),
            command: format!("-{}", row.command),
            when: b.when.clone(),
        }])),
    }
}

/// Deletes every keymap.json entry about `command`, so Athena's own bindings of it come back.
pub(super) fn reset(command: &str, entries: &[Option<String>]) -> Option<KeymapEdit> {
    let removal = format!("-{command}");
    let found: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, c)| c.as_deref() == Some(command) || c.as_deref() == Some(&removal))
        .map(|(i, _)| i)
        .collect();
    (!found.is_empty()).then_some(KeymapEdit::Remove(found))
}

fn row_matches(row: &Row, query: &str) -> bool {
    let binding = row.binding.as_ref();
    let source = match binding.map(|b| b.entry.is_some()) {
        Some(true) => "user",
        Some(false) => "default",
        None => "",
    };
    let hay = format!(
        "{} {} {} {} {} {source}",
        row.label,
        row.command,
        binding.map_or("", |b| b.key.as_str()),
        binding.map_or("", |b| b.symbols.as_str()),
        binding.and_then(|b| b.when.as_deref()).unwrap_or(""),
    )
    .to_lowercase();
    query
        .to_lowercase()
        .split_whitespace()
        .all(|word| hay.contains(word))
}

struct Recording {
    /// The row recorded for, by index in the whole table.
    row: usize,
    /// Adds a binding instead of changing the row's.
    adding: bool,
    recorder: Recorder,
    problem: Option<String>,
    focus: FocusHandle,
    _intercept: Subscription,
    _blur: Subscription,
}

/// An entry of keymap.json Athena could not use.
struct Invalid {
    text: String,
    why: String,
}

/// The keyboard shortcuts editor: every command with its bindings, searchable and rebindable.
pub(super) struct ShortcutsView {
    search: Entity<TextInput>,
    rows: Rc<Vec<Row>>,
    /// Rows the search leaves, by index in `rows`.
    filtered: Rc<Vec<usize>>,
    selected: Option<usize>,
    bindings: Vec<KeyBinding>,
    /// Each keymap.json entry's command, for Reset.
    entries: Vec<Option<String>>,
    invalid: Vec<Invalid>,
    recording: Option<Recording>,
    pending_record: Option<usize>,
    menu: Option<(Entity<ContextMenu>, Subscription)>,
    scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ShortcutsEvent> for ShortcutsView {}

impl Focusable for ShortcutsView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.search.focus_handle(cx)
    }
}

impl ShortcutsView {
    pub(super) fn new(cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| TextInput::new("Search keybindings", cx));
        let subscription =
            cx.subscribe(
                &search,
                |this: &mut Self, _, event: &InputEvent, cx| match event {
                    InputEvent::Changed => this.filter(cx),
                    InputEvent::Up => this.step(-1, cx),
                    InputEvent::Down => this.step(1, cx),
                    InputEvent::Cancel => this.search.update(cx, |s, cx| s.set_text("", cx)),
                    // The field has no window to hand over; the next frame starts recording.
                    InputEvent::Submit | InputEvent::SubmitBeside => {
                        this.pending_record = this.selected;
                        cx.notify();
                    }
                },
            );
        let mut view = Self {
            search,
            rows: Rc::default(),
            filtered: Rc::default(),
            selected: None,
            bindings: Vec::new(),
            entries: Vec::new(),
            invalid: Vec::new(),
            recording: None,
            pending_record: None,
            menu: None,
            scroll: UniformListScrollHandle::new(),
            _subscriptions: vec![subscription],
        };
        view.reload(cx);
        view
    }

    pub(super) fn label(&self) -> String {
        "Keyboard Shortcuts".into()
    }

    /// Reads the bindings in force and keymap.json again.
    pub(super) fn reload(&mut self, cx: &mut Context<Self>) {
        let selected = self.selected.and_then(|i| self.rows.get(i)).cloned();
        self.bindings = cx.key_bindings().borrow().bindings().cloned().collect();
        let mut labels: HashMap<&'static str, String> = super::palette::commands()
            .into_iter()
            .map(|(label, action)| (action.name(), label.to_string()))
            .collect();
        for (label, name) in super::palette::EDITOR_COMMANDS
            .iter()
            .chain(super::palette::TERMINAL_COMMANDS)
        {
            labels.insert(name, label.to_string());
        }
        labels.insert("editor::GoToLine", "Go to line".into());
        let commands: Vec<&'static str> = cx.all_action_names().to_vec();
        let rows = build_rows(&self.bindings, &commands, &labels, |name| {
            cx.build_action(name, None).is_ok()
        });
        let text = keymap::path()
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        self.entries = keymap::entry_commands(&text);
        self.invalid = keymap::problems_at(&text, |name, args| {
            cx.build_action(name, args)
                .map_err(|e| anyhow::anyhow!("{e}"))
        })
        .into_iter()
        .map(|(range, why, _)| Invalid {
            text: entry_text(&text, range),
            why,
        })
        .collect();
        self.selected = selected.and_then(|old| {
            rows.iter().position(|r| {
                r.command == old.command && r.binding.is_some() == old.binding.is_some()
            })
        });
        self.rows = Rc::new(rows);
        self.filter(cx);
    }

    fn filter(&mut self, cx: &mut Context<Self>) {
        let query = self.search.read(cx).text().to_string();
        self.filtered = Rc::new(
            self.rows
                .iter()
                .enumerate()
                .filter(|(_, r)| row_matches(r, &query))
                .map(|(i, _)| i)
                .collect(),
        );
        if self.selected.is_some_and(|s| !self.filtered.contains(&s)) {
            self.selected = None;
        }
        cx.notify();
    }

    fn step(&mut self, by: isize, cx: &mut Context<Self>) {
        if self.filtered.is_empty() {
            return;
        }
        let at = self
            .selected
            .and_then(|s| self.filtered.iter().position(|&i| i == s));
        let next = match at {
            Some(at) => (at as isize + by).clamp(0, self.filtered.len() as isize - 1) as usize,
            None => 0,
        };
        self.selected = Some(self.filtered[next]);
        self.scroll.scroll_to_item(next, ScrollStrategy::Center);
        cx.notify();
    }

    fn user_entries_of(&self, command: &str) -> bool {
        reset(command, &self.entries).is_some()
    }

    fn start_recording(
        &mut self,
        row: usize,
        adding: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.rows.get(row).is_some_and(|r| r.bindable) {
            return;
        }
        self.menu = None;
        self.selected = Some(row);
        let focus = cx.focus_handle();
        let this = cx.entity().downgrade();
        let watched = focus.clone();
        // Ahead of the keymap, so ⌘W is recorded instead of closing the tab.
        let intercept = cx.intercept_keystrokes(move |event, window, cx| {
            if !watched.is_focused(window) {
                return;
            }
            let pressed = event.keystroke.clone();
            let _ = this.update(cx, |v, cx| v.press(&pressed, window, cx));
            cx.stop_propagation();
        });
        let blur = cx.on_blur(&focus, window, |this, _, cx| {
            this.recording = None;
            cx.notify();
        });
        window.focus(&focus);
        self.recording = Some(Recording {
            row,
            adding,
            recorder: Recorder::default(),
            problem: None,
            focus,
            _intercept: intercept,
            _blur: blur,
        });
        cx.notify();
    }

    fn press(&mut self, pressed: &Keystroke, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rec) = self.recording.as_mut() else {
            return;
        };
        match rec.recorder.press(pressed) {
            Step::Recording => rec.problem = rec.recorder.problem(),
            Step::Cancel => self.finish_recording(window, cx),
            Step::Accept(key) => {
                if rec.recorder.problem().is_some() {
                    return;
                }
                let row = &self.rows[rec.row];
                let edit = match rec.adding {
                    true => add(row, &key),
                    false => change(row, &key),
                };
                self.finish_recording(window, cx);
                if let Some(edit) = edit {
                    cx.emit(ShortcutsEvent::Edit(edit));
                }
            }
        }
        cx.notify();
    }

    fn finish_recording(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.recording = None;
        window.focus(&self.search.focus_handle(cx));
        cx.notify();
    }

    fn open_menu(
        &mut self,
        row: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(r) = self.rows.get(row).cloned() else {
            return;
        };
        self.selected = Some(row);
        let this = cx.entity().downgrade();
        let act = |f: fn(&mut Self, usize, &mut Window, &mut Context<Self>)| {
            let this = this.clone();
            move |window: &mut Window, cx: &mut App| {
                let _ = this.update(cx, |v, cx| f(v, row, window, cx));
            }
        };
        let mut items = vec![
            MenuItem::new(
                "Change Keybinding…",
                act(|v, row, w, cx| v.start_recording(row, false, w, cx)),
            )
            .disabled(!r.bindable),
        ];
        if r.binding.is_some() {
            items.push(
                MenuItem::new(
                    "Add Keybinding…",
                    act(|v, row, w, cx| v.start_recording(row, true, w, cx)),
                )
                .disabled(!r.bindable),
            );
            items.push(MenuItem::new(
                "Remove Keybinding",
                act(|v, row, _, cx| {
                    if let Some(edit) = remove(&v.rows[row]) {
                        cx.emit(ShortcutsEvent::Edit(edit));
                    }
                }),
            ));
        }
        items.push(
            MenuItem::new(
                "Reset Keybinding",
                act(|v, row, _, cx| {
                    if let Some(edit) = reset(v.rows[row].command, &v.entries) {
                        cx.emit(ShortcutsEvent::Edit(edit));
                    }
                }),
            )
            .disabled(!self.user_entries_of(r.command)),
        );
        items.push(MenuItem::separator());
        let command = r.command.to_string();
        items.push(MenuItem::new("Copy Command ID", move |_, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(command.clone()))
        }));
        let menu = ContextMenu::build(position, items, window, cx);
        let subscription = cx.subscribe(&menu, |this, menu, _: &DismissEvent, cx| {
            if this.menu.as_ref().is_some_and(|(m, _)| *m == menu) {
                this.menu = None;
                cx.notify();
            }
        });
        self.menu = Some((menu, subscription));
        cx.notify();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let query = !self.search.read(cx).text().trim().is_empty();
        let found = match self.filtered.len() {
            1 => "1 keybinding".to_string(),
            n => format!("{n} keybindings"),
        };
        div()
            .flex_none()
            .px(t.ui(24.))
            .pt(t.ui(16.))
            .pb(t.ui(12.))
            .flex()
            .flex_col()
            .gap(t.ui(10.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(t.typography.heading)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(t.color.content)
                            .child("Keyboard Shortcuts"),
                    )
                    .child(
                        Button::new("keymap-json", "Open Keyboard Shortcuts (JSON)", ButtonKind::Secondary)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(ShortcutsEvent::OpenJson))),
                    ),
            )
            .child(
                div()
                    .h(t.ui(28.))
                    .px(t.ui(8.))
                    .flex()
                    .items_center()
                    .gap(t.ui(8.))
                    .bg(t.color.surface)
                    .border_1()
                    .border_color(t.color.border_strong)
                    .rounded(t.shape.radius_control)
                    .text_size(t.typography.body)
                    .child(div().flex_1().min_w_0().child(self.search.clone()))
                    .when(query, |el| {
                        el.child(
                            div()
                                .flex_none()
                                .text_size(t.typography.caption)
                                .text_color(t.color.content_muted)
                                .child(found),
                        )
                    }),
            )
            .child(
                div()
                    .text_size(t.typography.caption)
                    .text_color(t.color.content_muted)
                    .child("Double-click a row, or press ↩ on it, to record new keys. Right-click for more."),
            )
            .into_any_element()
    }

    fn render_invalid(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.invalid.is_empty() {
            return None;
        }
        let t = cx.theme().clone();
        let title = match self.invalid.len() {
            1 => "keymap.json has an entry Athena cannot use".to_string(),
            n => format!("keymap.json has {n} entries Athena cannot use"),
        };
        Some(
            div()
                .mx(t.ui(24.))
                .mb(t.ui(12.))
                .p(t.ui(12.))
                .flex()
                .gap(t.ui(12.))
                .rounded(t.shape.radius_panel)
                .bg(t.color.danger_surface)
                .text_size(t.typography.caption)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(t.ui(4.))
                        .child(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(t.color.danger)
                                .child(title),
                        )
                        .children(self.invalid.iter().map(|bad| {
                            div()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .font_family(t.typography.mono.clone())
                                        .text_color(t.color.content)
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .child(bad.text.clone()),
                                )
                                .child(
                                    div()
                                        .text_color(t.color.content_secondary)
                                        .child(bad.why.clone()),
                                )
                        })),
                )
                .child(
                    Button::new("keymap-fix", "Open keymap.json", ButtonKind::Secondary)
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(ShortcutsEvent::OpenJson))),
                )
                .into_any_element(),
        )
    }

    fn render_recording(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let rec = self.recording.as_ref()?;
        let t = cx.theme().clone();
        let row = &self.rows[rec.row];
        let keys = rec.recorder.strokes();
        let shown = match keys.is_empty() {
            true => "Press keys…".to_string(),
            false => keys
                .iter()
                .map(keymap::symbols)
                .collect::<Vec<_>>()
                .join(" "),
        };
        let others: Vec<String> = match keys.is_empty() {
            true => Vec::new(),
            false => keymap::conflicts(&self.bindings, &rec.recorder.text())
                .into_iter()
                .filter(|b| {
                    rec.adding
                        || row.binding.as_ref().is_none_or(|own| {
                            !(b.action().name() == row.command && Bound::of(b) == *own)
                        })
                })
                .map(|b| {
                    let name = self
                        .rows
                        .iter()
                        .find(|r| r.command == b.action().name())
                        .map_or_else(|| humanize(b.action().name()), |r| r.label.clone());
                    match b.predicate() {
                        Some(when) => format!("{name} (when {when})"),
                        None => name,
                    }
                })
                .collect(),
        };
        let conflict = match others.len() {
            0 => None,
            1 => Some(format!("Also bound to {}", others[0])),
            n => Some(format!(
                "{n} other commands use these keys: {}",
                others.join(", ")
            )),
        };
        let verb = if rec.adding {
            "Add keys for"
        } else {
            "Press the keys for"
        };
        Some(
            div()
                .absolute()
                .top(t.ui(72.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .id("keystroke-recorder")
                        .key_context("KeystrokeRecorder")
                        .track_focus(&rec.focus)
                        .w(t.ui(440.))
                        .p(t.ui(16.))
                        .flex()
                        .flex_col()
                        .gap(t.ui(10.))
                        .bg(t.color.surface)
                        .border_1()
                        .border_color(t.color.border_strong)
                        .rounded(t.shape.radius_panel)
                        .shadow(vec![t.popover_shadow()])
                        .text_size(t.typography.caption)
                        .child(
                            div()
                                .text_size(t.typography.body)
                                .text_color(t.color.content)
                                .child(format!("{verb} “{}”, then ↩", row.label)),
                        )
                        .child(
                            div()
                                .h(t.ui(36.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(t.shape.radius_control)
                                .border_1()
                                .border_color(t.color.focus_ring)
                                .bg(t.color.surface_sunken)
                                .text_size(t.typography.heading)
                                .text_color(if keys.is_empty() {
                                    t.color.content_muted
                                } else {
                                    t.color.content
                                })
                                .child(shown),
                        )
                        .children(
                            rec.problem
                                .clone()
                                .map(|why| div().text_color(t.color.danger).child(why)),
                        )
                        .children(
                            conflict.map(|text| div().text_color(t.color.warning).child(text)),
                        )
                        .child(
                            div()
                                .text_color(t.color.content_muted)
                                .child("↩ saves · Esc cancels · a third key starts over"),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_table(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let head = |label: &'static str| {
            div()
                .text_size(t.typography.caption)
                .font_weight(FontWeight::MEDIUM)
                .text_color(t.color.content_muted)
                .child(label)
        };
        let rows = self.rows.clone();
        let filtered = self.filtered.clone();
        let selected = self.selected;
        let resettable: Rc<Vec<bool>> = Rc::new(
            rows.iter()
                .map(|r| self.user_entries_of(r.command))
                .collect(),
        );
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(t.color.border)
            .child(
                div()
                    .flex_none()
                    .h(t.ui(ROW))
                    .px(t.ui(24.))
                    .flex()
                    .items_center()
                    .gap(t.ui(12.))
                    .border_b_1()
                    .border_color(t.color.border)
                    .child(head("Command").flex_1().min_w_0())
                    .child(head("Keybinding").w(t.ui(170.)).flex_none())
                    .child(head("When").w(t.ui(170.)).flex_none())
                    .child(head("Source").w(t.ui(64.)).flex_none()),
            )
            .child(
                uniform_list(
                    "shortcuts",
                    filtered.len(),
                    cx.processor(move |_this, range: Range<usize>, _window, cx| {
                        range
                            .map(|at| {
                                let i = filtered[at];
                                let r = &rows[i];
                                let on = selected == Some(i);
                                let b = r.binding.as_ref();
                                let source = match b.map(|b| b.entry.is_some()) {
                                    Some(true) => "User",
                                    Some(false) => "Default",
                                    None => "",
                                };
                                div()
                                    .id(("shortcut", i))
                                    .w_full()
                                    .h(t.ui(ROW))
                                    .px(t.ui(24.))
                                    .flex()
                                    .items_center()
                                    .gap(t.ui(12.))
                                    .text_size(t.typography.caption)
                                    .when(on, |el| el.bg(t.color.surface_active))
                                    .when(!on, |el| el.hover(|s| s.bg(t.color.surface_hover)))
                                    .on_click(cx.listener(
                                        move |this, e: &ClickEvent, window, cx| {
                                            this.selected = Some(i);
                                            if e.click_count() >= 2 {
                                                this.start_recording(i, false, window, cx);
                                            }
                                            cx.notify();
                                        },
                                    ))
                                    .on_mouse_down(
                                        MouseButton::Right,
                                        cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                                            this.open_menu(i, e.position, window, cx)
                                        }),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .items_center()
                                            .gap(t.ui(8.))
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .child(
                                                div()
                                                    .flex_none()
                                                    .text_color(t.color.content)
                                                    .child(SharedString::from(r.label.clone())),
                                            )
                                            .child(
                                                div()
                                                    .min_w_0()
                                                    .overflow_hidden()
                                                    .text_color(t.color.content_muted)
                                                    .child(r.command),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .w(t.ui(170.))
                                            .flex_none()
                                            .flex()
                                            .items_center()
                                            .gap(t.ui(6.))
                                            .children(b.map(|b| {
                                                div()
                                                    .px(t.ui(6.))
                                                    .rounded(t.shape.radius_control)
                                                    .border_1()
                                                    .border_color(t.color.border_strong)
                                                    .bg(t.color.surface_sunken)
                                                    .text_color(t.color.content)
                                                    .child(b.symbols.clone())
                                            }))
                                            .when(on && r.bindable, |el| {
                                                el.child(
                                                    div()
                                                        .id(("shortcut-edit", i))
                                                        .px(t.ui(4.))
                                                        .rounded(t.shape.radius_control)
                                                        .text_color(t.color.content_muted)
                                                        .cursor_pointer()
                                                        .hover(|s| {
                                                            s.bg(t.color.surface_hover)
                                                                .text_color(t.color.content)
                                                        })
                                                        .tooltip(|_, cx| {
                                                            athena_ui::Tooltip::view(
                                                                "Change keybinding (↩)",
                                                                cx,
                                                            )
                                                        })
                                                        .on_click(cx.listener(
                                                            move |this, _, window, cx| {
                                                                this.start_recording(
                                                                    i, false, window, cx,
                                                                )
                                                            },
                                                        ))
                                                        .child("✎"),
                                                )
                                            }),
                                    )
                                    .child(
                                        div()
                                            .w(t.ui(170.))
                                            .flex_none()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .font_family(t.typography.mono.clone())
                                            .text_color(t.color.content_secondary)
                                            .child(
                                                b.and_then(|b| b.when.clone()).unwrap_or_default(),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .w(t.ui(64.))
                                            .flex_none()
                                            .text_color(match source {
                                                "User" => t.color.accent,
                                                _ => t.color.content_muted,
                                            })
                                            .child(source)
                                            .when(resettable[i] && source != "User", |el| {
                                                el.child(
                                                    div().text_color(t.color.accent).child("•"),
                                                )
                                            }),
                                    )
                                    .into_any_element()
                            })
                            .collect()
                    }),
                )
                .track_scroll(self.scroll.clone())
                .flex_1(),
            )
            .into_any_element()
    }
}

/// An entry's text on one line, for the list of entries Athena could not use.
fn entry_text(text: &str, range: Range<usize>) -> String {
    let one: Vec<&str> = text[range].split_whitespace().collect();
    let line = one.join(" ");
    match line.chars().count() > 120 {
        true => format!("{}…", line.chars().take(120).collect::<String>()),
        false => line,
    }
}

impl Render for ShortcutsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(row) = self.pending_record.take() {
            let this = cx.entity();
            window.defer(cx, move |window, cx| {
                this.update(cx, |v, cx| v.start_recording(row, false, window, cx))
            });
        }
        let t = cx.theme().clone();
        div()
            .key_context("Shortcuts")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.color.surface)
            .font_family(t.typography.ui.clone())
            .child(self.render_header(cx))
            .children(self.render_invalid(cx))
            .child(self.render_table(cx))
            .children(self.render_recording(cx))
            .children(self.menu.as_ref().map(|(menu, _)| menu.clone()))
    }
}

#[cfg(test)]
mod tests {
    use anyhow::{Result, anyhow};
    use gpui::{Action, KeyBindingContextPredicate};

    use super::*;
    use crate::actions::{NewTerminal, QuickOpen, SplitDown, SplitRight};

    fn build(name: &str, _: Option<serde_json::Value>) -> Result<Box<dyn Action>> {
        match name {
            "athena::NewTerminal" => Ok(Box::new(NewTerminal)),
            "athena::QuickOpen" => Ok(Box::new(QuickOpen)),
            "athena::SplitRight" => Ok(Box::new(SplitRight)),
            "athena::SplitDown" => Ok(Box::new(SplitDown)),
            _ => Err(anyhow!("unknown")),
        }
    }

    fn defaults() -> Vec<KeyBinding> {
        vec![
            KeyBinding::new("cmd-d", SplitRight, None),
            KeyBinding::new("cmd-shift-d", SplitDown, None),
            KeyBinding::new("cmd-t", NewTerminal, Some("!Terminal")),
        ]
    }

    /// The table after `text` is keymap.json.
    fn table(text: &str) -> Vec<Row> {
        let (rules, problems) = keymap::parse(text, build);
        assert!(problems.is_empty(), "{problems:?}\n{text}");
        let bindings = keymap::merge(&defaults(), rules);
        let labels = HashMap::from([("athena::SplitRight", "Split right".to_string())]);
        let commands = [
            "athena::QuickOpen",
            "athena::SplitRight",
            "context_menu::Up",
        ];
        build_rows(&bindings, &commands, &labels, |n| build(n, None).is_ok())
    }

    fn keys(rows: &[Row], command: &str) -> Vec<(String, Option<String>, bool)> {
        rows.iter()
            .filter(|r| r.command == command)
            .filter_map(|r| r.binding.as_ref())
            .map(|b| (b.key.clone(), b.when.clone(), b.entry.is_some()))
            .collect()
    }

    fn edited(text: &str, edit: Option<KeymapEdit>) -> String {
        edit.expect("an edit").apply(text).unwrap()
    }

    #[test]
    fn rows_list_bindings_by_label_then_unbound_commands() {
        let rows = table("[]");
        let summary: Vec<(&str, Option<&str>)> = rows
            .iter()
            .map(|r| (r.label.as_str(), r.binding.as_ref().map(|b| b.key.as_str())))
            .collect();
        assert_eq!(
            summary,
            [
                ("New terminal", Some("cmd-t")),
                ("Split down", Some("cmd-shift-d")),
                ("Split right", Some("cmd-d")),
                ("Quick open", None),
            ]
        );
        assert_eq!(
            rows[0].binding.as_ref().unwrap().when.as_deref(),
            Some("!Terminal")
        );
        assert_eq!(humanize("editor::MoveLinesUp"), "Editor: Move lines up");
        assert_eq!(humanize("image_view::ZoomIn"), "Image view: Zoom in");
    }

    #[test]
    fn changing_a_default_adds_the_new_keys_and_removes_the_old_in_its_context() {
        let rows = table("[]");
        let terminal = rows
            .iter()
            .find(|r| r.command == "athena::NewTerminal")
            .unwrap();
        let text = edited("[]", change(terminal, "cmd-k cmd-t"));
        let rows = table(&text);
        assert_eq!(
            keys(&rows, "athena::NewTerminal"),
            [("cmd-k cmd-t".into(), Some("!Terminal".into()), true)]
        );
        assert!(
            KeyBindingContextPredicate::parse("!Terminal").is_ok(),
            "a default's context reads back"
        );
        let user = rows
            .iter()
            .find(|r| r.command == "athena::NewTerminal")
            .unwrap();
        let text = edited(&text, change(user, "f7"));
        assert_eq!(
            keys(&table(&text), "athena::NewTerminal"),
            [("f7".into(), Some("!Terminal".into()), true)],
            "a user binding changes where it is"
        );
        assert_eq!(change(user, "cmd-k cmd-t"), None, "the keys it has already");
        let same = &table(&text)[0];
        assert_eq!(change(same, "f7"), None, "the same keys change nothing");
        let reset_text = edited(
            &text,
            reset("athena::NewTerminal", &keymap::entry_commands(&text)),
        );
        assert_eq!(
            keys(&table(&reset_text), "athena::NewTerminal"),
            [("cmd-t".into(), Some("!Terminal".into()), false)],
            "reset brings the default back"
        );
    }

    #[test]
    fn removing_and_adding_bindings_round_trip_through_keymap_json() {
        let rows = table("[]");
        let split = rows
            .iter()
            .find(|r| r.command == "athena::SplitRight")
            .unwrap();
        let text = edited("[]", remove(split));
        assert!(keys(&table(&text), "athena::SplitRight").is_empty());
        let open = table(&text)
            .into_iter()
            .find(|r| r.command == "athena::QuickOpen")
            .unwrap();
        assert!(open.binding.is_none() && open.bindable);
        let text = edited(&text, change(&open, "cmd-d"));
        let rows = table(&text);
        assert_eq!(
            keys(&rows, "athena::QuickOpen"),
            [("cmd-d".into(), None, true)]
        );
        let user = rows
            .iter()
            .find(|r| r.command == "athena::QuickOpen")
            .unwrap();
        let text = edited(&text, add(user, "cmd-shift-o"));
        assert_eq!(keys(&table(&text), "athena::QuickOpen").len(), 2);
        let user = table(&text)
            .into_iter()
            .find(|r| r.command == "athena::QuickOpen")
            .unwrap();
        let text = edited(&text, remove(&user));
        assert_eq!(keys(&table(&text), "athena::QuickOpen").len(), 1);
        assert_eq!(
            reset("athena::NewTerminal", &keymap::entry_commands(&text)),
            None
        );
    }

    #[test]
    fn search_matches_labels_commands_keys_symbols_and_sources() {
        let rows = table(r#"[{"key": "f7", "command": "QuickOpen"}]"#);
        let hits = |q: &str| -> Vec<&str> {
            rows.iter()
                .filter(|r| row_matches(r, q))
                .map(|r| r.command)
                .collect()
        };
        assert_eq!(hits("split right"), ["athena::SplitRight"]);
        assert_eq!(hits("cmd-shift-d"), ["athena::SplitDown"]);
        assert_eq!(hits("⇧⌘D"), ["athena::SplitDown"]);
        assert_eq!(hits("user"), ["athena::QuickOpen"]);
        assert_eq!(hits("terminal"), ["athena::NewTerminal"]);
    }
}
