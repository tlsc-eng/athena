//! Whether a phone's prompt, interrupt or answer may reach a pane in its current state.

use daedalus_term::ClaudeState;

use crate::companion::server::Action;

/// Typing into a plain shell is allowed; Esc only makes sense for a running Claude.
pub(in crate::shell) fn gate(
    is_claude: bool,
    state: Option<ClaudeState>,
    action: &Action,
) -> Result<(), &'static str> {
    match action {
        Action::Prompt(_) => match state {
            Some(ClaudeState::NeedsInput) => Err("Claude is asking something; answer it first"),
            Some(ClaudeState::Running | ClaudeState::Finished) | None => Ok(()),
        },
        Action::Interrupt => match (is_claude, state) {
            (false, _) => Err("this is not a Claude session"),
            (true, Some(ClaudeState::Running)) => Ok(()),
            (true, Some(ClaudeState::NeedsInput | ClaudeState::Finished) | None) => {
                Err("Claude is not running")
            }
        },
        Action::Close => {
            if is_claude {
                Ok(())
            } else {
                Err("this is not a Claude session")
            }
        }
        Action::Answer { .. } => match (is_claude, state) {
            (false, _) => Err("this is not a Claude session"),
            (true, Some(ClaudeState::NeedsInput)) => Ok(()),
            (true, Some(ClaudeState::Running | ClaudeState::Finished) | None) => {
                Err("Claude is not asking anything")
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATES: [Option<ClaudeState>; 4] = [
        None,
        Some(ClaudeState::Running),
        Some(ClaudeState::NeedsInput),
        Some(ClaudeState::Finished),
    ];

    #[test]
    fn a_prompt_is_refused_only_while_claude_asks_something() {
        let prompt = Action::Prompt("hi".into());
        for is_claude in [false, true] {
            for state in STATES {
                let asking = state == Some(ClaudeState::NeedsInput);
                assert_eq!(gate(is_claude, state, &prompt).is_err(), asking);
            }
        }
    }

    #[test]
    fn an_answer_needs_claude_asking_something() {
        let answer = Action::Answer {
            key: "k".into(),
            answers: Vec::new(),
        };
        for is_claude in [false, true] {
            for state in STATES {
                let asking = is_claude && state == Some(ClaudeState::NeedsInput);
                assert_eq!(gate(is_claude, state, &answer).is_ok(), asking);
            }
        }
    }

    #[test]
    fn close_needs_a_claude_pane() {
        for state in STATES {
            assert!(gate(false, state, &Action::Close).is_err());
            assert!(gate(true, state, &Action::Close).is_ok());
        }
    }

    #[test]
    fn an_interrupt_needs_a_running_claude() {
        for is_claude in [false, true] {
            for state in STATES {
                let running = is_claude && state == Some(ClaudeState::Running);
                assert_eq!(gate(is_claude, state, &Action::Interrupt).is_ok(), running);
            }
        }
    }
}
