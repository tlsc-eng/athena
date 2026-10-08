# Claude Code IDE integration for Athena (research spike)

Date: 2026-10-09. Claude Code CLI inspected: **2.1.295** (native Mach-O arm64, build 2026-10-08).

## Summary and recommendation

**Go for v0.5 behind a setting (off by default for one release), scoped to what the CLI actually calls.**
The CLI's side of the protocol is smaller than the public tool lists suggest. Version 2.1.295 calls
four IDE RPCs (`openDiff`, `close_tab`, `closeAllDiffTabs`, `getDiagnostics`) and listens for two
IDE-to-CLI notifications that matter (`selection_changed`, `at_mentioned`). Anthropic's own VS Code docs now
document the transport, auth header, lock path and file permissions, so it is no longer reverse-engineered
guesswork. The Rust side is small: `tokio-tungstenite` plus our existing rmcp 3.5.1 through its
`SinkStreamTransport`. The real cost is a blocking diff-review tab in the editor. The deal-breaker
would be if that diff UI grows past about 5 days, or if the open questions below show `openDiff` does not
fire in the permission modes people actually use. In either case, cut the scope to selection plus
at-mention plus diagnostics, which is about 4 days and still worth shipping.

## What we verified locally

All commands were read-only. No Claude session was started and nothing under `~/.claude*` was modified.

| Command | Finding |
| - | - |
| `ls -la ~/.claude/ide/` | The directory exists (`drwx------`, created 2026-10-08) and is empty. No IDE is currently advertising. |
| `ls -la ~/.claude-*/ide` | None exist (`~/.claude-ai`, `~/.claude-tlsc`, `~/.claude-mem`), even though this shell has `CLAUDE_CONFIG_DIR=~/.claude-tlsc`. |
| `readlink ~/.local/bin/claude` | `~/.local/share/claude/versions/2.1.295`. `claude` in zsh is a profile-picking shell function. |
| `~/.local/bin/claude --version` | `2.1.295 (Claude Code)` |
| `strings -n 6 <binary>` + `grep` | See the table below. JS code was read from the embedded Bun bundle. |

There was no lock file to read, so the lock-file shape comes from the CLI's own parser (`Smn()` in the bundle). It
reads `workspaceFolders`, `pid`, `ideName`, `transport` (`"ws"` means WebSocket, anything else means legacy SSE),
`runningInWindows` and `authToken`. It takes the **port from the filename** (`<port>.lock`). A non-JSON file
falls back to newline-separated folders.

Keyword presence in the 2.1.295 binary:

| Present | Absent (0 hits, also checked as UTF-16) |
| - | - |
| `openDiff`, `close_tab`, `closeAllDiffTabs`, `getDiagnostics`, `executeCode`, `selection_changed`, `at_mentioned`, `log_event`, `ide_connected`, `FILE_SAVED`, `DIFF_REJECTED`, `TAB_CLOSED`, `X-Claude-Code-Ide-Authorization` (this capitalisation), `CLAUDE_CODE_SSE_PORT`, `CLAUDE_CODE_AUTO_CONNECT_IDE`, `CLAUDE_CODE_IDE_SKIP_VALID_CHECK`, `CLAUDE_CODE_IDE_HOST_OVERRIDE`, `ws-ide`, `sse-ide` | `getCurrentSelection`, `getLatestSelection`, `getOpenEditors`, `getWorkspaceFolders`, `checkDocumentDirty`, `saveDocument`, `ENABLE_IDE_INTEGRATION`, `openFile` as an IDE RPC (the only hits are LSP/fs internals) |

Behaviour read from the bundle (verified in the 2.1.295 binary):

- **Lock dirs.** `z0n()` scans `<configDir>/ide`, and *also* `~/.claude/ide` whenever `CLAUDE_CONFIG_DIR` is set.
  The getter is minified, so `configDir` is inferred. That matches the official docs and the empty `~/.claude/ide` here.
  **Outcome: a lock in `~/.claude/ide/` is seen by every profile.**
- **Auto-connect** (`kze()`): `CLAUDE_CODE_AUTO_CONNECT_IDE=false` disables it. Otherwise any one of these turns it on:
  the `autoConnectIde` config, `--ide`, a recognised VS Code or JetBrains terminal, `CLAUDE_CODE_SSE_PORT` being set,
  or `CLAUDE_CODE_AUTO_CONNECT_IDE=true`.
- **Matching** (`har()`/`gar()`): if `CLAUDE_CODE_SSE_PORT` equals a lock's port, that lock wins outright. Otherwise a
  lock matches when one of its `workspaceFolders` contains the cwd. Auto-connect needs **exactly one** matching lock
  and polls for up to 30 s. A stale port in the env falls back to cwd matching.
  The pid-ancestry check runs only for recognised VS Code or JetBrains terminals, so Athena (`TERM_PROGRAM=athena`)
  is not subject to it.
- **Stale locks.** The CLI deletes lock files whose `pid` is dead, or that it cannot parse.
- **Transport.** `ws://127.0.0.1:<port>` with WebSocket subprotocol `mcp` and headers `User-Agent` and
  `X-Claude-Code-Ide-Authorization: <authToken>`. `CLAUDE_CODE_IDE_HOST_OVERRIDE` changes the host.
  After connecting, the CLI sends the notification `ide_connected {pid}`. MCP versions the CLI knows:
  `2025-11-25`, `2025-06-18`, `2025-03-26`, `2024-11-05`, `2024-10-07`.
- **Model-visible tools** are hard-coded: `["mcp__ide__executeCode","mcp__ide__getDiagnostics"]`. Every other
  tool on the `ide` server is filtered out before the model sees it.
- **`openDiff`** is used only for the Edit and Write tools (identified by their input schemas; the names are
  minified), when `diffTool == "auto"` (the default; the other value is
  `"terminal"`) and an IDE is connected. The call is `openDiff {old_file_path, new_file_path (same path),
  new_file_contents, tab_name}`. `tab_name` must match `^[A-Za-z0-9][A-Za-z0-9 ._+()-]{0,63}$`.
  How the CLI reads each result:
  - `[{text:"FILE_SAVED"},{text:<final contents>}]` means accepted, with the contents taken from the IDE. The
    second element is **required**.
  - `[{text:"TAB_CLOSED"}]` means it keeps the proposed contents, unsaved.
  - `[{text:"DIFF_REJECTED"}]` means it keeps the old contents.
  - Anything else throws "Not accepted".

  Afterwards the CLI always calls `close_tab {tab_name}`. It also calls `closeAllDiffTabs {}` during cleanup.
- **`getDiagnostics {uri:"file://..."}`**: the result's first text item is a JSON array of
  `{uri, diagnostics:[{message, severity, source, code, range}]}`. Severity is `Error|Warning|Info|Hint`.
  The CLI keeps a per-file *baseline* before an edit and compares after it. The returned `uri` must match the
  requested one, otherwise the CLI logs "path mismatch" and drops the result.

## Protocol

**Discovery.** The IDE writes `~/.claude/ide/<port>.lock`. The official docs say the file is `0600` inside a `0700`
directory, and that it goes in `$CLAUDE_CONFIG_DIR/ide/` when that variable is set. Athena should
write to `~/.claude/ide/`, because the CLI always scans it when profiles are in use.

```json
{"pid": 12345, "workspaceFolders": ["/Users/json/code/hobby/athena"], "ideName": "Athena",
 "transport": "ws", "runningInWindows": false, "authToken": "<redacted 128-bit hex>"}
```

**Transport and auth.**
- Verified, and also in the official docs: JSON-RPC 2.0 MCP over `ws://` bound to `127.0.0.1`, on a random port in
  10000-65535, with a fresh token per IDE activation sent as `X-Claude-Code-Ide-Authorization`.
- From public docs (claudecode.nvim `PROTOCOL.md`): it cites MCP `2025-03-26` and a lowercase header. Headers
  are case-insensitive, so the lowercase form is equivalent.

**Env in the IDE's terminal.** Verified: `CLAUDE_CODE_SSE_PORT=<port>` both enables auto-connect and
pins the server. `ENABLE_IDE_INTEGRATION`, which the public nvim docs list, **does not exist in 2.1.295**.

**Tools.** Only the rows marked "calls" are used by 2.1.295. The rest come from public docs (claudecode.nvim) and are what
VS Code implements.

| Tool | Params | Returns | 2.1.295 |
| - | - | - | - |
| `openDiff` | `old_file_path, new_file_path, new_file_contents, tab_name` | blocks until the user acts; `FILE_SAVED`+contents / `DIFF_REJECTED` / `TAB_CLOSED` | calls |
| `close_tab` | `tab_name` | `TAB_CLOSED` | calls |
| `closeAllDiffTabs` | none | `CLOSED_<n>_DIFF_TABS` (public docs) | calls |
| `getDiagnostics` | `uri?` | JSON array as above | calls; model-visible |
| `executeCode` | `code` | text/image (Jupyter only) | model-visible; skip |
| `openFile`, `getCurrentSelection`, `getLatestSelection`, `getOpenEditors`, `getWorkspaceFolders`, `checkDocumentDirty`, `saveDocument` | see claudecode.nvim `PROTOCOL.md` | JSON `{success, ...}` | not called |

**Notifications.**

IDE to CLI (verified schemas):
- `selection_changed {selection?: {start:{line,character}, end:{line,character}} | null, text?, filePath?}`.
  Lines are 0-based. The CLI computes `lineCount`, and subtracts one line when `end.character == 0`.
  It ignores the notification if `start` or `end` is missing. Public docs also list `fileUrl` and `isEmpty`, which the CLI ignores.
- `at_mentioned {filePath, lineStart?, lineEnd?}`, with 0-based lines. The CLI inserts `@path#Lx-y` into the prompt.
- `log_event {eventName, eventData}`: telemetry only.

CLI to IDE: `ide_connected {pid}`.

## What Athena would need

**Where it fits.**
- The GUI owns editor state, so it hosts the server. It should run on its own thread with a tokio
  `current_thread` runtime, following the same pattern as `app_socket.rs`. Requests go to the window thread
  over `async_channel`.
- `openDiff` needs a reply with **no timeout**. The 75 s `REPLY_TIMEOUT` used by app.sock must not apply.
- Shells are spawned by the separate `athena-mux` daemon after `env_clear()` plus an allowlist (`pane.rs`).
  The port therefore has to travel from the GUI to the daemon and be injected as `CLAUDE_CODE_SSE_PORT`.
  Panes that outlive a GUI restart keep a stale port. That still works through cwd matching, as long as only one
  lock contains the cwd.

**Components.**
1. **Lock file and listener.** Bind `127.0.0.1:0`, then write `~/.claude/ide/<port>.lock` atomically with
   mode `0600` (create the directory as `0700` if it is missing). Rewrite `workspaceFolders` when projects open or close,
   and remove the file on exit.
2. **Auth.** Use `tokio_tungstenite::accept_hdr_async`. Before the upgrade, reject with 401 unless the header
   matches the token (constant-time compare), and echo subprotocol `mcp`.
   Browsers cannot set custom WebSocket headers, so cross-site WebSocket hijacking is blocked. Additionally refuse any
   request that carries an `Origin` header.
3. **MCP session per connection.** Use rmcp `ServerHandler` over `SinkStreamTransport`, mapping tungstenite
   `Message::Text` to and from the rmcp JSON-RPC types. Notifications we send go out as
   `ServerNotification::CustomNotification`.
   - Rmcp's own `transport-ws` is commented out in 3.5.1, so this adapter is about 40 lines.
   - Prefer it over hand-rolled JSON-RPC: it reuses `#[tool]` schemas and version negotiation, and the
     existing `athena mcp-stdio` already proves rmcp handshakes with this CLI.
4. **Multi-client registry.** Several `claude` panes will connect at once. Each connection is mapped to a pane from
   the `ide_connected` pid, using the existing `procinfo` lineage. `selection_changed` is broadcast, and each
   `openDiff` reply goes back to the connection that asked. VS Code's "one instance" limit belongs to its
   extension, not to the protocol.
5. **Diff review tab.** This is the bulk of the work. It needs:
   - A read-only side-by-side or inline diff. `athena-editor`'s `DiffView` (v0.4, branch x4-review) already
     does this with per-hunk events, so this item is mostly wiring.
   - Accept and Reject keybinds, plus close behaving as `TAB_CLOSED`.
   - Accept returns `FILE_SAVED` plus the contents. Editing the proposed side before accepting is v2.
6. **Selection push.** Debounce editor selection changes (about 100 ms) and convert positions to 0-based line and
   character. Character units are not significant, because the CLI only uses `end.character == 0`.
   Add an "Send to Claude" keybind that emits `at_mentioned`.
7. **`getDiagnostics`.** Map from `AppMsg::Diagnostics`, which already exists. Echo the exact requested `uri`.
8. **Setting and lifecycle.** An `ide_integration` setting turns the listener, lock file and env injection on or off.

**Deps.**
- `tokio-tungstenite` and `tungstenite`: MIT (crates.io metadata, not verified locally; neither is in
  the registry cache or `Cargo.lock`). MIT is allowed by `deny.toml`.
- Already in `Cargo.lock`: `getrandom` (for the token), `futures`, `http`, `httparse`, and `base64`.
- No rmcp feature changes are needed: `pub mod sink_stream;` in `transport.rs` has no `cfg` gate.

**Effort (one developer).**

| Component | Days |
| - | - |
| 1-2: listener, lock file, auth | 1.5 |
| 3-4: rmcp over WebSocket, registry, pid-to-pane mapping | 1.5 |
| Port injection from GUI to mux | 0.5 |
| 5: diff review tab on top of `DiffView` | 1.5 |
| 6-7: selection, at-mention, diagnostics | 1 |
| 8 plus tests (a fake WebSocket client replaying recorded CLI frames) | 1.5 |
| **Total** | **about 7** |

**What it gives over hooks plus the stdio MCP.**
- The user approves each edit in a real diff before it lands.
- Live selection and open-file context is attached to every prompt automatically.
- `@` mentions can be sent from the editor.
- The CLI's automatic before/after-edit diagnostics baseline, which feeds new errors back to the model. The
  baseline code is verified in the binary; that it reports to the model is inferred.

**What it does not replace.** The model-visible allowlist means Athena's `list_terminals`,
`read_terminal`, `run_in_terminal` and the other tools cannot move onto the `ide` connection. The stdio
MCP server and the hooks stay.

**Risks.**
- **Protocol drift.** The protocol is only partly documented, and it has already drifted: the public nvim doc lists
  `ENABLE_IDE_INTEGRATION` and 12 tools and shows `FILE_SAVED` without contents, while 2.1.295 differs on all three.
  Mitigations:
  - Pin behaviour with recorded-frame tests.
  - Answer any unknown tool with a clean MCP error.
  - Re-run this `strings` check on each CLI bump.
- **Lock files in a shared directory.** Athena writes into `~/.claude/ide/`, which the user's other IDEs also use. If
  VS Code has the same folder open, there are two cwd matches and auto-connect fails unless `CLAUDE_CODE_SSE_PORT`
  is set. That is another reason env injection is required.
- **Token exposure.** The token sits on disk at `0600`, and loopback is unencrypted. The official docs accept this
  because any process able to sniff loopback can also read the lock file.
- **A blocked `openDiff`.** The diff waits on the user indefinitely. If Athena quits, it must answer pending diffs with
  `DIFF_REJECTED` or close the socket so the CLI does not hang.

## Open questions

1. When the user is in auto-accept or bypass mode, does the CLI skip `openDiff`? How does accepting in the
   terminal prompt interact with a pending diff tab? (Unverified; needs a live test with a stub server.)
2. When the IDE answers `FILE_SAVED`, does the CLI still write the file itself, or does it expect the IDE to have
   saved it? This decides whether the diff tab writes to disk on Accept. (Unverified.)
3. What should a GUI restart do: keep a stable port across restarts, or re-inject the port into the env of
   existing panes? (Design choice.)
4. Should we also implement the 8 tools the CLI does not call (they are cheap and read-only) for forward
   compatibility, or wait until a CLI version needs them?
5. Does the CLI time out on a long-blocking `openDiff`? No explicit timeout was found around `callIdeRpc`, but
   that is unverified.

Sources: the local 2.1.295 binary;
[Claude Code VS Code docs, "The built-in IDE MCP server"](https://code.claude.com/docs/en/ide-integrations);
[claudecode.nvim PROTOCOL.md](https://github.com/coder/claudecode.nvim/blob/main/PROTOCOL.md);
[Zed patch](https://data.gpo.zugaina.org/bentoo/app-editors/zed/files/0002-claude-code-ide-integration.patch) and [Sublime plugin](https://packagecontrol.io/packages/Claude%20Code%20IDE) (search summaries only). No source for Emacs `claude-code-ide.el` was found.
