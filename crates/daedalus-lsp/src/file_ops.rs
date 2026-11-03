use std::path::{Path, PathBuf};

use globset::GlobBuilder;
use serde_json::{Value, json};

use crate::protocol::uri_from_path;

/// Whether `path` passes one of a server's `FileOperationFilter`s, as its
/// `workspace.fileOperations` capability lists them.
pub(crate) fn matches_filters(filters: &Value, path: &Path, is_dir: bool) -> bool {
    let text = path.to_string_lossy();
    filters
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .any(|filter| {
            if filter
                .get("scheme")
                .and_then(Value::as_str)
                .is_some_and(|s| s != "file")
            {
                return false;
            }
            let Some(pattern) = filter.get("pattern") else {
                return false;
            };
            match pattern.get("matches").and_then(Value::as_str) {
                Some("file") if is_dir => return false,
                Some("folder") if !is_dir => return false,
                _ => {}
            }
            let Some(glob) = pattern.get("glob").and_then(Value::as_str) else {
                return false;
            };
            let ignore_case = pattern
                .pointer("/options/ignoreCase")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let Ok(glob) = GlobBuilder::new(glob)
                .literal_separator(true)
                .case_insensitive(ignore_case)
                .build()
            else {
                return false;
            };
            let matcher = glob.compile_matcher();
            // A leading `**/` is meant to reach any folder, the filesystem root's too.
            matcher.is_match(text.as_ref()) || matcher.is_match(text.trim_start_matches('/'))
        })
}

/// The `files` parameter of `workspace/willRenameFiles` and `workspace/didRenameFiles`.
pub(crate) fn rename_params(renames: &[(PathBuf, PathBuf)]) -> Value {
    let files: Vec<Value> = renames
        .iter()
        .map(|(from, to)| json!({"oldUri": uri_from_path(from), "newUri": uri_from_path(to)}))
        .collect();
    json!({ "files": files })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_match_by_glob_kind_and_scheme_as_typescript_language_server_declares_them() {
        let tls = json!([
            {"scheme": "file", "pattern": {"glob": "**/*.{ts,js,jsx,tsx,mjs,mts,cjs,cts}", "matches": "file"}},
            {"scheme": "file", "pattern": {"glob": "**", "matches": "folder"}}
        ]);
        let p = Path::new;
        assert!(matches_filters(&tls, p("/Users/me/app/src/a.ts"), false));
        assert!(matches_filters(&tls, p("/Users/me/app/src/a.tsx"), false));
        assert!(!matches_filters(&tls, p("/Users/me/app/README.md"), false));
        assert!(matches_filters(&tls, p("/Users/me/app/src"), true));
        let files_only = json!([{"pattern": {"glob": "**/*.go", "matches": "file"}}]);
        assert!(
            !matches_filters(&files_only, p("/p/pkg.go"), true),
            "a folder"
        );
        assert!(matches_filters(&files_only, p("/p/x/main.go"), false));
        let untitled = json!([{"scheme": "untitled", "pattern": {"glob": "**"}}]);
        assert!(!matches_filters(&untitled, p("/p/a.go"), false));
        let shouty = json!([{"pattern": {"glob": "**/*.TS", "options": {"ignoreCase": true}}}]);
        assert!(matches_filters(&shouty, p("/p/a.ts"), false));
        let star = json!([{"pattern": {"glob": "/p/*.ts"}}]);
        assert!(
            !matches_filters(&star, p("/p/src/a.ts"), false),
            "* stays in one folder"
        );
        assert!(!matches_filters(&json!(null), p("/p/a.ts"), false));
    }

    #[test]
    fn rename_params_list_old_and_new_uris() {
        let params = rename_params(&[("/p/a b.ts".into(), "/p/c.ts".into())]);
        assert_eq!(
            params,
            json!({"files": [{"oldUri": "file:///p/a%20b.ts", "newUri": "file:///p/c.ts"}]})
        );
    }
}
