use athena_editor::{EditorView, Indent, Lang, LineEnding};
use athena_ui::{ActiveTheme, MenuItem, Tooltip};
use athena_workspace::git;
use gpui::{
    ClickEvent, Context, Entity, Focusable, IntoElement, SharedString, WeakEntity, Window, div,
    prelude::*, px,
};

use super::Shell;
use super::git_view::Remote;
use super::lsp::LspStatus;

const HEIGHT: f32 = 22.;

/// "Ln 12, Col 5", with the selection's size when there is one, or the caret count when there are
/// several, as VS Code words it.
fn position_label(line: usize, column: usize, selected: usize, carets: usize) -> String {
    match (carets, selected) {
        (0 | 1, 0) => format!("Ln {line}, Col {column}"),
        (0 | 1, n) => format!("Ln {line}, Col {column} ({n} selected)"),
        (carets, 0) => format!("{carets} selections"),
        (carets, n) => format!("{carets} selections ({n} characters selected)"),
    }
}

fn indent_label(indent: Indent) -> String {
    match indent {
        Indent::Spaces(n) => format!("Spaces: {n}"),
        Indent::Tab => format!("Tab Size: {}", indent.size()),
    }
}

fn eol_label(eol: LineEnding) -> &'static str {
    match eol {
        LineEnding::Lf => "LF",
        LineEnding::CrLf => "CRLF",
    }
}

fn lang_name(lang: Option<Lang>) -> &'static str {
    match lang {
        None => "Plain Text",
        Some(Lang::Go) => "Go",
        Some(Lang::TypeScript) => "TypeScript",
        Some(Lang::Tsx) => "TypeScript JSX",
        Some(Lang::JavaScript) => "JavaScript",
        Some(Lang::Yaml) => "YAML",
        Some(Lang::Json) => "JSON",
        Some(Lang::Toml) => "TOML",
        Some(Lang::Shell) => "Shell Script",
        Some(Lang::Rust) => "Rust",
        Some(Lang::Python) => "Python",
        Some(Lang::Css) => "CSS",
        Some(Lang::Html) => "HTML",
        Some(Lang::Markdown) => "Markdown",
        Some(Lang::Swift) => "Swift",
        Some(Lang::Dockerfile) => "Dockerfile",
        Some(Lang::DotEnv) => "Environment Variables",
    }
}

fn lsp_tooltip(status: &LspStatus) -> String {
    match status {
        LspStatus::Starting(program) => format!("{program} is starting"),
        LspStatus::Ready(program) => format!("{program} is running"),
        LspStatus::Failed(program, error) => format!("{program} failed: {error}"),
    }
}

impl Shell {
    /// The bottom row: branch on the left; cursor, indentation, encoding, line breaks, language
    /// and language server of the focused editor on the right.
    pub(super) fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let root = self.workspace.active_project().map(|p| p.root.clone());
        let branch = root.as_deref().and_then(|r| self.cached_branch(r));
        let editor = self.focused_editor();
        let status = editor.as_ref().and_then(|e| e.read(cx).status());
        let lsp = status
            .as_ref()
            .and_then(|s| self.lsp_status(root.as_deref()?, s.lang?));

        let item = |id: &'static str, label: SharedString| {
            div()
                .id(id)
                .h_full()
                .px(px(8.))
                .flex()
                .items_center()
                .gap(px(6.))
                .whitespace_nowrap()
                .child(label)
        };
        let button = |id: &'static str, label: SharedString, tip: &'static str| {
            item(id, label)
                .cursor_pointer()
                .hover(|s| s.bg(t.color.surface_hover).text_color(t.color.content))
                .tooltip(move |_, cx| Tooltip::view(tip, cx))
        };

        let tracking = root.as_deref().and_then(|r| self.cached_tracking(r));
        let busy = self.git.remote_busy;
        let left = branch.map(|branch| {
            let sync = sync_label(tracking.as_ref(), busy);
            div()
                .flex()
                .h_full()
                .child(
                    button("status-branch", branch.into(), "Switch branch").on_click(
                        cx.listener(|this, _, window, cx| this.open_branches(window, cx)),
                    ),
                )
                .child(
                    button("status-sync", sync.into(), "Pull, push, fetch or stash").on_click(
                        cx.listener(move |this, event: &ClickEvent, window, cx| {
                            let shell = cx.entity().downgrade();
                            let items = sync_menu(tracking.is_some(), shell);
                            this.open_context_menu(event.position(), items, window, cx);
                        }),
                    ),
                )
        });
        let right = status.zip(editor).map(|(status, editor)| {
            let position =
                position_label(status.line, status.column, status.selected, status.carets);
            let indent = status.indent;
            let lang = status.lang;
            let lsp_dot = lsp.map(|lsp| {
                let color = match lsp {
                    LspStatus::Ready(_) => t.color.success,
                    LspStatus::Starting(_) => t.color.warning,
                    LspStatus::Failed(..) => t.color.danger,
                };
                let tip = lsp_tooltip(&lsp);
                div()
                    .id("status-lsp")
                    .h_full()
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .tooltip(move |_, cx| Tooltip::view(tip.clone(), cx))
                    .child(div().size(px(6.)).rounded_full().bg(color))
            });
            div()
                .flex()
                .h_full()
                .child(
                    button("status-position", position.into(), "Go to Line  ⌃G").on_click({
                        let editor = editor.clone();
                        move |_, window, cx| {
                            window.focus(&editor.focus_handle(cx));
                            if let Ok(action) = cx.build_action("editor::GoToLine", None) {
                                window.dispatch_action(action, cx);
                            }
                        }
                    }),
                )
                .child(
                    button(
                        "status-indent",
                        indent_label(indent).into(),
                        "Select Indentation",
                    )
                    .on_click({
                        let editor = editor.clone();
                        cx.listener(move |this, event: &ClickEvent, window, cx| {
                            let items = indent_menu(&editor, indent);
                            this.open_context_menu(event.position(), items, window, cx);
                        })
                    }),
                )
                .child(item("status-encoding", "UTF-8".into()))
                .child(
                    button(
                        "status-eol",
                        eol_label(status.line_ending).into(),
                        "Select End of Line Sequence",
                    )
                    .on_click({
                        let editor = editor.clone();
                        let current = status.line_ending;
                        cx.listener(move |this, event: &ClickEvent, window, cx| {
                            let items = eol_menu(&editor, current);
                            this.open_context_menu(event.position(), items, window, cx);
                        })
                    }),
                )
                .child(
                    button(
                        "status-lang",
                        lang_name(lang).into(),
                        "Select Language Mode",
                    )
                    .on_click(cx.listener(
                        move |this, event: &ClickEvent, window, cx| {
                            let items = language_menu(&editor, lang);
                            this.open_context_menu(event.position(), items, window, cx);
                        },
                    )),
                )
                .children(lsp_dot)
        });

        div()
            .id("status-bar")
            .flex_none()
            .h(px(HEIGHT))
            .flex()
            .items_center()
            .justify_between()
            .px(px(4.))
            .bg(t.color.surface)
            .border_t_1()
            .border_color(t.color.border)
            .text_size(t.typography.caption)
            .text_color(t.color.content_muted)
            .child(div().flex().h_full().children(left))
            .child(div().flex().h_full().children(right))
    }
}

/// The branch's sync state: the remote operation under way, "↓2 ↑1" against its upstream, or
/// "Publish" when it has none.
fn sync_label(tracking: Option<&git::Tracking>, busy: Option<&'static str>) -> String {
    match (busy, tracking) {
        (Some(busy), _) => busy.to_string(),
        (None, Some(t)) => format!("↓{} ↑{}", t.behind, t.ahead),
        (None, None) => "Publish".to_string(),
    }
}

fn sync_menu(has_upstream: bool, shell: WeakEntity<Shell>) -> Vec<MenuItem> {
    let item = |label: &'static str, run: fn(&mut Shell, &mut Window, &mut Context<Shell>)| {
        let shell = shell.clone();
        MenuItem::new(label, move |window, cx| {
            shell.update(cx, |this, cx| run(this, window, cx)).ok();
        })
    };
    let mut items = Vec::new();
    if has_upstream {
        items.push(item("Pull", |this, w, cx| {
            this.git_remote(Remote::Pull, false, w, cx)
        }));
        items.push(item("Push", |this, w, cx| {
            this.git_remote(Remote::Push, false, w, cx)
        }));
    } else {
        items.push(item("Publish Branch", |this, w, cx| {
            this.git_remote(Remote::Push, false, w, cx)
        }));
    }
    items.push(item("Fetch", |this, w, cx| {
        this.git_remote(Remote::Fetch, false, w, cx)
    }));
    items.push(MenuItem::separator());
    items.push(item("Stash Changes", |this, _, cx| {
        this.git_stash(false, cx)
    }));
    items.push(item("Stash Changes (Include Untracked)", |this, _, cx| {
        this.git_stash(true, cx)
    }));
    items.push(item("Pop Stash…", |this, w, cx| this.open_stashes(w, cx)));
    items
}

/// VS Code's indentation picker, as a menu at the status item.
fn indent_menu(editor: &Entity<EditorView>, current: Indent) -> Vec<MenuItem> {
    let set = |label: String, indent: Indent| {
        let editor = editor.downgrade();
        MenuItem::new(label, move |_, cx| {
            editor.update(cx, |e, cx| e.set_indent(indent, cx)).ok();
        })
        .disabled(indent == current)
    };
    let convert = |label: &'static str, indent: Indent| {
        let editor = editor.downgrade();
        MenuItem::new(label, move |_, cx| {
            editor
                .update(cx, |e, cx| e.convert_indentation(indent, cx))
                .ok();
        })
    };
    let spaces = match current {
        Indent::Spaces(n) => n,
        Indent::Tab => 4,
    };
    let mut items: Vec<MenuItem> = [2, 4, 8]
        .into_iter()
        .map(|n| set(format!("Indent Using Spaces: {n}"), Indent::Spaces(n)))
        .collect();
    items.push(set("Indent Using Tabs".into(), Indent::Tab));
    items.push(MenuItem::separator());
    items.push(convert(
        "Convert Indentation to Spaces",
        Indent::Spaces(spaces),
    ));
    items.push(convert("Convert Indentation to Tabs", Indent::Tab));
    items
}

/// VS Code's end-of-line picker; choosing converts every line break as one undo step.
fn eol_menu(editor: &Entity<EditorView>, current: LineEnding) -> Vec<MenuItem> {
    [LineEnding::Lf, LineEnding::CrLf]
        .into_iter()
        .map(|eol| {
            let editor = editor.downgrade();
            MenuItem::new(eol_label(eol), move |_, cx| {
                editor
                    .update(cx, |e, cx| e.convert_line_endings(eol, cx))
                    .ok();
            })
            .disabled(eol == current)
        })
        .collect()
}

fn language_menu(editor: &Entity<EditorView>, current: Option<Lang>) -> Vec<MenuItem> {
    let mut langs: Vec<Option<Lang>> = std::iter::once(None)
        .chain(Lang::ALL.into_iter().map(Some))
        .collect();
    langs.sort_by_key(|l| lang_name(*l));
    langs
        .into_iter()
        .map(|lang| {
            let editor = editor.downgrade();
            MenuItem::new(lang_name(lang), move |_, cx| {
                editor.update(cx, |e, cx| e.set_language(lang, cx)).ok();
            })
            .disabled(lang == current)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_shows_the_selection_size_only_when_there_is_one() {
        assert_eq!(position_label(12, 5, 0, 1), "Ln 12, Col 5");
        assert_eq!(position_label(1, 1, 42, 1), "Ln 1, Col 1 (42 selected)");
    }

    #[test]
    fn several_carets_show_their_count_instead_of_the_position() {
        assert_eq!(position_label(12, 5, 0, 3), "3 selections");
        assert_eq!(
            position_label(12, 5, 15, 3),
            "3 selections (15 characters selected)"
        );
    }

    #[test]
    fn indentation_and_line_breaks_read_as_in_vs_code() {
        assert_eq!(indent_label(Indent::Spaces(2)), "Spaces: 2");
        assert_eq!(indent_label(Indent::Tab), "Tab Size: 4");
        assert_eq!(eol_label(LineEnding::Lf), "LF");
        assert_eq!(eol_label(LineEnding::CrLf), "CRLF");
    }

    #[test]
    fn every_language_has_a_distinct_name() {
        let mut names: Vec<&str> = Lang::ALL.into_iter().map(|l| lang_name(Some(l))).collect();
        names.push(lang_name(None));
        let count = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), count);
    }

    #[test]
    fn the_sync_cell_shows_behind_then_ahead_or_offers_to_publish() {
        let t = git::Tracking {
            upstream: "origin/main".into(),
            ahead: 1,
            behind: 2,
        };
        assert_eq!(sync_label(Some(&t), None), "↓2 ↑1");
        assert_eq!(sync_label(None, None), "Publish");
        assert_eq!(sync_label(Some(&t), Some("Pulling…")), "Pulling…");
    }

    #[test]
    fn language_server_tooltips_name_the_program() {
        assert_eq!(
            lsp_tooltip(&LspStatus::Failed("gopls", "not found".into())),
            "gopls failed: not found"
        );
        assert_eq!(lsp_tooltip(&LspStatus::Ready("gopls")), "gopls is running");
    }
}
