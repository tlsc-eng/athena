use std::path::Path;

use gpui::{Context, InteractiveElement, Window};
use serde_json::Value;

use super::Shell;
use super::item::ItemView;
use super::palette::Mode;

/// A package.json script or Makefile target, as Run Task lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Task {
    /// "npm: build", "make: test".
    pub label: String,
    /// The script's text, or the file the target is in.
    pub detail: String,
    pub command: String,
}

/// The package manager the lockfile says the project uses.
fn package_runner(root: &Path) -> &'static str {
    [
        ("pnpm-lock.yaml", "pnpm"),
        ("yarn.lock", "yarn"),
        ("bun.lock", "bun"),
        ("bun.lockb", "bun"),
    ]
    .iter()
    .find(|(lock, _)| root.join(lock).is_file())
    .map_or("npm", |(_, runner)| runner)
}

/// Quotes a word for the shell only when it needs it.
fn quote(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:@+=".contains(c));
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// `scripts` of a package.json, in file order.
pub(super) fn package_scripts(package: &Value, runner: &str) -> Vec<Task> {
    package["scripts"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(name, script)| {
            Some(Task {
                label: format!("{runner}: {name}"),
                detail: script.as_str()?.to_string(),
                command: format!("{runner} run {}", quote(name)),
            })
        })
        .collect()
}

/// The targets a Makefile names, skipping special (`.PHONY`), pattern (`%.o`) and variable
/// targets, in file order.
pub(super) fn makefile_targets(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut in_define = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("define ") || trimmed == "define" {
            in_define = true;
        }
        if in_define {
            in_define = trimmed != "endef";
            continue;
        }
        if line.starts_with('\t') || trimmed.starts_with('#') {
            continue;
        }
        let Some(colon) = line.find(':') else {
            continue;
        };
        // `X := y`, `X ::= y` and `X = a:b` are assignments, not rules.
        let rest = &line[colon..];
        if rest.starts_with(":=") || rest.starts_with("::=") || line[..colon].contains('=') {
            continue;
        }
        for target in line[..colon].split_whitespace() {
            let usable = !target.starts_with('.')
                && !target.contains(['%', '$', '(', ')'])
                && !out.iter().any(|t| t == target);
            if usable {
                out.push(target.to_string());
            }
        }
    }
    out
}

/// Every task the project's root package.json and Makefile offer.
pub(super) fn discover(root: &Path) -> Vec<Task> {
    let mut tasks = Vec::new();
    if let Some(package) = std::fs::read_to_string(root.join("package.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    {
        tasks.extend(package_scripts(&package, package_runner(root)));
    }
    let makefile = ["GNUmakefile", "makefile", "Makefile"]
        .iter()
        .find(|n| root.join(n).is_file());
    if let Some(name) = makefile
        && let Ok(text) = std::fs::read_to_string(root.join(name))
    {
        tasks.extend(makefile_targets(&text).into_iter().map(|t| Task {
            label: format!("make: {t}"),
            detail: name.to_string(),
            command: format!("make {}", quote(&t)),
        }));
    }
    tasks
}

impl Shell {
    /// VS Code's Run Task: the project's scripts and Makefile targets in the palette.
    pub(super) fn open_tasks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.active_root() else {
            return;
        };
        if discover(&root).is_empty() {
            return self.transient_notice(
                "No tasks found",
                "Add scripts to package.json or targets to a Makefile in the project folder.",
                cx,
            );
        }
        self.open_palette(Mode::Tasks, window, cx);
    }

    /// Runs `command` in a new terminal tab, which starts in the project folder.
    pub(super) fn run_task(
        &mut self,
        command: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.active_root() else {
            return;
        };
        self.new_terminal(window, cx);
        let Some(item) = self
            .workspace
            .active_project()
            .and_then(|p| p.layout.as_ref())
            .and_then(|l| l.focused_pane())
            .and_then(|p| p.active_item())
            .cloned()
        else {
            return;
        };
        if let Some(ItemView::Terminal(view)) = self.item_view(&root, &item, cx) {
            view.update(cx, |v, _| v.run_on_start(format!("{command}\r")));
        }
    }
}

/// Binds Run Task and the test commands on the shell's root element.
pub(super) fn bind_run_actions(el: gpui::Div, cx: &mut Context<Shell>) -> gpui::Div {
    el.on_action(cx.listener(|this, _: &crate::actions::RunTask, w, cx| this.open_tasks(w, cx)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scripts_run_through_the_package_manager_in_file_order() {
        let package = json!({ "scripts": { "dev": "vite", "build:prod": "vite build", "x": 1 } });
        let tasks = package_scripts(&package, "pnpm");
        let commands: Vec<&str> = tasks.iter().map(|t| t.command.as_str()).collect();
        assert_eq!(commands, ["pnpm run dev", "pnpm run build:prod"]);
        assert_eq!(tasks[0].label, "pnpm: dev");
        assert_eq!(tasks[1].detail, "vite build");
        assert!(package_scripts(&json!({}), "npm").is_empty());
    }

    #[test]
    fn makefile_rules_are_targets_but_assignments_and_specials_are_not() {
        let text = "\
CC := clang
VERSION ?= 1.0
URL = http://x:80
.PHONY: build test
# lint: commented out
build: main.o
\t$(CC) -o app main.o
%.o: %.c
\t$(CC) -c $<
test lint:: build
install: ; cp app /usr/local/bin
$(BIN): x
define HELP
help: not a rule
endef
build: again
";
        assert_eq!(makefile_targets(text), ["build", "test", "lint", "install"]);
    }

    #[test]
    fn the_lockfile_picks_the_runner_and_odd_names_are_quoted() {
        let dir = std::env::temp_dir().join(format!("athena-tasks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(package_runner(&dir), "npm");
        std::fs::write(dir.join("yarn.lock"), "").unwrap();
        assert_eq!(package_runner(&dir), "yarn");
        std::fs::write(dir.join("package.json"), r#"{"scripts":{"it's":"echo"}}"#).unwrap();
        std::fs::write(dir.join("Makefile"), "all:\n\ttrue\n").unwrap();
        let tasks = discover(&dir);
        let commands: Vec<&str> = tasks.iter().map(|t| t.command.as_str()).collect();
        assert_eq!(commands, [r"yarn run 'it'\''s'", "make all"]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
