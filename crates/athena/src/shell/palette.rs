use std::path::{Path, PathBuf};

use athena_ui::motion::{self, Closing};
use athena_ui::{ActiveTheme, InputEvent, TextInput};
use gpui::{
    Action, Animation, AnyElement, ClickEvent, Context, Entity, Focusable, FontWeight,
    HighlightStyle, MouseButton, SharedString, StyledText, Subscription, Window, div, prelude::*,
    px,
};

use super::Shell;
use super::fuzzy;
use crate::actions;

const MAX_FILES: usize = 20_000;
const MAX_ROWS: usize = 50;

pub(super) enum Target {
    File(PathBuf),
    Command(Box<dyn Action>),
    /// A command that starts Claude Code in this project.
    Claude(String),
}

impl Clone for Target {
    fn clone(&self) -> Self {
        match self {
            Self::File(path) => Self::File(path.clone()),
            Self::Command(action) => Self::Command(action.boxed_clone()),
            Self::Claude(command) => Self::Claude(command.clone()),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Files,
    /// Files that open in a new pane beside the focused one.
    FilesBeside,
    Commands,
    Claude,
}

/// Commands offered for starting Claude; typing anything else offers that too.
const CLAUDE_COMMANDS: &[&str] = &["claude", "claude-tlsc", "claude-ai"];

pub(super) struct Entry {
    label: String,
    detail: Option<String>,
    /// What the query is matched against (a project-relative path for files).
    key: String,
    target: Target,
}

pub(super) struct Palette {
    mode: Mode,
    input: Entity<TextInput>,
    placeholder_hint: &'static str,
    entries: Vec<Entry>,
    hits: Vec<(usize, Vec<usize>)>,
    selected: usize,
    _subscription: Subscription,
}

/// Commands offered in the palette, in the order shown before any typing.
fn commands() -> Vec<(&'static str, Box<dyn Action>)> {
    vec![
        ("New terminal", Box::new(actions::NewTerminal)),
        ("Split right", Box::new(actions::SplitRight)),
        ("Split down", Box::new(actions::SplitDown)),
        ("Close tab", Box::new(actions::CloseTab)),
        ("Save as…", Box::new(actions::SaveAs)),
        ("Toggle auto save", Box::new(actions::ToggleAutoSave)),
        ("Zoom pane", Box::new(actions::TogglePaneZoom)),
        ("Next tab", Box::new(actions::NextTab)),
        ("Previous tab", Box::new(actions::PrevTab)),
        ("Go to file", Box::new(actions::QuickOpen)),
        ("Source control changes", Box::new(actions::ShowChanges)),
        ("Toggle inline blame", Box::new(actions::ToggleBlame)),
        ("Open file to the side", Box::new(actions::QuickOpenBeside)),
        ("New Claude session", Box::new(actions::NewClaudeSession)),
        (
            "Change the command that starts Claude",
            Box::new(actions::ChangeClaudeCommand),
        ),
        (
            "Enable Claude Code hooks for this project",
            Box::new(actions::EnableClaudeHooks),
        ),
        (
            "Disable Claude Code hooks for this project",
            Box::new(actions::DisableClaudeHooks),
        ),
        ("Toggle file tree", Box::new(actions::ToggleFileTree)),
        ("Notifications", Box::new(actions::ToggleNotifications)),
        ("Containers", Box::new(actions::ShowContainers)),
        ("Playwright", Box::new(actions::ShowPlaywright)),
        ("Run Playwright tests", Box::new(actions::RunPlaywright)),
        ("New browser preview", Box::new(actions::NewPreview)),
        ("Open Markdown preview", Box::new(actions::TogglePreview)),
        (
            "Enable Playwright MCP for Claude in this project",
            Box::new(actions::EnablePlaywrightMcp),
        ),
        (
            "Disable Playwright MCP for Claude in this project",
            Box::new(actions::DisablePlaywrightMcp),
        ),
        ("Open project", Box::new(actions::AddProject)),
        ("Close project", Box::new(actions::CloseProject)),
        ("Next project", Box::new(actions::NextProject)),
        ("Previous project", Box::new(actions::PrevProject)),
        ("Toggle full screen", Box::new(actions::ToggleFullScreen)),
    ]
}

/// Project files, honouring .gitignore and skipping hidden files.
fn project_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(root)
        .hidden(true)
        .build()
        .flatten()
    {
        if entry.file_type().is_some_and(|t| t.is_file())
            && let Ok(rel) = entry.path().strip_prefix(root)
        {
            out.push(rel.to_string_lossy().into_owned());
            if out.len() >= MAX_FILES {
                break;
            }
        }
    }
    out.sort();
    out
}

impl Shell {
    pub(super) fn open_palette(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        let placeholder = match mode {
            Mode::Files => "Go to file…",
            Mode::FilesBeside => "Open to the side…",
            Mode::Commands => "Run a command…",
            Mode::Claude => "Command that starts Claude Code here…",
        };
        let input = cx.new(|cx| TextInput::new(placeholder, cx));
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Changed => this.filter_palette(cx),
                InputEvent::Up => this.move_palette(-1, cx),
                InputEvent::Down => this.move_palette(1, cx),
                InputEvent::Submit => this.run_palette(None, false, window, cx),
                InputEvent::SubmitBeside => this.run_palette(None, true, window, cx),
                InputEvent::Cancel => this.close_palette(window, cx),
            },
        );
        let entries = match mode {
            Mode::Files | Mode::FilesBeside => Vec::new(),
            Mode::Claude => claude_entries(""),
            Mode::Commands => {
                let root = self.workspace.active_project().map(|p| p.root.as_path());
                let hooks_on = root.is_some_and(crate::claude_hooks::enabled);
                let mcp_on = root.is_some_and(athena_playwright::mcp_enabled);
                let playwright = root.is_some_and(|r| athena_playwright::find_config(r).is_some());
                commands()
                    .into_iter()
                    .filter(|(label, _)| {
                        !(label.starts_with("Enable Claude Code hooks") && hooks_on
                            || label.starts_with("Disable Claude Code hooks") && !hooks_on
                            || label.starts_with("Enable Playwright MCP")
                                && (mcp_on || !playwright)
                            || label.starts_with("Disable Playwright MCP") && !mcp_on
                            || *label == "Run Playwright tests" && !playwright)
                    })
                    .map(|(label, action)| Entry {
                        detail: window
                            .highest_precedence_binding_for_action(action.as_ref())
                            .map(|b| keystrokes(&b)),
                        key: label.to_string(),
                        label: label.to_string(),
                        target: Target::Command(action),
                    })
                    .collect()
            }
        };
        let files = matches!(mode, Mode::Files | Mode::FilesBeside);
        window.focus(&input.focus_handle(cx));
        self.palette_closing = None;
        self.palette = Some(Palette {
            mode,
            input,
            placeholder_hint: match mode {
                Mode::Files | Mode::FilesBeside => "No matching files",
                _ => "No matching commands",
            },
            entries,
            hits: Vec::new(),
            selected: 0,
            _subscription: subscription,
        });
        self.filter_palette(cx);

        if files && let Some(root) = self.workspace.active_project().map(|p| p.root.clone()) {
            let walk = cx
                .background_executor()
                .spawn(async move { (project_files(&root), root) });
            cx.spawn(async move |this, cx| {
                let (files, root) = walk.await;
                let _ = this.update(cx, |this, cx| {
                    let Some(palette) = this.palette.as_mut() else {
                        return;
                    };
                    palette.entries = files
                        .into_iter()
                        .map(|rel| {
                            let path = root.join(&rel);
                            let (dir, name) = match rel.rfind('/') {
                                Some(i) => (Some(rel[..i].to_string()), rel[i + 1..].to_string()),
                                None => (None, rel.clone()),
                            };
                            Entry {
                                label: name,
                                detail: dir,
                                key: rel,
                                target: Target::File(path),
                            }
                        })
                        .collect();
                    this.filter_palette(cx);
                });
            })
            .detach();
        }
        cx.notify();
    }

    fn filter_palette(&mut self, cx: &mut Context<Self>) {
        let Some(palette) = self.palette.as_mut() else {
            return;
        };
        let query = palette.input.read(cx).text().to_string();
        if palette.mode == Mode::Claude {
            palette.entries = claude_entries(&query);
        }
        let mut hits: Vec<(i32, usize, Vec<usize>)> = palette
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| fuzzy::score(&query, &e.key).map(|(s, pos)| (s, i, pos)))
            .collect();
        if !query.is_empty() {
            hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        }
        palette.hits = hits
            .into_iter()
            .take(MAX_ROWS)
            .map(|(_, i, pos)| (i, pos))
            .collect();
        palette.selected = 0;
        cx.notify();
    }

    fn move_palette(&mut self, step: isize, cx: &mut Context<Self>) {
        if let Some(p) = self.palette.as_mut()
            && !p.hits.is_empty()
        {
            p.selected = (p.selected as isize + step).rem_euclid(p.hits.len() as isize) as usize;
            cx.notify();
        }
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(palette) = self.palette.take() {
            self.fade_out_palette(palette, cx);
        }
        self.focus_active_item(window, cx);
        cx.notify();
    }

    /// Keeps a dismissed palette drawn while it fades; focus has already moved on.
    fn fade_out_palette(&mut self, palette: Palette, cx: &mut Context<Self>) {
        let generation = self.next_generation();
        self.palette_closing = Some((palette, Closing::new(generation)));
        let t = cx.theme();
        let delay = motion::exit_delay(t.motion.reduced, t.motion.fast);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| {
                if this
                    .palette_closing
                    .as_ref()
                    .is_some_and(|(_, c)| c.generation == generation)
                {
                    this.palette_closing = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Runs the selected row, or `row`; `beside` opens a file in a new pane next to the focused one.
    fn run_palette(
        &mut self,
        row: Option<usize>,
        beside: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(palette) = self.palette.take() else {
            return;
        };
        let Some((index, _)) = palette.hits.get(row.unwrap_or(palette.selected)) else {
            self.palette = Some(palette);
            return;
        };
        let beside = beside || palette.mode == Mode::FilesBeside;
        let target = palette.entries.get(*index).map(|e| e.target.clone());
        self.fade_out_palette(palette, cx);
        self.focus_active_item(window, cx);
        match target {
            Some(Target::File(path)) if beside => self.open_file_beside(path, window, cx),
            Some(Target::File(path)) => self.open_file(path, window, cx),
            Some(Target::Command(action)) => window.dispatch_action(action, cx),
            Some(Target::Claude(command)) => self.start_claude_with(command, window, cx),
            None => {}
        }
        cx.notify();
    }

    pub(super) fn render_palette(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (palette, closing) = match (&self.palette, &self.palette_closing) {
            (Some(palette), _) => (palette, None),
            (None, Some((palette, closing))) => (palette, Some(*closing)),
            (None, None) => return None,
        };
        let t = cx.theme().clone();
        let rows: Vec<AnyElement> = palette
            .hits
            .iter()
            .enumerate()
            .map(|(row, (index, positions))| {
                let entry = &palette.entries[*index];
                let selected = row == palette.selected;
                // Positions index `key`; for files the label is the name at the end of it.
                let offset = entry.key.chars().count() - entry.label.chars().count();
                let label_bytes: Vec<usize> = entry.label.char_indices().map(|(b, _)| b).collect();
                let bold: Vec<_> = positions
                    .iter()
                    .filter(|&&p| p >= offset)
                    .filter_map(|&p| {
                        let b = *label_bytes.get(p - offset)?;
                        let len = entry.label[b..].chars().next()?.len_utf8();
                        Some((
                            b..b + len,
                            HighlightStyle {
                                font_weight: Some(FontWeight::SEMIBOLD),
                                ..Default::default()
                            },
                        ))
                    })
                    .collect();
                div()
                    .id(("palette-row", row))
                    .h(px(32.))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(12.))
                    .cursor_pointer()
                    .text_color(if selected {
                        t.color.accent
                    } else {
                        t.color.content
                    })
                    .when(!selected, |el| el.hover(|s| s.bg(t.color.surface_hover)))
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        this.run_palette(Some(row), event.modifiers().platform, window, cx)
                    }))
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap(px(8.))
                            .min_w_0()
                            .overflow_hidden()
                            .child(
                                StyledText::new(SharedString::from(entry.label.clone()))
                                    .with_highlights(bold),
                            )
                            .children(
                                matches!(entry.target, Target::File(_))
                                    .then(|| entry.detail.clone())
                                    .flatten()
                                    .map(|d| {
                                        div()
                                            .text_size(t.typography.caption)
                                            .text_color(t.color.content_muted)
                                            .child(d)
                                    }),
                            ),
                    )
                    .children(
                        matches!(entry.target, Target::Command(_))
                            .then(|| entry.detail.clone())
                            .flatten()
                            .map(|k| {
                                div()
                                    .font_family(t.typography.mono.clone())
                                    .text_size(t.typography.caption)
                                    .text_color(t.color.content_muted)
                                    .child(k)
                            }),
                    )
                    .into_any_element()
            })
            .collect();

        let empty = palette.hits.is_empty() && !palette.input.read(cx).text().is_empty();
        let panel = div()
            .id("palette")
            .w(px(560.))
            .max_h(px(44. + 32. * 8.5))
            .flex()
            .flex_col()
            .bg(t.color.surface)
            .border_1()
            .border_color(t.color.border)
            .rounded(t.shape.radius_panel)
            .shadow(vec![t.popover_shadow()])
            .overflow_hidden()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .h(px(44.))
                    .flex_none()
                    .px(px(14.))
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(t.color.border)
                    .child(palette.input.clone()),
            )
            .child(
                div()
                    .id("palette-rows")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .py(px(4.))
                    .children(rows)
                    .when(empty, |el| {
                        el.child(
                            div()
                                .h(px(32.))
                                .px(px(12.))
                                .flex()
                                .items_center()
                                .text_color(t.color.content_muted)
                                .child(palette.placeholder_hint),
                        )
                    }),
            );
        let panel = match closing {
            Some(closing) => motion::animate_exit(
                t.motion.reduced,
                panel,
                ("palette-exit", closing.generation),
                t.motion.fast,
                |el, d| el.opacity(1. - d).top(px(2. * d)),
            ),
            None => motion::animate_if(
                t.motion.reduced,
                panel,
                "palette-enter",
                Animation::new(t.motion.fast).with_easing(motion::ease_enter()),
                |el, d| el.opacity(d).top(px(2. * (1. - d))),
            ),
        };
        Some(
            div()
                .id("palette-layer")
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .items_start()
                .pt(px(96.))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| this.close_palette(window, cx)),
                )
                .child(panel)
                .into_any_element(),
        )
    }
}

/// `⌘⇧P`-style label for a binding.
fn keystrokes(binding: &gpui::KeyBinding) -> String {
    binding
        .keystrokes()
        .iter()
        .map(|k| {
            let m = k.modifiers();
            let mut s = String::new();
            if m.control {
                s.push('⌃');
            }
            if m.alt {
                s.push('⌥');
            }
            if m.shift {
                s.push('⇧');
            }
            if m.platform {
                s.push('⌘');
            }
            let key = match k.key() {
                "enter" => "↩".to_string(),
                "left" => "←".into(),
                "right" => "→".into(),
                "up" => "↑".into(),
                "down" => "↓".into(),
                other => other.to_uppercase(),
            };
            s + &key
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn claude_entries(query: &str) -> Vec<Entry> {
    let typed = query.trim();
    let mut names: Vec<String> = CLAUDE_COMMANDS.iter().map(|c| c.to_string()).collect();
    if !typed.is_empty() && !names.iter().any(|n| n == typed) {
        names.insert(0, typed.to_string());
    }
    names
        .into_iter()
        .map(|name| Entry {
            label: name.clone(),
            detail: None,
            key: name.clone(),
            target: Target::Claude(name),
        })
        .collect()
}
