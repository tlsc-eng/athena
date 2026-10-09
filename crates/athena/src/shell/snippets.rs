use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use athena_editor::{Completion, EditorView, HoverBlock, Lang};
use gpui::{Context, Entity, InteractiveElement as _, Window, actions};
use serde_json::Value;

use super::Shell;
use crate::settings::language_id;

actions!(athena, [ConfigureSnippets, ConfigureGlobalSnippets]);

/// The protocol's CompletionItemKind for a snippet.
const SNIPPET_KIND: u32 = 15;

/// What a new language snippets file holds; `LANGUAGE` is replaced by the language's name.
const LANGUAGE_TEMPLATE: &str = include_str!("../snippets-template.jsonc");
const GLOBAL_TEMPLATE: &str = include_str!("../snippets-global-template.jsonc");

/// One snippet of a VS Code snippets file.
#[derive(Clone, Debug, PartialEq, Eq)]
struct UserSnippet {
    name: String,
    prefixes: Vec<String>,
    body: String,
    description: Option<String>,
    /// The language ids a `.code-snippets` file limits it to; `None` for every language.
    scope: Option<Vec<String>>,
}

/// Where the user's snippet files live, beside settings.json.
fn snippets_dir() -> Option<PathBuf> {
    Some(athena_proto::data_dir().ok()?.join("snippets"))
}

/// A string or a list of strings, as VS Code takes a prefix, body or description.
fn strings(value: Option<&Value>) -> Option<Vec<String>> {
    match value? {
        Value::String(s) => Some(vec![s.clone()]),
        Value::Array(list) => list
            .iter()
            .map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => None,
    }
}

/// The snippets of a VS Code snippets file; an entry without a prefix or body is skipped.
fn parse_snippets(text: &str) -> Result<Vec<UserSnippet>, String> {
    let json = crate::keymap::strip_jsonc(text);
    let value: Value = serde_json::from_str(&json).map_err(|e| e.to_string())?;
    let Value::Object(map) = value else {
        return Err("a snippets file is an object of named snippets".into());
    };
    let mut out: Vec<UserSnippet> = map
        .iter()
        .filter_map(|(name, s)| {
            let prefixes = strings(s.get("prefix"))?;
            let body = strings(s.get("body"))?.join("\n");
            (!prefixes.is_empty()).then(|| UserSnippet {
                name: name.clone(),
                prefixes,
                body,
                description: strings(s.get("description")).map(|d| d.join("\n")),
                scope: s.get("scope").and_then(Value::as_str).map(|scope| {
                    scope
                        .split(',')
                        .map(|id| id.trim().to_string())
                        .filter(|id| !id.is_empty())
                        .collect()
                }),
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

type Parsed = Arc<Result<Vec<UserSnippet>, String>>;

/// Snippet files as last read, by path, with the modification time they were read at.
static FILES: Mutex<Option<HashMap<PathBuf, (SystemTime, Parsed)>>> = Mutex::new(None);

/// A snippets file, read again only once it has changed on disk.
fn read_file(path: &Path) -> Parsed {
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    let mut files = FILES.lock().unwrap_or_else(|e| e.into_inner());
    let files = files.get_or_insert_with(HashMap::new);
    if let (Some(modified), Some((at, parsed))) = (modified, files.get(path))
        && *at == modified
    {
        return parsed.clone();
    }
    let parsed: Parsed = Arc::new(match std::fs::read_to_string(path) {
        Ok(text) => parse_snippets(&text),
        Err(e) => Err(e.to_string()),
    });
    if let Some(modified) = modified {
        files.insert(path.to_path_buf(), (modified, parsed.clone()));
    }
    parsed
}

/// The snippets for files of `language`: its own file's, and those of every `.code-snippets`
/// file whose scope includes it or that has none.
fn snippets_for(dir: &Path, language: &str) -> Vec<UserSnippet> {
    let mut out = Vec::new();
    if let Ok(list) = &*read_file(&dir.join(format!("{language}.json"))) {
        out.extend(list.iter().cloned());
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut global: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "code-snippets"))
        .collect();
    global.sort();
    for path in global {
        if let Ok(list) = &*read_file(&path) {
            let applies = |s: &&UserSnippet| {
                s.scope
                    .as_ref()
                    .is_none_or(|ids| ids.iter().any(|id| id == language))
            };
            out.extend(list.iter().filter(applies).cloned());
        }
    }
    out
}

/// A broken-down local time, for the CURRENT_* variables.
struct LocalTime {
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
}

fn local_now() -> LocalTime {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs()) as libc::time_t;
    // SAFETY: localtime_r only writes the tm it is given.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm
    };
    LocalTime {
        year: tm.tm_year + 1900,
        month: (tm.tm_mon + 1) as u32,
        day: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
        second: tm.tm_sec as u32,
    }
}

/// What VS Code's snippet variables stand for where a snippet is inserted.
struct Variables {
    file: PathBuf,
    root: Option<PathBuf>,
    clipboard: String,
    now: LocalTime,
    line_comment: Option<&'static str>,
}

impl Variables {
    /// The value of the variable `name`; `None` for one Athena does not know, which the snippet
    /// then shows as its default text, as VS Code does for an unset one.
    fn get(&self, name: &str) -> Option<String> {
        let file_name = || {
            self.file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        };
        let text = |path: &Path| path.to_string_lossy().into_owned();
        let n = &self.now;
        Some(match name {
            "TM_FILENAME" => file_name()?,
            "TM_FILENAME_BASE" => {
                let name = file_name()?;
                match name.split_once('.') {
                    Some((base, _)) if !base.is_empty() => base.to_string(),
                    _ => name,
                }
            }
            "TM_DIRECTORY" => text(self.file.parent()?),
            "TM_FILEPATH" => text(&self.file),
            "RELATIVE_FILEPATH" => match &self.root {
                Some(root) => text(self.file.strip_prefix(root).ok()?),
                None => text(&self.file),
            },
            "WORKSPACE_NAME" => self.root.as_ref()?.file_name()?.to_string_lossy().into(),
            "WORKSPACE_FOLDER" => text(self.root.as_ref()?),
            "CLIPBOARD" => self.clipboard.clone(),
            "TM_SELECTED_TEXT" => String::new(),
            "CURRENT_YEAR" => n.year.to_string(),
            "CURRENT_YEAR_SHORT" => format!("{:02}", n.year.rem_euclid(100)),
            "CURRENT_MONTH" => format!("{:02}", n.month),
            "CURRENT_DATE" => format!("{:02}", n.day),
            "CURRENT_HOUR" => format!("{:02}", n.hour),
            "CURRENT_MINUTE" => format!("{:02}", n.minute),
            "CURRENT_SECOND" => format!("{:02}", n.second),
            "CURRENT_SECONDS_UNIX" => SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .ok()?
                .as_secs()
                .to_string(),
            "LINE_COMMENT" => self.line_comment?.to_string(),
            _ => return None,
        })
    }
}

/// `value` as literal snippet text, so its `$`, `}` and `\` are not read as syntax.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '$' | '}' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The text up to the unescaped `stop` from `i`, unescaping `\stop`; `i` ends past `stop`.
fn read_until(chars: &[char], i: &mut usize, stop: char) -> Option<String> {
    let mut out = String::new();
    while *i < chars.len() {
        match chars[*i] {
            '\\' if chars.get(*i + 1) == Some(&stop) => {
                out.push(stop);
                *i += 2;
            }
            c if c == stop => {
                *i += 1;
                return Some(out);
            }
            c => {
                out.push(c);
                *i += 1;
            }
        }
    }
    None
}

/// One piece of a transform's format string, as VS Code reads it.
#[derive(Debug, PartialEq)]
enum Format {
    Text(String),
    /// `$1`, `${1}`, `${1:/upcase}`, `${1:+if}`, `${1:-else}`, `${1:else}` or `${1:?if:else}`.
    Group {
        index: usize,
        case: Option<String>,
        set: Option<String>,
        unset: Option<String>,
    },
}

/// The text from `j` up to the unescaped `stop`, unescaping `\$`, `\}` and `\\`; another escape
/// or an empty text fails, as in VS Code's parser. Returns the text and the index past `stop`.
fn format_until(chars: &[char], mut j: usize, stop: char) -> Option<(String, usize)> {
    let mut out = String::new();
    loop {
        match *chars.get(j)? {
            '\\' => {
                let next = *chars.get(j + 1)?;
                if !matches!(next, '$' | '}' | '\\') {
                    return None;
                }
                out.push(next);
                j += 2;
            }
            c if c == stop => return (!out.is_empty()).then_some((out, j + 1)),
            c => {
                out.push(c);
                j += 1;
            }
        }
    }
}

/// The group reference starting with the `$` at `j`, and the index past it.
fn format_group(chars: &[char], j: usize) -> Option<(Format, usize)> {
    let braced = chars.get(j + 1) == Some(&'{');
    let mut k = j + 1 + usize::from(braced);
    let digits = chars[k.min(chars.len())..]
        .iter()
        .take_while(|c| c.is_ascii_digit())
        .count();
    let index = chars[k..k + digits]
        .iter()
        .collect::<String>()
        .parse()
        .ok()?;
    k += digits;
    let group = |case, set, unset| Format::Group {
        index,
        case,
        set,
        unset,
    };
    if !braced {
        return Some((group(None, None, None), k));
    }
    match chars.get(k)? {
        '}' => return Some((group(None, None, None), k + 1)),
        ':' => k += 1,
        _ => return None,
    }
    match chars.get(k)? {
        '/' => {
            let len = chars[k + 1..]
                .iter()
                .take_while(|c| c.is_ascii_alphanumeric() || **c == '_')
                .count();
            let name: String = chars[k + 1..k + 1 + len].iter().collect();
            let valid = name.starts_with(|c: char| !c.is_ascii_digit());
            (valid && chars.get(k + 1 + len) == Some(&'}'))
                .then(|| (group(Some(name), None, None), k + len + 2))
        }
        '+' => {
            format_until(chars, k + 1, '}').map(|(set, end)| (group(None, Some(set), None), end))
        }
        '-' => format_until(chars, k + 1, '}')
            .map(|(unset, end)| (group(None, None, Some(unset)), end)),
        '?' => {
            let (set, k) = format_until(chars, k + 1, ':')?;
            let (unset, end) = format_until(chars, k, '}')?;
            Some((group(None, Some(set), Some(unset)), end))
        }
        _ => format_until(chars, k, '}').map(|(unset, end)| (group(None, None, Some(unset)), end)),
    }
}

/// A transform's format string up to its closing `/`, from `i`; `i` ends past the `/`.
fn parse_format(chars: &[char], i: &mut usize) -> Option<Vec<Format>> {
    let mut out = Vec::new();
    let mut text = String::new();
    loop {
        match *chars.get(*i)? {
            '/' => {
                *i += 1;
                break;
            }
            '\\' if matches!(chars.get(*i + 1), Some('\\' | '/')) => {
                text.push(chars[*i + 1]);
                *i += 2;
            }
            '$' => match format_group(chars, *i) {
                Some((group, next)) => {
                    out.push(Format::Text(std::mem::take(&mut text)));
                    out.push(group);
                    *i = next;
                }
                None => {
                    text.push('$');
                    *i += 1;
                }
            },
            c => {
                text.push(c);
                *i += 1;
            }
        }
    }
    out.push(Format::Text(text));
    Some(out)
}

/// The words VS Code's pascalcase and camelcase join.
fn case_words(value: &str) -> Vec<&str> {
    value
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect()
}

fn capitalized(word: &str) -> String {
    let mut chars = word.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// A group's text as `case` (`upcase`, `capitalize`, `pascalcase`...) shapes it.
fn shaped(value: &str, case: &str) -> String {
    let words = case_words(value);
    match case {
        "upcase" => value.to_uppercase(),
        "downcase" => value.to_lowercase(),
        "capitalize" => capitalized(value),
        "pascalcase" | "camelcase" if words.is_empty() => value.to_string(),
        "pascalcase" => words.into_iter().map(capitalized).collect(),
        "camelcase" => words
            .into_iter()
            .enumerate()
            .map(|(i, w)| match i {
                0 => {
                    let mut chars = w.chars();
                    chars.next().map_or_else(String::new, |first| {
                        first.to_lowercase().chain(chars).collect()
                    })
                }
                _ => capitalized(w),
            })
            .collect(),
        _ => value.to_string(),
    }
}

/// The format with each group filled from `groups`, as VS Code's `Transform` does.
fn formatted(format: &[Format], groups: Option<&regex::Captures>) -> String {
    let mut out = String::new();
    for part in format {
        match part {
            Format::Text(text) => out.push_str(text),
            Format::Group {
                index,
                case,
                set,
                unset,
            } => {
                let value = groups
                    .and_then(|g| g.get(*index))
                    .map_or("", |m| m.as_str());
                match (case, set, unset) {
                    (Some(case), ..) => out.push_str(&shaped(value, case)),
                    (_, Some(set), _) if !value.is_empty() => out.push_str(set),
                    (_, _, Some(unset)) if value.is_empty() => out.push_str(unset),
                    _ => out.push_str(value),
                }
            }
        }
    }
    out
}

/// `${NAME/regex/format/options}`: the value with `regex` replaced by `format`, read as VS Code
/// reads it (`$1`, `${1:/upcase}`, `${1:?if:else}`...), every match with the `g` option and
/// `i`, `m` and `s` as in JavaScript.
fn transform(value: &str, chars: &[char], i: &mut usize) -> Option<String> {
    let pattern = read_until(chars, i, '/')?;
    let format = parse_format(chars, i)?;
    let options = read_until(chars, i, '}')?;
    let mut re = regex::RegexBuilder::new(&pattern);
    let mut every = false;
    for option in options.chars() {
        match option {
            'g' => every = true,
            'i' => {
                re.case_insensitive(true);
            }
            'm' => {
                re.multi_line(true);
            }
            's' => {
                re.dot_matches_new_line(true);
            }
            'u' => {}
            _ => return None,
        }
    }
    let re = re.build().ok()?;
    let has_else = format
        .iter()
        .any(|f| matches!(f, Format::Group { unset: Some(_), .. }));
    if !re.is_match(value) && has_else {
        return Some(formatted(&format, None));
    }
    let limit = if every { 0 } else { 1 };
    Some(
        re.replacen(value, limit, |caps: &regex::Captures| {
            formatted(&format, Some(caps))
        })
        .into_owned(),
    )
}

/// Where the `}` closing a `${NAME:default}` whose default starts at `j` is.
fn closing_brace(chars: &[char], mut j: usize) -> Option<usize> {
    let mut depth = 0;
    while j < chars.len() {
        match chars[j] {
            '\\' => j += 1,
            '{' => depth += 1,
            '}' if depth == 0 => return Some(j),
            '}' => depth -= 1,
            _ => {}
        }
        j += 1;
    }
    None
}

/// Where the `${1|a,b|}` choice at `i` ends; its options are literal text, as in VS Code.
fn choice_end(chars: &[char], i: usize) -> Option<usize> {
    if chars.get(i) != Some(&'$') || chars.get(i + 1) != Some(&'{') {
        return None;
    }
    let digits = chars[i + 2..]
        .iter()
        .take_while(|c| c.is_ascii_digit())
        .count();
    let mut j = i + 2 + digits;
    if digits == 0 || chars.get(j) != Some(&'|') {
        return None;
    }
    j += 1;
    while j < chars.len() {
        match chars[j] {
            '\\' => j += 2,
            '|' if chars.get(j + 1) == Some(&'}') => return Some(j + 2),
            _ => j += 1,
        }
    }
    None
}

/// `body` with the variables `get` knows put in as literal text; tab stops, unknown variables
/// and escapes are left for the snippet parser.
fn resolve_variables(body: &str, get: impl Fn(&str) -> Option<String>) -> String {
    let chars: Vec<char> = body.chars().collect();
    let mut out = String::with_capacity(body.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() {
            out.push(c);
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if let Some(end) = choice_end(&chars, i) {
            out.extend(&chars[i..end]);
            i = end;
            continue;
        }
        let braced = chars.get(i + 1) == Some(&'{');
        let start = i + 1 + usize::from(braced);
        let len = chars
            .get(start..)
            .unwrap_or_default()
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric() || **c == '_')
            .count();
        let value = match chars.get(start) {
            Some(first) if c == '$' && len > 0 && !first.is_ascii_digit() => {
                get(&chars[start..start + len].iter().collect::<String>())
            }
            _ => None,
        };
        let after = start + len;
        // A known but empty variable takes its default, which the snippet parser reads.
        let resolved = match (value, braced, chars.get(after)) {
            (Some(value), false, _) => Some((escape(&value), after)),
            (Some(value), true, Some('}')) => Some((escape(&value), after + 1)),
            (Some(value), true, Some(':')) if !value.is_empty() => {
                closing_brace(&chars, after + 1).map(|end| (escape(&value), end + 1))
            }
            (Some(value), true, Some('/')) => {
                let mut end = after + 1;
                transform(&value, &chars, &mut end).map(|v| (escape(&v), end))
            }
            _ => None,
        };
        match resolved {
            Some((text, next)) => {
                out.push_str(&text);
                i = next;
            }
            None => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// A suggestion for each of the snippet's prefixes, its body expanded with `vars`.
fn completions(snippet: &UserSnippet, vars: &Variables) -> Vec<Completion> {
    let body = resolve_variables(&snippet.body, |name| vars.get(name));
    let (text, stops) = athena_lsp::snippet_stops(&body);
    let (_, select) = athena_lsp::expand_snippet(&body);
    let detail = snippet
        .description
        .clone()
        .unwrap_or_else(|| snippet.name.clone());
    snippet
        .prefixes
        .iter()
        .map(|prefix| Completion {
            label: prefix.clone(),
            kind: Some(SNIPPET_KIND),
            detail: Some(detail.clone()),
            filter_text: prefix.clone(),
            sort_text: prefix.clone(),
            text: text.clone(),
            range: None,
            select: select.clone(),
            stops: stops.clone(),
            additional_edits: Vec::new(),
            preselect: false,
            documentation: vec![HoverBlock::Code(text.clone())],
            resolve: false,
        })
        .collect()
}

/// Lets typing in an editor of a language no server runs for bring up the user's snippets,
/// where it has any; Markdown keeps them to Ctrl+Space, as VS Code does.
pub(super) fn attach_snippets(editor: &Entity<EditorView>, cx: &mut Context<Shell>) {
    let Some(lang) = editor.read(cx).lang().filter(|l| *l != Lang::Markdown) else {
        return;
    };
    let Some(dir) = snippets_dir() else {
        return;
    };
    if !snippets_for(&dir, language_id(lang)).is_empty() {
        editor.update(cx, |e, _| e.offer_snippets());
    }
}

impl Shell {
    /// The user's snippets for `editor`'s language, as suggestions, with their variables filled
    /// in for its file.
    pub(super) fn snippet_completions(
        &self,
        editor: &Entity<EditorView>,
        cx: &mut Context<Self>,
    ) -> Vec<Completion> {
        let Some(dir) = snippets_dir() else {
            return Vec::new();
        };
        let (path, lang) = {
            let e = editor.read(cx);
            (e.path().to_path_buf(), e.lang())
        };
        let snippets = snippets_for(&dir, lang.map_or("plaintext", language_id));
        if snippets.is_empty() {
            return Vec::new();
        }
        let vars = Variables {
            root: self.project_root_of(&path),
            file: path,
            clipboard: cx
                .read_from_clipboard()
                .and_then(|c| c.text())
                .unwrap_or_default(),
            now: local_now(),
            line_comment: lang.and_then(Lang::comment_prefix),
        };
        snippets
            .iter()
            .flat_map(|s| completions(s, &vars))
            .collect()
    }

    /// After a snippets file is saved: says what is wrong with it, or lets editors of its
    /// languages offer its snippets as you type.
    pub(super) fn snippets_saved(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(dir) = snippets_dir() else {
            return;
        };
        let parent = path.parent().and_then(|p| p.canonicalize().ok());
        if parent.is_none() || parent != dir.canonicalize().ok() {
            return;
        }
        if let Err(why) = &*read_file(path) {
            let name = path
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            self.transient_notice(format!("{name} could not be read"), why.clone(), cx);
            return;
        }
        let editors: Vec<Entity<EditorView>> = self
            .items
            .values()
            .filter_map(|view| match view {
                super::item::ItemView::Editor(e) => Some(e.clone()),
                _ => None,
            })
            .collect();
        for editor in editors {
            attach_snippets(&editor, cx);
        }
    }

    /// Opens the user's snippets file for the active editor's language, or the global one,
    /// creating it with an example first.
    pub(super) fn configure_snippets(
        &mut self,
        global: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let lang = self
            .focused_editor()
            .and_then(|e| e.read(cx).lang())
            .filter(|_| !global);
        let Some(dir) = snippets_dir() else {
            return;
        };
        let (path, template) = match lang {
            Some(lang) => (
                dir.join(format!("{}.json", language_id(lang))),
                LANGUAGE_TEMPLATE.replace("LANGUAGE", &format!("{lang:?}")),
            ),
            None => (dir.join("global.code-snippets"), GLOBAL_TEMPLATE.into()),
        };
        let created = match path.exists() {
            true => Ok(()),
            false => std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, template)),
        };
        match created {
            Err(e) => {
                self.transient_notice("Could not create the snippets file", e.to_string(), cx)
            }
            Ok(()) if self.workspace.active.is_some() => self.open_file(path, window, cx),
            Ok(()) => self.transient_notice(
                "Open a project to edit snippets",
                format!(
                    "Snippet files open as a tab in a project: {}",
                    path.display()
                ),
                cx,
            ),
        }
    }
}

/// Binds the snippet commands on the shell's root element.
pub(super) fn bind_snippet_actions(el: gpui::Div, cx: &mut Context<Shell>) -> gpui::Div {
    el.on_action(cx.listener(|this, _: &ConfigureSnippets, window, cx| {
        this.configure_snippets(false, window, cx)
    }))
    .on_action(
        cx.listener(|this, _: &ConfigureGlobalSnippets, window, cx| {
            this.configure_snippets(true, window, cx)
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Variables {
        Variables {
            file: "/work/app/src/main.test.go".into(),
            root: Some("/work/app".into()),
            clipboard: "cost $5 {x}".into(),
            now: LocalTime {
                year: 2026,
                month: 3,
                day: 7,
                hour: 9,
                minute: 5,
                second: 0,
            },
            line_comment: Some("//"),
        }
    }

    #[test]
    fn snippet_files_read_vs_codes_shapes_with_comments() {
        let list = parse_snippets(
            r#"{
              // a comment
              "Print": {"prefix": ["log", "print"], "body": ["fmt.Println($1)", "$0"],
                         "description": "Print a line"},
              "Header": {"prefix": "hdr", "body": "// $TM_FILENAME", "scope": "go, typescript"},
              "Broken": {"body": "no prefix"},
              "Empty": {"prefix": [], "body": "x"},
            }"#,
        )
        .unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].name, "Print");
        assert_eq!(list[1].prefixes, ["log", "print"]);
        assert_eq!(list[1].body, "fmt.Println($1)\n$0");
        assert_eq!(list[1].description.as_deref(), Some("Print a line"));
        let scope = list[0].scope.clone().unwrap();
        assert_eq!(scope, ["go", "typescript"]);
        assert!(parse_snippets("[1]").is_err());
        assert!(parse_snippets("{").is_err());
    }

    #[test]
    fn variables_become_literal_text_and_leave_tab_stops_alone() {
        let v = vars();
        let get = |name: &str| v.get(name);
        let r = |body: &str| resolve_variables(body, get);
        assert_eq!(
            r("// $TM_FILENAME ${CURRENT_YEAR}-$CURRENT_MONTH-$CURRENT_DATE"),
            "// main.test.go 2026-03-07"
        );
        assert_eq!(
            r("$TM_FILENAME_BASE ${RELATIVE_FILEPATH}"),
            "main src/main.test.go"
        );
        assert_eq!(r("$CLIPBOARD"), "cost \\$5 {x\\}", "escaped for the parser");
        assert_eq!(r("${1:$CLIPBOARD} $0"), "${1:cost \\$5 {x\\}} $0");
        assert_eq!(
            r("\\$TM_FILENAME $UNKNOWN ${UNKNOWN:d}"),
            "\\$TM_FILENAME $UNKNOWN ${UNKNOWN:d}"
        );
        assert_eq!(r("${TM_SELECTED_TEXT:none}"), "${TM_SELECTED_TEXT:none}");
        assert_eq!(r("${WORKSPACE_NAME:x}!"), "app!");
        assert_eq!(r("${TM_FILENAME/(.*)\\..+$/$1/}"), "main.test");
        assert_eq!(r("${TM_FILENAME/[.]/_/g}"), "main_test_go");
        assert_eq!(r("$CURRENT_YEAR_SHORT $LINE_COMMENT"), "26 //");
        assert_eq!(r("cost: $"), "cost: $");
    }

    #[test]
    fn transforms_read_vs_code_format_strings_and_regex_options() {
        let v = vars();
        let r = |body: &str| resolve_variables(body, |name| v.get(name));
        assert_eq!(r("${TM_FILENAME_BASE/(.*)/$1_test/}"), "main_test");
        assert_eq!(r("${TM_FILENAME_BASE/(.*)/${1}_test/}"), "main_test");
        assert_eq!(r("${TM_FILENAME/(\\w+).*/${1:/upcase}/}"), "MAIN");
        assert_eq!(r("${TM_FILENAME/(.*)/${1:/pascalcase}/}"), "MainTestGo");
        assert_eq!(r("${TM_FILENAME/(.*)/${1:/camelcase}/}"), "mainTestGo");
        assert_eq!(r("${TM_FILENAME/(m)/${1:/capitalize}/}"), "Main.test.go");
        assert_eq!(r("${TM_FILENAME/MAIN/x/i}"), "x.test.go");
        assert_eq!(r("${TM_FILENAME/[.](\\w)/_${1:/upcase}/g}"), "main_Test_Go");
        assert_eq!(
            r("${TM_FILENAME/(test)|(x)/${1:+T}${2:-none}/}"),
            "main.Tnone.go"
        );
        assert_eq!(r("${TM_FILENAME/(go)$/${1:?yes:no}/}"), "main.test.yes");
        assert_eq!(
            r("${TM_FILENAME/^(zz)?$/${1:?yes:no}/}"),
            "no",
            "else on no match"
        );
        assert_eq!(
            r("${TM_FILENAME/main/a\\/b\\\\c\\$/}"),
            "a/b\\\\c\\\\\\$.test.go"
        );
        assert_eq!(r("${TM_FILENAME/main/$x/}"), "\\$x.test.go");
        assert_eq!(r("${CLIPBOARD/^c/C/m}"), "Cost \\$5 {x\\}");
        assert_eq!(
            r("${TM_FILENAME/(/x/}"),
            "${TM_FILENAME/(/x/}",
            "a bad regex is left for the snippet parser"
        );
    }

    #[test]
    fn a_choice_keeps_its_options_as_written() {
        let v = vars();
        let r = |body: &str| resolve_variables(body, |name| v.get(name));
        assert_eq!(
            r("${1|$CLIPBOARD,b|} $CLIPBOARD"),
            "${1|$CLIPBOARD,b|} cost \\$5 {x\\}"
        );
    }

    #[test]
    fn a_snippet_becomes_one_suggestion_per_prefix_with_its_stops() {
        let snippet = UserSnippet {
            name: "Header".into(),
            prefixes: vec!["hdr".into(), "header".into()],
            body: "$LINE_COMMENT ${1:Title} for $TM_FILENAME\n$CLIPBOARD$0".into(),
            description: None,
            scope: None,
        };
        let items = completions(&snippet, &vars());
        assert_eq!(items.len(), 2);
        let item = &items[1];
        assert_eq!(item.label, "header");
        assert_eq!(item.kind, Some(SNIPPET_KIND));
        assert_eq!(item.detail.as_deref(), Some("Header"));
        assert_eq!(item.text, "// Title for main.test.go\ncost $5 {x}");
        assert_eq!(item.select, Some(3..8), "the first placeholder is selected");
        let end = item.text.chars().count();
        assert!(item.stops.iter().any(|(n, r)| *n == 0 && r.start == end));
    }

    #[test]
    fn language_files_and_scoped_global_files_both_apply() {
        let dir = std::env::temp_dir().join(format!("athena-snippets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let go = dir.join("go.json");
        std::fs::write(
            &go,
            r#"{"Err": {"prefix": "iferr", "body": "if err != nil {\n\t$0\n}"}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("global.code-snippets"),
            r#"{"Go only": {"scope": "go", "prefix": "g", "body": "g"},
                "TS only": {"scope": "typescript", "prefix": "t", "body": "t"},
                "Any": {"prefix": "a", "body": "a"}}"#,
        )
        .unwrap();
        let names = |lang: &str| -> Vec<String> {
            snippets_for(&dir, lang)
                .into_iter()
                .map(|s| s.name)
                .collect()
        };
        assert_eq!(names("go"), ["Err", "Any", "Go only"]);
        assert_eq!(names("typescript"), ["Any", "TS only"]);
        std::fs::write(&go, "{ broken").unwrap();
        let later = SystemTime::now() + std::time::Duration::from_secs(5);
        let file = std::fs::File::options().write(true).open(&go).unwrap();
        file.set_modified(later).unwrap();
        assert_eq!(
            names("go"),
            ["Any", "Go only"],
            "a changed file is read again; a broken one is skipped"
        );
        assert!(read_file(&go).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
