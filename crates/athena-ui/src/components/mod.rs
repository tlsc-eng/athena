mod badge;
mod button;
mod empty_state;
mod icon_button;
mod list_row;
mod segmented_control;
mod tab;
mod tooltip;

pub use badge::{Badge, DotState, StatusDot};
pub use button::{Button, ButtonKind};
pub use empty_state::empty_state;
pub use icon_button::IconButton;
pub use list_row::ListRow;
pub use segmented_control::SegmentedControl;
pub use tab::{Tab, TabBar, TabVariant};
pub use tooltip::Tooltip;

type ClickHandler = Box<dyn Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static>;
