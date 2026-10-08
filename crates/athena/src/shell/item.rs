use athena_editor::EditorView;
use athena_preview::PreviewView;
use athena_term::{ClaudeState, TerminalView};
use gpui::{AnyElement, App, Entity, FocusHandle, Focusable, IntoElement};

/// The live view behind a tab.
#[derive(Clone)]
pub(super) enum ItemView {
    Terminal(Entity<TerminalView>),
    Editor(Entity<EditorView>),
    Preview(Entity<PreviewView>),
}

impl ItemView {
    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self {
            Self::Terminal(v) => v.focus_handle(cx),
            Self::Editor(v) => v.focus_handle(cx),
            Self::Preview(v) => v.focus_handle(cx),
        }
    }

    pub fn element(&self) -> AnyElement {
        match self {
            Self::Terminal(v) => v.clone().into_any_element(),
            Self::Editor(v) => v.clone().into_any_element(),
            Self::Preview(v) => v.clone().into_any_element(),
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
            Self::Editor(v) => v
                .read(cx)
                .path()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Untitled".into()),
            Self::Preview(v) => v.read(cx).label(),
        }
    }

    pub fn claude_state(&self, cx: &App) -> Option<ClaudeState> {
        match self {
            Self::Terminal(v) => v.read(cx).claude_state(),
            Self::Editor(_) | Self::Preview(_) => None,
        }
    }

    pub fn has_bell(&self, cx: &App) -> bool {
        matches!(self, Self::Terminal(v) if v.read(cx).has_bell())
    }

    pub fn is_dirty(&self, cx: &App) -> bool {
        matches!(self, Self::Editor(v) if v.read(cx).is_dirty())
    }
}
