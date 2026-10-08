use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{ItemKind, Layout};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Project {
    pub root: PathBuf,
    /// `None` once every pane has been closed; a file without the key gets one terminal.
    #[serde(default = "default_layout")]
    pub layout: Option<Layout>,
    /// The command that starts Claude Code here (e.g. a profile wrapper like `claude-tlsc`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_command: Option<String>,
    /// Phase 3 stored a single shell here; read only to migrate it into `layout`.
    #[serde(default, skip_serializing)]
    terminal: Option<u64>,
}

fn default_layout() -> Option<Layout> {
    Some(Layout::new(ItemKind::Terminal { session: None }))
}

impl Project {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            layout: default_layout(),
            claude_command: None,
            terminal: None,
        }
    }

    /// Moves a Phase 3 `terminal` session id into the layout so the running shell is kept.
    pub(crate) fn migrate(&mut self) {
        let Some(session) = self.terminal.take() else {
            return;
        };
        let layout = self
            .layout
            .get_or_insert_with(|| Layout::new(ItemKind::Terminal { session: None }));
        let first_terminal = layout
            .items()
            .find(|i| matches!(i.kind, ItemKind::Terminal { session: None }))
            .map(|i| i.id);
        if let Some(id) = first_terminal
            && let Some(item) = layout.item_mut(id)
        {
            item.kind = ItemKind::Terminal {
                session: Some(session),
            };
        }
    }

    pub fn name(&self) -> String {
        self.root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.root.display().to_string())
    }

    /// Two-letter rail label: initials of the first two words, else the first two letters.
    pub fn monogram(&self) -> String {
        let name = self.name();
        let words: Vec<&str> = name
            .split(['-', '_', ' ', '.'])
            .filter(|w| !w.is_empty())
            .collect();
        let letters: String = match words.as_slice() {
            [a, b, ..] => a.chars().take(1).chain(b.chars().take(1)).collect(),
            _ => name
                .chars()
                .filter(|c| c.is_alphanumeric())
                .take(2)
                .collect(),
        };
        letters.to_uppercase()
    }
}

/// Current branch from `.git/HEAD`, or a short commit id when detached; `None` outside a repo.
pub fn git_branch(root: &Path) -> Option<String> {
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_file() {
        let pointer = fs::read_to_string(&dot_git).ok()?;
        let dir = PathBuf::from(pointer.strip_prefix("gitdir:")?.trim());
        if dir.is_absolute() {
            dir
        } else {
            root.join(dir)
        }
    } else {
        dot_git
    };
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => Some(branch.to_string()),
        None => Some(head.chars().take(7).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mono(path: &str) -> String {
        Project::new(path.into()).monogram()
    }

    #[test]
    fn monogram_rules() {
        assert_eq!(mono("/x/hephaestus"), "HE");
        assert_eq!(mono("/x/hestia-infra"), "HI");
        assert_eq!(mono("/x/pi_dashboard"), "PD");
        assert_eq!(mono("/x/a"), "A");
    }

    #[test]
    fn phase3_terminal_migrates_into_layout() {
        let json = r#"{"root":"/n/a","terminal":1791452910000001}"#;
        let mut p: Project = serde_json::from_str(json).unwrap();
        p.migrate();
        let items: Vec<_> = p
            .layout
            .as_ref()
            .unwrap()
            .items()
            .map(|i| i.kind.clone())
            .collect();
        assert_eq!(
            items,
            vec![ItemKind::Terminal {
                session: Some(1791452910000001)
            }]
        );
        assert!(!serde_json::to_string(&p).unwrap().contains("\"terminal\""));
    }

    #[test]
    fn closed_layout_stays_closed() {
        let p: Project = serde_json::from_str(r#"{"root":"/n/a","layout":null}"#).unwrap();
        assert!(p.layout.is_none());
        let p: Project = serde_json::from_str(r#"{"root":"/n/a"}"#).unwrap();
        assert!(p.layout.is_some());
    }

    #[test]
    fn branch_from_head() {
        let dir = std::env::temp_dir().join(format!("athena-git-{}", std::process::id()));
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join(".git/HEAD"), "ref: refs/heads/feat/x\n").unwrap();
        assert_eq!(git_branch(&dir).as_deref(), Some("feat/x"));
        fs::write(dir.join(".git/HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(git_branch(&dir).as_deref(), Some("0123456"));
        fs::remove_dir_all(&dir).unwrap();
    }
}
