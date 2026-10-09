use std::collections::{HashMap, VecDeque};

/// A shell-integration mark (OSC 133, or VS Code's OSC 633 with the same letters).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// `A`: the prompt starts.
    Prompt,
    /// `B`: the prompt ends and the command line starts.
    Command,
    /// `C`: the command runs; its output starts here.
    Output,
    /// `D[;code]`: the command finished, with its exit code when the shell sent one.
    Finished(Option<i32>),
}

/// Bytes of an OSC body kept; a mark's letter and exit code come first, and the rest (such as
/// the command line zsh's `C` carries) is not needed.
const KEPT_BODY: usize = 32;

#[derive(Default)]
enum State {
    #[default]
    Ground,
    Escape,
    /// Inside `ESC ]`, holding the start of the body.
    Osc(Vec<u8>),
    /// An `ESC` inside the body, which `\` turns into the string terminator.
    OscEscape(Vec<u8>),
}

/// Finds marks in the byte stream before the terminal parses it, carrying partial sequences
/// across chunks.
#[derive(Default)]
pub struct Scanner {
    state: State,
}

impl Scanner {
    /// The marks in `bytes`, each with the offset just past its terminator.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<(usize, Mark)> {
        let mut found = Vec::new();
        for (i, &b) in bytes.iter().enumerate() {
            self.state = match (std::mem::take(&mut self.state), b) {
                (State::Ground, 0x1b) => State::Escape,
                (State::Ground, _) => State::Ground,
                (State::Escape, b']') => State::Osc(Vec::new()),
                (State::Escape, 0x1b) => State::Escape,
                (State::Escape, _) => State::Ground,
                (State::Osc(body), 0x07) => {
                    found.extend(mark(&body).map(|m| (i + 1, m)));
                    State::Ground
                }
                (State::Osc(body), 0x1b) => State::OscEscape(body),
                // CAN and SUB abort a control string.
                (State::Osc(_), 0x18 | 0x1a) => State::Ground,
                (State::Osc(mut body), b) => {
                    if body.len() < KEPT_BODY {
                        body.push(b);
                    }
                    State::Osc(body)
                }
                (State::OscEscape(body), b'\\') => {
                    found.extend(mark(&body).map(|m| (i + 1, m)));
                    State::Ground
                }
                (State::OscEscape(_), b']') => State::Osc(Vec::new()),
                (State::OscEscape(_), _) => State::Ground,
            };
        }
        found
    }
}

fn mark(body: &[u8]) -> Option<Mark> {
    let body = String::from_utf8_lossy(body);
    let rest = body
        .strip_prefix("133;")
        .or_else(|| body.strip_prefix("633;"))?;
    let mut params = rest.split(';');
    Some(match params.next()? {
        "A" => Mark::Prompt,
        "B" => Mark::Command,
        "C" => Mark::Output,
        "D" => Mark::Finished(params.next().and_then(|code| code.trim().parse().ok())),
        _ => return None,
    })
}

/// First of the private-use characters that tag a prompt's first cell with a command id; plane
/// 16, as Nerd Fonts put icons in plane 15.
const TAG_BASE: u32 = 0x10_0000;
/// Ids cycle through this many tags; older commands have long left the scrollback by then.
const TAGS: u32 = 0xFFFE;
/// Commands remembered at once; the scrollback holds far fewer prompts.
const MAX_COMMANDS: usize = 4096;

/// The zero-width character tagging a prompt line with `id`.
pub fn tag(id: u32) -> char {
    char::from_u32(TAG_BASE + id % TAGS).unwrap_or('\u{100000}')
}

/// The command id a zero-width character tags, if it is one of Athena's tags.
pub fn tag_id(c: char) -> Option<u32> {
    let c = c as u32;
    (TAG_BASE..TAG_BASE + TAGS)
        .contains(&c)
        .then(|| c - TAG_BASE)
}

/// A command between two prompts, as far as the shell reported it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Command {
    /// `None` until the command finishes; `Some(None)` when it finished without a code.
    pub exit: Option<Option<i32>>,
    /// Lines from the prompt line to the first output line.
    pub output_start: Option<i32>,
    /// Lines from the prompt line to just past the output.
    pub output_end: Option<i32>,
}

/// What is known about the commands whose prompts are tagged in the grid.
#[derive(Default)]
pub struct Commands {
    next: u32,
    by_id: HashMap<u32, Command>,
    order: VecDeque<u32>,
}

impl Commands {
    /// Starts a command and returns its id.
    pub fn start(&mut self) -> u32 {
        let id = self.next;
        self.next = (self.next + 1) % TAGS;
        self.by_id.insert(id, Command::default());
        self.order.retain(|&old| old != id);
        self.order.push_back(id);
        while self.order.len() > MAX_COMMANDS {
            if let Some(old) = self.order.pop_front() {
                self.by_id.remove(&old);
            }
        }
        id
    }

    /// Drops a command that never ran, such as Enter on an empty prompt.
    pub fn forget(&mut self, id: u32) {
        self.by_id.remove(&id);
        self.order.retain(|&old| old != id);
    }

    pub fn get(&self, id: u32) -> Option<&Command> {
        self.by_id.get(&id)
    }

    pub fn get_mut(&mut self, id: u32) -> Option<&mut Command> {
        self.by_id.get_mut(&id)
    }

    /// Ids from the newest command back.
    pub fn newest_first(&self) -> impl Iterator<Item = u32> + '_ {
        self.order.iter().rev().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_are_found_with_either_terminator_and_the_offset_after_them() {
        let mut s = Scanner::default();
        let bytes = b"x\x1b]133;A\x07$ \x1b]133;B\x1b\\ls\r\n\x1b]633;C\x07out\x1b]133;D;1\x07";
        let found = s.feed(bytes);
        let marks: Vec<Mark> = found.iter().map(|(_, m)| *m).collect();
        assert_eq!(
            marks,
            [
                Mark::Prompt,
                Mark::Command,
                Mark::Output,
                Mark::Finished(Some(1))
            ]
        );
        assert_eq!(&bytes[..found[0].0], b"x\x1b]133;A\x07");
        assert_eq!(found[3].0, bytes.len());
    }

    #[test]
    fn a_mark_split_across_chunks_is_found_in_the_chunk_that_ends_it() {
        let whole = b"ab\x1b]133;D;127\x1b\\cd";
        for split in 1..whole.len() {
            let mut s = Scanner::default();
            let first = s.feed(&whole[..split]);
            let second = s.feed(&whole[split..]);
            let all: Vec<Mark> = first.iter().chain(&second).map(|(_, m)| *m).collect();
            assert_eq!(all, [Mark::Finished(Some(127))], "split at {split}");
            if let Some((end, _)) = second.first() {
                assert_eq!(split + end, whole.len() - 2);
            }
        }
    }

    #[test]
    fn other_sequences_are_ignored_and_long_marks_still_count() {
        let mut s = Scanner::default();
        let mut long = b"\x1b]133;A".to_vec();
        long.extend([b'x'; 100]);
        long.push(0x07);
        let mut command = b"\x1b]133;C;".to_vec();
        command.extend([b'Q'; 300]);
        command.push(0x07);
        let bytes = [
            &b"\x1b]0;title\x07\x1b[31m\x1b]1337;A\x07\x1b]133;Z\x07\x1b]133;A\x18"[..],
            &long,
            &command,
            b"\x1b]133;D\x07",
        ]
        .concat();
        let found: Vec<Mark> = s.feed(&bytes).into_iter().map(|(_, m)| m).collect();
        assert_eq!(found, [Mark::Output, Mark::Finished(None)]);
    }

    #[test]
    fn tags_round_trip_and_ordinary_characters_are_not_tags() {
        assert_eq!(tag_id(tag(0)), Some(0));
        assert_eq!(tag_id(tag(41)), Some(41));
        assert_eq!(tag_id('\u{301}'), None);
        assert_eq!(tag_id('a'), None);
        assert_eq!(tag_id('\u{F0493}'), None, "a Nerd Font icon");
    }

    #[test]
    fn old_commands_are_forgotten_beyond_the_cap() {
        let mut commands = Commands::default();
        let first = commands.start();
        for _ in 0..MAX_COMMANDS {
            commands.start();
        }
        assert!(commands.get(first).is_none());
        assert_eq!(commands.newest_first().count(), MAX_COMMANDS);
    }
}
