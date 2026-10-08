# Athena

Athena is a macOS IDE built around terminals that outlive the window and around Claude Code.
Shells run in a separate session daemon (`athena-mux`), so quitting or updating the app does not
kill them; the window reattaches on the next launch. Projects get split panes of terminals, an
editor with language-server support, a browser preview, and read-only views of Playwright results
and Docker containers. Claude Code sessions show their state on the tab, and an MCP server lets
Claude read the editor and terminals.

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

`brew uninstall --zap --cask athena` also deletes `~/Library/Application Support/athena`.

## Features

**Terminals**
- Shell sessions are owned by `athena-mux`, which keeps running when the window closes. Restored
  tabs reattach to the same shells with their scrollback.
- Split panes right and down, zoom a pane, move focus between panes with the keyboard.
- zsh shell integration (OSC 133) marks commands; one that runs longer than 10 seconds posts a
  notification when it finishes. `ATHENA_NOTIFY_AFTER_SECS` changes the threshold when Athena
  (and so its daemon) is started from a shell; `ATHENA_SHELL_INTEGRATION=0` turns the
  integration off.

**Editor**

![The editor with diagnostics](docs/screenshots/editor.png)

- Syntax highlighting (tree-sitter) for Go, TypeScript, TSX and JavaScript.
- Find in file, toggle comment, undo/redo.
- Diagnostics and go-to-definition from `gopls` and `typescript-language-server` when they are
  installed; Athena starts them for open files.
- Go to file (fuzzy) and a command palette.

**Claude Code**

![A Claude Code session in a terminal tab](docs/screenshots/claude.png)

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
| `cmd-j` | Notifications |
| `ctrl-cmd-f` | Toggle full screen |
| `cmd-m` | Minimize |
| `cmd-h` | Hide Athena |
| `cmd-alt-h` | Hide others |
| `cmd-q` | Quit |

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
| `f12` | Go to definition |
| `cmd`-click | Go to definition |
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
| `escape` | Close find / clear selection |

### Terminal (`crates/athena-term/src/view.rs`)

| Keys | Action |
|---|---|
| `cmd-c` | Copy selection |
| `cmd-v` | Paste |
| `cmd-k` | Clear scrollback |

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

Everything lives in `~/Library/Application Support/athena`: `workspace.json` (projects and
layout), `notifications.json`, the daemon and app sockets, and `mux.log` (the session daemon's
log). Check `athena mux status` first when a terminal does not attach.

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
