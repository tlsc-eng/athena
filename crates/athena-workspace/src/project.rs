use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Item, ItemId, ItemKind, Layout};

/// The first id of a panel terminal, far above any layout's, so the two never share a key.
pub const PANEL_IDS: u64 = 1 << 32;

/// The bottom panel's terminals, shown there as sub-tabs like VS Code's panel terminals.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Panel {
    pub terminals: Vec<Item>,
    pub active: usize,
}

impl Panel {
    pub fn is_empty(&self) -> bool {
        self.terminals.is_empty()
    }

    pub fn holds(id: ItemId) -> bool {
        id.0 >= PANEL_IDS
    }

    pub fn active_item(&self) -> Option<&Item> {
        self.terminals
            .get(self.active)
            .or_else(|| self.terminals.last())
    }

    pub fn item_mut(&mut self, id: ItemId) -> Option<&mut Item> {
        self.terminals.iter_mut().find(|i| i.id == id)
    }

    /// Takes `item` in under a panel id, makes it active and returns that id.
    pub fn adopt(&mut self, mut item: Item) -> ItemId {
        let highest = self.terminals.iter().map(|i| i.id.0).max();
        item.id = ItemId(highest.map_or(PANEL_IDS, |h| h + 1));
        let id = item.id;
        self.terminals.push(item);
        self.active = self.terminals.len() - 1;
        id
    }

    /// Removes a terminal; the one after it shows next, or the one before when it was last.
    pub fn take(&mut self, id: ItemId) -> Option<Item> {
        let at = self.terminals.iter().position(|i| i.id == id)?;
        let item = self.terminals.remove(at);
        if self.active > at {
            self.active -= 1;
        }
        self.active = self.active.min(self.terminals.len().saturating_sub(1));
        Some(item)
    }

    pub fn activate(&mut self, id: ItemId) -> bool {
        match self.terminals.iter().position(|i| i.id == id) {
            Some(at) => {
                self.active = at;
                true
            }
            None => false,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Project {
    pub root: PathBuf,
    /// `None` once every pane has been closed; a file without the key gets one terminal.
    #[serde(default = "default_layout")]
    pub layout: Option<Layout>,
    /// The command that starts Claude Code here (e.g. a profile wrapper like `claude-tlsc`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_command: Option<String>,
    #[serde(default, skip_serializing_if = "Panel::is_empty")]
    pub panel: Panel,
    /// Whether the ESLint and Biome the project installs may run; they are the project's code.
    #[serde(default, skip_serializing_if = "LinterTrust::is_not_asked")]
    pub linters: LinterTrust,
    /// Phase 3 stored a single shell here; read only to migrate it into `layout`.
    #[serde(default, skip_serializing)]
    terminal: Option<u64>,
}

/// The answer to "Run this project's linters?", asked once per project.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LinterTrust {
    #[default]
    NotAsked,
    Allowed,
    Denied,
}

impl LinterTrust {
    fn is_not_asked(&self) -> bool {
        *self == Self::NotAsked
    }
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
            panel: Panel::default(),
            linters: LinterTrust::NotAsked,
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

    /// Every tab: the editor area's, then the bottom panel's terminals.
    pub fn items(&self) -> impl Iterator<Item = &Item> {
        self.layout
            .iter()
            .flat_map(|l| l.items())
            .chain(&self.panel.terminals)
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
    fn linters_are_not_asked_about_until_answered_and_the_answer_is_kept() {
        let p: Project = serde_json::from_str(r#"{"root":"/n/a"}"#).unwrap();
        assert_eq!(p.linters, LinterTrust::NotAsked);
        assert!(!serde_json::to_string(&p).unwrap().contains("linters"));
        for answer in [LinterTrust::Allowed, LinterTrust::Denied] {
            let mut p = p.clone();
            p.linters = answer;
            let json = serde_json::to_string(&p).unwrap();
            let back: Project = serde_json::from_str(&json).unwrap();
            assert_eq!(back.linters, answer, "{json}");
        }
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

    fn terminal() -> Item {
        Item {
            id: ItemId(2),
            kind: ItemKind::Terminal { session: Some(7) },
            view: None,
        }
    }

    #[test]
    fn panel_terminals_take_ids_no_layout_uses_and_keep_their_session() {
        let mut panel = Panel::default();
        let a = panel.adopt(terminal());
        let b = panel.adopt(terminal());
        assert_eq!((a, b), (ItemId(PANEL_IDS), ItemId(PANEL_IDS + 1)));
        assert!(Panel::holds(a) && !Panel::holds(ItemId(2)));
        assert_eq!(panel.active_item().map(|i| i.id), Some(b));
        assert_eq!(
            panel.take(b).map(|i| i.kind),
            Some(ItemKind::Terminal { session: Some(7) })
        );
        assert_eq!(panel.active_item().map(|i| i.id), Some(a));
    }

    #[test]
    fn closing_a_panel_terminal_shows_its_neighbour() {
        let mut panel = Panel::default();
        let ids: Vec<ItemId> = (0..3).map(|_| panel.adopt(terminal())).collect();
        assert!(panel.activate(ids[0]));
        panel.take(ids[2]);
        assert_eq!(panel.active_item().map(|i| i.id), Some(ids[0]));
        assert!(panel.activate(ids[1]));
        panel.take(ids[0]);
        assert_eq!(panel.active_item().map(|i| i.id), Some(ids[1]));
        panel.take(ids[1]);
        assert!(panel.is_empty() && panel.active == 0);
    }

    #[test]
    fn the_panel_is_saved_only_when_it_holds_a_terminal() {
        let mut p = Project::new("/n/a".into());
        assert!(!serde_json::to_string(&p).unwrap().contains("panel"));
        p.panel.adopt(terminal());
        let json = serde_json::to_string(&p).unwrap();
        let back: Project = serde_json::from_str(&json).unwrap();
        assert_eq!(back.panel, p.panel);
        assert_eq!(
            p.items().count(),
            2,
            "the layout's terminal and the panel's"
        );
        let old: Project = serde_json::from_str(r#"{"root":"/n/a"}"#).unwrap();
        assert!(old.panel.is_empty());
        let mut stale = p.panel.clone();
        stale.active = 9;
        assert!(stale.active_item().is_some());
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
