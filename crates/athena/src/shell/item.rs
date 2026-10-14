use std::path::Path;

use athena_editor::{DiffView, EditorView, ImageView, LargeFileView};
use athena_preview::{DocView, PreviewView};
use athena_term::{ClaudeState, TerminalView};
use gpui::{AnyElement, AnyView, App, Entity, FocusHandle, Focusable, IntoElement, Render};
use gpui::{StyleRefinement, Styled};

use super::settings_ui::SettingsView;
use super::shortcuts_ui::ShortcutsView;

/// The live view behind a tab.
#[derive(Clone)]
pub(super) enum ItemView {
    Terminal(Entity<TerminalView>),
    Editor(Entity<EditorView>),
    Image(Entity<ImageView>),
    /// A file over the editor's size limit, shown read-only.
    Large(Entity<LargeFileView>),
    Preview(Entity<PreviewView>),
    Doc(Entity<DocView>),
    Diff(Entity<DiffView>),
    Settings(Entity<SettingsView>),
    Shortcuts(Entity<ShortcutsView>),
}

impl ItemView {
    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self {
            Self::Terminal(v) => v.focus_handle(cx),
            Self::Editor(v) => v.focus_handle(cx),
            Self::Image(v) => v.focus_handle(cx),
            Self::Large(v) => v.focus_handle(cx),
            Self::Preview(v) => v.focus_handle(cx),
            Self::Doc(v) => v.focus_handle(cx),
            Self::Diff(v) => v.focus_handle(cx),
            Self::Settings(v) => v.focus_handle(cx),
            Self::Shortcuts(v) => v.focus_handle(cx),
        }
    }

    /// `cached` reuses the view's last frame until it notifies; terminals always draw.
    pub fn element(&self, cached: bool) -> AnyElement {
        fn view<V: Render>(v: &Entity<V>, cached: bool) -> AnyElement {
            let view = AnyView::from(v.clone());
            match cached {
                true => view.cached(StyleRefinement::default().size_full()),
                false => view,
            }
            .into_any_element()
        }
        match self {
            Self::Terminal(v) => v.clone().into_any_element(),
            Self::Editor(v) => view(v, cached),
            Self::Image(v) => view(v, cached),
            Self::Large(v) => view(v, cached),
            Self::Preview(v) => view(v, cached),
            Self::Doc(v) => view(v, cached),
            Self::Diff(v) => view(v, cached),
            Self::Settings(v) => view(v, cached),
            Self::Shortcuts(v) => view(v, cached),
        }
    }

    /// Hangs up a terminal's shell; editors have nothing to release.
    pub fn close(&self, cx: &mut App) {
        if let Self::Terminal(v) = self {
            v.update(cx, |v, _| v.kill());
        }
    }

    pub fn label(&self, cx: &App) -> String {
        match self {
            Self::Terminal(v) => v.read(cx).label(),
            Self::Editor(v) => file_label(v.read(cx).path()),
            Self::Image(v) => file_label(v.read(cx).path()),
            Self::Large(v) => file_label(v.read(cx).path()),
            Self::Preview(v) => v.read(cx).label(),
            Self::Doc(v) => v.read(cx).label(),
            Self::Diff(v) => v.read(cx).label(),
            Self::Settings(v) => v.read(cx).label(),
            Self::Shortcuts(v) => v.read(cx).label(),
        }
    }

    pub fn claude_state(&self, cx: &App) -> Option<ClaudeState> {
        match self {
            Self::Terminal(v) => v.read(cx).claude_state(),
            Self::Editor(_)
            | Self::Image(_)
            | Self::Large(_)
            | Self::Preview(_)
            | Self::Doc(_)
            | Self::Diff(_)
            | Self::Settings(_)
            | Self::Shortcuts(_) => None,
        }
    }

    pub fn is_stale(&self, cx: &App) -> bool {
        matches!(self, Self::Terminal(v) if v.read(cx).is_stale())
    }

    /// Claude's state as the tab and rail show it: a finished session only until it is seen.
    pub fn shown_claude_state(&self, cx: &App) -> Option<ClaudeState> {
        match self.claude_state(cx) {
            Some(ClaudeState::Finished) if !self.has_bell(cx) => None,
            state => state,
        }
    }

    pub fn has_bell(&self, cx: &App) -> bool {
        matches!(self, Self::Terminal(v) if v.read(cx).has_bell())
    }

    pub fn is_dirty(&self, cx: &App) -> bool {
        matches!(self, Self::Editor(v) if v.read(cx).is_dirty())
    }
}

pub(super) fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Untitled".into())
}
