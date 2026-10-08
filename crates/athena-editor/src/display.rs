/// Columns a tab advances to the next multiple of.
pub const TAB_WIDTH: usize = 4;

/// A buffer line as drawn: tabs expanded to spaces, with offsets back to buffer chars.
pub struct DisplayLine {
    pub text: String,
    /// Byte offset in `text` for each buffer char, plus one past the end.
    pub char_to_byte: Vec<usize>,
}

impl DisplayLine {
    pub fn new(line: &str) -> Self {
        let mut text = String::with_capacity(line.len());
        let mut char_to_byte = Vec::with_capacity(line.len() + 1);
        let mut col = 0;
        for c in line.chars() {
            char_to_byte.push(text.len());
            if c == '\t' {
                let n = TAB_WIDTH - col % TAB_WIDTH;
                text.extend(std::iter::repeat_n(' ', n));
                col += n;
            } else {
                text.push(c);
                col += 1;
            }
        }
        char_to_byte.push(text.len());
        Self { text, char_to_byte }
    }

    /// Buffer char (within the line) for a display byte, rounding into the char that contains it.
    pub fn char_for_byte(&self, byte: usize) -> usize {
        match self.char_to_byte.binary_search(&byte) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_tabs_to_stops() {
        let d = DisplayLine::new("\tx\ty");
        assert_eq!(d.text, "    x   y");
        assert_eq!(d.char_to_byte, vec![0, 4, 5, 8, 9]);
        assert_eq!(d.char_for_byte(2), 0);
        assert_eq!(d.char_for_byte(4), 1);
        assert_eq!(d.char_for_byte(9), 4);
    }
}
