use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::{Framework, Job, Outcome, Report, Suite, TestCase};

/// A Go module: the folder holding its go.mod and the import path it declares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoModule {
    pub root: PathBuf,
    pub path: String,
}

impl GoModule {
    /// The folder of a package of this module.
    pub fn dir_of(&self, import: &str) -> PathBuf {
        match import.strip_prefix(&self.path) {
            Some(rest) => self.root.join(rest.trim_start_matches('/')),
            None => self.root.clone(),
        }
    }

    /// The folder of the package `import` when it belongs to this module.
    pub fn package_dir(&self, import: &str) -> Option<PathBuf> {
        let rest = import.strip_prefix(&self.path)?;
        if !rest.is_empty() && !rest.starts_with('/') {
            return None;
        }
        Some(self.root.join(rest.trim_start_matches('/')))
    }

    /// The `./sub/dir` that names the package in `dir` to `go test`, run from the module root.
    pub fn package_arg(&self, dir: &Path) -> String {
        match dir.strip_prefix(&self.root) {
            Ok(rel) if rel.as_os_str().is_empty() => ".".into(),
            Ok(rel) => format!("./{}", rel.display()),
            Err(_) => dir.display().to_string(),
        }
    }
}

/// The module `from` belongs to, looking no higher than `stop`.
pub fn find_go_module(from: &Path, stop: &Path) -> Option<GoModule> {
    let (from, stop) = crate::within(from, stop)?;
    for dir in from.ancestors() {
        if let Ok(text) = std::fs::read_to_string(dir.join("go.mod")) {
            let path = text.lines().find_map(|l| {
                let rest = l.trim().strip_prefix("module")?;
                rest.starts_with([' ', '\t'])
                    .then(|| rest.trim().trim_matches('"').to_string())
            })?;
            return Some(GoModule {
                root: dir.to_path_buf(),
                path,
            });
        }
        if dir == stop {
            break;
        }
    }
    None
}

/// `^(TestA|TestB)$` for `go test -run`.
pub fn go_run_pattern(names: &[String]) -> String {
    let escaped: Vec<String> = names.iter().map(|n| regex::escape(n)).collect();
    match escaped.as_slice() {
        [one] => format!("^{one}$"),
        many => format!("^({})$", many.join("|")),
    }
}

/// `^TestA$/^sub$` for one subtest: `go test` matches each `/`-separated part against one level.
pub fn go_subtest_pattern(titles: &[String]) -> String {
    titles
        .iter()
        .map(|t| format!("^{}$", regex::escape(t)))
        .collect::<Vec<_>>()
        .join("/")
}

/// `go test -json` from the module root for `packages`, limited to tests matching `run`.
pub fn go_job(module: &GoModule, packages: &[String], run: Option<String>) -> Job {
    let mut args = vec!["test".to_string(), "-json".to_string()];
    if let Some(pattern) = run {
        args.push("-run".into());
        args.push(pattern);
    }
    args.extend(packages.iter().cloned());
    Job {
        framework: Framework::Go,
        dir: module.root.clone(),
        program: "go".into(),
        args,
        report: None,
    }
}

/// Also has `go test` write a coverage profile to `profile`.
pub fn with_coverage(mut job: Job, profile: &Path) -> Job {
    let at = job.args.len().min(2);
    job.args
        .insert(at, format!("-coverprofile={}", profile.display()));
    job
}

/// A file's measured lines, zero-based, and whether any statement on each one ran.
pub type FileCoverage = BTreeMap<usize, bool>;

/// Reads a `-coverprofile` file into per-file line coverage; files outside `module` are dropped.
pub fn parse_coverprofile(text: &str, module: &GoModule) -> HashMap<PathBuf, FileCoverage> {
    let mut out: HashMap<PathBuf, FileCoverage> = HashMap::new();
    for line in text.lines().filter(|l| !l.starts_with("mode:")) {
        let Some((file, lines, covered)) = cover_block(line) else {
            continue;
        };
        let path = if file.starts_with('/') {
            PathBuf::from(file)
        } else {
            let Some((dir, name)) = file
                .rsplit_once('/')
                .and_then(|(pkg, name)| Some((module.package_dir(pkg)?, name)))
            else {
                continue;
            };
            dir.join(name)
        };
        let file = out.entry(path).or_default();
        for line in lines {
            *file.entry(line).or_default() |= covered;
        }
    }
    out
}

/// `example.com/m/a.go:3.24,5.2 1 1`: the file, its zero-based lines and whether it ran.
fn cover_block(line: &str) -> Option<(&str, std::ops::RangeInclusive<usize>, bool)> {
    let (file, rest) = line.rsplit_once(':')?;
    let mut fields = rest.split(' ');
    let (span, _statements, count) = (fields.next()?, fields.next()?, fields.next()?);
    let (start, end) = span.split_once(',')?;
    let line_of = |at: &str| at.split_once('.')?.0.parse::<usize>().ok()?.checked_sub(1);
    let (start, end) = (line_of(start)?, line_of(end)?);
    let count: u64 = count.trim().parse().ok()?;
    (start <= end).then_some((file, start..=end, count > 0))
}

/// `=== RUN` and `--- PASS:` lines say what the tree already shows.
fn is_framing(line: &str) -> bool {
    let t = line.trim_start();
    [
        "=== RUN",
        "=== PAUSE",
        "=== CONT",
        "=== NAME",
        "--- PASS:",
        "--- FAIL:",
        "--- SKIP:",
    ]
    .iter()
    .any(|p| t.starts_with(p))
}

fn outcome(action: &str) -> Option<Outcome> {
    match action {
        "pass" => Some(Outcome::Passed),
        "fail" => Some(Outcome::Failed),
        "skip" => Some(Outcome::Skipped),
        _ => None,
    }
}

/// Reads `go test -json` output; lines that are not JSON (a shell's greeting) are ignored.
pub fn parse_go_json(text: &str, module: Option<&GoModule>) -> Report {
    struct Pending {
        suite: Suite,
        tests: Vec<TestCase>,
        /// The package whose compile error stopped this one, which may be a dependency.
        failed_build: Option<String>,
    }
    let mut order: Vec<String> = Vec::new();
    let mut pending: HashMap<String, Pending> = HashMap::new();
    let mut build: HashMap<String, String> = HashMap::new();
    for line in text.lines().filter(|l| l.starts_with('{')) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let action = v["Action"].as_str().unwrap_or_default();
        if let Some(import) = v["ImportPath"].as_str() {
            let pkg = import.split(' ').next().unwrap_or(import).to_string();
            if let Some(out) = v["Output"].as_str() {
                build.entry(pkg).or_default().push_str(out);
            }
            continue;
        }
        let Some(pkg) = v["Package"].as_str() else {
            continue;
        };
        let entry = pending.entry(pkg.to_string()).or_insert_with(|| {
            order.push(pkg.to_string());
            Pending {
                suite: Suite {
                    framework: Framework::Go,
                    name: pkg.to_string(),
                    dir: module.map(|m| m.dir_of(pkg)).unwrap_or_default(),
                    file: None,
                    outcome: Outcome::Passed,
                    output: String::new(),
                    tests: Vec::new(),
                },
                tests: Vec::new(),
                failed_build: None,
            }
        });
        let output = v["Output"].as_str().filter(|o| !is_framing(o));
        let Some(test) = v["Test"].as_str() else {
            if let Some(out) = output {
                entry.suite.output.push_str(out);
            }
            if let Some(o) = outcome(action) {
                entry.suite.outcome = o;
            }
            if let Some(failed) = v["FailedBuild"].as_str() {
                entry.failed_build = failed.split(' ').next().map(String::from);
            }
            continue;
        };
        let titles: Vec<String> = test.split('/').map(String::from).collect();
        let slot = match entry.tests.iter().position(|t| t.titles == titles) {
            Some(i) => i,
            None => {
                // Failed until it says otherwise: a test that never reports back was running
                // when its test binary died.
                entry.tests.push(TestCase {
                    titles,
                    line: None,
                    outcome: Outcome::Failed,
                    duration_ms: 0,
                    output: String::new(),
                });
                entry.tests.len() - 1
            }
        };
        let case = &mut entry.tests[slot];
        if let Some(out) = output {
            case.output.push_str(out);
        }
        if let Some(o) = outcome(action) {
            case.outcome = o;
            case.duration_ms = (v["Elapsed"].as_f64().unwrap_or(0.) * 1000.) as u64;
        }
    }
    let mut suites = Vec::new();
    for pkg in order {
        let Some(mut p) = pending.remove(&pkg) else {
            continue;
        };
        let culprit = p.failed_build.as_deref().unwrap_or(&pkg);
        if let Some(out) = build.get(culprit).or_else(|| build.get(&pkg)) {
            p.suite.output.insert_str(0, out);
        }
        p.suite.tests = p.tests;
        let no_tests = p.suite.tests.is_empty() && p.suite.outcome == Outcome::Skipped;
        if !no_tests {
            suites.push(p.suite);
        }
    }
    Report { suites }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../fixtures/go_test.json");

    fn module() -> GoModule {
        GoModule {
            root: PathBuf::from("/work/m"),
            path: "example.com/m".into(),
        }
    }

    fn find<'a>(r: &'a Report, pkg: &str, test: &str) -> &'a TestCase {
        let suite = r.suites.iter().find(|s| s.name == pkg).unwrap();
        suite
            .tests
            .iter()
            .find(|t| t.titles.join("/") == test)
            .unwrap_or_else(|| panic!("{test} not in {pkg}"))
    }

    #[test]
    fn pass_fail_skip_and_subtests_come_through() {
        let r = parse_go_json(FIXTURE, Some(&module()));
        let names: Vec<&str> = r.suites.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "example.com/m/broken",
                "example.com/m/pan",
                "example.com/m/sub"
            ]
        );
        let sub = "example.com/m/sub";
        assert_eq!(find(&r, sub, "TestPass").outcome, Outcome::Passed);
        assert_eq!(find(&r, sub, "TestPass").output, "    a_test.go:6: hello\n");
        let fail = find(&r, sub, "TestFail");
        assert_eq!(fail.outcome, Outcome::Failed);
        assert_eq!(fail.output, "    a_test.go:11: Add(1, 1) = 2, want 3\n");
        assert_eq!(find(&r, sub, "TestSkip").outcome, Outcome::Skipped);
        assert_eq!(find(&r, sub, "TestSub/one").outcome, Outcome::Passed);
        assert_eq!(find(&r, sub, "TestSub/two").titles, ["TestSub", "two"]);
        assert_eq!(find(&r, sub, "TestSub/two").outcome, Outcome::Failed);
        assert_eq!(find(&r, sub, "TestSub").outcome, Outcome::Failed);
        let suite = r.suites.iter().find(|s| s.name == sub).unwrap();
        assert_eq!(suite.dir, PathBuf::from("/work/m/sub"));
        assert_eq!(suite.status(), Outcome::Failed);
    }

    #[test]
    fn a_panic_fails_its_test_and_names_the_line() {
        let r = parse_go_json(FIXTURE, Some(&module()));
        let panicked = find(&r, "example.com/m/pan", "TestPanics");
        assert_eq!(panicked.outcome, Outcome::Failed);
        assert!(
            panicked
                .output
                .starts_with("panic: assignment to entry in nil map")
        );
        let at = panicked
            .output
            .lines()
            .flat_map(crate::links)
            .find(|l| l.path.starts_with("/work"))
            .unwrap();
        assert_eq!(
            (at.path, at.line),
            (PathBuf::from("/work/m/pan/p_test.go"), 9)
        );
        assert_eq!(
            find(&r, "example.com/m/pan", "TestOK").outcome,
            Outcome::Passed
        );
    }

    #[test]
    fn a_test_cut_off_by_its_binary_dying_is_failed() {
        let text = r#"{"Action":"run","Package":"m/p","Test":"TestA"}
{"Action":"output","Package":"m/p","Test":"TestA","Output":"panic: boom\n"}
{"Action":"output","Package":"m/p","Output":"FAIL\tm/p\t0.1s\n"}
{"Action":"fail","Package":"m/p","Elapsed":0.1}"#;
        let r = parse_go_json(text, None);
        assert_eq!(r.suites[0].tests[0].outcome, Outcome::Failed);
        assert_eq!(r.suites[0].tests[0].output, "panic: boom\n");
    }

    #[test]
    fn a_build_failure_keeps_the_compiler_output_and_no_test_packages_are_dropped() {
        let r = parse_go_json(&format!("zsh greeting\n{FIXTURE}"), Some(&module()));
        let broken = &r.suites[0];
        assert!(broken.tests.is_empty());
        assert_eq!(broken.status(), Outcome::Failed);
        assert!(
            broken
                .output
                .contains("broken/b_test.go:5:28: undefined: undefined"),
            "{}",
            broken.output
        );
        assert!(!r.suites.iter().any(|s| s.name.ends_with("notests")));
        assert_eq!(r.broken(), 1);
    }

    #[test]
    fn a_dependency_that_does_not_compile_shows_its_error_on_the_package() {
        let text = r##"{"ImportPath":"internal/goos","Action":"build-output","Output":"# internal/goos\n"}
{"ImportPath":"internal/goos","Action":"build-output","Output":"compile: version mismatch\n"}
{"ImportPath":"internal/goos","Action":"build-fail"}
{"Action":"start","Package":"m/calc"}
{"Action":"output","Package":"m/calc","Output":"FAIL\tm/calc [build failed]\n"}
{"Action":"fail","Package":"m/calc","Elapsed":0,"FailedBuild":"internal/goos"}"##;
        let r = parse_go_json(text, None);
        assert_eq!(
            r.suites[0].output,
            "# internal/goos\ncompile: version mismatch\nFAIL\tm/calc [build failed]\n"
        );
    }

    #[test]
    fn modules_map_packages_to_folders_and_back() {
        let dir = std::env::temp_dir().join(format!("athena-gomod-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("svc/api")).unwrap();
        std::fs::write(
            dir.join("svc/go.mod"),
            "// x\nmodule \"example.com/svc\"\n\ngo 1.22\n",
        )
        .unwrap();
        let m = find_go_module(&dir.join("svc/api"), &dir).unwrap();
        assert_eq!(m.root, dir.join("svc"));
        assert_eq!(m.path, "example.com/svc");
        assert_eq!(m.dir_of("example.com/svc/api"), dir.join("svc/api"));
        assert_eq!(m.package_arg(&dir.join("svc/api")), "./api");
        assert_eq!(m.package_arg(&dir.join("svc")), ".");
        assert_eq!(
            find_go_module(&dir.join("svc/api"), &dir.join("svc/api")),
            None
        );
        std::fs::create_dir_all(dir.join("other")).unwrap();
        assert_eq!(
            find_go_module(&dir.join("svc/api"), &dir.join("other")),
            None,
            "outside the project, never climbing to /"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn coverage_profiles_mark_lines_that_ran_and_lines_that_did_not() {
        let profile = "mode: set\n\
                       example.com/m/sub/a.go:3.24,4.11 1 1\n\
                       example.com/m/sub/a.go:4.11,6.3 1 0\n\
                       example.com/m/sub/a.go:7.2,7.14 1 1\n\
                       example.com/m/b.go:1.1,1.9 2 0\n\
                       example.com/mx/c.go:1.1,2.1 1 1\n\
                       golang.org/x/y/d.go:1.1,2.1 1 1\n\
                       /abs/e.go:2.1,2.5 1 3\n\
                       garbage line\n";
        let cov = parse_coverprofile(profile, &module());
        let a = &cov[&PathBuf::from("/work/m/sub/a.go")];
        let lines: Vec<(usize, bool)> = a.iter().map(|(l, c)| (*l, *c)).collect();
        // Line 4 holds the end of a block that ran, so it counts as run.
        assert_eq!(
            lines,
            [(2, true), (3, true), (4, false), (5, false), (6, true)]
        );
        assert!(!cov[&PathBuf::from("/work/m/b.go")][&0]);
        assert!(cov[&PathBuf::from("/abs/e.go")][&1]);
        assert_eq!(cov.len(), 3);
        let job = with_coverage(
            go_job(&module(), &["./...".into()], None),
            Path::new("/t/c.out"),
        );
        assert_eq!(
            job.args,
            ["test", "-json", "-coverprofile=/t/c.out", "./..."]
        );
    }

    #[test]
    fn run_patterns_are_anchored() {
        assert_eq!(go_run_pattern(&["TestA".into()]), "^TestA$");
        assert_eq!(
            go_run_pattern(&["TestA".into(), "TestB".into()]),
            "^(TestA|TestB)$"
        );
        assert_eq!(
            go_subtest_pattern(&["TestA".into(), "adds_1+1".into(), "(x)".into()]),
            r"^TestA$/^adds_1\+1$/^\(x\)$"
        );
        let job = go_job(&module(), &["./sub".into()], Some("^TestA$".into()));
        assert_eq!(job.args, ["test", "-json", "-run", "^TestA$", "./sub"]);
        assert_eq!(job.dir, PathBuf::from("/work/m"));
    }
}
