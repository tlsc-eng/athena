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

/// The environment a language server runs with.
pub fn server_env() -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = login_env()
        .iter()
        .filter(|(k, _)| {
            INHERITED.contains(&k.as_str()) || TOOLCHAIN_PREFIXES.iter().any(|p| k.starts_with(p))
        })
        .filter(|(k, _)| k != "GOTOOLCHAIN")
        .cloned()
        .collect();
    // go.mod's `toolchain` line would otherwise make gopls download and run another Go.
    env.push(("GOTOOLCHAIN".into(), "local".into()));
    env
}

/// Finds `name` on the login shell's PATH.
pub fn find_program(name: &str) -> Option<PathBuf> {
    let path = login_env().iter().find(|(k, _)| k == "PATH")?.1.clone();
    std::env::split_paths(&path)
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
        let env = server_env();
        assert!(env.contains(&("GOTOOLCHAIN".into(), "local".into())));
        assert!(
            env.iter()
                .all(|(k, _)| !k.starts_with("CLAUDE") && !k.ends_with("_TOKEN"))
        );
        assert!(env.iter().any(|(k, _)| k == "PATH"));
    }
}
