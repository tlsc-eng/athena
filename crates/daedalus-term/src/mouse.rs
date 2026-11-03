use alacritty_terminal::term::TermMode;

/// A mouse button as the xterm mouse protocol numbers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

impl Button {
    fn code(self) -> u32 {
        match self {
            Button::Left => 0,
            Button::Middle => 1,
            Button::Right => 2,
            Button::WheelUp => 64,
            Button::WheelDown => 65,
        }
    }

    fn is_wheel(self) -> bool {
        matches!(self, Button::WheelUp | Button::WheelDown)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseEvent {
    Press(Button),
    Release(Button),
    /// Movement to a new cell, with the button held if any.
    Move(Option<Button>),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
}

/// Highest 1-based coordinate the legacy encoding fits in one byte.
const LEGACY_MAX: usize = 255 - 32;
/// Highest 1-based coordinate UTF-8 mode (1005) fits in a two-byte character.
const UTF8_MAX: usize = 0x7ff - 32;

/// The report for `event` at a 0-based viewport cell, or `None` when the program did not ask for
/// this kind of event or the cell cannot be encoded.
pub fn encode(
    event: MouseEvent,
    col: usize,
    row: usize,
    mods: Mods,
    mode: TermMode,
) -> Option<Vec<u8>> {
    let wanted = match event {
        MouseEvent::Press(_) => mode.intersects(TermMode::MOUSE_MODE),
        MouseEvent::Release(button) => mode.intersects(TermMode::MOUSE_MODE) && !button.is_wheel(),
        MouseEvent::Move(Some(_)) => mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION),
        MouseEvent::Move(None) => mode.contains(TermMode::MOUSE_MOTION),
    };
    if !wanted {
        return None;
    }
    let sgr = mode.contains(TermMode::SGR_MOUSE);
    let mut code = match event {
        // Only SGR can say which button came up; the legacy encodings send 3 for any.
        MouseEvent::Release(_) if !sgr => 3,
        MouseEvent::Press(button)
        | MouseEvent::Release(button)
        | MouseEvent::Move(Some(button)) => button.code(),
        MouseEvent::Move(None) => 3,
    };
    if matches!(event, MouseEvent::Move(_)) {
        code += 32;
    }
    code += 4 * mods.shift as u32 + 8 * mods.alt as u32 + 16 * mods.control as u32;
    let (x, y) = (col + 1, row + 1);

    if sgr {
        let end = if matches!(event, MouseEvent::Release(_)) {
            'm'
        } else {
            'M'
        };
        return Some(format!("\x1b[<{code};{x};{y}{end}").into_bytes());
    }
    let mut out = b"\x1b[M".to_vec();
    out.push(32 + code as u8);
    for v in [x, y] {
        if mode.contains(TermMode::UTF8_MOUSE) {
            if v > UTF8_MAX {
                return None;
            }
            let c = char::from_u32(32 + v as u32)?;
            out.extend(c.encode_utf8(&mut [0; 4]).as_bytes());
        } else {
            if v > LEGACY_MAX {
                return None;
            }
            out.push(32 + v as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLICK: TermMode = TermMode::MOUSE_REPORT_CLICK;

    fn sgr(mode: TermMode) -> TermMode {
        mode | TermMode::SGR_MOUSE
    }

    fn report(event: MouseEvent, col: usize, row: usize, mods: Mods, mode: TermMode) -> String {
        let bytes = encode(event, col, row, mods, mode).expect("a report");
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn sgr_reports_buttons_with_one_based_cells() {
        let none = Mods::default();
        let mode = sgr(CLICK);
        assert_eq!(
            report(MouseEvent::Press(Button::Left), 0, 0, none, mode),
            "\x1b[<0;1;1M"
        );
        assert_eq!(
            report(MouseEvent::Press(Button::Middle), 4, 9, none, mode),
            "\x1b[<1;5;10M"
        );
        assert_eq!(
            report(MouseEvent::Press(Button::Right), 79, 23, none, mode),
            "\x1b[<2;80;24M"
        );
    }

    #[test]
    fn sgr_release_names_the_button_and_ends_in_lowercase_m() {
        let mode = sgr(CLICK);
        let none = Mods::default();
        assert_eq!(
            report(MouseEvent::Release(Button::Left), 2, 3, none, mode),
            "\x1b[<0;3;4m"
        );
        assert_eq!(
            report(MouseEvent::Release(Button::Right), 2, 3, none, mode),
            "\x1b[<2;3;4m"
        );
    }

    #[test]
    fn modifiers_add_their_bits() {
        let mode = sgr(CLICK);
        let press = MouseEvent::Press(Button::Left);
        let shift = Mods {
            shift: true,
            ..Mods::default()
        };
        let alt = Mods {
            alt: true,
            ..Mods::default()
        };
        let control = Mods {
            control: true,
            ..Mods::default()
        };
        let all = Mods {
            shift: true,
            alt: true,
            control: true,
        };
        assert_eq!(report(press, 0, 0, shift, mode), "\x1b[<4;1;1M");
        assert_eq!(report(press, 0, 0, alt, mode), "\x1b[<8;1;1M");
        assert_eq!(report(press, 0, 0, control, mode), "\x1b[<16;1;1M");
        assert_eq!(report(press, 0, 0, all, mode), "\x1b[<28;1;1M");
        assert_eq!(
            report(MouseEvent::Press(Button::WheelUp), 0, 0, control, mode),
            "\x1b[<80;1;1M"
        );
    }

    #[test]
    fn wheel_reports_64_and_65_and_never_a_release() {
        let mode = sgr(CLICK);
        let none = Mods::default();
        assert_eq!(
            report(MouseEvent::Press(Button::WheelUp), 1, 1, none, mode),
            "\x1b[<64;2;2M"
        );
        assert_eq!(
            report(MouseEvent::Press(Button::WheelDown), 1, 1, none, mode),
            "\x1b[<65;2;2M"
        );
        assert_eq!(
            encode(MouseEvent::Release(Button::WheelUp), 1, 1, none, mode),
            None
        );
        assert_eq!(
            encode(MouseEvent::Press(Button::WheelUp), 1, 1, none, CLICK),
            Some(vec![0x1b, b'[', b'M', 32 + 64, 34, 34])
        );
    }

    #[test]
    fn motion_adds_32_and_is_sent_only_in_the_modes_that_ask_for_it() {
        let none = Mods::default();
        let drag = MouseEvent::Move(Some(Button::Left));
        let hover = MouseEvent::Move(None);
        assert_eq!(
            encode(drag, 0, 0, none, sgr(CLICK)),
            None,
            "1000 reports clicks only"
        );
        assert_eq!(
            report(drag, 5, 0, none, sgr(TermMode::MOUSE_DRAG)),
            "\x1b[<32;6;1M"
        );
        assert_eq!(
            encode(hover, 5, 0, none, sgr(TermMode::MOUSE_DRAG)),
            None,
            "1002 needs a button"
        );
        assert_eq!(
            report(hover, 5, 0, none, sgr(TermMode::MOUSE_MOTION)),
            "\x1b[<35;6;1M"
        );
        assert_eq!(
            report(
                MouseEvent::Move(Some(Button::Right)),
                0,
                0,
                none,
                sgr(TermMode::MOUSE_MOTION)
            ),
            "\x1b[<34;1;1M"
        );
    }

    #[test]
    fn nothing_is_reported_without_a_mouse_mode() {
        let none = Mods::default();
        assert_eq!(
            encode(
                MouseEvent::Press(Button::Left),
                0,
                0,
                none,
                TermMode::SGR_MOUSE
            ),
            None
        );
        assert_eq!(
            encode(
                MouseEvent::Press(Button::WheelUp),
                0,
                0,
                none,
                TermMode::empty()
            ),
            None
        );
    }

    #[test]
    fn legacy_encoding_offsets_by_32_and_releases_as_button_3() {
        let none = Mods::default();
        assert_eq!(
            encode(MouseEvent::Press(Button::Left), 0, 0, none, CLICK),
            Some(b"\x1b[M !!".to_vec())
        );
        let alt = Mods {
            alt: true,
            ..Mods::default()
        };
        assert_eq!(
            encode(MouseEvent::Release(Button::Right), 9, 4, alt, CLICK),
            Some(vec![0x1b, b'[', b'M', 32 + 3 + 8, 32 + 10, 32 + 5])
        );
        assert_eq!(
            encode(
                MouseEvent::Move(Some(Button::Left)),
                0,
                0,
                none,
                TermMode::MOUSE_DRAG
            ),
            Some(vec![0x1b, b'[', b'M', 32 + 32, 33, 33])
        );
    }

    #[test]
    fn legacy_encoding_drops_cells_past_223() {
        let none = Mods::default();
        let press = MouseEvent::Press(Button::Left);
        assert_eq!(
            encode(press, 222, 0, none, CLICK),
            Some(vec![0x1b, b'[', b'M', 32, 255, 33])
        );
        assert_eq!(encode(press, 223, 0, none, CLICK), None);
        assert_eq!(encode(press, 0, 223, none, CLICK), None);
        assert!(
            encode(press, 500, 0, none, sgr(CLICK)).is_some(),
            "SGR has no limit"
        );
    }

    #[test]
    fn utf8_mode_widens_the_coordinate_range() {
        let none = Mods::default();
        let mode = CLICK | TermMode::UTF8_MOUSE;
        let press = MouseEvent::Press(Button::Left);
        let bytes = encode(press, 299, 0, none, mode).unwrap();
        let mut expected = b"\x1b[M ".to_vec();
        expected.extend("\u{14c}".as_bytes());
        expected.push(33);
        assert_eq!(bytes, expected, "column 300 + 32 as a two-byte character");
        assert_eq!(encode(press, 0, 0, none, mode), Some(b"\x1b[M !!".to_vec()));
        assert_eq!(encode(press, UTF8_MAX, 0, none, mode), None);
    }

    #[test]
    fn sgr_wins_over_utf8() {
        let mode = sgr(CLICK) | TermMode::UTF8_MOUSE;
        assert_eq!(
            report(
                MouseEvent::Press(Button::Left),
                299,
                0,
                Mods::default(),
                mode
            ),
            "\x1b[<0;300;1M"
        );
    }
}
