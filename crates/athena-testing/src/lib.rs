//! Tests for Athena: which runner a folder uses, the command that runs some of its tests, and
//! what `go test -json`, Vitest's and Jest's JSON reporters say happened.

mod go;
mod js;
mod run;

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;

pub use go::{GoModule, find_go_module, go_job, go_run_pattern, parse_go_json};
pub use js::{find_js_package, js_framework, js_job, js_name_pattern, parse_js_report};
pub use run::{Ended, Finished, run};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Framework {
    Go,
    Vitest,
    Jest,
}

impl Framework {
    pub fn label(self) -> &'static str {
        match self {
            Self::Go => "go test",
            Self::Vitest => "Vitest",
            Self::Jest => "Jest",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Passed,
    Failed,
    Skipped,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TestCase {
    /// The names from the outermost in: a Go test then its subtests, or `describe` titles then
    /// the test's own.
    pub titles: Vec<String>,
    /// One-based line of the test in its suite's file, when the reporter says.
    pub line: Option<u32>,
    pub outcome: Outcome,
    pub duration_ms: u64,
    pub output: String,
}

impl TestCase {
    /// `TestSub/two` for Go, `math › adds` for JavaScript.
    pub fn name(&self, framework: Framework) -> String {
        let sep = if framework == Framework::Go {
            "/"
        } else {
            " › "
        };
        self.titles.join(sep)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Suite {
    pub framework: Framework,
    /// A Go package's import path, or a test file's path.
    pub name: String,
    /// The package's folder or the test file's, which relative paths in output start from.
    pub dir: PathBuf,
    /// The test file, for JavaScript suites.
    pub file: Option<PathBuf>,
    pub outcome: Outcome,
    /// Output that belongs to no one test, such as a compile error.
    pub output: String,
    pub tests: Vec<TestCase>,
}

impl Suite {
    /// Failed when a test failed or the suite could not run; skipped when nothing ran.
    pub fn status(&self) -> Outcome {
        let broken = self.outcome == Outcome::Failed
            && !self.tests.iter().any(|t| t.outcome == Outcome::Failed);
        if broken || self.tests.iter().any(|t| t.outcome == Outcome::Failed) {
            Outcome::Failed
        } else if self.tests.iter().any(|t| t.outcome == Outcome::Passed) {
            Outcome::Passed
        } else {
            self.outcome
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub suites: Vec<Suite>,
}

impl Report {
    pub fn count(&self, outcome: Outcome) -> usize {
        self.suites
            .iter()
            .flat_map(|s| &s.tests)
            .filter(|t| t.outcome == outcome)
            .count()
    }

    /// Suites that failed without any test failing, such as a package that does not compile.
    pub fn broken(&self) -> usize {
        self.suites
            .iter()
            .filter(|s| {
                s.outcome == Outcome::Failed
                    && !s.tests.iter().any(|t| t.outcome == Outcome::Failed)
            })
            .count()
    }

    /// Folds a later run in: its tests replace the same tests here, and the rest stay.
    pub fn merge(&mut self, newer: Report) {
        for suite in newer.suites {
            let Some(old) = self
                .suites
                .iter_mut()
                .find(|s| s.framework == suite.framework && s.name == suite.name)
            else {
                self.suites.push(suite);
                continue;
            };
            old.dir = suite.dir;
            old.file = suite.file.or(old.file.take());
            old.outcome = suite.outcome;
            old.output = suite.output;
            for test in suite.tests {
                match old.tests.iter_mut().find(|t| t.titles == test.titles) {
                    Some(slot) => *slot = test,
                    None => old.tests.push(test),
                }
            }
        }
    }

    /// The failed tests by suite, and the suites that could not run at all.
    pub fn failed(&self) -> Vec<(&Suite, Vec<&TestCase>)> {
        self.suites
            .iter()
            .filter(|s| s.status() == Outcome::Failed)
            .map(|s| {
                let tests = s
                    .tests
                    .iter()
                    .filter(|t| t.outcome == Outcome::Failed)
                    .collect();
                (s, tests)
            })
            .collect()
    }
}

/// What to start, from which folder, and where its JSON report lands if not on stdout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    pub framework: Framework,
    pub dir: PathBuf,
    pub program: String,
    pub args: Vec<String>,
    pub report: Option<PathBuf>,
}

/// A `path:line[:column]` in test output, with where it sits in the line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub span: Range<usize>,
    pub path: PathBuf,
    pub line: u32,
    pub column: Option<u32>,
}

/// Source locations in one line of output, as Go, Node and Vitest print them.
pub fn links(line: &str) -> Vec<Link> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r"(?:file://)?([^\s:()'\x22]+\.(?:go|ts|tsx|mts|cts|js|jsx|mjs|cjs)):(\d+)(?::(\d+))?",
        )
        .expect("valid pattern")
    });
    re.captures_iter(line)
        .filter_map(|c| {
            let whole = c.get(0)?;
            Some(Link {
                span: whole.range(),
                path: PathBuf::from(c.get(1)?.as_str()),
                line: c.get(2)?.as_str().parse().ok()?,
                column: c.get(3).and_then(|m| m.as_str().parse().ok()),
            })
        })
        .collect()
}

/// A link's file: absolute as printed, or relative to the suite's folder.
pub fn resolve(dir: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        dir.join(path)
    }
}

/// Reporters colour their messages; the panel shows plain text.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            for d in chars.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// The report a finished job left: parsed from stdout for Go, from its report file otherwise.
pub fn report(job: &Job, finished: &Finished, module: Option<&GoModule>) -> anyhow::Result<Report> {
    let said = || {
        let text = [&finished.stderr, &finished.stdout]
            .into_iter()
            .map(|s| strip_ansi(s.trim()))
            .find(|s| !s.is_empty())
            .unwrap_or_else(|| "The test run ended without a report.".into());
        let tail: Vec<&str> = text.lines().rev().take(12).collect();
        tail.into_iter().rev().collect::<Vec<_>>().join("\n")
    };
    let report = match (job.framework, &job.report) {
        (Framework::Go, _) => parse_go_json(&finished.stdout, module),
        (_, Some(path)) => match std::fs::read_to_string(path) {
            Ok(text) => {
                let _ = std::fs::remove_file(path);
                let value: serde_json::Value = serde_json::from_str(&text)
                    .map_err(|e| anyhow::anyhow!("the JSON report is not valid: {e}"))?;
                parse_js_report(&value, job.framework)
            }
            Err(_) => anyhow::bail!("{}", said()),
        },
        (_, None) => anyhow::bail!("no report file was asked for"),
    };
    if report.suites.is_empty() && finished.code != Some(0) {
        anyhow::bail!("{}", said());
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(name: &str, outcome: Outcome) -> TestCase {
        TestCase {
            titles: name.split('/').map(String::from).collect(),
            line: None,
            outcome,
            duration_ms: 1,
            output: String::new(),
        }
    }

    fn suite(name: &str, outcome: Outcome, tests: Vec<TestCase>) -> Suite {
        Suite {
            framework: Framework::Go,
            name: name.into(),
            dir: PathBuf::from("/m"),
            file: None,
            outcome,
            output: String::new(),
            tests,
        }
    }

    #[test]
    fn a_later_run_of_one_test_keeps_the_others() {
        let mut all = Report {
            suites: vec![suite(
                "m/a",
                Outcome::Failed,
                vec![
                    case("TestA", Outcome::Passed),
                    case("TestB", Outcome::Failed),
                ],
            )],
        };
        all.merge(Report {
            suites: vec![
                suite("m/a", Outcome::Passed, vec![case("TestB", Outcome::Passed)]),
                suite("m/b", Outcome::Passed, vec![case("TestC", Outcome::Passed)]),
            ],
        });
        assert_eq!(all.suites.len(), 2);
        assert_eq!(all.count(Outcome::Passed), 3);
        assert_eq!(all.suites[0].status(), Outcome::Passed);
        assert!(all.failed().is_empty());
    }

    #[test]
    fn a_suite_that_did_not_build_counts_as_failed() {
        let r = Report {
            suites: vec![suite("m/broken", Outcome::Failed, Vec::new())],
        };
        assert_eq!(r.broken(), 1);
        assert_eq!(r.suites[0].status(), Outcome::Failed);
        assert_eq!(r.failed().len(), 1);
    }

    #[test]
    fn links_are_found_in_go_node_and_vitest_output() {
        let go = links("    a_test.go:11: Add(1, 1) = 2, want 3");
        assert_eq!(go[0].path, PathBuf::from("a_test.go"));
        assert_eq!((go[0].line, go[0].column), (11, None));
        assert_eq!(go[0].span, 4..16);
        let panic = links("\t/work/m/pan/p_test.go:9 +0x34");
        assert_eq!(panic[0].path, PathBuf::from("/work/m/pan/p_test.go"));
        let node = links("    at Object.<anonymous> (/w/src/x.test.ts:6:19)");
        assert_eq!(node[0].path, PathBuf::from("/w/src/x.test.ts"));
        assert_eq!(node[0].column, Some(19));
        let url = links("at file:///w/node_modules/v/dist/a.js:155:11");
        assert_eq!(url[0].path, PathBuf::from("/w/node_modules/v/dist/a.js"));
        assert!(links("ok  \texample.com/m\t0.5s").is_empty());
        assert_eq!(
            resolve(Path::new("/m/sub"), Path::new("a_test.go")),
            PathBuf::from("/m/sub/a_test.go")
        );
    }

    #[test]
    fn colour_codes_are_dropped() {
        assert_eq!(
            strip_ansi("\u{1b}[31mExpected\u{1b}[39m 401"),
            "Expected 401"
        );
    }
}
