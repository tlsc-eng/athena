use alacritty_terminal::term::TermMode;
use gpui::Keystroke;

/// Escape bytes for keys a shell expects as control sequences.
///
/// Returns `None` for plain text and Cmd chords: text goes through the IME so dead keys and
/// composition work, and Cmd belongs to the app.
pub fn to_esc(ks: &Keystroke, mode: TermMode) -> Option<Vec<u8>> {
    let m = &ks.modifiers;
    if m.platform {
        return None;
    }
    let param = 1 + m.shift as u8 + 2 * m.alt as u8 + 4 * m.control as u8;
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let cursor = |fin: char| -> Vec<u8> {
        if param > 1 {
            format!("\x1b[1;{param}{fin}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{fin}").into_bytes()
        } else {
            format!("\x1b[{fin}").into_bytes()
        }
    };
    let tilde = |n: u8| -> Vec<u8> {
        if param > 1 {
            format!("\x1b[{n};{param}~").into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };
    let ss3 = |fin: char| -> Vec<u8> {
        if param > 1 {
            format!("\x1b[1;{param}{fin}").into_bytes()
        } else {
            format!("\x1bO{fin}").into_bytes()
        }
    };
    let only_alt = m.alt && !m.control && !m.shift;

    let bytes = match ks.key.as_str() {
        "enter" if m.alt => b"\x1b\r".to_vec(),
        "enter" => b"\r".to_vec(),
        "tab" if m.shift => b"\x1b[Z".to_vec(),
        "tab" => b"\t".to_vec(),
        "backspace" if m.control => vec![0x08],
        "backspace" if m.alt => b"\x1b\x7f".to_vec(),
        "backspace" => vec![0x7f],
        "escape" => vec![0x1b],
        // Matches Terminal.app: Option-arrow moves by word in readline and zle.
        "left" if only_alt => b"\x1bb".to_vec(),
        "right" if only_alt => b"\x1bf".to_vec(),
        "up" => cursor('A'),
        "down" => cursor('B'),
        "right" => cursor('C'),
        "left" => cursor('D'),
        "home" => cursor('H'),
        "end" => cursor('F'),
        "insert" => tilde(2),
        "delete" => tilde(3),
        "pageup" => tilde(5),
        "pagedown" => tilde(6),
        "f1" => ss3('P'),
        "f2" => ss3('Q'),
        "f3" => ss3('R'),
        "f4" => ss3('S'),
        "f5" => tilde(15),
        "f6" => tilde(17),
        "f7" => tilde(18),
        "f8" => tilde(19),
        "f9" => tilde(20),
        "f10" => tilde(21),
        "f11" => tilde(23),
        "f12" => tilde(24),
        "space" if m.control => vec![0],
        key if m.control => {
            let mut chars = key.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else {
                return None;
            };
            let byte = control_byte(c)?;
            if m.alt { vec![0x1b, byte] } else { vec![byte] }
        }
        _ => return None,
    };
    Some(bytes)
}

fn control_byte(c: char) -> Option<u8> {
    Some(match c.to_ascii_lowercase() {
        c @ 'a'..='z' => c as u8 - b'a' + 1,
        '@' | '2' | ' ' => 0,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '-' | '7' | '/' => 0x1f,
        '8' | '?' => 0x7f,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn esc(s: &str) -> Option<Vec<u8>> {
        to_esc(&Keystroke::parse(s).unwrap(), TermMode::empty())
    }

    #[test]
    fn control_letters() {
        assert_eq!(esc("ctrl-c"), Some(vec![3]));
        assert_eq!(esc("ctrl-a"), Some(vec![1]));
        assert_eq!(esc("ctrl-["), Some(vec![0x1b]));
        assert_eq!(esc("ctrl-alt-b"), Some(vec![0x1b, 2]));
    }

    #[test]
    fn plain_text_goes_to_ime() {
        assert_eq!(esc("a"), None);
        assert_eq!(esc("alt-e"), None);
        assert_eq!(esc("cmd-c"), None);
    }

    #[test]
    fn cursor_keys_respect_app_mode() {
        assert_eq!(esc("up"), Some(b"\x1b[A".to_vec()));
        let app = to_esc(&Keystroke::parse("up").unwrap(), TermMode::APP_CURSOR);
        assert_eq!(app, Some(b"\x1bOA".to_vec()));
        assert_eq!(esc("shift-up"), Some(b"\x1b[1;2A".to_vec()));
        assert_eq!(esc("alt-left"), Some(b"\x1bb".to_vec()));
    }

    #[test]
    fn editing_keys() {
        assert_eq!(esc("enter"), Some(b"\r".to_vec()));
        assert_eq!(esc("backspace"), Some(vec![0x7f]));
        assert_eq!(esc("shift-tab"), Some(b"\x1b[Z".to_vec()));
        assert_eq!(esc("delete"), Some(b"\x1b[3~".to_vec()));
    }
}
