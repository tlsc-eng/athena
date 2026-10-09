use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::ServerKind;
use crate::protocol::uri_from_path;

/// The project's own copy of a linter's server, from `<root>/node_modules/.bin`.
///
/// It is the project's code, so it only runs when the binary resolves, symlinks followed, to a
/// file inside the project's own `node_modules`: never from a parent folder, a `node_modules`
/// linked in from elsewhere, or a `.bin` entry pointing out of the project.
pub fn project_server(root: &Path, kind: ServerKind) -> Option<PathBuf> {
    if !kind.is_project_local() {
        return None;
    }
    let root = root.canonicalize().ok()?;
    let modules = root.join("node_modules");
    let real_modules = modules.canonicalize().ok()?;
    if !real_modules.starts_with(&root) {
        return None;
    }
    let real = modules
        .join(".bin")
        .join(kind.program())
        .canonicalize()
        .ok()?;
    (real.starts_with(&real_modules) && is_executable(&real)).then_some(real)
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The settings vscode-eslint-language-server reads, as the VS Code extension sends them; it
/// asks for them all at once and does nothing without them.
pub fn eslint_settings(root: &Path) -> Value {
    let flat = [
        "eslint.config.js",
        "eslint.config.mjs",
        "eslint.config.cjs",
        "eslint.config.ts",
        "eslint.config.mts",
        "eslint.config.cts",
    ]
    .iter()
    .any(|name| root.join(name).is_file());
    let name = root
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    json!({
        "validate": "on",
        "run": "onType",
        "packageManager": "npm",
        "useESLintClass": false,
        "useFlatConfig": flat,
        "experimental": {"useFlatConfig": flat},
        "codeActionOnSave": {"enable": false, "mode": "all"},
        "format": false,
        "quiet": false,
        "onIgnoredFiles": "off",
        "options": {},
        "rulesCustomizations": [],
        "problems": {"shortenToSingleLine": false},
        "nodePath": root.join("node_modules"),
        "workingDirectory": {"mode": "location"},
        "workspaceFolder": {"uri": uri_from_path(root), "name": name},
        "codeAction": {
            "disableRuleComment": {"enable": true, "location": "separateLine"},
            "showDocumentation": {"enable": true}
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn project(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("athena-local-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn script(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn a_linked_bin_entry_inside_node_modules_is_found() {
        let root = project("npm");
        let real = root.join("node_modules/@biomejs/biome/bin/biome");
        script(&real);
        std::fs::create_dir_all(root.join("node_modules/.bin")).unwrap();
        symlink(
            "../@biomejs/biome/bin/biome",
            root.join("node_modules/.bin/biome"),
        )
        .unwrap();
        assert_eq!(project_server(&root, ServerKind::Biome), Some(real));
        assert_eq!(
            project_server(&root, ServerKind::Eslint),
            None,
            "not installed"
        );
        assert_eq!(
            project_server(&root, ServerKind::Go),
            None,
            "gopls comes from PATH"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_plain_script_in_bin_is_found_as_pnpm_writes_them() {
        let root = project("pnpm");
        let bin = root.join("node_modules/.bin/vscode-eslint-language-server");
        script(&bin);
        assert_eq!(project_server(&root, ServerKind::Eslint), Some(bin.clone()));
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            project_server(&root, ServerKind::Eslint),
            None,
            "not executable"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn binaries_resolving_outside_the_project_node_modules_are_refused() {
        let root = project("escape");
        std::fs::create_dir_all(root.join("node_modules/.bin")).unwrap();
        symlink("/usr/bin/true", root.join("node_modules/.bin/biome")).unwrap();
        assert_eq!(project_server(&root, ServerKind::Biome), None, "links out");
        let elsewhere = root.join("vendor/biome");
        script(&elsewhere);
        symlink(
            &elsewhere,
            root.join("node_modules/.bin/vscode-eslint-language-server"),
        )
        .unwrap();
        assert_eq!(
            project_server(&root, ServerKind::Eslint),
            None,
            "inside the project but outside node_modules"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_node_modules_linked_from_outside_or_a_parent_folder_is_not_used() {
        let outside = project("outside");
        script(&outside.join("node_modules/.bin/biome"));
        let root = project("linked");
        symlink(outside.join("node_modules"), root.join("node_modules")).unwrap();
        assert_eq!(project_server(&root, ServerKind::Biome), None, "linked in");
        let child = outside.join("packages/app");
        std::fs::create_dir_all(&child).unwrap();
        assert_eq!(
            project_server(&child, ServerKind::Biome),
            None,
            "the parent's"
        );
        assert!(project_server(&outside, ServerKind::Biome).is_some());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn eslint_settings_name_the_folder_and_detect_flat_config() {
        let root = project("eslint");
        let settings = eslint_settings(&root);
        assert_eq!(settings["validate"], "on");
        assert_eq!(settings["useFlatConfig"], false);
        assert_eq!(settings["workspaceFolder"]["uri"], uri_from_path(&root));
        assert_eq!(
            settings["nodePath"],
            root.join("node_modules").to_str().unwrap(),
            "ESLint resolves its library from the project, never a folder above it"
        );
        std::fs::write(root.join("eslint.config.mjs"), "export default [];\n").unwrap();
        assert_eq!(
            eslint_settings(&root)["experimental"]["useFlatConfig"],
            true
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
