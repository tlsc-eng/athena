# Athena

Athena is a macOS IDE built around terminals that outlive the window and around Claude Code.
Shells run in a separate session daemon (`athena-mux`), so quitting or updating the app does not
kill them; the window reattaches on the next launch. Projects get split panes of terminals, an
editor with language-server support, a git diff viewer with hunk staging and a commit box, a
browser preview, and read-only views of Playwright results and Docker containers. Claude Code
sessions show their state on the tab, each file Claude edits can be reviewed as one diff against
the version from before the session, and an MCP server lets Claude read the editor and terminals.

It is written in Rust on [GPUI](https://crates.io/crates/gpui) and runs only on Apple silicon.

![Athena with a project open](docs/screenshots/overview.png)

## Install

Requires Apple silicon and macOS 26 (Tahoe).

```sh
brew tap tlsc-eng/athena
brew install --cask athena
```

The cask installs `Athena.app` and links the `athena` command onto your `PATH`. The app is
ad-hoc signed without a Developer ID, so the cask removes the quarantine attribute after install;
a copy downloaded by hand from the releases page is refused by Gatekeeper until you do the same
(`xattr -dr com.apple.quarantine /Applications/Athena.app`).

`brew uninstall --zap --cask athena` also stops the session daemon (its shells are hung up) and
deletes `~/Library/Application Support/athena`. A plain uninstall or upgrade leaves the daemon and
its shells running.

## Features

**Terminals**

![Terminals in split panes](docs/screenshots/terminal.png)

- Shell sessions are owned by `athena-mux`, which keeps running when the window closes. Restored
  tabs reattach to the same shells with their scrollback.
- Split panes right and down, zoom a pane, move focus between panes with the keyboard.
- zsh shell integration (OSC 133) marks commands; one that runs longer than 10 seconds posts a
  notification when it finishes. `ATHENA_NOTIFY_AFTER_SECS` changes the threshold and
  `ATHENA_SHELL_INTEGRATION=0` turns the integration off; set them in the environment the daemon
  starts from, e.g. `open -a Athena --env ATHENA_NOTIFY_AFTER_SECS=30`.
- With those marks each prompt gets a dot in the left margin (the success colour, red after a
  non-zero exit, an outline while the command runs); `cmd-up` and `cmd-down` scroll the previous
  or next prompt to the top, and **Copy Last Command Output** (right-click menu or command
  palette) copies what the last command printed. Other shells can send the same marks; for bash,
  add this to `~/.bashrc` (VS Code's OSC 633 spelling works too):

  ```bash
  if [[ "$TERM_PROGRAM" == "athena" ]]; then
    __athena_preexec() {
      [[ $__athena_quiet == 1 || $BASH_COMMAND == __athena_* ]] && return
      __athena_quiet=1 __athena_ran=1
      printf '\e]133;C\a'
    }
    __athena_precmd() {
      [[ $__athena_ran == 1 ]] && printf '\e]133;D;%d\a' "$__athena_status"
      __athena_quiet=0 __athena_ran=0
      printf '\e]133;A\a'
    }
    PROMPT_COMMAND="__athena_status=\$?;__athena_quiet=1;${PROMPT_COMMAND:+$PROMPT_COMMAND;}__athena_precmd"
    trap '__athena_preexec' DEBUG
  fi
  ```
- If a new version cannot attach to the shells of an older session daemon, their tabs are marked
  **stale**: **Restart sessions** ends those shells and starts new ones, **Keep** leaves them
  running and reconnects once the old daemon exits.
- Tabs take the title programs set (OSC 0/2), so a Claude Code tab shows its session name; the
  window title follows the active tab.
- Find in the terminal (`cmd-f`): searches scrollback and screen, highlights every visible match,
  shows "3 of 17", with match-case and regex toggles (both off by default).
- Programs that ask for the mouse (vim, htop, lazygit, tmux) get clicks, drags and the wheel;
  hold Shift to select text instead, Cmd still opens links.
- `cmd-k` clears scrollback; under a running program (a dev server, `tail -f`) it also clears the
  screen above the cursor line instead of sending ^L to that program.
- File references in the output are links: hold Cmd to underline one, Cmd+click to open the file
  in the editor at that line and column. This covers `go build`/`vet`/`test`, `tsc` (plain and
  `--pretty`), eslint headers, cargo and rustc arrows, Rust panics, Python tracebacks and Claude
  Code's own `path:line` mentions, plus OSC 8 `file://` hyperlinks to regular files. Paths resolve
  against the shell's current folder, then the project; a bare relative name (Go test output is
  relative to the package) is looked up in the project, and Go to file opens when several files
  match.

**Editor**

![The editor with a diagnostic and the References drawer](docs/screenshots/editor.png)

- Syntax highlighting for 16 languages (see [Languages](#languages)), each token class in its own
  colour, and the bracket matching the one at the cursor highlighted.
- Find and replace in file (`cmd-f`, `cmd-alt-f` for the replace row): Enter in the replace field
  replaces the current match and moves on, `cmd-enter` or **Replace All** replaces every match,
  each as one undo step. Toggle comment, undo/redo, go to line (`line`, `line:column` or
  `line,column`).
- Indenting: Tab on a selection spanning lines indents them, Shift+Tab, `cmd-]` and `cmd-[`
  indent and outdent to the next tab stop in the file's style, skipping empty lines. Tab on a
  selection inside one line still replaces it, as in VS Code.
- Brackets and the language's quotes close themselves before blanks and closers, but not after a
  word (apostrophes) or inside strings and comments. Typing the closer steps over one that was
  inserted for you, Backspace right after an inserted pair removes both halves, and typing an
  opener with a selection surrounds it. Enter between a bracket pair opens an indented line.
- Line operations: move lines up or down, copy them up or down, delete them, insert a line below
  (keys under [Keyboard shortcuts](#keyboard-shortcuts)); Insert line above is in the palette
  only.
- Indentation guides, one per indent step (blank lines continue the block's guides; offside for
  Python and YAML), with the guide of the cursor's block drawn brighter. Spaces and tabs inside the
  selection are shown as dots and arrows.
- Zoom editor and terminal text together with `cmd-=` / `cmd--` / `cmd-0` (1 px steps, 6 to 40 px,
  default 13 px; also in the View menu). The zoom is saved with the workspace. The rest of the
  interface keeps its size.
- A status bar under the panes shows the project's branch (click: switch branch) and, for the
  focused editor, `Ln N, Col M` with the selected character count (click: go to line), the
  indentation (click: indent with 2, 4 or 8 spaces or tabs, or convert the file's indentation),
  `UTF-8`, `LF` or `CRLF`, the language (click: highlight as another language) and a dot for its
  language server (starting, running or failed; hover for the program).
- Code folding by brackets, falling back to indentation: chevrons in the gutter on hover, fold
  and unfold at the cursor or everywhere with the `cmd-k` chords below.
- With a language server: diagnostics, go to definition, find references (listed in a drawer
  tab; clicking a row opens the file at that line), hover docs (rest the pointer on a word for
  half a second), completion as you type (Up/Down to move, Enter or Tab to accept, Escape to
  close; accepting can also add an import) and signature help while typing call arguments.
- Also with a language server: rename symbol (`f2` opens a field over the symbol with the old name
  selected; Enter renames it in every file, open files as one undo step each and closed files saved
  to disk), quick fixes and refactorings (`cmd-.` lists them in a menu at the cursor, preferred
  fixes first), go to implementation (`cmd-f12`) and go to type definition (editor menu and
  palette). A dot in the gutter marks the cursor's line when a diagnostic there has a quick fix;
  clicking it opens the same menu. Several implementations are listed in the References tab.
- Go to symbol in the file (`cmd-shift-o`, or `@` in Go to file), previewing each one as the
  selection moves and going back on Escape, and in the workspace (`cmd-alt-o`, or `#`). `>` in Go
  to file switches to commands.
- A **Problems** drawer tab lists the project's errors, warnings and infos grouped by file, errors
  first, with a count on the tab; clicking a row opens the file there. `f8` / `shift-f8` in an
  editor step to the next or previous problem across files, and `cmd-shift-m` or the
  error/warning counter in the title bar toggles the tab.
- Format on save: `cmd-s` asks the language server to format the file first, by default in Go
  files only; for Go it also organizes imports. "Toggle format on save" (palette, File menu) turns
  it on or off for every language with a server. Auto save does not format.
- Go to file (fuzzy) and a command palette. A file opens in the editor pane used last, or in a
  new pane beside the focused one with Cmd+click in the tree or Cmd+Enter in Go to file.
- Two tabs on the same file share one buffer: edits, undo and the unsaved marker are the file's;
  each tab keeps its own cursor, scroll and folds.
- Auto save one second after you stop typing (palette: "Toggle auto save"), unsaved markers on
  tabs, tree rows and the window title, Save As, and a Reload / Overwrite bar when a file changes
  on disk while you have unsaved edits (unchanged files just reload). Project folders are
  watched, so changes made outside Athena show up without switching windows.
- Markdown and Mermaid preview (`cmd-shift-v`) for `.md`, `.markdown`, `.mdx`, `.mmd` and
  `.mermaid`: tables, task lists, local images and links, ```` ```mermaid ```` blocks. It follows
  unsaved edits as you type and re-renders on save.
- Images (PNG, JPEG, GIF, WebP, BMP, TIFF, ICO, SVG) open in a viewer that fits and zooms.
- File-type icons from [seti-ui](https://github.com/jesseweed/seti-ui) in the tree and on tabs.

**Git**

Needs the Xcode command line tools (Athena runs `/usr/bin/git`; without the tools it shows no git
information rather than triggering the install dialog).

- Tree rows and tab labels take the file's status colour (modified, added, untracked, deleted,
  conflicted, ignored), tree rows also show the status letter, and folders take the most severe
  status inside them.
- The editor gutter marks added, modified and removed lines against `HEAD` (from the saved file).
- Inline blame (`cmd-alt-shift-g`, off by default): "author · 3 days ago · summary" after the
  cursor's line, "Not committed yet" for edited lines.
- A **Changes** drawer tab lists Staged Changes, Changes and Untracked, with Stage / Unstage on
  each row and group (Stage All and Discard All leave conflicted files alone). Hovering a row also
  offers Open File and Discard.
- Clicking a row opens its diff in a tab: a staged row compares `HEAD` with the index ("main.go
  (Index)"), an unstaged or untracked one the index with the file on disk ("main.go (Working
  Tree)"); a conflicted file opens in the editor instead. Diffs are read-only, side by side or
  inline (toolbar switch), syntax highlighted, with changed words marked inside changed lines.
  `alt-f5` / `shift-alt-f5` step through the changes. Above each change, **Stage**, **Unstage** or
  **Revert** acts on that change alone; a staging action is refused if the index changed since
  the diff was made, and Revert refuses a file that changed since. Open diffs reload when git
  status is re-read. Binary, non-UTF-8 and files over 20 MB show why there is no diff.
- A commit box above the list: type a one-line message and press `cmd-enter` or **Commit**.
  **Amend** fills in the last commit's subject, keeps its body, and refuses to commit if `HEAD`
  moved since. With nothing staged it offers to stage everything and commit. **Discard** asks
  first: tracked files go back to their staged or committed version, untracked ones move to the
  Trash. Discard and Revert keep a copy of the replaced file in
  `~/Library/Application Support/athena/discarded` for 30 days.
- **Switch branch…** (palette, the branch button by the commit box, or the branch in the status
  bar) lists local then remote branches with their age and last subject. Typing a new name offers
  to create it; a remote branch without a local one is checked out tracking it. Switching is never
  forced, so git's refusal over local changes is shown as is.
- Status is re-read every 5 seconds while the window is in front and shortly after a save, a file
  operation or a change on disk. The title bar and the status bar show the branch.

**Workspace**

- Find and replace in project (`cmd-shift-f`): a Search drawer tab with a literal, smart-case
  search (case matters once the query has a capital), honouring `.gitignore` and skipping binary
  files and files over 1 MB; up to 2000 matches. Up/Down walk the matches, Enter opens one.
  **Replace All** asks first, then replaces in open editors (one undo step each) and on disk.
- Right-click menus. Tree: New File, New Folder, Rename, Delete (to the Trash), Reveal in Finder,
  Copy Path, Copy Relative Path, Open to the Side. Tabs: Close, Close Others, Close to the Right,
  Close All, Reveal in Finder, Copy Path, Copy Relative Path, Reveal in File Tree, Split Right /
  Down with that tab. Editor: Go to Definition, Find References, Go to Implementations, Go to Type
  Definition, Rename Symbol, Quick Fix…, Cut, Copy, Paste, Toggle Line Comment. Terminal: Copy,
  Paste, Select All, Find, Clear.
- Drag a tab onto another tab or strip to move it, onto the middle of a pane to join it, or onto
  an edge of a pane to split it there; the terminal or editor keeps running. Drop a folder from
  Finder to open it as a project, a file to open it in a tab.
- Back and forward through visited tabs and cursor positions (mouse buttons 4 and 5, `ctrl--` /
  `ctrl-shift--`, View menu); in a browser preview they walk the page's history.
- The tab strip scrolls sideways when tabs overflow; a middle click closes a tab.
- Drag the file tree's right edge or the drawer's top edge to resize them (double-click restores
  240 px); double-click a divider between panes to split the space evenly. Sizes are saved.
- Panels, tabs, panes, the palette, menus, toasts and find bars fade in and out; with Reduce
  Motion on they appear at once.

**Claude Code**

- A new Claude session opens in its own terminal tab; the command that starts Claude is asked
  once per project.
- Tabs show whether Claude is working or waiting for input. With the project's hooks enabled,
  Athena also posts a notification when a session finishes or needs you.
- An MCP server, `athena mcp-stdio`, answers from the running window (see below).
- With the project's hooks enabled, each file Claude edits posts a toast ("Claude edited main.go")
  whose **Review diff** opens the file's changes since before the session's first edit to it (see
  [Reviewing Claude's edits](#reviewing-claudes-edits)).
- Optional title-bar indicator for Claude plan usage (5-hour and weekly windows).

**Also**
- Browser preview pane (WKWebView); `3000` or `localhost:3000` opens the local dev server.
- Playwright: run a project's tests, list failures from the JSON report, open traces, and
  optionally add the Playwright MCP server for Claude.
- Containers: a read-only view of the local Docker engine (Docker Desktop or Rancher Desktop):
  list, stats and logs. Athena never starts, stops or removes containers.
- Workspace layout, open projects and tabs are restored on launch, with each editor's cursor
  where it was left.
- Open Recent (`ctrl-r` outside a terminal, or File > Open Recent) lists the last 20 project
  folders you closed.
- Light and dark themes. By default Athena follows the macOS appearance and switches with it;
  View > Theme or the palette's **Theme:** commands pin light or dark. Both themes cover the
  interface, code, the terminal's 16 colours (tuned so Claude Code stays readable) and Markdown
  previews.

## Languages

| Language | Files | Language server |
|---|---|---|
| Go | `.go` | `gopls` |
| TypeScript, TSX | `.ts`, `.mts`, `.cts`, `.tsx` | `typescript-language-server` |
| JavaScript | `.js`, `.mjs`, `.cjs`, `.jsx` | `typescript-language-server` |
| YAML | `.yaml`, `.yml` | |
| JSON | `.json`, `.jsonc`, `.json5`, `.prettierrc`, `.eslintrc`, `.babelrc` | |
| TOML | `.toml`, `Cargo.lock`, `uv.lock`, `poetry.lock` | |
| Shell | `.sh`, `.bash`, `.zsh`, `.zshrc`, `.bashrc`, `.profile`, `.envrc` and similar, `#!` scripts | |
| Rust | `.rs` | |
| Python | `.py`, `.pyi` | |
| CSS | `.css` | |
| HTML | `.html`, `.htm` | |
| Markdown | `.md`, `.markdown` | |
| Swift | `.swift` | |
| Dockerfile | `Dockerfile*`, `Containerfile*`, `.dockerfile`, `.containerfile` | |
| Environment files | `.env`, `.env.*` | |

Highlighting uses tree-sitter grammars, except Dockerfile and `.env`, which use a line scanner.
Language servers are started only for Go and TypeScript/JavaScript, when `gopls` or
`typescript-language-server` is on your login shell's `PATH`; other languages get highlighting,
folding and bracket matching without one.

## Claude Code integration

### MCP server

Register Athena's MCP server once per Claude Code profile:

```sh
claude mcp add -s user athena -- athena mcp-stdio
```

The server holds no state; each tool asks the running Athena window, which works out from the
process tree which pane the calling Claude session runs in.

| Tool | What it does |
|---|---|
| `list_projects` | Projects open in Athena; `current` marks the one this session runs in. |
| `get_active_file` | Focused editor's path, cursor, selection and unsaved state. |
| `open_file` | Opens a project file in the editor, optionally at a line. |
| `list_terminals` | Terminals with session id, title, cwd, running program and Claude state. |
| `read_terminal` | Recent output of a terminal as plain text. |
| `run_in_terminal` | Types a command into a terminal; runs only if you approve it within 60 seconds. |
| `list_project_files` | A project's files, honouring `.gitignore`; secrets such as `.env` and keys are left out. |
| `get_diagnostics` | Errors and warnings from the language servers. |
| `open_diff` | Shows the user a file's uncommitted changes in the diff viewer (unstaged by default, `staged` for the index). |

### Hooks

The command palette entry **Enable Claude Code hooks for this project** adds these hooks to the
project's `.claude/settings.local.json` (which Claude Code keeps out of version control), after
any of your own on the same events:

- `UserPromptSubmit`, `Stop`, and `Notification` for permission and idle prompts, which make the
  tab state exact;
- `PreToolUse` and `PostToolUse` on `Edit|MultiEdit|Write`, for reviewing Claude's edits (below).

Each calls `athena notify --event …`. **Disable Claude Code hooks for this project** removes only
Athena's commands, keeping your own even when they share an entry with ours. A settings file whose
shape is not what Claude Code expects is left untouched and the command reports an error; a
symlinked settings file stays a symlink and keeps its permissions.

Projects whose hooks were enabled by Athena 0.3 or earlier have only the first three. The palette
offers **Enable Claude Code hooks for this project** again for them; run it once to add the edit
hooks.

### Reviewing Claude's edits

Before Claude's first edit to a file in a session, the `PreToolUse` hook copies the file into
`~/Library/Application Support/athena/snapshots/<session>/` (or notes that the session is creating
it). The hook always exits 0, so it never blocks an edit. After each edit, the `PostToolUse` hook
tells the window, which reloads open diffs of that file and shows "Claude edited main.go" with a
**Review diff** action (one toast per file, kept for 15 seconds). When no copy was kept for that
session, Review diff opens the file's unstaged changes instead.

Review diff opens "main.go (Claude's Edits)": "Before Claude" on the left, the file on disk on the
right, with **Revert** on each change. Edits made by you or anyone else after Claude's first edit
are in the diff too, since it compares with the file as it was then. If no copy was kept because
the file was over 20 MB or not a regular file, the review compares with the index instead,
labels that side "Index" and says why. A review tab whose copy no longer exists (for example
after pruning) says no copy was kept.

Snapshots older than 7 days are deleted, then the oldest sessions until the folder is under
200 MB. `athena notify --edited <file>` reports an edit by hand.

### Claude Code as an IDE (off by default)

**Toggle Claude Code integration** (command palette, or File > Toggle Claude Code Integration)
lets Claude Code connect to Athena the way it connects to VS Code. It is off by default while the
protocol is undocumented and may change between Claude Code releases. When it is on:

- Athena listens on a random loopback port and writes `~/.claude/ide/<port>.lock` (owner-only,
  with a fresh token each launch). Claude Code finds Athena there; the lock lists the open
  projects and is removed when Athena quits or the setting is turned off.
- New terminals get `CLAUDE_CODE_SSE_PORT`, so `claude` started in them connects by itself. In a
  session that was already running, type `/ide`. Athena listens on the same port after a restart
  when it is free, so older terminals keep finding it.
- When Claude asks to edit a file, the proposed change opens as "main.rs (Claude's Proposal)":
  the file on disk against Claude's version. **Accept** (`cmd-enter`) lets Claude Code write it;
  **Reject** (`cmd-backspace`) or closing the tab declines it. You can still answer in the
  terminal instead; the tab then closes. Athena never writes the file itself. As far as
  Claude Code's own code shows, it asks this way only when it would otherwise ask for
  permission, so auto-accept modes skip it.
- The editor selection goes with each prompt (Claude Code shows "N lines selected"), and
  `cmd-alt-k` (**Send selection to Claude**) puts `@file#L3-7` into the prompt of the Claude
  session running in that project's terminals and focuses it.
- Claude Code reads diagnostics from Athena's language servers before and after each edit.

Quitting with a proposal open answers it as rejected.

### Playwright MCP

**Enable Playwright MCP** adds a `playwright` server (`@playwright/mcp@0.0.83`, pinned) to the
project's `.mcp.json` and lists that file in `.git/info/exclude`.

### Plan usage

Clicking **Usage** in the title bar asks before turning the indicator on. Athena then reads the
sign-in Claude Code keeps in the Keychain and asks api.anthropic.com for usage every 5 minutes;
nothing is stored. It uses the same undocumented endpoint as Claude Code's `/usage`, so it may stop
working after a Claude Code update.

## Keyboard shortcuts

Keys use GPUI's binding syntax as written in the source. `1…9` means each digit from 1 to 9.

### App (`crates/athena/src/actions.rs`)

| Keys | Action |
|---|---|
| `cmd-shift-p` | Command palette |
| `cmd-p` | Go to file |
| `cmd-o` | Open project |
| `cmd-shift-w` | Close project |
| `cmd-alt-[` | Previous project |
| `cmd-alt-]` | Next project |
| `cmd-alt-1…9` | Select project 1–9 |
| `cmd-t` | New terminal |
| `cmd-shift-t` | New Claude session |
| `cmd-w` | Close tab |
| `cmd-{` | Previous tab |
| `cmd-}` | Next tab |
| `cmd-1…9` | Select tab 1–9 |
| `cmd-d` | Split right |
| `cmd-shift-d` | Split down |
| `cmd-alt-left` | Focus pane left |
| `cmd-alt-right` | Focus pane right |
| `cmd-alt-up` | Focus pane up |
| `cmd-alt-down` | Focus pane down |
| `cmd-shift-enter` | Zoom pane |
| `cmd-b` | Toggle file tree |
| `cmd-shift-s` | Save as |
| `cmd-shift-v` | Markdown preview beside the editor / back to the source |
| `cmd-shift-f` | Find in project |
| `cmd-alt-shift-g` | Toggle inline blame |
| `cmd-=` (or `cmd-+`) / `cmd--` | Zoom editor and terminal text in / out |
| `cmd-0` | Reset zoom |
| `cmd-shift-m` | Problems tab |
| `cmd-shift-o` | Go to symbol in file |
| `cmd-alt-o` | Go to symbol in workspace |
| `cmd-alt-k` | Send the file and selected lines to Claude (in an editor, with Claude Code integration on) |
| `f8` / `shift-f8` | Next / previous problem (in an editor only) |
| `ctrl--` | Go back |
| `ctrl-shift--` | Go forward (also bound as `ctrl-_`, which is what macOS reports for it) |
| `cmd-j` | Notifications |
| `ctrl-cmd-f` | Toggle full screen |
| `cmd-m` | Minimize |
| `cmd-h` | Hide Athena |
| `ctrl-r` | Open recent folder (not in a terminal, where it is the shell's history search) |
| `cmd-alt-h` | Hide others |
| `cmd-q` | Quit |

Mouse buttons 4 and 5 go back and forward; a middle click on a tab closes it. The palette also has
commands without a key: Toggle auto save, Toggle format on save, Source control changes, Switch
branch…, Reveal active file in tree, Open file to the side, Open Markdown preview, New browser
preview and the Claude Code and Playwright commands. With an editor focused it also offers Go to
line, Indent lines, Outdent lines, Toggle replace, Insert line above (which has no key, since
`cmd-shift-enter` is Zoom pane), Insert line below, Rename symbol, Quick fix, Go to
implementations and Go to type definition. In Go to file, `@` lists the file's symbols, `#`
searches workspace symbols and `>` lists commands.

On a Mac keyboard the `f`-keys need Fn unless "Use F1, F2, etc. keys as standard function keys" is
on in System Settings. A system or app shortcut set on the same keys (`cmd-.` is a common one) can
take them before Athena sees them; Quick Fix… in the editor's right-click menu does the same as
`cmd-.`.

### Editor (`crates/athena-editor/src/view.rs`, `lsp_ui.rs`)

| Keys | Action |
|---|---|
| `cmd-s` | Save |
| `cmd-z` | Undo |
| `cmd-shift-z` | Redo |
| `cmd-x` / `cmd-c` / `cmd-v` | Cut / copy / paste |
| `cmd-a` | Select all |
| `cmd-f` | Find |
| `cmd-g` | Find next |
| `cmd-shift-g` | Find previous |
| `cmd-/` | Toggle comment |
| `f12`, `cmd-alt-g` | Go to definition |
| `cmd`-click | Go to definition |
| `shift-f12`, `cmd-alt-r` | Find references |
| `alt-left` / `alt-right` | Move by word |
| `cmd-left` / `cmd-right`, `home` / `end` | Line start / end |
| `cmd-up` / `cmd-down` | Document start / end |
| `pageup` / `pagedown` | Page up / down |
| `shift-left` / `shift-right` / `shift-up` / `shift-down` | Extend selection |
| `alt-shift-left` / `alt-shift-right` | Extend selection by word |
| `cmd-shift-left` / `cmd-shift-right` | Extend selection to line start / end |
| `cmd-shift-up` / `cmd-shift-down` | Extend selection to document start / end |
| `alt-backspace` | Delete word back |
| `cmd-backspace` | Delete to line start |
| `escape` | Close find, suggestions, hover or signature help / clear selection |
| `ctrl-g`, `cmd-l` | Go to line |
| `ctrl-space` | Show completions |
| `cmd-k cmd-i` | Show hover docs at the cursor |
| `cmd-k cmd-[` | Fold at the cursor |
| `cmd-k cmd-]` | Unfold at the cursor |
| `cmd-k cmd-0` | Fold all |
| `cmd-k cmd-j` | Unfold all |
| `tab` (selection over several or whole lines) | Indent lines |
| `shift-tab`, `cmd-[` | Outdent lines |
| `cmd-]` | Indent lines |
| `alt-up` / `alt-down` | Move lines up / down |
| `alt-shift-up` / `alt-shift-down` | Copy lines up / down |
| `cmd-shift-k` | Delete lines |
| `cmd-enter` | Insert line below |
| `cmd-alt-f` | Find and replace |
| `f2` | Rename symbol |
| `cmd-.` | Quick fix |
| `cmd-f12` | Go to implementations |

While the suggestion list is open, Up / Down move through it and Enter or Tab accepts. Clicking a
chevron in the gutter folds or unfolds that block. In the find bar's replace field, `enter`
replaces the current match and `cmd-enter` replaces all. In the rename field, `enter` renames and
`escape` or a click elsewhere cancels.

Image viewer (`crates/athena-editor/src/image.rs`): `cmd-=` (or `cmd-+`) / `cmd--` zoom in / out,
`cmd-0` fit to the pane, `cmd`-scroll zooms at the pointer. These take over from the text zoom
keys while the image viewer has focus.

### Diff view (`crates/athena-editor/src/diff_view.rs`)

| Keys | Action |
|---|---|
| `alt-f5` / `shift-alt-f5` | Next / previous change (wraps round) |
| `up` / `down` | Scroll a line |
| `pageup` / `pagedown` | Scroll a page |
| `cmd-up` / `cmd-down` | Top / bottom |
| `cmd-enter` / `cmd-backspace` | Accept / reject a change Claude proposes |

### File tree, palette and Search tab (text field keys: `crates/athena-ui/src/input.rs`)

| Keys | Action |
|---|---|
| `cmd`-click a file in the tree | Open it in a new pane beside the focused one |
| `enter` in Go to file | Open the file |
| `cmd-enter` in Go to file | Open the file in a new pane beside the focused one |
| `up` / `down`, `enter` in the Search tab | Walk the matches, open the selected one |
| `escape` in the Search tab | Close it |
| `enter` / `escape` in a tree name field | Create or rename / cancel |
| `cmd-enter` in the Changes tab's message field | Commit |
| `cmd-a` in any text field | Select all |

### Terminal (`crates/athena-term/src/view.rs`)

| Keys | Action |
|---|---|
| `cmd-c` | Copy selection |
| `cmd-v` | Paste |
| `cmd-k` | Clear scrollback |
| `cmd-a` | Select all |
| `cmd-f` | Find |
| `cmd-g` | Find next (older) |
| `cmd-shift-g` | Find previous (newer) |
| `cmd-up` | Scroll to the previous command's prompt |
| `cmd-down` | Scroll to the next command's prompt, or back to the live screen |
| `cmd`-click | Open a link, or a `file:line:col` reference in the editor |

In the terminal's find bar, `enter` and `up` go to the next older match, `shift-enter` and
`down` to the next newer one, `alt-c` toggles match case, `alt-r` toggles regex and `escape`
closes it.

### Your own shortcuts

**Open Keyboard Shortcuts File** (palette, or File > Keyboard Shortcuts) opens
`~/Library/Application Support/athena/keymap.json`, creating it with examples. It takes the same
shape as VS Code's `keybindings.json`, and Athena applies it as soon as you save:

```jsonc
[
  // A new chord for an existing command (athena:: may be left out of Athena's own commands).
  {"key": "cmd-k cmd-t", "command": "athena::NewTerminal"},
  // VS Code's key spelling works too.
  {"key": "cmd+shift+e", "command": "athena::RevealInTree"},
  // Only while an editor has focus.
  {"key": "f5", "command": "athena::ShowProblems", "when": "Editor"},
  // A leading - removes a default binding (here cmd-d, Split right).
  {"key": "cmd-d", "command": "-athena::SplitRight"}
]
```

Command names are the action names in the source: `athena::…` (`crates/athena/src/actions.rs`),
`editor::…`, `terminal::…` and so on. `when` takes the key contexts `Shell`, `Editor`,
`Terminal`, `DiffView`, `ImageView`, `DocView` and `TextInput`, combined with `&&`, `||` and `!`.
Your entries come after Athena's, so on the same key in the same context yours win. Comments and
trailing commas are allowed. Entries Athena cannot use (an unknown command, a key it cannot
parse, a broken `when`) are listed in a toast and in `app.log`; the rest still apply.

## Command line

```text
athena [<folder>]             open a folder in the running window, or start Athena with it
athena --version
athena mux status             list the daemon's sessions
athena mux stop               stop the daemon; its shells are hung up
athena notify --event <claude-stop|claude-needs-input|claude-running|claude-will-edit|claude-edited>
athena notify --edited <file> [--session <id>]
athena notify --title <t> [--body <b>]
athena mcp-stdio              MCP server for Claude Code
```

## Files and logs

Everything lives in `~/Library/Application Support/athena`: `workspace.json` (projects, layout,
editor cursors, recently closed folders, panel sizes, the text zoom, the theme (`System`, `Light`
or `Dark`) and the `autosave_delay_ms`, `format_on_save` and `ide_integration` settings),
`ide.env` (the Claude Code integration port that new terminals get), `keymap.json` (your
shortcuts),
`notifications.json`, the daemon and app sockets, `snapshots/` (copies taken before Claude's
edits), `discarded/` (copies kept by Discard and Revert), `app.log` (the window's log) and
`mux.log` (the session daemon's log). Both logs are created owner-only (mode 600); a log larger
than 5 MB is renamed to `app.log.1` or `mux.log.1` at the next start, replacing the previous one.

`ATHENA_LOG` sets the log level in `tracing` filter syntax (default `info`). The `athena` command
hands off to Launch Services, which does not pass your shell's environment, so quit Athena and
start it with the variable set:

```sh
open -a Athena --env ATHENA_LOG=debug
open -a Athena --env ATHENA_LOG=info,athena_lsp=debug     # debug for language server traffic only
```

A session daemon that Athena starts inherits the level; one already running keeps its own. At
`debug`, language server requests and replies are logged.

Check `athena mux status` first when a terminal does not attach, then `mux.log` and `app.log`.

## Build from source

Requirements:
- Apple silicon, macOS 26
- Xcode with the Metal toolchain (GPUI compiles its shaders at build time):
  ```sh
  sudo xcode-select -s /Applications/Xcode.app
  xcodebuild -downloadComponent MetalToolchain
  ```
- Rust 1.98 or later (`rustup toolchain install 1.98`)

```sh
cargo build --release --locked -p athena -p athena-mux
./target/release/athena      # finds athena-mux next to itself
./scripts/bundle.sh          # dist/Athena.app and dist/Athena-<version>-arm64.zip, ad-hoc signed
```

macOS banners and the Dock badge only work from the bundled app; a `cargo run` build shows
notifications inside the window.

The checks CI runs:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo deny check
```

## License

Apache License 2.0. See [LICENSE](LICENSE).

Bundled third-party assets keep their own licences: the Geist fonts (SIL OFL,
`crates/athena-ui/assets/fonts/OFL.txt`), seti-ui icons (MIT,
`crates/athena-ui/assets/icons/LICENSE-seti.md`) and Mermaid (MIT,
`crates/athena-preview/assets/LICENSE-mermaid`).
