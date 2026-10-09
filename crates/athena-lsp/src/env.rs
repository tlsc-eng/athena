use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// Variables passed through to language servers; everything else (tokens, CLAUDE_*) stays behind.
const INHERITED: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "PATH",
    "TMPDIR",
    "LANG",
    "SSH_AUTH_SOCK",
];

/// Toolchain settings the servers need: Go's module and proxy config, nodenv and goenv.
const TOOLCHAIN_PREFIXES: &[&str] = &["GO", "NODENV_", "NODE_", "NVM_", "LC_"];

/// Kept from a project's own linters: an SSH agent, preloaded modules or extra module folders
/// would hand the project's code more than it needs to lint.
const WITHHELD_FROM_PROJECT: &[&str] = &["SSH_AUTH_SOCK", "NODE_OPTIONS", "NODE_PATH"];

/// The login shell's environment: an app started from the Dock has only `/usr/bin:/bin` on PATH.
fn login_env() -> &'static [(String, String)] {
    static ENV: OnceLock<Vec<(String, String)>> = OnceLock::new();
    ENV.get_or_init(|| {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let output = Command::new(shell)
            .args(["-lc", "env -0"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        match output {
            Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
                .split('\0')
                .filter_map(|kv| kv.split_once('='))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            _ => std::env::vars().collect(),
        }
    })
}

/// The environment a language server runs with; `project_local` for one from the project's
/// node_modules. `toolchain` is the GOTOOLCHAIN the user's settings chose, if any.
pub fn server_env(project_local: bool, toolchain: Option<&str>) -> Vec<(String, String)> {
    filter_env(login_env(), project_local, toolchain)
}

fn filter_env(
    login: &[(String, String)],
    project_local: bool,
    toolchain: Option<&str>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = login
        .iter()
        .filter(|(k, _)| {
            INHERITED.contains(&k.as_str()) || TOOLCHAIN_PREFIXES.iter().any(|p| k.starts_with(p))
        })
        .filter(|(k, _)| k != "GOTOOLCHAIN")
        .filter(|(k, _)| !project_local || !WITHHELD_FROM_PROJECT.contains(&k.as_str()))
        .cloned()
        .collect();
    // go.mod's `toolchain` line would otherwise make gopls download and run another Go.
    env.push(("GOTOOLCHAIN".into(), toolchain.unwrap_or("local").into()));
    env
}

/// Finds `name` on the login shell's PATH.
pub fn find_program(name: &str) -> Option<PathBuf> {
    let path = &login_env().iter().find(|(k, _)| k == "PATH")?.1;
    find_in(path, name)
}

fn find_in(path: &str, name: &str) -> Option<PathBuf> {
    // A relative entry resolves against wherever Athena happens to run, often a project.
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_env_pins_the_go_toolchain_and_drops_secrets() {
        let env = server_env(false, None);
        assert!(env.contains(&("GOTOOLCHAIN".into(), "local".into())));
        let chosen = server_env(false, Some("auto"));
        assert!(chosen.contains(&("GOTOOLCHAIN".into(), "auto".into())));
        assert_eq!(chosen.iter().filter(|(k, _)| k == "GOTOOLCHAIN").count(), 1);
        assert!(
            env.iter()
                .all(|(k, _)| !k.starts_with("CLAUDE") && !k.ends_with("_TOKEN"))
        );
        assert!(env.iter().any(|(k, _)| k == "PATH"));
    }

    #[test]
    fn programs_are_found_only_in_absolute_path_folders() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("athena-find-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = dir.join("athena-tool");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cwd = std::env::current_dir().unwrap();
        let mut relative: PathBuf = cwd.components().skip(1).map(|_| "..").collect();
        relative.push(dir.strip_prefix("/").unwrap());
        assert!(relative.is_relative() && relative.join("athena-tool").exists());
        let both = format!("{}:{}", relative.display(), dir.display());
        let found = find_in(&relative.display().to_string(), "athena-tool");
        let absolute = find_in(&both, "athena-tool");
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            found, None,
            "a folder relative to wherever Athena started is skipped"
        );
        assert_eq!(absolute, Some(tool));
    }

    #[test]
    fn project_linters_get_no_ssh_agent_preloads_or_extra_module_paths() {
        let login: Vec<(String, String)> = [
            ("PATH", "/usr/bin"),
            ("SSH_AUTH_SOCK", "/tmp/agent"),
            ("NODE_OPTIONS", "--require /tmp/hook.js"),
            ("NODE_PATH", "/opt/modules"),
            ("NODENV_VERSION", "22.1.0"),
            ("NODE_EXTRA_CA_CERTS", "/etc/ca.pem"),
            ("GITHUB_TOKEN", "secret"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .into();
        let names = |local| -> Vec<String> {
            filter_env(&login, local, None)
                .into_iter()
                .map(|(k, _)| k)
                .collect()
        };
        assert_eq!(
            names(true),
            [
                "PATH",
                "NODENV_VERSION",
                "NODE_EXTRA_CA_CERTS",
                "GOTOOLCHAIN"
            ]
        );
        assert_eq!(
            names(false),
            [
                "PATH",
                "SSH_AUTH_SOCK",
                "NODE_OPTIONS",
                "NODE_PATH",
                "NODENV_VERSION",
                "NODE_EXTRA_CA_CERTS",
                "GOTOOLCHAIN"
            ],
            "gopls and tsserver keep them"
        );
    }
}
