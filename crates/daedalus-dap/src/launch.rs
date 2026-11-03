use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

/// What a Go launch configuration builds and runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `test` for a `_test.go` file in the editor, else `debug`, as VS Code's Go extension decides.
    Auto,
    Debug,
    Test,
    Exec,
}

impl Mode {
    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "auto" => Self::Auto,
            "debug" => Self::Debug,
            "test" => Self::Test,
            "exec" => Self::Exec,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Debug => "debug",
            Self::Test => "test",
            Self::Exec => "exec",
        }
    }
}

/// A `"type": "go"` entry of `.vscode/launch.json`, before its `${…}` variables are filled in.
#[derive(Clone, Debug, PartialEq)]
pub struct LaunchConfig {
    pub name: String,
    pub request: String,
    pub mode: Mode,
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// A string of flags or a list of them, passed to Delve as written.
    pub build_flags: Option<Value>,
    pub cwd: Option<String>,
}

/// The configuration used when the project has no launch.json: VS Code's "Launch Package".
pub fn default_config() -> LaunchConfig {
    LaunchConfig {
        name: "Launch Package".into(),
        request: "launch".into(),
        mode: Mode::Auto,
        program: "${fileDirname}".into(),
        args: Vec::new(),
        env: Vec::new(),
        build_flags: None,
        cwd: None,
    }
}

/// The Go configurations in a parsed launch.json, in order; other debuggers' entries are skipped.
pub fn go_configurations(launch: &Value) -> Result<Vec<LaunchConfig>, String> {
    let Some(list) = launch.get("configurations") else {
        return Ok(Vec::new());
    };
    let list = list.as_array().ok_or("\"configurations\" must be a list")?;
    list.iter()
        .filter(|c| c.get("type").and_then(Value::as_str) == Some("go"))
        .map(parse_config)
        .collect()
}

fn parse_config(c: &Value) -> Result<LaunchConfig, String> {
    let name = c
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("Go")
        .to_string();
    let text = |key: &str| -> Result<Option<String>, String> {
        match c.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(format!("{name}: \"{key}\" must be a string")),
        }
    };
    let mode = match text("mode")? {
        None => Mode::Auto,
        Some(m) => Mode::parse(&m).ok_or_else(|| {
            format!("{name}: mode \"{m}\" is not supported; use auto, debug, test or exec")
        })?,
    };
    let args = match c.get("args") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|a| a.as_str().map(str::to_string))
            .collect::<Option<_>>()
            .ok_or_else(|| format!("{name}: \"args\" must be a list of strings"))?,
        Some(_) => return Err(format!("{name}: \"args\" must be a list of strings")),
    };
    let env = match c.get("env") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(k, v)| match v {
                Value::String(s) => Some((k.clone(), s.clone())),
                Value::Number(n) => Some((k.clone(), n.to_string())),
                Value::Bool(b) => Some((k.clone(), b.to_string())),
                _ => None,
            })
            .collect::<Option<_>>()
            .ok_or_else(|| format!("{name}: \"env\" values must be strings"))?,
        Some(_) => return Err(format!("{name}: \"env\" must be an object")),
    };
    let build_flags = match c.get("buildFlags") {
        None | Some(Value::Null) => None,
        Some(v @ Value::String(_)) => Some(v.clone()),
        Some(Value::Array(items)) if items.iter().all(Value::is_string) => {
            Some(Value::Array(items.clone()))
        }
        Some(_) => return Err(format!("{name}: \"buildFlags\" must be a string or a list")),
    };
    Ok(LaunchConfig {
        request: text("request")?.unwrap_or_else(|| "launch".into()),
        mode,
        program: text("program")?.unwrap_or_else(|| "${fileDirname}".into()),
        args,
        env,
        build_flags,
        cwd: text("cwd")?,
        name,
    })
}

/// What `${…}` variables stand for.
#[derive(Clone, Copy, Debug)]
pub struct Context<'a> {
    pub workspace: &'a Path,
    /// The file in the focused editor, if any.
    pub file: Option<&'a Path>,
}

/// A configuration ready to hand to Delve.
#[derive(Clone, Debug, PartialEq)]
pub struct Launch {
    pub name: String,
    pub mode: Mode,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub build_flags: Option<Value>,
    pub cwd: PathBuf,
}

impl LaunchConfig {
    /// Fills in the variables and paths; `Err` says what in the configuration cannot be used.
    pub fn resolve(&self, ctx: &Context) -> Result<Launch, String> {
        if self.request != "launch" {
            return Err(format!(
                "{}: Athena launches programs and tests; attaching (\"request\": \"{}\") is not \
                 supported yet",
                self.name, self.request
            ));
        }
        let sub = |text: &str| substitute(text, ctx).map_err(|e| format!("{}: {e}", self.name));
        let absolute = |text: &str| -> Result<PathBuf, String> {
            let path = PathBuf::from(sub(text)?);
            Ok(match path.is_absolute() {
                true => path,
                false => ctx.workspace.join(path),
            })
        };
        let mode = match self.mode {
            Mode::Auto => match ctx.file.is_some_and(is_go_test_file) {
                true => Mode::Test,
                false => Mode::Debug,
            },
            mode => mode,
        };
        let mut program = absolute(&self.program)?;
        if mode == Mode::Test && program.extension().is_some_and(|e| e == "go") {
            program.pop();
        }
        let folder = match program.extension().is_some_and(|e| e == "go") || program.is_file() {
            true => program
                .parent()
                .map_or_else(|| ctx.workspace.to_path_buf(), Path::to_path_buf),
            false => program.clone(),
        };
        let cwd = match &self.cwd {
            Some(cwd) => absolute(cwd)?,
            None => folder,
        };
        let build_flags = match &self.build_flags {
            Some(Value::String(s)) => Some(Value::String(sub(s)?)),
            Some(Value::Array(items)) => Some(Value::Array(
                items
                    .iter()
                    .map(|i| sub(i.as_str().unwrap_or_default()).map(Value::String))
                    .collect::<Result<_, _>>()?,
            )),
            _ => None,
        };
        Ok(Launch {
            name: self.name.clone(),
            mode,
            program,
            args: self.args.iter().map(|a| sub(a)).collect::<Result<_, _>>()?,
            env: self
                .env
                .iter()
                .map(|(k, v)| Ok((k.clone(), sub(v)?)))
                .collect::<Result<_, String>>()?,
            build_flags,
            cwd,
        })
    }
}

impl Launch {
    /// Debugs the Go tests in package folder `dir` that `run` matches, as `go test -run` does.
    pub fn test(name: String, dir: &Path, run: String) -> Self {
        Self {
            name,
            mode: Mode::Test,
            program: dir.to_path_buf(),
            args: vec!["-test.run".into(), run],
            env: Vec::new(),
            build_flags: None,
            cwd: dir.to_path_buf(),
        }
    }

    /// Delve's `launch` arguments; it builds the program into `output`.
    pub fn delve_arguments(&self, output: &Path) -> Value {
        let env: Map<String, Value> = self
            .env
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        let mut args = json!({
            "request": "launch",
            "mode": self.mode.name(),
            "program": self.program,
            "args": self.args,
            "env": env,
            "cwd": self.cwd,
            "stopOnEntry": false,
            // The program's output arrives as events for the Debug Console.
            "outputMode": "remote",
        });
        if self.mode != Mode::Exec {
            args["output"] = json!(output);
        }
        if let Some(flags) = &self.build_flags {
            args["buildFlags"] = flags.clone();
        }
        args
    }
}

fn is_go_test_file(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| n.to_string_lossy().ends_with("_test.go"))
}

/// Replaces VS Code's `${…}` variables; one Athena does not know is an error, not left in place.
pub fn substitute(text: &str, ctx: &Context) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| format!("\"{text}\" has an unclosed ${{"))?;
        out.push_str(&variable(&after[..end], ctx)?);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn variable(name: &str, ctx: &Context) -> Result<String, String> {
    let show = |p: &Path| p.to_string_lossy().into_owned();
    let file = || {
        ctx.file
            .ok_or_else(|| format!("${{{name}}} needs a file open in the editor"))
    };
    let relative = |p: &Path| show(p.strip_prefix(ctx.workspace).unwrap_or(p));
    Ok(match name {
        "workspaceFolder" | "workspaceRoot" => show(ctx.workspace),
        "workspaceFolderBasename" => ctx
            .workspace
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        "file" => show(file()?),
        "fileDirname" => show(file()?.parent().unwrap_or(ctx.workspace)),
        "fileBasename" => file()?
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        "fileBasenameNoExtension" => file()?
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        "relativeFile" => relative(file()?),
        "relativeFileDirname" => relative(file()?.parent().unwrap_or(ctx.workspace)),
        "pathSeparator" => "/".into(),
        other => return Err(format!("${{{other}}} is not a variable Athena knows")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(workspace: &'a Path, file: Option<&'a Path>) -> Context<'a> {
        Context { workspace, file }
    }

    #[test]
    fn go_entries_are_read_and_others_skipped() {
        let launch = json!({"version": "0.2.0", "configurations": [
            {"name": "Node", "type": "node", "request": "launch"},
            {"name": "Server", "type": "go", "request": "launch", "mode": "debug",
             "program": "${workspaceFolder}/cmd/server", "args": ["-port", "8080"],
             "env": {"APP_ENV": "dev", "WORKERS": 4}, "buildFlags": "-tags=integration",
             "cwd": "${workspaceFolder}"},
            {"name": "Tests", "type": "go", "request": "launch", "mode": "test",
             "program": "${fileDirname}", "buildFlags": ["-race"]}
        ]});
        let configs = go_configurations(&launch).unwrap();
        assert_eq!(configs.len(), 2);
        let server = &configs[0];
        assert_eq!(server.mode, Mode::Debug);
        assert_eq!(server.args, ["-port", "8080"]);
        assert_eq!(
            server.env,
            [
                ("APP_ENV".into(), "dev".into()),
                ("WORKERS".into(), "4".into())
            ]
        );
        assert_eq!(server.build_flags, Some(json!("-tags=integration")));
        assert_eq!(configs[1].build_flags, Some(json!(["-race"])));
        assert!(go_configurations(&json!({})).unwrap().is_empty());
    }

    #[test]
    fn malformed_entries_say_which_field_is_wrong() {
        let bad = |c: Value| go_configurations(&json!({"configurations": [c]})).unwrap_err();
        let base = |extra: Value| {
            let mut c = json!({"name": "X", "type": "go"});
            c.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            c
        };
        assert!(bad(base(json!({"args": "-v"}))).contains("\"args\""));
        assert!(bad(base(json!({"mode": "remote"}))).contains("remote"));
        assert!(bad(base(json!({"env": {"A": [1]}}))).contains("\"env\""));
        assert!(bad(base(json!({"buildFlags": 3}))).contains("buildFlags"));
        assert!(go_configurations(&json!({"configurations": {}})).is_err());
    }

    #[test]
    fn variables_and_relative_paths_resolve_against_the_project_and_the_open_file() {
        let root = Path::new("/w/proj");
        let file = Path::new("/w/proj/pkg/util/strings_test.go");
        let c = ctx(root, Some(file));
        assert_eq!(
            substitute("${relativeFileDirname}:${fileBasenameNoExtension}", &c).unwrap(),
            "pkg/util:strings_test"
        );
        assert_eq!(
            substitute("${workspaceFolderBasename}", &c).unwrap(),
            "proj"
        );
        assert!(
            substitute("${env:HOME}", &c)
                .unwrap_err()
                .contains("env:HOME")
        );
        assert!(substitute("${file", &c).unwrap_err().contains("unclosed"));
        assert!(
            substitute("${file}", &ctx(root, None))
                .unwrap_err()
                .contains("file open")
        );

        let mut config = default_config();
        let launch = config.resolve(&c).unwrap();
        assert_eq!(
            launch.mode,
            Mode::Test,
            "auto picks test for a _test.go file"
        );
        assert_eq!(launch.program, Path::new("/w/proj/pkg/util"));
        config.program = "cmd/app/main.go".into();
        config.mode = Mode::Debug;
        config.args = vec!["--root=${workspaceFolder}".into()];
        let launch = config.resolve(&ctx(root, None)).unwrap();
        assert_eq!(launch.program, Path::new("/w/proj/cmd/app/main.go"));
        assert_eq!(launch.cwd, Path::new("/w/proj/cmd/app"));
        assert_eq!(launch.args, ["--root=/w/proj"]);
        config.request = "attach".into();
        assert!(config.resolve(&c).unwrap_err().contains("attach"));
    }

    #[test]
    fn delve_gets_mode_program_output_and_flags() {
        let launch = Launch {
            name: "x".into(),
            mode: Mode::Debug,
            program: "/w/p".into(),
            args: vec!["a".into()],
            env: vec![("K".into(), "v".into())],
            build_flags: Some(json!("-tags=x")),
            cwd: "/w/p".into(),
        };
        let args = launch.delve_arguments(Path::new("/tmp/s/bin"));
        assert_eq!(args["mode"], "debug");
        assert_eq!(args["output"], "/tmp/s/bin");
        assert_eq!(args["env"], json!({"K": "v"}));
        assert_eq!(args["buildFlags"], "-tags=x");
        assert_eq!(args["outputMode"], "remote");
        let test = Launch::test("TestAdd".into(), Path::new("/w/p/pkg"), "^TestAdd$".into());
        let args = test.delve_arguments(Path::new("/tmp/s/bin"));
        assert_eq!(args["mode"], "test");
        assert_eq!(args["args"], json!(["-test.run", "^TestAdd$"]));
        assert!(args.get("buildFlags").is_none());
    }
}
