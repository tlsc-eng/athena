use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use athena_ui::{ActiveTheme, Button, ButtonKind, ContextMenu, InputEvent, MenuItem, TextInput};
use gpui::{
    AnyElement, App, ClickEvent, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, FontWeight, Pixels, Point, ScrollHandle, SharedString, Subscription, Task, Window,
    div, prelude::*, px,
};
use serde_json::{Value, json};

use crate::settings::schema::{Kind, SETTINGS, Setting};
use crate::settings::{self, Settings};

/// How long a typed number waits before it is written, so `16` is not first written as `1`.
const TYPING_DELAY: Duration = Duration::from_millis(600);

/// Which file the Settings tab edits: the user's settings.json or the project's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Scope {
    User,
    Project,
}

pub(super) enum SettingsEvent {
    Set {
        scope: Scope,
        keys: &'static [&'static str],
        value: Value,
    },
    Unset {
        scope: Scope,
        keys: &'static [&'static str],
    },
    OpenJson(Scope),
}

/// A file's settings, or why it could not be read.
type Read = Result<Settings, String>;

/// How one setting shows in a scope.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct RowState {
    /// What applies to files in this scope.
    pub value: Value,
    /// The scope's own file sets it, so it can be reset.
    pub modified: bool,
    /// The other scope's file sets it too.
    pub elsewhere: bool,
}

/// `setting` as `scope` sees it: the project's value over the user's over workspace.json's
/// choice (`fallback`) over the default, as Athena applies them.
pub(super) fn row_state(
    setting: &Setting,
    scope: Scope,
    user: Option<&Settings>,
    project: Option<&Settings>,
    fallback: Option<&Value>,
) -> RowState {
    let user = user.and_then(|s| setting.value_in(s));
    let project = project.and_then(|s| setting.value_in(s));
    let (own, other) = match scope {
        Scope::User => (&user, &project),
        Scope::Project => (&project, &user),
    };
    let value = match scope {
        Scope::User => user.clone(),
        Scope::Project => project.clone().or_else(|| user.clone()),
    };
    RowState {
        value: value
            .or_else(|| fallback.cloned())
            .unwrap_or_else(|| setting.default_value()),
        modified: own.is_some(),
        elsewhere: other.is_some(),
    }
}

/// Whether every word of `query` is in the setting's group, title, name or description.
fn matches(setting: &Setting, query: &str) -> bool {
    let hay = format!(
        "{} {} {} {} {}",
        setting.group.title(),
        setting.title,
        setting.id(),
        setting.aliases.join(" "),
        setting.description
    )
    .to_lowercase();
    query
        .to_lowercase()
        .split_whitespace()
        .all(|word| hay.contains(word))
}

/// A number as a field shows it.
fn field_text(value: &Value) -> String {
    match value {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => String::new(),
    }
}

/// The settings editor: every setting with its control, for the user's or the project's file.
pub(super) struct SettingsView {
    root: PathBuf,
    search: Entity<TextInput>,
    scope: Scope,
    user: Read,
    project: Read,
    /// What applies where no file sets a setting, by id, when workspace.json chose it.
    fallback: HashMap<String, Value>,
    /// The number fields, by index in [`SETTINGS`].
    fields: HashMap<usize, Entity<TextInput>>,
    /// Why a field's text cannot be saved.
    errors: HashMap<usize, String>,
    typing: HashMap<usize, Task<()>>,
    menu: Option<(Entity<ContextMenu>, Subscription)>,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SettingsEvent> for SettingsView {}

impl Focusable for SettingsView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.search.focus_handle(cx)
    }
}

impl SettingsView {
    pub(super) fn new(
        root: PathBuf,
        fallback: HashMap<String, Value>,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| TextInput::new("Search settings", cx));
        let mut subscriptions =
            vec![
                cx.subscribe(&search, |this: &mut Self, _, event: &InputEvent, cx| {
                    if *event == InputEvent::Changed {
                        this.scroll.set_offset(Point::default());
                        cx.notify();
                    }
                }),
            ];
        let mut fields = HashMap::new();
        for (i, setting) in SETTINGS.iter().enumerate() {
            if !matches!(setting.kind, Kind::Whole { .. } | Kind::Number { .. }) {
                continue;
            }
            let input = cx.new(|cx| TextInput::new(field_text(&setting.default_value()), cx));
            subscriptions.push(
                cx.subscribe(&input, move |this, _, event: &InputEvent, cx| match event {
                    InputEvent::Changed => this.typed(i, false, cx),
                    InputEvent::Submit | InputEvent::SubmitBeside => this.typed(i, true, cx),
                    _ => {}
                }),
            );
            fields.insert(i, input);
        }
        let mut view = Self {
            root,
            search,
            scope: Scope::User,
            user: Ok(Settings::default()),
            project: Ok(Settings::default()),
            fallback: HashMap::new(),
            fields,
            errors: HashMap::new(),
            typing: HashMap::new(),
            menu: None,
            scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        };
        view.reload(fallback, cx);
        view
    }

    pub(super) fn label(&self) -> String {
        "Settings".into()
    }

    /// Reads both files again, as they changed or were written.
    pub(super) fn reload(&mut self, fallback: HashMap<String, Value>, cx: &mut Context<Self>) {
        self.user = settings::load().map(|(s, _)| s);
        self.project = read_project(&self.root);
        self.fallback = fallback;
        self.sync_fields(None, cx);
        cx.notify();
    }

    fn read(&self, scope: Scope) -> &Read {
        match scope {
            Scope::User => &self.user,
            Scope::Project => &self.project,
        }
    }

    fn state(&self, i: usize) -> RowState {
        let s = &SETTINGS[i];
        row_state(
            s,
            self.scope,
            self.user.as_ref().ok(),
            self.project.as_ref().ok(),
            self.fallback.get(&s.id()),
        )
    }

    /// Puts each number field's value in, except in a field being typed in.
    fn sync_fields(&mut self, window: Option<&Window>, cx: &mut Context<Self>) {
        let fields: Vec<(usize, Entity<TextInput>)> =
            self.fields.iter().map(|(i, f)| (*i, f.clone())).collect();
        for (i, field) in fields {
            let editing = window.is_some_and(|w| field.focus_handle(cx).is_focused(w))
                || self.typing.contains_key(&i);
            let text = field_text(&self.state(i).value);
            if !editing && field.read(cx).text() != text {
                self.errors.remove(&i);
                field.update(cx, |f, cx| f.set_text(text, cx));
            }
        }
    }

    fn typed(&mut self, i: usize, now: bool, cx: &mut Context<Self>) {
        let Some(field) = self.fields.get(&i) else {
            return;
        };
        let setting = &SETTINGS[i];
        let text = field.read(cx).text().to_string();
        self.typing.remove(&i);
        let value = match setting.parse_input(&text) {
            Ok(value) => value,
            Err(why) => {
                self.errors.insert(i, why);
                return cx.notify();
            }
        };
        self.errors.remove(&i);
        cx.notify();
        if value == self.state(i).value && !now {
            return;
        }
        let scope = self.scope;
        if now {
            return self.set(scope, i, value, cx);
        }
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TYPING_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                this.typing.remove(&i);
                this.set(scope, i, value, cx);
            });
        });
        self.typing.insert(i, task);
    }

    fn set(&mut self, scope: Scope, i: usize, value: Value, cx: &mut Context<Self>) {
        let state = self.state(i);
        if state.modified && state.value == value {
            return;
        }
        cx.emit(SettingsEvent::Set {
            scope,
            keys: SETTINGS[i].keys,
            value,
        });
    }

    fn reset(&mut self, i: usize, cx: &mut Context<Self>) {
        self.typing.remove(&i);
        self.errors.remove(&i);
        cx.emit(SettingsEvent::Unset {
            scope: self.scope,
            keys: SETTINGS[i].keys,
        });
    }

    fn choose(
        &mut self,
        i: usize,
        value: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The file watcher applies the rest; the theme changes now, as the menu item does.
        if SETTINGS[i].keys == ["theme"] {
            let choice = match value {
                "light" => athena_workspace::ThemeChoice::Light,
                "dark" => athena_workspace::ThemeChoice::Dark,
                _ => athena_workspace::ThemeChoice::System,
            };
            athena_ui::set_appearance(super::appearance::resolve(choice, window), cx);
        }
        self.set(self.scope, i, json!(value), cx);
    }

    fn open_choice(
        &mut self,
        i: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Kind::Choice { options, .. } = SETTINGS[i].kind else {
            return;
        };
        let current = self.state(i).value;
        let this = cx.entity().downgrade();
        let items = options
            .iter()
            .map(|&(value, label)| {
                let this = this.clone();
                let mark = if current == json!(value) {
                    "✓  "
                } else {
                    "    "
                };
                MenuItem::new(format!("{mark}{label}"), move |window, cx| {
                    let _ = this.update(cx, |v, cx| v.choose(i, value, window, cx));
                })
            })
            .collect();
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

    fn set_scope(&mut self, scope: Scope, window: &mut Window, cx: &mut Context<Self>) {
        if self.scope == scope {
            return;
        }
        self.scope = scope;
        self.typing.clear();
        self.errors.clear();
        self.sync_fields(Some(window), cx);
        self.scroll.set_offset(Point::default());
        cx.notify();
    }

    fn render_header(&self, shown: usize, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let query = !self.search.read(cx).text().trim().is_empty();
        let scope = self.scope;
        let project_name = self
            .root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let tab = |id: &'static str, label: String, this_scope: Scope| {
            let on = scope == this_scope;
            div()
                .id(id)
                .h(t.ui(28.))
                .px(t.ui(10.))
                .flex()
                .items_center()
                .border_b_2()
                .border_color(if on {
                    t.color.accent
                } else {
                    gpui::transparent_black()
                })
                .text_color(if on {
                    t.color.content
                } else {
                    t.color.content_muted
                })
                .font_weight(if on {
                    FontWeight::MEDIUM
                } else {
                    FontWeight::NORMAL
                })
                .cursor_pointer()
                .hover(|s| s.text_color(t.color.content))
                .child(label)
        };
        div()
            .flex_none()
            .px(t.ui(24.))
            .pt(t.ui(16.))
            .flex()
            .flex_col()
            .gap(t.ui(10.))
            .border_b_1()
            .border_color(t.color.border)
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
                            .child("Settings"),
                    )
                    .child(
                        Button::new(
                            "settings-json",
                            "Open Settings (JSON)",
                            ButtonKind::Secondary,
                        )
                        .on_click(
                            cx.listener(move |_, _, _, cx| cx.emit(SettingsEvent::OpenJson(scope))),
                        ),
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
                        let found = match shown {
                            1 => "1 setting found".to_string(),
                            n => format!("{n} settings found"),
                        };
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
                    .flex()
                    .items_end()
                    .gap(t.ui(4.))
                    .text_size(t.typography.caption)
                    .child(tab("scope-user", "User".into(), Scope::User).on_click(
                        cx.listener(|this, _, window, cx| this.set_scope(Scope::User, window, cx)),
                    ))
                    .child(
                        tab(
                            "scope-project",
                            format!("Project · {project_name}"),
                            Scope::Project,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.set_scope(Scope::Project, window, cx)
                        })),
                    ),
            )
            .into_any_element()
    }

    fn render_unreadable(&self, why: &str, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let scope = self.scope;
        let file = match scope {
            Scope::User => "settings.json",
            Scope::Project => ".athena/settings.json",
        };
        div()
            .mx(t.ui(24.))
            .mt(t.ui(12.))
            .p(t.ui(12.))
            .flex()
            .items_center()
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
                    .gap(t.ui(2.))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(t.color.danger)
                            .child(format!(
                                "{file} could not be read, so nothing here can change"
                            )),
                    )
                    .child(div().text_color(t.color.content).child(why.to_string())),
            )
            .child(
                Button::new(
                    "settings-fix",
                    format!("Open {file}"),
                    ButtonKind::Secondary,
                )
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(SettingsEvent::OpenJson(scope)))),
            )
            .into_any_element()
    }

    fn render_row(&self, i: usize, editable: bool, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let s = &SETTINGS[i];
        let state = self.state(i);
        let elsewhere = match (state.elsewhere, self.scope) {
            (false, _) => None,
            (true, Scope::User) => Some("Also set for this project"),
            (true, Scope::Project) => Some("Also set in User"),
        };
        let title = div()
            .flex()
            .items_center()
            .gap(t.ui(8.))
            .text_size(t.typography.body)
            .child(
                div()
                    .text_color(t.color.content)
                    .child(format!("{}: ", s.group.title()))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(s.title)),
                    )
                    .flex(),
            )
            .when(state.modified, |el| {
                el.child(
                    div()
                        .text_size(t.typography.caption)
                        .text_color(t.color.accent)
                        .child("Modified"),
                )
            })
            .children(elsewhere.map(|text| {
                div()
                    .text_size(t.typography.caption)
                    .text_color(t.color.content_muted)
                    .child(text)
            }))
            .when(state.modified && editable, |el| {
                el.child(
                    Button::new(("setting-reset", i), "Reset", ButtonKind::Ghost)
                        .on_click(cx.listener(move |this, _, _, cx| this.reset(i, cx))),
                )
            });
        let description = |el: gpui::Div| {
            el.text_size(t.typography.caption)
                .text_color(t.color.content_secondary)
                .child(SharedString::from(s.description))
        };
        let control: AnyElement = match s.kind {
            Kind::Toggle { .. } => {
                let on = state.value == json!(true);
                div()
                    .id(("setting-toggle", i))
                    .flex()
                    .items_start()
                    .gap(t.ui(8.))
                    .when(editable, |el| {
                        el.cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let scope = this.scope;
                                this.set(scope, i, json!(!on), cx)
                            }))
                    })
                    .child(
                        div()
                            .flex_none()
                            .mt(t.ui(1.))
                            .size(t.ui(14.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(3.))
                            .border_1()
                            .border_color(if on {
                                t.color.accent
                            } else {
                                t.color.border_strong
                            })
                            .bg(if on { t.color.accent } else { t.color.surface })
                            .text_size(t.ui(10.))
                            .text_color(t.color.content_on_accent)
                            .when(on, |el| el.child("✓")),
                    )
                    .child(description(div().flex_1().min_w_0()))
                    .into_any_element()
            }
            Kind::Choice { options, .. } => {
                let label = options
                    .iter()
                    .find(|(v, _)| state.value == json!(v))
                    .map_or_else(|| field_text(&state.value), |(_, l)| l.to_string());
                div()
                    .flex()
                    .flex_col()
                    .gap(t.ui(6.))
                    .child(description(div()))
                    .child(
                        div()
                            .id(("setting-choice", i))
                            .w(t.ui(240.))
                            .h(t.ui(26.))
                            .px(t.ui(8.))
                            .flex()
                            .items_center()
                            .justify_between()
                            .bg(t.color.surface)
                            .border_1()
                            .border_color(t.color.border_strong)
                            .rounded(t.shape.radius_control)
                            .text_size(t.typography.caption)
                            .text_color(t.color.content)
                            .when(editable, |el| {
                                el.cursor_pointer()
                                    .hover(|s| s.bg(t.color.surface_hover))
                                    .on_click(cx.listener(
                                        move |this, e: &ClickEvent, window, cx| {
                                            this.open_choice(i, e.position(), window, cx)
                                        },
                                    ))
                            })
                            .child(label)
                            .child(div().text_color(t.color.content_muted).child("▾")),
                    )
                    .into_any_element()
            }
            Kind::Whole { .. } | Kind::Number { .. } => {
                let error = self.errors.get(&i).cloned();
                div()
                    .flex()
                    .flex_col()
                    .gap(t.ui(6.))
                    .child(description(div()))
                    .children(self.fields.get(&i).map(|field| {
                        div()
                            .w(t.ui(120.))
                            .h(t.ui(26.))
                            .px(t.ui(8.))
                            .flex()
                            .items_center()
                            .bg(t.color.surface)
                            .border_1()
                            .border_color(if error.is_some() {
                                t.color.danger
                            } else {
                                t.color.border_strong
                            })
                            .rounded(t.shape.radius_control)
                            .text_size(t.typography.caption)
                            .child(div().flex_1().min_w_0().child(field.clone()))
                    }))
                    .children(error.map(|why| {
                        div()
                            .text_size(t.typography.caption)
                            .text_color(t.color.danger)
                            .child(why)
                    }))
                    .into_any_element()
            }
            Kind::Json { .. } => {
                let scope = self.scope;
                let file = match scope {
                    Scope::User => "settings.json",
                    Scope::Project => ".athena/settings.json",
                };
                div()
                    .flex()
                    .flex_col()
                    .gap(t.ui(6.))
                    .child(description(div()))
                    .child(
                        div()
                            .id(("setting-json", i))
                            .text_size(t.typography.caption)
                            .text_color(t.color.accent)
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(SettingsEvent::OpenJson(scope))
                            }))
                            .child(format!("Edit in {file}")),
                    )
                    .into_any_element()
            }
        };
        div()
            .id(("setting", i))
            .w_full()
            .max_w(t.ui(880.))
            .flex()
            .gap(t.ui(12.))
            .py(t.ui(10.))
            .child(
                div()
                    .flex_none()
                    .w(px(2.))
                    .rounded(px(1.))
                    .bg(if state.modified {
                        t.color.accent
                    } else {
                        gpui::transparent_black()
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(t.ui(6.))
                    .child(title)
                    .child(control)
                    .child(
                        div()
                            .font_family(t.typography.mono.clone())
                            .text_size(t.ui(11.))
                            .text_color(t.color.content_muted)
                            .child(s.id()),
                    ),
            )
            .into_any_element()
    }
}

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let query = self.search.read(cx).text().to_string();
        let shown: Vec<usize> = SETTINGS
            .iter()
            .enumerate()
            .filter(|(_, s)| !(self.scope == Scope::Project && s.user_only))
            .filter(|(_, s)| matches(s, &query))
            .map(|(i, _)| i)
            .collect();
        let unreadable = self.read(self.scope).as_ref().err().cloned();
        let editable = unreadable.is_none();
        let mut body: Vec<AnyElement> = Vec::new();
        let mut group = None;
        for &i in &shown {
            let g = SETTINGS[i].group;
            if group != Some(g) {
                group = Some(g);
                body.push(
                    div()
                        .pt(t.ui(16.))
                        .pb(t.ui(2.))
                        .text_size(t.typography.body)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(t.color.content)
                        .child(g.title())
                        .into_any_element(),
                );
            }
            body.push(self.render_row(i, editable, cx));
        }
        if shown.is_empty() {
            body.push(
                div()
                    .pt(t.ui(48.))
                    .flex()
                    .justify_center()
                    .child(athena_ui::empty_state(
                        "No settings found",
                        "Try other words, or a setting's name such as editor.tab_size.",
                        None,
                        cx,
                    ))
                    .into_any_element(),
            );
        }
        let note = (self.scope == Scope::Project).then(|| {
            div()
                .pt(t.ui(12.))
                .text_size(t.typography.caption)
                .text_color(t.color.content_muted)
                .child(
                    "These apply to this project's files, over your own, and are saved in \
                     .athena/settings.json. Settings that apply to all of Athena are under User.",
                )
        });
        div()
            .key_context("Settings")
            .size_full()
            .flex()
            .flex_col()
            .bg(t.color.surface)
            .font_family(t.typography.ui.clone())
            .child(self.render_header(shown.len(), cx))
            .children(unreadable.map(|why| self.render_unreadable(&why, cx)))
            .child(
                div()
                    .id("settings-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .px(t.ui(24.))
                    .pb(t.ui(24.))
                    .children(note)
                    .children(body),
            )
            .children(self.menu.as_ref().map(|(menu, _)| menu.clone()))
    }
}

/// A project's `.athena/settings.json`; a missing file sets nothing.
fn read_project(root: &Path) -> Read {
    match std::fs::read_to_string(root.join(settings::PROJECT_FILE)) {
        Ok(text) => settings::parse_project(&text).map(|(s, _)| s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::schema::find;

    fn setting(id: &str) -> &'static Setting {
        let keys: Vec<&str> = id.split('.').collect();
        find(&keys).unwrap()
    }

    #[test]
    fn a_row_shows_the_project_over_the_user_over_workspace_json_over_the_default() {
        let wrap = setting("editor.word_wrap");
        let (user, _) = settings::parse(r#"{"editor.word_wrap": true}"#).unwrap();
        let (project, _) = settings::parse_project(r#"{"editor": {"word_wrap": false}}"#).unwrap();
        let none = Settings::default();
        let chosen = json!(true);
        let cases = [
            (Scope::User, &none, &none, None, json!(false), false, false),
            (
                Scope::User,
                &none,
                &none,
                Some(&chosen),
                json!(true),
                false,
                false,
            ),
            (Scope::User, &user, &none, None, json!(true), true, false),
            (Scope::User, &user, &project, None, json!(true), true, true),
            (Scope::Project, &user, &none, None, json!(true), false, true),
            (
                Scope::Project,
                &user,
                &project,
                None,
                json!(false),
                true,
                true,
            ),
            (
                Scope::Project,
                &none,
                &project,
                Some(&chosen),
                json!(false),
                true,
                false,
            ),
        ];
        for (scope, u, p, fallback, value, modified, elsewhere) in cases {
            assert_eq!(
                row_state(wrap, scope, Some(u), Some(p), fallback),
                RowState {
                    value,
                    modified,
                    elsewhere
                },
                "{scope:?}"
            );
        }
    }

    #[test]
    fn every_control_writes_through_the_settings_writer_and_reset_takes_it_back() {
        let template = crate::settings::TEMPLATE;
        for s in SETTINGS {
            let value = match s.kind {
                Kind::Toggle { default } => json!(!default),
                Kind::Whole { min, .. } => json!(min + 1),
                Kind::Number { max, .. } => json!(max - 0.5),
                Kind::Choice { options, default } => {
                    json!(options.iter().find(|(v, _)| *v != default).unwrap().0)
                }
                Kind::Json { example, .. } => serde_json::from_str(example).unwrap(),
            };
            let written = settings::set_value(template, s.keys, &value).unwrap();
            let (parsed, problems) = settings::parse(&written).unwrap();
            assert!(problems.is_empty(), "{}: {problems:?}", s.id());
            let state = row_state(s, Scope::User, Some(&parsed), None, None);
            assert!(state.modified, "{}", s.id());
            if !matches!(s.kind, Kind::Json { .. }) {
                assert_eq!(state.value, value, "{}", s.id());
            }
            let reset = settings::unset_value(&written, s.keys).unwrap();
            assert_eq!(reset, template, "{} reset", s.id());
        }
    }

    #[test]
    fn search_matches_every_word_in_titles_names_and_descriptions() {
        let tab = setting("editor.tab_size");
        assert!(matches(tab, "tab size"));
        assert!(matches(tab, "EDITOR.TAB"));
        assert!(matches(tab, "tabSize"));
        assert!(matches(tab, "indentation"));
        assert!(!matches(tab, "tab theme"));
        assert!(matches(setting("theme"), "workbench dark"));
    }

    #[test]
    fn number_fields_show_whole_numbers_without_a_fraction() {
        assert_eq!(field_text(&json!(13)), "13");
        assert_eq!(field_text(&json!(13.5)), "13.5");
        assert_eq!(
            field_text(&setting("editor.font_size").default_value()),
            "13"
        );
    }
}
