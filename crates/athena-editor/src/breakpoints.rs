//! Breakpoints in the gutter's left margin, the line a paused program stopped on, and the
//! expression a debugger is asked about while the pointer rests on code.

use std::path::PathBuf;

use athena_ui::motion::{self, Opening};
use athena_ui::{ActiveTheme, InputEvent, MenuItem, TextInput};
use gpui::{
    Action, Animation, App, Context, Entity, Focusable, Hsla, IntoElement, KeyBinding, MouseButton,
    Pixels, Point, Subscription, Window, actions, div, prelude::*, px,
};

use crate::buffer::{Buffer, Edit};
use crate::element::GUTTER_PAD;
use crate::run_marks::RunTestAt;
use crate::view::EditorView;

actions!(editor, [ToggleBreakpoint]);

/// Adds or removes the breakpoint on a zero-based line of `path`.
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = editor, no_json)]
pub struct ToggleBreakpointAt {
    pub path: PathBuf,
    pub line: usize,
}

/// Sets the breakpoint on a zero-based line with its condition, hit count and log message.
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = editor, no_json)]
pub struct SetBreakpointAt {
    pub path: PathBuf,
    pub line: usize,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
    pub log_message: Option<String>,
}

#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = editor, no_json)]
pub struct EnableBreakpointAt {
    pub path: PathBuf,
    pub line: usize,
    pub enabled: bool,
}

/// Debugs the test whose run mark is on a zero-based line of `path`.
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = editor, no_json)]
pub struct DebugTestAt {
    pub path: PathBuf,
    pub line: usize,
}

pub(crate) fn init(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("f9", ToggleBreakpoint, Some("Editor"))]);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Breakpoint {
    /// Zero-based.
    pub line: usize,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
    /// Set for a logpoint, which prints instead of stopping.
    pub log_message: Option<String>,
    pub enabled: bool,
    /// The debugger could not place it, so it is drawn hollow.
    pub unverified: bool,
}

impl Breakpoint {
    pub fn at(line: usize) -> Self {
        Self {
            line,
            condition: None,
            hit_condition: None,
            log_message: None,
            enabled: true,
            unverified: false,
        }
    }

    fn conditional(&self) -> bool {
        self.condition.is_some() || self.hit_condition.is_some()
    }
}

/// How a breakpoint is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BreakpointLook {
    Plain,
    Conditional,
    Log,
    Disabled,
    Unverified,
}

/// Which field of a breakpoint the inline box edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Condition,
    HitCount,
    Log,
}

impl Field {
    const ALL: [Self; 3] = [Self::Condition, Self::HitCount, Self::Log];

    fn label(self) -> &'static str {
        match self {
            Self::Condition => "Expression",
            Self::HitCount => "Hit Count",
            Self::Log => "Log Message",
        }
    }

    fn placeholder(self) -> &'static str {
        match self {
            Self::Condition => "Break when the expression is true",
            Self::HitCount => "Break when the hit count is met, e.g. > 5",
            Self::Log => "Message to log; {expression} is evaluated",
        }
    }

    fn of(self, b: &Breakpoint) -> Option<&String> {
        match self {
            Self::Condition => b.condition.as_ref(),
            Self::HitCount => b.hit_condition.as_ref(),
            Self::Log => b.log_message.as_ref(),
        }
    }
}

/// The box under a line where a breakpoint's condition or log message is typed.
struct BreakpointBox {
    line: usize,
    field: Field,
    input: Entity<TextInput>,
    opened: Opening,
    _subscriptions: [Subscription; 2],
}

/// The debugger's marks on a view.
#[derive(Default)]
pub(crate) struct DebugMarks {
    breakpoints: Vec<Breakpoint>,
    /// The line a paused program is on, and whether it is the top frame rather than a caller.
    stopped: Option<(usize, bool)>,
    /// The margin line the pointer is over, which shows a faint dot where a click would add one.
    ghost: Option<usize>,
    editing: Option<BreakpointBox>,
    /// Hovering asks the debugger, not the language server, while the program is paused.
    paused: bool,
}

/// Moves breakpoints with an edit, as VS Code's decorations move: lines removed take their
/// breakpoints to where they joined, and a line pushed down from its start takes its own along.
pub(crate) fn follow_edit(marks: &mut DebugMarks, e: &Edit, at_line_start: bool) {
    if marks.breakpoints.is_empty() {
        return;
    }
    let delta = e.lines_inserted as isize - e.lines_removed as isize;
    let last = e.line + e.lines_removed;
    for b in &mut marks.breakpoints {
        if b.line == e.line && at_line_start && e.lines_removed == 0 {
            b.line += e.lines_inserted;
        } else if b.line > last {
            b.line = (b.line as isize + delta) as usize;
        } else if b.line > e.line {
            b.line = e.line;
        }
    }
    marks.breakpoints.sort_by_key(|b| b.line);
    marks.breakpoints.dedup_by_key(|b| b.line);
}

/// The identifier at char `at` with the selectors before it; none for a number or blank.
fn expression_around(b: &Buffer, at: usize) -> Option<String> {
    let word = b.word_at(at)?;
    let rope = b.rope();
    let mut start = word.start;
    while start > 1 && rope.char(start - 1) == '.' {
        let before = b.word_start(start - 1);
        if before == start - 1 {
            break;
        }
        start = before;
    }
    let text: String = rope.slice(start..word.end).chars().collect();
    text.starts_with(|c: char| !c.is_ascii_digit())
        .then_some(text)
}

impl EditorView {
    /// The breakpoints to draw; the shell owns them and hands every view of a file the same list.
    pub fn set_breakpoints(&mut self, mut breakpoints: Vec<Breakpoint>, cx: &mut Context<Self>) {
        breakpoints.sort_by_key(|b| b.line);
        if self.debug.breakpoints != breakpoints {
            self.debug.breakpoints = breakpoints;
            cx.notify();
        }
    }

    /// This view's breakpoints, moved by the edits made since they were set.
    pub fn breakpoints(&self) -> &[Breakpoint] {
        &self.debug.breakpoints
    }

    /// Marks the zero-based line a paused program is on; `top` is false for a caller's frame.
    pub fn set_stopped_line(&mut self, stopped: Option<(usize, bool)>, cx: &mut Context<Self>) {
        if self.debug.stopped != stopped {
            self.debug.stopped = stopped;
            cx.notify();
        }
    }

    /// While paused, hovering asks for [`Self::expression_at`] instead of documentation.
    pub fn set_debug_paused(&mut self, paused: bool) {
        self.debug.paused = paused;
    }

    pub fn debug_paused(&self) -> bool {
        self.debug.paused
    }

    /// The expression a debugger should evaluate for a hover at a zero-based line and UTF-16
    /// column: the identifier there with the selectors before it, such as `cfg.Server.Port`.
    pub fn expression_at(&self, line: u32, character: u32) -> Option<String> {
        let b = self.buf()?;
        expression_around(&b, b.char_at_utf16(line, character))
    }

    pub(crate) fn breakpoint_at(&self, line: usize) -> Option<&Breakpoint> {
        self.debug.breakpoints.iter().find(|b| b.line == line)
    }

    pub(crate) fn breakpoint_look(&self, line: usize) -> Option<BreakpointLook> {
        let Some(b) = self.breakpoint_at(line) else {
            return (self.debug.ghost == Some(line)).then_some(BreakpointLook::Unverified);
        };
        Some(match b {
            b if !b.enabled => BreakpointLook::Disabled,
            b if b.unverified => BreakpointLook::Unverified,
            b if b.log_message.is_some() => BreakpointLook::Log,
            b if b.conditional() => BreakpointLook::Conditional,
            _ => BreakpointLook::Plain,
        })
    }

    /// The pointer's faint dot, drawn only where nothing else claims the margin.
    pub(crate) fn breakpoint_ghost(&self, line: usize) -> bool {
        self.debug.ghost == Some(line) && self.breakpoint_at(line).is_none()
    }

    /// The tint of a line a paused program is on: amber where it stopped, green for a caller.
    pub(crate) fn stopped_band(&self, line: usize, theme: &athena_ui::Theme) -> Option<Hsla> {
        let (at, top) = self.debug.stopped?;
        (at == line).then(|| match top {
            true => theme.color.warning.opacity(0.18),
            false => theme.color.success.opacity(0.14),
        })
    }

    pub(crate) fn stopped_arrow(&self, line: usize) -> Option<bool> {
        let (at, top) = self.debug.stopped?;
        (at == line).then_some(top)
    }

    /// The buffer line whose left margin is under `position`, if a line starts on that row.
    fn margin_line(&self, position: Point<Pixels>) -> Option<usize> {
        let layout = self.layout.as_ref()?;
        if position.x < layout.origin.x || position.x >= layout.origin.x + px(GUTTER_PAD) {
            return None;
        }
        self.gutter_line(position)
    }

    /// The buffer line whose gutter row, numbers included, is under `position`.
    fn gutter_line(&self, position: Point<Pixels>) -> Option<usize> {
        let layout = self.layout.as_ref()?;
        if position.x < layout.origin.x || position.x >= layout.fold_column.0 {
            return None;
        }
        if self.sticky_at(position).is_some() {
            return None;
        }
        let y = position.y - layout.origin.y + px(self.scroll.y);
        let row = (y / layout.line_height).floor().max(0.) as usize;
        layout.row(row).map(|r| r.line)
    }

    /// A click in the margin toggles a breakpoint, except on a test's run mark without one.
    pub(crate) fn click_breakpoint_margin(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(line) = self.margin_line(position) else {
            return false;
        };
        let marked = self.breakpoint_at(line).is_some();
        if self.lightbulb == Some(line) || (!marked && self.run_mark(line).is_some()) {
            return false;
        }
        self.debug.ghost = None;
        let action = ToggleBreakpointAt {
            path: self.path().to_path_buf(),
            line,
        };
        window.dispatch_action(Box::new(action), cx);
        true
    }

    pub(crate) fn hover_breakpoint_margin(
        &mut self,
        position: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        let ghost = position
            .and_then(|p| self.margin_line(p))
            .filter(|&l| self.run_mark(l).is_none() && self.lightbulb != Some(l));
        if ghost != self.debug.ghost {
            self.debug.ghost = ghost;
            cx.notify();
        }
    }

    /// The right-click menu for a line's gutter: its breakpoint, and its test when it has one.
    pub(crate) fn gutter_menu(
        &self,
        position: Point<Pixels>,
        cx: &Context<Self>,
    ) -> Option<Vec<MenuItem>> {
        let line = self.gutter_line(position)?;
        let path = self.path().to_path_buf();
        let send = |action: Box<dyn Action>| {
            move |window: &mut Window, cx: &mut App| {
                window.dispatch_action(action.boxed_clone(), cx)
            }
        };
        let mut items = Vec::new();
        if self.run_mark(line).is_some() {
            let at = |path: &PathBuf| RunTestAt {
                path: path.clone(),
                line,
            };
            items.push(MenuItem::new("Run Test", send(Box::new(at(&path)))));
            let debug = DebugTestAt {
                path: path.clone(),
                line,
            };
            items.push(MenuItem::new("Debug Test", send(Box::new(debug))));
            items.push(MenuItem::separator());
        }
        let toggle = ToggleBreakpointAt {
            path: path.clone(),
            line,
        };
        let entity = cx.entity().downgrade();
        let edit = move |field: Field| {
            let entity = entity.clone();
            move |window: &mut Window, cx: &mut App| {
                if let Some(view) = entity.upgrade() {
                    view.update(cx, |v, cx| v.edit_breakpoint(line, field, window, cx));
                }
            }
        };
        match self.breakpoint_at(line) {
            Some(b) => {
                items.push(MenuItem::new("Remove Breakpoint", send(Box::new(toggle))).hint("F9"));
                let (label, field) = match b.log_message.is_some() {
                    true => ("Edit Logpoint…", Field::Log),
                    false => ("Edit Condition…", Field::Condition),
                };
                items.push(MenuItem::new(label, edit(field)));
                let enable = EnableBreakpointAt {
                    path,
                    line,
                    enabled: !b.enabled,
                };
                let label = match b.enabled {
                    true => "Disable Breakpoint",
                    false => "Enable Breakpoint",
                };
                items.push(MenuItem::new(label, send(Box::new(enable))));
            }
            None => {
                items.push(MenuItem::new("Add Breakpoint", send(Box::new(toggle))).hint("F9"));
                items.push(MenuItem::new(
                    "Add Conditional Breakpoint…",
                    edit(Field::Condition),
                ));
                items.push(MenuItem::new("Add Logpoint…", edit(Field::Log)));
            }
        }
        Some(items)
    }

    fn toggle_breakpoint_at_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(line) = self.buf().map(|b| b.line_of(self.cursor.head())) else {
            return;
        };
        let action = ToggleBreakpointAt {
            path: self.path().to_path_buf(),
            line,
        };
        window.dispatch_action(Box::new(action), cx);
    }

    /// Opens the box under `line` for its breakpoint's condition, hit count or log message.
    fn edit_breakpoint(
        &mut self,
        line: usize,
        field: Field,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = self.breakpoint_at(line).cloned();
        let text = existing
            .as_ref()
            .and_then(|b| field.of(b).cloned())
            .unwrap_or_default();
        let input = cx.new(|cx| {
            let mut input = TextInput::new(field.placeholder(), cx);
            input.set_text(text, cx);
            input
        });
        let events =
            cx.subscribe_in(
                &input,
                window,
                move |this, input, event, window, cx| match event {
                    InputEvent::Submit | InputEvent::SubmitBeside => {
                        let text = input.read(cx).text().trim().to_string();
                        this.submit_breakpoint(text, window, cx);
                    }
                    InputEvent::Cancel => this.close_breakpoint_box(window, cx),
                    InputEvent::Up | InputEvent::Down => {
                        let step = if matches!(event, InputEvent::Down) {
                            1
                        } else {
                            2
                        };
                        this.switch_breakpoint_field(step, cx);
                    }
                    InputEvent::Changed => {}
                },
            );
        let blur = cx.on_blur(&input.focus_handle(cx), window, |this, _, cx| {
            if this.debug.editing.take().is_some() {
                cx.notify();
            }
        });
        window.focus(&input.focus_handle(cx));
        self.debug.editing = Some(BreakpointBox {
            line,
            field,
            input,
            opened: Opening::now(),
            _subscriptions: [events, blur],
        });
        cx.notify();
    }

    /// Moves the box to the next kind of setting, showing what that kind holds now.
    fn switch_breakpoint_field(&mut self, step: usize, cx: &mut Context<Self>) {
        let Some(editing) = self.debug.editing.as_mut() else {
            return;
        };
        let at = Field::ALL
            .iter()
            .position(|f| *f == editing.field)
            .unwrap_or(0);
        editing.field = Field::ALL[(at + step) % Field::ALL.len()];
        let (field, line, input) = (editing.field, editing.line, editing.input.clone());
        let text = self
            .breakpoint_at(line)
            .and_then(|b| field.of(b).cloned())
            .unwrap_or_default();
        input.update(cx, |i, cx| i.set_text(text, cx));
        cx.notify();
    }

    fn submit_breakpoint(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = self.debug.editing.as_ref() else {
            return;
        };
        let (line, field) = (editing.line, editing.field);
        let mut b = self
            .breakpoint_at(line)
            .cloned()
            .unwrap_or_else(|| Breakpoint::at(line));
        let value = (!text.is_empty()).then_some(text);
        match field {
            Field::Condition => b.condition = value,
            Field::HitCount => b.hit_condition = value,
            Field::Log => b.log_message = value,
        }
        let action = SetBreakpointAt {
            path: self.path().to_path_buf(),
            line,
            condition: b.condition,
            hit_condition: b.hit_condition,
            log_message: b.log_message,
        };
        self.close_breakpoint_box(window, cx);
        window.dispatch_action(Box::new(action), cx);
    }

    fn close_breakpoint_box(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.debug.editing = None;
        window.focus(&self.focus);
        cx.notify();
    }

    pub(crate) fn on_debug_actions(
        el: gpui::Stateful<gpui::Div>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        el.on_action(cx.listener(|this, _: &ToggleBreakpoint, window, cx| {
            this.toggle_breakpoint_at_cursor(window, cx)
        }))
    }

    pub(crate) fn render_breakpoint_box(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let editing = self.debug.editing.as_ref()?;
        let layout = self.layout.as_ref()?;
        let origin = self.char_origin(self.buf()?.line_start(editing.line))?;
        let top = origin.y + layout.line_height - layout.origin.y;
        let left = layout.text_left - layout.origin.x;
        let t = cx.theme();
        let tabs = Field::ALL.map(|field| {
            let active = field == editing.field;
            div()
                .id(field.label())
                .px(px(6.))
                .rounded(t.shape.radius_control)
                .cursor_pointer()
                .text_color(if active {
                    t.color.content
                } else {
                    t.color.content_muted
                })
                .when(active, |el| el.bg(t.color.surface_active))
                .when(!active, |el| {
                    el.hover(|s| s.text_color(t.color.content_secondary))
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        let Some(editing) = this.debug.editing.as_ref() else {
                            return;
                        };
                        let at = Field::ALL
                            .iter()
                            .position(|f| *f == editing.field)
                            .unwrap_or(0);
                        let to = Field::ALL.iter().position(|f| *f == field).unwrap_or(0);
                        this.switch_breakpoint_field((to + 3 - at) % 3, cx);
                        let focus = this
                            .debug
                            .editing
                            .as_ref()
                            .map(|e| e.input.focus_handle(cx));
                        if let Some(focus) = focus {
                            window.focus(&focus);
                        }
                    }),
                )
                .child(field.label())
        });
        let panel = div()
            .w(px(460.))
            .p(px(6.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .bg(t.color.surface)
            .border_1()
            .border_color(t.color.border)
            .rounded(t.shape.radius_panel)
            .shadow(vec![t.popover_shadow()])
            .text_size(t.typography.caption)
            .child(div().flex().gap(px(2.)).children(tabs))
            .child(
                div()
                    .h(px(24.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .bg(t.color.surface_sunken)
                    .border_1()
                    .border_color(t.color.danger)
                    .rounded(t.shape.radius_control)
                    .child(editing.input.clone()),
            )
            .child(
                div()
                    .px(px(2.))
                    .text_color(t.color.content_muted)
                    .child("Enter to accept, Escape to cancel, ↑↓ for the other settings"),
            );
        let panel = motion::animate_enter(
            t.motion.reduced,
            editing.opened.running(t.motion.fast),
            panel,
            "breakpoint-box-open",
            Animation::new(t.motion.fast).with_easing(motion::ease_enter()),
            |el, d| el.opacity(d).mt(px(-4. * (1. - d))),
        );
        Some(div().absolute().top(top).left(left).child(panel))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marks(lines: &[usize]) -> DebugMarks {
        DebugMarks {
            breakpoints: lines.iter().map(|&l| Breakpoint::at(l)).collect(),
            ..Default::default()
        }
    }

    fn lines(m: &DebugMarks) -> Vec<usize> {
        m.breakpoints.iter().map(|b| b.line).collect()
    }

    fn edit(line: usize, removed: usize, inserted: usize) -> Edit {
        Edit {
            at: 0,
            removed: 0,
            inserted: 0,
            line,
            lines_removed: removed,
            lines_inserted: inserted,
        }
    }

    #[test]
    fn breakpoints_below_an_edit_move_with_the_lines_added_or_removed() {
        let mut m = marks(&[2, 5, 9]);
        follow_edit(&mut m, &edit(4, 0, 2), false);
        assert_eq!(lines(&m), [2, 7, 11]);
        follow_edit(&mut m, &edit(0, 1, 0), false);
        assert_eq!(lines(&m), [1, 6, 10]);
    }

    #[test]
    fn a_line_pushed_down_from_its_start_takes_its_breakpoint_but_not_from_its_end() {
        let mut m = marks(&[3]);
        follow_edit(&mut m, &edit(3, 0, 1), false);
        assert_eq!(lines(&m), [3], "Enter at the end of the line");
        follow_edit(&mut m, &edit(3, 0, 1), true);
        assert_eq!(lines(&m), [4], "Enter at the start of the line");
    }

    #[test]
    fn a_hover_asks_about_the_identifier_with_the_selectors_before_it() {
        let b = Buffer::new("x := cfg.Server.Port + 12\n", Some("a.go".into()));
        let at = |needle: &str| b.full_text().find(needle).unwrap();
        assert_eq!(
            expression_around(&b, at("Port")).as_deref(),
            Some("cfg.Server.Port")
        );
        assert_eq!(
            expression_around(&b, at("Server")).as_deref(),
            Some("cfg.Server")
        );
        assert_eq!(expression_around(&b, at("x")).as_deref(), Some("x"));
        assert_eq!(expression_around(&b, at("12")), None);
        assert_eq!(expression_around(&b, at("+")), None);
    }

    #[test]
    fn deleted_lines_bring_their_breakpoints_to_the_joined_line_once() {
        let mut m = marks(&[3, 4, 5, 8]);
        follow_edit(&mut m, &edit(3, 2, 0), false);
        assert_eq!(lines(&m), [3, 6]);
    }
}
