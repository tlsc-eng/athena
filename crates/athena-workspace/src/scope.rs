use std::path::{Component, Path, PathBuf};

/// Names Claude may not read or open through Athena, even inside a project.
fn denied_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        ".git" | ".netrc" | ".npmrc" | ".pypirc" | ".aws" | ".ssh" | ".gnupg" | ".env"
    ) || lower.starts_with(".env.")
        || ["id_rsa", "id_ed25519", "id_ecdsa", "id_dsa"]
            .iter()
            .any(|k| lower.starts_with(k))
        || [".pem", ".key", ".p12", ".pfx", ".keystore"]
            .iter()
            .any(|ext| lower.ends_with(ext))
        || lower.contains("credentials")
}

/// True if any part of a project-relative path is one Claude may not touch.
pub fn denied(relative: &Path) -> bool {
    relative
        .components()
        .any(|c| matches!(c, Component::Normal(n) if denied_name(&n.to_string_lossy())))
}

/// Resolves `path` (symlinks included) and accepts it only inside one of `roots` and not denied.
pub fn resolve_in_roots(path: &Path, roots: &[PathBuf]) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("{} is not an absolute path", path.display()));
    }
    let real = path
        .canonicalize()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    for root in roots {
        let Ok(root) = root.canonicalize() else {
            continue;
        };
        if let Ok(relative) = real.strip_prefix(&root) {
            if denied(relative) {
                return Err(format!("{} is not shared with Claude", path.display()));
            }
            return Ok(real);
        }
    }
    Err(format!("{} is outside the open projects", path.display()))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("athena-scope-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("proj/src")).unwrap();
        fs::write(dir.join("proj/src/main.go"), "package main").unwrap();
        fs::write(dir.join("proj/.env"), "SECRET=1").unwrap();
        fs::write(dir.join("outside.txt"), "nope").unwrap();
        dir
    }

    #[test]
    fn accepts_files_inside_a_project() {
        let dir = tmp("ok");
        let roots = vec![dir.join("proj")];
        assert!(resolve_in_roots(&dir.join("proj/src/main.go"), &roots).is_ok());
        assert!(
            resolve_in_roots(&dir.join("proj"), &roots).is_ok(),
            "the root itself"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_escapes() {
        let dir = tmp("escape");
        let roots = vec![dir.join("proj")];
        assert!(resolve_in_roots(&dir.join("proj/../outside.txt"), &roots).is_err());
        symlink(dir.join("outside.txt"), dir.join("proj/link.txt")).unwrap();
        assert!(
            resolve_in_roots(&dir.join("proj/link.txt"), &roots).is_err(),
            "symlink out of the project"
        );
        assert!(
            resolve_in_roots(Path::new("src/main.go"), &roots).is_err(),
            "relative paths"
        );
        assert!(resolve_in_roots(&dir.join("proj/missing.go"), &roots).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_secrets_inside_a_project() {
        let dir = tmp("secret");
        let roots = vec![dir.join("proj")];
        assert!(resolve_in_roots(&dir.join("proj/.env"), &roots).is_err());
        for p in [
            ".git/config",
            ".env.local",
            "certs/server.pem",
            "aws_credentials.json",
            ".ssh/id_ed25519",
        ] {
            assert!(denied(Path::new(p)), "{p}");
        }
        for p in [
            "src/main.go",
            "envoy.yaml",
            "keyboard.ts",
            "trivy-secret.yaml",
        ] {
            assert!(!denied(Path::new(p)), "{p}");
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
