use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::{Framework, Job, Outcome, Report, Suite, TestCase, strip_ansi};

/// Vitest or Jest, from the package's dependencies or else its `test` script.
pub fn js_framework(package: &Value) -> Option<Framework> {
    let depends = |name: &str| {
        ["dependencies", "devDependencies"]
            .iter()
            .any(|k| package[k].get(name).is_some())
    };
    if depends("vitest") {
        return Some(Framework::Vitest);
    }
    if depends("jest") {
        return Some(Framework::Jest);
    }
    let script = package["scripts"]["test"].as_str().unwrap_or_default();
    if script.contains("vitest") {
        Some(Framework::Vitest)
    } else if script.contains("jest") {
        Some(Framework::Jest)
    } else {
        None
    }
}

/// The nearest package at or above `from` (no higher than `stop`) that runs Vitest or Jest.
pub fn find_js_package(from: &Path, stop: &Path) -> Option<(PathBuf, Framework)> {
    for dir in from.ancestors() {
        let framework = std::fs::read_to_string(dir.join("package.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| js_framework(&v));
        if let Some(framework) = framework {
            return Some((dir.to_path_buf(), framework));
        }
        if dir == stop {
            break;
        }
    }
    None
}

/// A `-t` pattern for a test, or with `exact` off for every test under a `describe`.
pub fn js_name_pattern(titles: &[String], exact: bool) -> String {
    let name = regex::escape(&titles.join(" "));
    if exact {
        format!("^{name}$")
    } else {
        format!("^{name}( |$)")
    }
}

/// Runs `files` (all tests when empty) of the package in `dir`, the JSON report going to `report`.
pub fn js_job(
    framework: Framework,
    dir: &Path,
    files: &[PathBuf],
    pattern: Option<String>,
    report: PathBuf,
) -> Job {
    // `--no` refuses to download a runner the project does not have.
    let mut args = vec!["--no".to_string()];
    let out = format!("--outputFile={}", report.display());
    match framework {
        Framework::Jest => args.extend(["jest".into(), "--json".into(), out]),
        _ => args.extend(["vitest".into(), "run".into(), "--reporter=json".into(), out]),
    }
    for file in files {
        let rel = file.strip_prefix(dir).unwrap_or(file);
        // Jest reads file arguments as patterns; Vitest as plain filters.
        args.push(match framework {
            Framework::Jest => regex::escape(&rel.to_string_lossy()),
            _ => rel.display().to_string(),
        });
    }
    if let Some(pattern) = pattern {
        args.push("-t".into());
        args.push(pattern);
    }
    Job {
        framework,
        dir: dir.to_path_buf(),
        program: "npx".into(),
        args,
        report: Some(report),
    }
}

fn outcome(status: &str) -> Outcome {
    match status {
        "passed" => Outcome::Passed,
        "failed" => Outcome::Failed,
        _ => Outcome::Skipped,
    }
}

/// Reads the report Vitest's `json` reporter and Jest's `--json` both write.
pub fn parse_js_report(v: &Value, framework: Framework) -> Report {
    let mut suites = Vec::new();
    for file in v["testResults"].as_array().into_iter().flatten() {
        let path = PathBuf::from(file["name"].as_str().unwrap_or_default());
        let tests: Vec<TestCase> = file["assertionResults"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|a| {
                let mut titles: Vec<String> = a["ancestorTitles"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect();
                titles.push(a["title"].as_str().unwrap_or_default().to_string());
                let output = a["failureMessages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|m| m.as_str().map(strip_ansi))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                TestCase {
                    titles,
                    line: a["location"]["line"].as_u64().map(|l| l as u32),
                    outcome: outcome(a["status"].as_str().unwrap_or_default()),
                    duration_ms: a["duration"].as_f64().unwrap_or(0.) as u64,
                    output,
                }
            })
            .collect();
        suites.push(Suite {
            framework,
            name: path.display().to_string(),
            dir: path.parent().map(Path::to_path_buf).unwrap_or_default(),
            file: Some(path),
            outcome: outcome(file["status"].as_str().unwrap_or_default()),
            output: strip_ansi(file["message"].as_str().unwrap_or_default()),
            tests,
        });
    }
    Report { suites }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn vitest_reports_read_with_titles_lines_and_messages() {
        let v: Value = serde_json::from_str(include_str!("../fixtures/vitest.json")).unwrap();
        let r = parse_js_report(&v, Framework::Vitest);
        let suite = &r.suites[0];
        assert_eq!(
            suite.file.as_deref(),
            Some(Path::new("/work/web/src/math.test.ts"))
        );
        assert_eq!(suite.dir, PathBuf::from("/work/web/src"));
        let names: Vec<String> = suite
            .tests
            .iter()
            .map(|t| t.name(Framework::Vitest))
            .collect();
        assert_eq!(
            names,
            ["math › adds", "math › fails", "math › skipped", "top level"]
        );
        let fails = &suite.tests[1];
        assert_eq!(fails.outcome, Outcome::Failed);
        assert_eq!(fails.line, Some(5));
        assert!(
            fails
                .output
                .starts_with("AssertionError: expected 2 to be 3")
        );
        let link = fails.output.lines().flat_map(crate::links).next().unwrap();
        assert_eq!(link.path, PathBuf::from("/work/web/src/math.test.ts"));
        assert_eq!((link.line, link.column), (6, Some(19)));
        assert_eq!(suite.tests[2].outcome, Outcome::Skipped);
        assert_eq!(r.count(Outcome::Passed), 2);
        assert_eq!(suite.status(), Outcome::Failed);
    }

    #[test]
    fn a_jest_suite_that_cannot_load_is_broken_with_its_message() {
        let v = json!({ "testResults": [{
            "name": "/w/a.test.js", "status": "failed",
            "message": "\u{1b}[1mTest suite failed to run\u{1b}[22m\n\n  SyntaxError: x",
            "assertionResults": []
        }, {
            "name": "/w/b.test.js", "status": "passed", "message": "",
            "assertionResults": [
                { "ancestorTitles": [], "title": "t", "status": "pending", "failureMessages": [] },
                { "ancestorTitles": [], "title": "u", "status": "todo", "failureMessages": [] }
            ]
        }]});
        let r = parse_js_report(&v, Framework::Jest);
        assert_eq!(r.broken(), 1);
        assert!(r.suites[0].output.starts_with("Test suite failed to run"));
        assert_eq!(r.count(Outcome::Skipped), 2);
    }

    #[test]
    fn the_runner_comes_from_dependencies_then_the_test_script() {
        assert_eq!(
            js_framework(&json!({ "devDependencies": { "vitest": "^3" } })),
            Some(Framework::Vitest)
        );
        assert_eq!(
            js_framework(&json!({ "dependencies": { "jest": "29" } })),
            Some(Framework::Jest)
        );
        assert_eq!(
            js_framework(&json!({ "scripts": { "test": "jest --ci" } })),
            Some(Framework::Jest)
        );
        assert_eq!(
            js_framework(&json!({ "scripts": { "test": "mocha" } })),
            None
        );
    }

    #[test]
    fn a_workspace_package_without_a_runner_uses_the_one_above_it() {
        let dir = std::env::temp_dir().join(format!("athena-jspkg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("packages/ui/src")).unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"devDependencies":{"vitest":"3"}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("packages/ui/package.json"), r#"{"name":"ui"}"#).unwrap();
        assert_eq!(
            find_js_package(&dir.join("packages/ui/src"), &dir),
            Some((dir.clone(), Framework::Vitest))
        );
        assert_eq!(
            find_js_package(&dir.join("packages/ui/src"), &dir.join("packages")),
            None
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn jobs_name_files_and_tests() {
        let file = PathBuf::from("/w/app/[id]/page.test.ts");
        let pattern = js_name_pattern(&["math".into(), "adds (1+1)".into()], true);
        assert_eq!(pattern, r"^math adds \(1\+1\)$");
        assert_eq!(js_name_pattern(&["math".into()], false), "^math( |$)");
        let job = js_job(
            Framework::Vitest,
            Path::new("/w"),
            std::slice::from_ref(&file),
            Some(pattern.clone()),
            PathBuf::from("/r/out.json"),
        );
        assert_eq!(
            job.args,
            [
                "--no",
                "vitest",
                "run",
                "--reporter=json",
                "--outputFile=/r/out.json",
                "app/[id]/page.test.ts",
                "-t",
                pattern.as_str()
            ]
        );
        let jest = js_job(
            Framework::Jest,
            Path::new("/w"),
            &[file],
            None,
            "/r/o.json".into(),
        );
        assert_eq!(jest.args[1..3], ["jest", "--json"]);
        assert_eq!(jest.args[4], r"app/\[id\]/page\.test\.ts");
    }
}
