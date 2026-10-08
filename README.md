# Athena

Athena is a macOS IDE built around terminals that outlive the window and around Claude Code.
Shells run in a separate session daemon (`athena-mux`), so quitting or updating the app does not
kill them; the window reattaches on the next launch. Projects get split panes of terminals, an
editor with language-server support and git decorations, a browser preview, and read-only views
of Playwright results and Docker containers. Claude Code sessions show their state on the tab,
and an MCP server lets Claude read the editor and terminals.

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

**Editor**

![The editor with a diagnostic and the References drawer](docs/screenshots/editor.png)

- Syntax highlighting for 16 languages (see [Languages](#languages)), each token class in its own
  colour, and the bracket matching the one at the cursor highlighted.
- Find in file, toggle comment, undo/redo, go to line (`line`, `line:column` or `line,column`).
- Code folding by brackets, falling back to indentation: chevrons in the gutter on hover, fold
  and unfold at the cursor or everywhere with the `cmd-k` chords below.
- With a language server: diagnostics, go to definition, find references (listed in a drawer
  tab; clicking a row opens the file at that line), hover docs (rest the pointer on a word for
  half a second), completion as you type (Up/Down to move, Enter or Tab to accept, Escape to
  close; accepting can also add an import) and signature help while typing call arguments.
- Format on save: `cmd-s` asks the language server to format the file first, by default in Go
  files only. "Toggle format on save" (palette, File menu) turns it on or off for every language
  with a server. Auto save does not format.
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
  each row and group; clicking a row opens the file.
- Status is re-read every 5 seconds while the window is in front and shortly after a save, a file
  operation or a change on disk. The title bar shows the branch.

**Workspace**

- Find and replace in project (`cmd-shift-f`): a Search drawer tab with a literal, smart-case
  search (case matters once the query has a capital), honouring `.gitignore` and skipping binary
  files and files over 1 MB; up to 2000 matches. Up/Down walk the matches, Enter opens one.
  **Replace All** asks first, then replaces in open editors (one undo step each) and on disk.
- Right-click menus. Tree: New File, New Folder, Rename, Delete (to the Trash), Reveal in Finder,
  Copy Path, Copy Relative Path, Open to the Side. Tabs: Close, Close Others, Close to the Right,
  Close All, Reveal in Finder, Copy Path, Copy Relative Path, Reveal in File Tree, Split Right /
  Down with that tab. Editor: Go to
  Definition, Find References, Cut, Copy, Paste, Toggle Line Comment. Terminal: Copy, Paste,
  Select All, Find, Clear.
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
- Optional title-bar indicator for Claude plan usage (5-hour and weekly windows).

**Also**
- Browser preview pane (WKWebView); `3000` or `localhost:3000` opens the local dev server.
- Playwright: run a project's tests, list failures from the JSON report, open traces, and
  optionally add the Playwright MCP server for Claude.
- Containers: a read-only view of the local Docker engine (Docker Desktop or Rancher Desktop):
  list, stats and logs. Athena never starts, stops or removes containers.
- Workspace layout, open projects and tabs are restored on launch.

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

### Hooks

The command palette entry **Enable Claude Code hooks for this project** adds three hooks to the
project's `.claude/settings.local.json` (which Claude Code keeps out of version control):
`UserPromptSubmit`, `Stop`, and `Notification` for permission and idle prompts. Each calls
`athena notify --event …`, which makes the tab state exact. **Disable Claude Code hooks for this
project** removes only Athena's entries.

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
| `ctrl--` | Go back |
| `ctrl-shift--` | Go forward (also bound as `ctrl-_`, which is what macOS reports for it) |
| `cmd-j` | Notifications |
| `ctrl-cmd-f` | Toggle full screen |
| `cmd-m` | Minimize |
| `cmd-h` | Hide Athena |
| `cmd-alt-h` | Hide others |
| `cmd-q` | Quit |

Mouse buttons 4 and 5 go back and forward; a middle click on a tab closes it. The palette also has
commands without a key: Toggle auto save, Toggle format on save, Source control changes, Reveal
active file in tree, Open Markdown preview, New browser preview and the Claude Code and Playwright
commands; with an editor focused it also offers Go to line.

### Editor (`crates/athena-editor/src/view.rs`)

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

While the suggestion list is open, Up / Down move through it and Enter or Tab accepts. Clicking a
chevron in the gutter folds or unfolds that block.

Image viewer (`crates/athena-editor/src/image.rs`): `cmd-=` (or `cmd-+`) / `cmd--` zoom in / out,
`cmd-0` fit to the pane, `cmd`-scroll zooms at the pointer.

### File tree, palette and Search tab (text field keys: `crates/athena-ui/src/input.rs`)

| Keys | Action |
|---|---|
| `cmd`-click a file in the tree | Open it in a new pane beside the focused one |
| `enter` in Go to file | Open the file |
| `cmd-enter` in Go to file | Open the file in a new pane beside the focused one |
| `up` / `down`, `enter` in the Search tab | Walk the matches, open the selected one |
| `escape` in the Search tab | Close it |
| `enter` / `escape` in a tree name field | Create or rename / cancel |

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

In the terminal's find bar, `enter` and `up` go to the next older match, `shift-enter` and
`down` to the next newer one, `alt-c` toggles match case, `alt-r` toggles regex and `escape`
closes it.

## Command line

```text
athena [<folder>]             open a folder in the running window, or start Athena with it
athena --version
athena mux status             list the daemon's sessions
athena mux stop               stop the daemon; its shells are hung up
athena notify --event <claude-stop|claude-needs-input|claude-running>
athena notify --title <t> [--body <b>]
athena mcp-stdio              MCP server for Claude Code
```

## Files and logs

Everything lives in `~/Library/Application Support/athena`: `workspace.json` (projects, layout,
panel sizes and the `autosave_delay_ms` and `format_on_save` settings), `notifications.json`, the daemon and app sockets, `app.log` (the window's log) and
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
