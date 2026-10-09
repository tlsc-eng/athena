# Roadmap report: v0.2.0, v0.3.0, v0.4.0 and v0.5.0

What happened while you were away, for the roadmap in `piped-orbiting-stearns.md` (Phases A–E).

- Release: v0.2.0 — https://github.com/tlsc-eng/athena/releases/tag/v0.2.0 (tap `f65669b`,
  installed here with `brew upgrade --cask athena`)
- Release: v0.3.0 — https://github.com/tlsc-eng/athena/releases/tag/v0.3.0 (tap `5f58c02`,
  installed here with `brew upgrade --cask athena`)
- Release: v0.4.0 — https://github.com/tlsc-eng/athena/releases/tag/v0.4.0 (tap `7aa6afb`, installed here)
- Release: v0.5.0 — <link added at release>

The screen was locked for most of the work after v0.2.0; it became unlocked only near the end of
v0.5. Everything below marked **unverified on screen** is covered by unit tests, logs or
synthetic-key runs, but nobody has looked at it. Start with [What to check when you're back](#what-to-check-when-youre-back).

## v0.5.0

Plan: [plans/v0.5.md](plans/v0.5.md), three file-disjoint lanes (Claude Code IDE integration,
multi-cursor and word wrap, workbench polish), `v0.4.0..main`, then a review pass and a keystroke
performance follow-up. The v0.4.0 section follows this one.

### What shipped

**Claude Code IDE integration** (`742cfb8`, `ca861b0`, `0e6bf73`, `5c56709`, `293b4a4`, `d1f688c`)
- An MCP server over a loopback WebSocket that Claude Code finds through
  `~/.claude/ide/<port>.lock`, answering the four tools the CLI (2.1.295) calls: `openDiff`,
  `close_tab`, `closeAllDiffTabs` and `getDiagnostics`. Off by default; **Toggle Claude Code
  integration** in the palette or the File menu turns it on.
- `openDiff` opens "main.rs (Claude's Proposal)", the file on disk against Claude's version, with
  Accept (Cmd+Enter) and Reject (Cmd+Backspace, or closing the tab).
- The focused editor's selection is sent as `selection_changed`; Cmd+Alt+K (**Send selection to
  Claude**) sends an at-mention to the project's Claude session and focuses its terminal.
- New terminals get `CLAUDE_CODE_SSE_PORT` through `<data dir>/ide.env`, which `athena-mux` reads
  at each spawn, so the daemon protocol did not change.

**Editor** (`83080a1`, `81c4398`, `28c05be`, `eb7f514`, `bbc9c29`, `8989eff`, `3bd051b`)
- Multiple cursors: Cmd+D, Cmd+K Cmd+D, Cmd+Shift+L, Cmd+Alt+Up/Down, Alt+click, Shift+Alt+drag
  column selection, Escape back to one caret. Every edit runs at each caret as one undo step, and
  one keystroke at many carets costs one tree-sitter parse.
- Soft word wrap (Alt+Z per tab; Markdown wraps by default) through the display map, so folds and
  wrapped rows share one row mapping; Up/Down by visual row, Home/End at the row's edges first.
- Sticky scroll: the headers of the scopes around the top line stay pinned, at most five and never
  more than a third of the view; a click jumps to one.
- A view state (cursor, top line, folds, wrap choice) the shell saves and restores.
- Reparse off the UI thread: the UI thread's cost per keystroke in a 2k-line Rust file went from
  7.2 ms average (p99 11.6 ms) to 0.15 ms (p99 0.24 ms); method and numbers in
  [perf/2026-10-v0.5-reparse.md](perf/2026-10-v0.5-reparse.md). Keys that open a pair still parse
  inline when the tree lags, since auto-closing depends on whether the caret is in a string.

**Workbench** (`b778722`, `d649b4c`, `6b25ecf`, `bce268a`, `ec6ad9a`, `8f2be73`, `45f33fb`,
`17a5653`, `d9c86da`, `49ca228`, `1f63eef`, `c00c614`)
- Session restore keeps each editor tab's cursor, scroll line, folds and word wrap choice.
- Open Recent: the last 20 closed project folders, from Ctrl+R (outside terminals), File > Open
  Recent and the palette.
- `keymap.json` in the data folder, VS Code-shaped (`key`, `command`, `when`, `args`), reloaded
  when saved, with a leading `-` to remove a default binding, VS Code's key spelling accepted and
  problems listed in a toast.
- A light theme, and by default the theme follows the macOS appearance.
- Terminal command marks from OSC 133 (and OSC 633): a dot per prompt for success, failure or
  running, Cmd+Up/Down to jump between prompts, Copy Last Command Output; a bash snippet in the
  README sends the same marks.
- A word wrap default setting, a Selection menu, multi-cursor palette entries and a status bar
  that reads "3 selections" with several carets.

### Decisions made without you

From the commit messages and the lane plans:
- The IDE integration is off by default for this release, since the protocol is undocumented and
  can change with any Claude Code release. Turning it off removes the lock file and `ide.env`.
- Accept answers `FILE_SAVED` with the proposed text and Athena does not write the file: Claude
  Code's Edit and Write tools write it after the permission prompt returns (read from the 2.1.295
  bundle; claudecode.nvim does the same). Closing a proposal tab answers `DIFF_REJECTED`, because
  `TAB_CLOSED` would count as accepting. Quitting, a crash and dropping the server reject pending
  proposals and remove the lock.
- Handshakes with an `Origin` header are refused (403), a wrong or missing token gets 401
  (constant-time compare), and each connection is its own MCP session. The previous port is bound
  again after a restart when it is free.
- The port reaches shells through `ide.env` rather than a mux protocol change, so a v0.4 daemon
  keeps working; its new terminals simply do not get the port until it is restarted (`/ide` works
  meanwhile; the README says how).
- Selections are checked every 150 ms while a session is connected and sent only to sessions in
  that project's terminals (matched by pid through the process tree) or whose terminal is unknown.
- In an editor, Cmd+D and Cmd+Alt+Up/Down are VS Code's multi-cursor keys and shadow Split Right
  and Focus Pane Up/Down; terminals keep the pane keys, and Cmd+Alt+Left/Right still move focus
  from an editor. Cmd+D from a bare caret matches whole words, case-sensitively, as VS Code does
  with the find widget closed.
- Carets are capped at 2048 so an edit at every caret still fits the edit log other tabs on the
  file follow. Copy and Cut take whole lines only when no caret has a selection.
- Word wrap is off by default except for Markdown; it never breaks inside a line's leading
  indentation, and continuation rows keep the line's indent unless that leaves under half the
  width. Horizontal scrolling is off while wrapping.
- Sticky headers come from the existing fold regions, at most five and a third of the view.
- Restored folds are stored as header lines and re-applied to whatever region heads there now; a
  tab that was never drawn stores no scroll line and restores with the cursor centred.
- Ctrl+R has the context `!Terminal`, so it stays the shell's reverse history search.
- `keymap.json`: the user's entries come after Athena's; the folder is watched and the file
  reloaded only when its bytes change, because FSEvents reports writes to `app.log` and
  `workspace.json` as folder changes.
- Theme: System is the default. Light terminal "white" and "bright white" are greys, as in VS Code,
  so text printed in white stays readable; switching keeps font zoom and Reduce Motion.
- Command marks: the terminal tags a prompt's line with a zero-width private-use character so the
  mark moves with the text through scrollback and reflow; copied text has the tags removed. The
  scanner keeps the first 32 bytes of any OSC body (zsh's C mark carries the command in base64),
  never needs B, and forgets a prompt where nothing ran (Enter on an empty line, ^C).

### Review fixes

A review of `v0.4.0..main` found 15 issues (1 high, 7 medium, 7 low) and four performance notes.
All 15 are fixed for v0.5.0, apart from two checks against the real CLI (see Known gaps); two of
the performance notes are fixed and two remain:
- Moving a selection that ended at the start of an unterminated last line down left it past the
  text, and the next key panicked (high, present since v0.4) (`6cf2457`).
- A fold whose header was already folded away was kept, hiding the header and, with wrap on,
  underflowing the row count; session restore could reach it (`32efe73`).
- Alt+click removing a caret left a drag armed; Cut with some carets selecting deleted the empty
  carets' whole lines without copying them (`98ad0d9`, with `ea839cb` restoring Cut on an empty
  last line).
- Autoscroll let the caret hide under the sticky headers (`56e2a56`); edits counted line breaks
  by LF only while the rope also breaks at a lone CR (`598393f`); a completion's extra edits
  landed mid-word with several carets, and carets are now capped (`7b19854`).
- Performance: a live resize re-wraps only lines too wide for the new width (`f1aca3f`); sticky
  scroll measures indentation without copying lines (`5c03515`).
- IDE integration:
  - A panic on any thread stopped the IDE server for good, so later proposals were rejected and Accept did nothing. It now reacts only to a main-thread panic (`ce5b538`).
  - Connections get a 5 s handshake timeout and at most 16 may wait to authenticate, so a local process cannot exhaust the window's file descriptors (`8888c27`).
  - Incoming `file://` URIs are percent-decoded and the file URL Athena sends is encoded. Diagnostics URIs stay unencoded, because Claude Code compares them as raw paths (`8c249bd`).
  - A proposal for a file that is not a regular text file under 20 MB is refused (`3c7d0e6`). A file outside every open project, once symlinks and `..` are resolved, is refused too, so Claude Code asks in the terminal instead (`709c898`). Before, such a file opened in the active project.
- Keymap: the review's premise did not reproduce, because a user binding already wins in gpui 0.2.2. The real bug was that a key whose user action did nothing fell through to Athena's default on the same key. User entries now replace every default on their key, as in VS Code (`f241bdf`). A symlinked `keymap.json` is watched through its target (`78c6c67`).
- Terminal:
  - Copying text stripped all of Unicode plane 15, which also holds Nerd Font icons, not just Athena's prompt tags. The tags moved to plane 16 (`5832a80`).
  - Mark positions are kept correctly once the scrollback is full. Copy Last Output refuses after a column reflow instead of copying the wrong lines (`bc66006`).

Before the review, within the lanes: the IDE server's shutdown now frees its port and the
selection poll no longer copies a large selection every 150 ms (`293b4a4`); the selection getter
reports the primary caret (`d1f688c`); and a batch of caret, wrap and fold fixes (`8989eff`).

### Verification

- Unit tests throughout, including a recorded-frame fake Claude Code client (handshake, 401/403,
  accept and reject, `close_tab`, `closeAllDiffTabs`, diagnostics, disconnects, port reuse), a
  daemon test for `ide.env`, keymap parsing and merging, WCAG contrast for both themes, the OSC 133
  scanner fed byte by byte, and the multi-cursor, wrap and fold matrices.
- An isolated QA app (`HOME=/tmp/athena-qa-x5-ide`) driven with the fake client: accept, reject,
  Cmd+W and `closeAllDiffTabs` closed the tab with the right answer and left the file on disk
  unchanged; selections and Cmd+Alt+K reached the client; Cmd+Q with a pending diff answered
  `DIFF_REJECTED` and removed the lock.
- Synthetic-key runs in the app for multi-cursor (Cmd+D three times with no split, carets below,
  typing and undo), wrap (moving onto a wrapped row, End, toggling back) and restore (folds,
  cursor and top line surviving a launch and quit), checking the saved files each time.
- The screen became unlocked near the end, so some v0.5 screenshots exist in
  `docs/screenshots/v0.5/` (a debug build with a seeded workspace): a restored tab wrapped with
  four carets on wrapped rows and the status bar reading "4 selections" ([`02-carets.png`](screenshots/v0.5/02-carets.png)), `(`
  typed inside a string and inside a comment left unclosed, in the build with the off-thread
  reparse ([`03-typed.png`](screenshots/v0.5/03-typed.png)), the palette's two word wrap commands ([`04-palette.png`](screenshots/v0.5/04-palette.png)) and the "Word wrap is
  on" notice ([`05-default-on.png`](screenshots/v0.5/05-default-on.png)).
- **Unverified on screen**: sticky scroll (the screenshots stop just above a closing brace, where
  nothing is pinned), the light theme, proposal tabs and their toolbar, the prompt dots, the
  Selection and Open Recent menus, the keymap toast and column selection.

### Known gaps

- `openDiff` in the auto-accept permission modes was not tried against the real CLI; Claude Code's
  code suggests those modes skip the proposal, which the README hedges.
- The IDE protocol is undocumented and pinned to what 2.1.295 sends; a Claude Code release can
  change it. Two details were checked only against recorded frames: the `DIFF_REJECTED` payload
  shape and the `Origin` refusal against the real (Bun) client.
- A symlinked `keymap.json` is not watched for changes.
- Keys that open a pair still parse inline when a background parse has not landed, up to about
  10 ms in the benchmark when typing faster than parses.
- The terminal's command list drops forgotten prompts with a scan per prompt (a review note, not
  measured as a problem).

### Dependabot: `grid` 0.18

GitHub flags `grid` 0.18.0 (a RUSTSEC/GHSA integer overflow in `Grid::expand_rows`, fixed in
`grid` 1.0.1). It comes in through `taffy` 0.9.0, which `gpui` 0.2.2 uses, and `gpui` is pinned
exactly (`=0.2.2`), so it cannot be updated without a `gpui` upgrade. Athena's layouts do not use
CSS grid (the terminal's grid is alacritty's own type), so the overflowing code is not reached in
practice and the risk is low. Revisit with the next `gpui` bump.

## v0.4.0

Plan: [plans/v0.4.md](plans/v0.4.md), three file-disjoint lanes (editing, navigation and language
features, reviewing changes), `v0.3.0..main`. The sections after this one cover v0.2.0 and v0.3.0.

### What shipped

**Editing** (`b472f7f`, `3242abb`, `04f08e2`, `26d9d70`, `c9ad865`)
- Tab, Shift+Tab, Cmd+] and Cmd+[ indent and outdent selected lines (Tab used to replace a
  multi-line selection with whitespace).
- Auto-closing brackets and quotes with type-over, surround selection and Backspace of an inserted
  pair, skipped inside strings and comments; Enter between brackets opens an indented line.
- Line operations: move (Alt+Up/Down), copy (Alt+Shift+Up/Down), delete (Cmd+Shift+K), insert
  below (Cmd+Enter); insert above in the palette.
- Find and replace in file (Cmd+Alt+F), each replace one undo step.
- Indentation guides with the active block brighter; whitespace drawn inside the selection.
- Text zoom for editors and terminals (Cmd+= / Cmd+- / Cmd+0), saved in the workspace.
- A VS Code style status bar: branch, Ln/Col and selection count, indentation (menu to change or
  convert), encoding, line ending, language (menu to re-highlight) and a language server dot.

**Navigation and language features** (`8db43e5`, `48f3ab3`, `17c548d`, `b18b33d`, `a59db35`,
`9922de4`, `26a84fb`, `9002017`)
- Cmd+click `file:line:col` references in terminal output (Go, tsc, eslint, cargo/rustc, Rust
  panics, Python tracebacks, Claude Code) and OSC 8 `file://` links.
- Problems drawer tab grouped by file, F8 / Shift+F8 across files, Cmd+Shift+M, and an
  error/warning counter in the title bar.
- Go to symbol in file (Cmd+Shift+O, `@`) and in workspace (Cmd+Alt+O, `#`).
- A workspace edit applier for rename, code actions and server `workspace/applyEdit`: checked
  whole before anything changes, one undo step per open file, atomic writes for closed ones.
- Rename symbol (F2) with an inline field; quick fixes and refactorings (Cmd+., editor menu, gutter
  marker); organize imports folded into format on save for Go.
- Go to implementation (Cmd+F12) and type definition (menu and palette).

**Git and diffs** (`95f47f4`, `f51faea`, `a36628c`, `9542b5b`, `1bbb928`, `b54305d`)
- Diff tabs, side by side or inline, with syntax highlighting, word-level marks, Alt+F5 change
  navigation and Stage / Unstage / Revert per change. A Changes row opens its diff.
- Commit box with Cmd+Enter, Amend (keeps the last message's body) and smart commit; Discard per
  file and group with a copy kept for 30 days (untracked files go to the Trash).
- Branch picker (palette, the branch button, the status bar branch): switch, create, track a remote
  branch; never forced.
- MCP `open_diff` tool so Claude can show the user a diff.

**Claude Code** (`9909f8d`, `86945dc`)
- PreToolUse and PostToolUse hooks on `Edit|MultiEdit|Write`. The first takes a copy of each file
  before the session's first edit to it; the second shows "Claude edited main.go" with **Review
  diff**, which opens the session's changes to the file against that copy. Existing installs have
  to run **Enable Claude Code hooks for this project** again to get them.
- Research on Claude Code's IDE protocol, see [Next](#next-the-claude-code-ide-protocol).

### Decisions made without you

From the commit messages:
- VS Code keys where free; otherwise: workspace symbols on Cmd+Alt+O because Cmd+T is New
  terminal; Insert Line Above unbound because Cmd+Shift+Enter is Zoom pane; Go to Type Definition
  has no key; F8 works only in an editor, since in a terminal it belongs to the running program.
- Tab on a selection inside one line still replaces it, as VS Code does. Auto-close follows VS
  Code's "auto": only inserted closers are typed over or deleted with their opener.
- Text zoom changes the code font only, in 1 px steps between 6 and 40 px; the image viewer keeps
  its own Cmd+= / Cmd+- / Cmd+0 while focused.
- The status bar's branch opens the branch picker, as in VS Code, not the Changes tab.
- Problems leaves out hints, as VS Code does. Several implementations are listed in the References
  tab, which now names what it lists.
- The code action marker is painted as a dot, not a lightbulb emoji, and shows only when a
  diagnostic on the cursor's line has a quick fix.
- Organize imports and formatting are computed against the same text and merged; a formatting
  edit inside the import block is dropped.
- Workspace edits: only file create and rename are advertised, deletes are refused; an edit with
  no document version is refused if an open file it touches changed since the request; creating
  over a non-empty file is refused. Server-initiated `applyEdit` has nothing to compare against
  and is not checked for staleness.
- In terminal links, a relative name that neither the shell's folder nor the project has (Go test
  output) is looked up in the project, opening Go to file when several match.
- The diff is a hand-rolled Myers (about 300 lines with tests) rather than the `similar` crate,
  under the 200-line replacement bar once its glue is counted, with xdiff's cost limit so large
  unrelated files finish quickly. The diff view copies `element.rs`'s token colour match instead
  of touching another lane's file.
- The commit message field holds one line; Enter does nothing there. Amend shows the subject and
  sends the body back unchanged, and refuses if `HEAD` moved since. With nothing staged, Commit
  offers to stage everything.
- Discard and Revert keep a copy under `Application Support/athena/discarded` for 30 days.
- Commit, amend, stage, unstage, discard and branch switch get a 10 minute limit (pre-commit hooks,
  big checkouts); status keeps 30 s.
- Edit hooks: the PreToolUse hook always exits 0, since a failing one would block the edit;
  snapshots are pruned after 7 days, then oldest first beyond 200 MB; files over 20 MB or not
  regular files are skipped, and their review compares with the index and says so. The edit
  notice goes over `app.sock` rather than the mux protocol, which an older daemon could not
  decode. One toast per file, kept 15 s.
- "Hooks enabled" now means every current hook is installed, so Enable shows again for an older
  install; Disable removes only Athena's commands, even from an entry shared with the user's.

### Review fixes

A review of `v0.3.0..8b536bc` found 1 critical, 10 major and 4 minor issues plus 8 smaller ones;
all are fixed on `main`:
- Hunk staging (`caf999a`): the index path was wrong for a project opened on a subfolder
  (critical); a stale index is now refused; eol and clean/smudge filters (git-lfs) are honoured;
  the last hunk of a deleted or new file stages the deletion or untracks the file; large diffs no
  longer restart on every 5 s poll; Amend refuses a moved `HEAD`; longer git limits.
- Claude edit hooks (`b84d2a1`, `5e23fb9`): a Write larger than 64 KB recorded the edited file as
  its own baseline; the hook input is now stream-parsed. Large and non-regular files are skipped
  with a marker and their review says why. FIFOs no longer block the hook.
- Hook settings (`c08b5b7`, `e789a43`): the user's own commands sharing an entry with Athena's are
  kept; a wrongly shaped settings file is left untouched; symlinks and 0600 permissions survive.
- Workspace edits (`a2efcb6`): stale unversioned edits and creating over a non-empty file are
  refused; case-only renames work.
- Editor: lines break only at LF, CRLF and CR, as language servers count them (`02f65e9`); Enter
  between brackets after a non-ASCII blank no longer deletes the closer and uses CRLF in CRLF
  files (`273cd30`); Convert Indentation to Tabs uses the file's indent size (`f4964d6`); format on
  save keeps a replace that shares its start with an insert (`79a95e4`).
- Smaller: OSC 8 links open only regular files (`638b75b`); a code action with `"edit": null` is
  resolved (`5e1dc60`); F8 no longer sticks on a clamped stale problem (`786ea82`); Go to symbol
  in file sends pending edits first and ignores a stale reply (`011c3da`).

Before the review: Enter over a downward multi-line selection panicked (`9e2082f`); Amend folded a
multi-line message into one line and Discard All / Stage All touched conflicted files (`1bbb928`).

### Unverified on screen

The screen stayed locked for the whole release, so nothing visual was looked at: the status bar,
indentation guides and selection whitespace, auto-pairs, zoom, the replace row, the Problems tab
and title bar counter, the symbol palettes, the rename field, the code action menu and gutter
marker, the diff view in both modes with its word marks and change bars, the commit box and Amend,
the branch picker, the "Claude edited" toast and review tab, and the terminal link underline.

What was checked: unit tests throughout; a gopls 0.23 integration test for rename (cross-file),
prepareRename, the missing-import quick fix, organize imports, document and workspace symbols and
implementation; temp-repository tests for every git operation; link detection built from real
go 1.27, cargo and python 3 output; a debug app under an isolated `HOME` where the edit hooks took
snapshots from Claude-shaped input and the MCP `open_diff` call opened a diff tab that survived a
restart.

### Known gaps

- The diff view has no text selection or copy, and no way to hide unchanged regions.
- The commit message is a single line.
- Moved lines (Alt+Up/Down) are not re-indented to their new block.
- Zoom changes only the code font; the rest of the interface keeps its size.
- The gutter marker appears only for quick fixes, not for refactorings available on a line.
- Cmd+. may be taken by macOS or another app before Athena sees it; Quick Fix… in the editor menu
  is the fallback.
- F-keys (F2, F8, F12, Alt+F5) need Fn on a Mac keyboard unless standard function keys are on.

### Next: the Claude Code IDE protocol

[plans/claude-ide.md](plans/claude-ide.md) recommends implementing Claude Code's IDE protocol in
v0.5, behind a setting that is off by default for one release, scoped to what the CLI (2.1.295)
actually calls: `openDiff`, `close_tab`, `closeAllDiffTabs` and `getDiagnostics`, plus the
`selection_changed` and `at_mentioned` notifications. `openDiff` gives approval before an edit is
applied, which hooks cannot; the new diff view covers most of its UI, so the estimate is about 7
days. If the blocking diff tab grows past about 5 days, or `openDiff` turns out not to fire in the
permission modes people use, the fallback is selection, at-mention and diagnostics only (about 4
days). Open questions need a stub-server test first.

## What shipped

**v0.2.0 (Phase A: prod fixes and infra)**
- Cold launch from the Dock starts exactly one session daemon (the "Terminal unavailable / session
  daemon is not running" race). Older-daemon terminals are marked **stale** with Restart / Keep;
  `brew uninstall --zap` stops the daemon.
- Logging to `app.log` / `mux.log` (`ATHENA_LOG`), owner-only, rotated at 5 MB.
- Claude plan usage reads `percent` from the API (was 0 %).
- Terminal tabs take the OSC title (Claude Code's session name); the window title follows.
- Claude Code colours in the terminal; box-drawing glyphs drawn as aligned rectangles.
- Files open in the editor pane used last, or beside it (Cmd+click, Cmd+Enter).
- Go to definition reports why it went nowhere; find references (Shift+F12) in a drawer tab.
- Release workflow shares the CI Rust cache; README, LICENSE, three screenshots.

**v0.3.0 (Phases B–E plus extras)**
- Editor breadth: 12 new languages (16 total), distinct colour per token class, bracket match,
  folding by brackets, image viewer, Markdown/Mermaid preview, seti-ui file icons, auto save with
  unsaved markers, external-change bar, Save As, shared buffer for two tabs on one file.
- Language features (extras): hover docs, completion, signature help, format on save for Go,
  go to line.
- UX: animations on every panel/tab/pane/overlay (Reduce Motion respected), context menus, tab drag
  and drop with split-on-drop, Finder drop, back/forward history, horizontal tab scrolling,
  double-click divider to 50 %, resizable tree and drawer, full-width tree rows, middle-click
  close, tooltips, zoomed-pane frame, reveal active file in tree.
- Git: status colours in tree and tabs, gutter bars, inline blame, Changes tab with stage/unstage.
- Find and replace in project.
- Terminal: find (Cmd+F), mouse reporting, Cmd+K under running programs, Cmd+A.
- File watcher per project (notify/FSEvents).
- Performance: terminal row cache (E1), daemon foreground poll off the lock (E4), non-blocking
  delivery with lagging clients dropped (E5), closed tabs no longer leak a thread (E6), coalesced
  shell redraws (E2), notices capped (E3).
- Hardening (a robustness sweep run before tagging v0.3.0):
  - **Crash and data-loss fixes:** Cmd+/ no longer crashes on lines indented with NBSP or U+3000. Saving through a symlink updates the target and keeps the link. Language servers can no longer crash the editor with overlapping edits.
  - **Unsaved work:** panics are logged to `app.log` with a copy of each unsaved buffer. A Dock quit or logout saves or keeps recovery copies, offered on the next launch. Auto save no longer recreates a file deleted on disk.
  - **Workspace file:** `workspace.json` is fsynced, and only a file that fails to parse is moved aside, to a timestamped copy.
  - **Replace All** acts only on the results the user saw.
  - **Session daemon:** closing a pane hangs up every process on its pty and kills anything that ignores it. Shells no tab ever attached to end after 60 s. No signal is sent to a pid that may have been reused. A client dropped for falling behind gets a short replay.
  - **Language servers** restart after a crash, with backoff, and reopen their files. Writes to a server go through one thread.
  - **Smaller fixes:** git output waits have a time limit. App-socket replies are trimmed to fit the frame limit. A closed project's state is cleared.

## Your requests

| # | Request | Status | What was done |
|---|---|---|---|
| 1 | Terminal colours for Claude Code | done (0.2) | Palette matched to Claude Code, dim text readable, box glyphs on the grid. |
| 2 | Terminal title auto rename | done (0.2) | Tabs use the OSC 0/2 title, so Claude tabs show the session name. |
| 3 | YAML / JSON / Dockerfile / .env / .md / .mmd | done (render), `.mmd` source highlighting not done | Highlighting for YAML, JSON, Dockerfile, .env and Markdown; `.mmd` renders in the preview but its source opens as plain text. |
| 4 | New file opens only in the last window | done (0.2) | One window exists; the bug was pane choice. Opens in the editor pane used last. |
| 5 | Smooth animation everywhere | done, unverified on screen | Drawer (containers, Playwright, notifications, search), tree, tabs, panes, palette, usage popover, toasts, find bars, menus. |
| 6 | Right click | done, unverified on screen | Menus on tree rows, tabs, editor and terminal. |
| 7 | File drag and drop | done, unverified on screen | Finder drop opens folders as projects and files as tabs. Dragging tree rows to move files was not built. |
| 8 | Go to where it's called | done (0.2) | Definition fixed (LSP errors were swallowed); find references added. |
| 9 | Search whole project | done, UI unverified on screen | Cmd+Shift+F Search tab, plus Replace All. |
| 10 | Render .md / .mmd | done | WKWebView preview with Mermaid 11.17.2; checked in WebKit, not in the app on screen. |
| 11 | Open jpg / png / svg | done | Image viewer, QA screenshots taken. |
| 12 | Hover the full file/folder row | done, unverified on screen | Rows take the list's full width. |
| 13 | Git blame + git integration | done, unverified on screen | Tree/tab colours, gutter, inline blame, Changes tab. Deleted files appear only in the Changes tab. |
| 14 | Auto save + unsaved indicator | done | 1 s after typing; markers on tabs, tree rows, window title. Checked with synthetic keys. |
| 15 | Code folding by brackets | done | Bracket regions, indentation fallback; checked in a QA window. |
| 16 | Mouse back / forward | done, unverified on screen | Buttons 4/5, `ctrl--` / `ctrl-shift--`; whether your mouse sends 4/5 is untested. |
| 17 | Drag tabs between panes / windows, split on drop | partial | Between panes with split on the edges: done (unverified on screen). Between windows: not built, Athena has one window. |
| 18 | Horizontal tab scrolling | done, unverified on screen | Wheel scrolls the strip; the active tab is scrolled into view; edge fades. |
| 19 | Double-click divider to 50 % | done, unverified on screen | 120 ms ease; whether terminals flicker during it is unchecked. |
| 20 | Resize containers / Playwright / notifications | done, unverified on screen | They are one drawer: drag its top edge. Tree width too. Sizes persist. |
| 21 | Claude usage shows 0 % | done (0.2) | Live-checked: 5 h 39 %, 7 d 31 %. |
| 22 | Richer per-language colours | done | 23 token classes; QA screenshots for 14 languages. |
| 23 | File-type icons | done | 62 seti-ui glyphs, tinted from the palette. |
| 24 | Performance / leaks | partial | Thread leak and daemon stalls fixed and measured; rendering cost not measured (screen locked). |
| 25 | CI caching | done (0.2) | Release workflow shares the CI cache. |
| 26 | Prod "terminal not available" | done (0.2) | Single daemon spawn on cold launch, checked on the installed app. |
| 27 | README with screenshots | partial | v0.2 README with three screenshots; v0.3 text updated, no new screenshots and no Claude Code one. |

## Decisions made without you

From the running log:
- Usage `utilization` is treated as 0–100 everywhere; the profile is cached 30 min, bypassed by
  Refresh.
- OSC titles older than 1 s are dropped when the foreground program changes (clearing on every
  change lost titles).
- Box drawing and block elements are drawn as rectangles; rounded corners come out square.
- "Open to the side" splits even if the file is already open (VS Code behaviour).
- Stale-session **Keep** is per tab.
- A bare `athena` inside an `.app` hands off to Launch Services, so shell variables no longer
  reach the app; the README documents `open -a Athena --env …`.
- Markdown headings are bold blue; a fold hides the lines between brackets (closing brace stays
  visible); Fold All folds the outermost regions; chevrons show on gutter hover.
- Cmd+Shift+V opens the preview beside the editor; folders get the seti folder icon; Mermaid
  11.17.2 is vendored.

From the commits:
- Two tabs on one file share one buffer (VS Code's model), replacing the logged fallback
  ("sibling tabs reload on save").
- Format on save starts on for Go only; "Toggle format on save" (palette, File menu) switches it
  on or off for every language with a server. Auto save never formats; a formatter slower than
  1 s is skipped and the file saves unformatted.
- Closing a dirty tab while auto save is on saves quietly instead of asking.
- Moving a file to the Trash keeps tabs with unsaved edits open.
- An inline tree name field commits when you click elsewhere in the same project, as in VS Code;
  scrolling it away, hiding the tree or switching projects cancels it.
- Close Project saves unsaved files quietly when auto save is on, otherwise asks like Quit.
- Inline blame is off by default (`cmd-alt-shift-g`, chosen because `cmd-alt-g` is go to
  definition).
- Git runs as `/usr/bin/git` with `--no-optional-locks`, literal pathspecs, `core.fsmonitor=false`
  and a 30 s kill; a status slower than 1 s switches that repo to the cheaper untracked listing.
- Project search is literal and smart-case (no regex); Replace All keeps `$` literal and asks
  first.
- Terminal: copy-on-select stays off; Shift bypasses mouse reporting; find bar match-case and
  regex are off by default and Enter walks to older matches.
- Navigate forward is bound as `ctrl-_` because that is what macOS reports for ⌃⇧-.
- The double-click divider ease is the one size animation allowed over live terminals.
- "No definition found" and similar lookups are transient toasts, not saved notifications.
- Hardening:
  - A project whose folder is missing at launch is dropped from the list, but its shells are left running in the daemon. A temporarily unmounted volume should not cost live sessions.
  - Shells are ended automatically only when no tab ever attached to them within 60 s.
  - Recovery copies live under `~/Library/Application Support/athena/recovery/`. The newest 10 offered sessions are kept.
  - Cmd+S on a file deleted on disk shows the bar instead of silently recreating it, as VS Code would.
  - Hard-linked files are rewritten in place, so links survive but that save is not atomic.
  - A language server that stops 5 times within 3 minutes stays stopped.

## Corrections to the running log

The scratch log written during the work is out of date on these points; the code and commits say:
- Shared buffer per path: done (`55efc42`), not "NOT done".
- `athena <path>` with `/tmp` vs `/private/tmp` duplicating projects: fixed (`0f8d7a1`).
- LSP `didChange` versions going backwards with two tabs on one file: fixed by the shared buffer
  (`55efc42`).
- Closing a dirty tab inside the auto save window still prompting: fixed (`38852fa`).
- Flaky daemon tests: fixed (`74a8bfa`), see Incidents.

## Incidents

- During the `a-daemon` lane, one synthetic click (about 14:52) landed in your foreground window
  while you were active.
- The `c-prims` trash test, run once by hand, left `athena-trash-test.txt` in your Trash.
- Two daemon tests were flaky (`socket_is_owner_only_and_second_daemon_refuses`,
  `zsh_integration_reports_finished_commands`): the tests connected before the socket was
  listening. Fixed in `74a8bfa` (3 of 30 runs failed before, 40 of 40 passed after). One
  unexplained failure in the `a-daemon` gate was seen once and not reproduced.

## Unverified items

Checked with QA screenshots: syntax colours for 14 languages, file icons, image viewer, folding,
Markdown/Mermaid pages rendered in WebKit (outside the app), the context-menu primitive through a
temporary hook.

Checked with synthetic keys and logs only: auto save and the conflict bar, completion with an
import, signature help, format on save, hover fetches, git status/diff runs.

Tests or build only, never seen on screen:
- every animation (drawer, tree, tabs, panes, palette, usage popover, toasts, find bars), and the
  double-click divider ease;
- context menus in the app (mouse hover, click, outside click);
- tab drag and drop, Finder drop, tab strip scrolling and its edge fades, resize handles, full-width
  tree row hover;
- the preview tab in the app, Save As, the conflict bar's look;
- git colours in the tree and tabs, gutter bars, the blame caption, the Changes and Search tabs;
- completion, hover and signature popovers, the go-to-line box;
- the terminal find bar and mouse reporting in a real program;
- mouse buttons 4/5, tooltips, the zoomed-pane frame;
- the E1 terminal row cache (no stale rows after scrolling, selecting, resizing);
- the post-review shell fixes (per-pane fades, Cmd+W during a fade, drawer focus hand-back, Close
  Project prompt, previews hidden under editor/terminal menus, fading overlays ignoring clicks),
  checked by build, clippy and unit tests where they have them;
- usage numbers in the live title bar against `/usage`.

## Performance

Full method and numbers: [perf/2026-10-v0.2.md](perf/2026-10-v0.2.md). Ten terminals, release
builds, 5 min idle then 5 min printing about 40 KB/s, screen locked.

| | v0.2.0 | v0.3 (e-perf) |
|---|---|---|
| App CPU idle / printing | 0.00 % / 0.34 % | 0.00 % / 0.36 % |
| Daemon CPU idle / printing | 0.08 % / 0.16 % | 0.07 % / 0.17 % |
| Closed tab's reader thread | leaked forever | exits |
| A client that stops reading | stalls other clients on the pane | hung up after 2 s; others keep flowing |

Thread counts were flat in both phases; the daemon's memory grew only as scrollback filled towards
its 8 MB cap, and the small app footprint difference at launch is allocation noise, not these
changes. Rendering was not measured: a locked screen draws no frames. The doc lists the commands
to repeat it on an unlocked machine (`ATHENA_LOG=info,athena::render=trace`, then count
prepaints and `rebuilt=` rows in `app.log`). The plan's 10-minute run with a live Claude
session and the `xctrace` leak pass were not done.

## Known issues and backlog

All findings of the v0.3 review (`v0.2.0..c30b577`) are fixed on `main`:
- Replace All scope and Cancel (`64c3cb2`), literal git pathspecs and fsmonitor off (`8b074cb`),
  git timeout (`47f69de`), git churn from ignored build output (`f9dd330`), untracked folders in
  Changes (`6af4191`);
- per-pane content fade (`0110fd9`), Cmd+W during a fade and per-pane fade tracking (`847c298`,
  `1cde693`), drawer focus (`a5c266c`), Close Project prompt (`3d7f556`), previews under editor
  and terminal menus (`c3ca7ea`), tree name field blur (`576eb06`), fading overlays ignoring
  clicks (`67049c3`), divider ease keyed by project (`a3d7c54`);
- `..` in Markdown links (`011fb1a`), tabs that failed to open retrying and then reaching the
  language server and git (`90bb92d`, `8f86060`);
- the plausible ones: terminal find drift once scrollback is full (`55bdb62`), Cmd+K and scroll
  regions (`67096db`), reload copies on the UI thread (`a84eb9d`), preview re-encoding images
  (`81a3e19`).

Still open:
- Toasts can sit under a web preview (the preview-hiding check counts menus, the palette and the
  usage popover, not toasts).
- Rust `ALL_CAPS` constants are coloured as types (upstream query); Markdown inline spans are not
  highlighted (block grammar only); `.mmd` source has no highlighting.
- Tree-row drag to move files and multiple windows are not built.
- Rendering performance is unmeasured (see Performance).

Backlog candidates from the plan: none left; rename symbol, document symbols, the problems panel,
the diff viewer and the commit UI shipped in v0.4.0, and multi-cursor, word wrap, the light theme
and the keymap file in v0.5.0.

## What to check when you're back

1. `brew upgrade --cask athena`, launch from the Dock: terminals attach, no "unavailable".
2. Cmd+B, Cmd+J, Cmd+Shift+P, Cmd+F: each fades in and out without a jump.
3. Right-click a tree row, a tab, the editor and a terminal; Escape and click-away close them.
4. Drag a terminal tab to another pane's right edge: it splits and the shell keeps running.
   Drop a folder from Finder onto the window.
5. Edit and save a Go file: tab and tree row turn amber, gutter bars appear, extra spaces are
   formatted away. Cmd+Alt+Shift+G shows blame. View > Source Control Changes lists it.
6. Type `strings.` in Go: suggestions appear; rest the pointer on a call for hover docs.
7. Cmd+Shift+F, type a word, Enter opens the match.
8. In a terminal run `seq 1 5000`, Cmd+F `4999`; run `htop` (or `vim` after `:set mouse=a`)
   and click inside it.
9. Mouse back button after jumping to a definition.
10. Open `README.md`, Cmd+Shift+V: preview beside it. Open a PNG.
11. Double-click a divider between two terminals: no flicker. Scroll a tab strip with 15 tabs.
12. Leave a file unsaved with auto save off and press Cmd+Shift+W: it asks first.
13. Compare the title-bar usage with `/usage` in Claude Code.
14. Empty `athena-trash-test.txt` from the Trash.

For v0.4.0:

15. In a project, run **Enable Claude Code hooks for this project** again (older installs lack the
    edit hooks), start Claude and have it edit a file: "Claude edited …" appears, **Review diff**
    shows "Before Claude" against the file, and Revert on one change undoes just that change.
16. Edit a Go file, open the Changes tab and click its row: a diff tab opens. Stage one change,
    step with Alt+F5 (with Fn), switch to Inline, then commit with Cmd+Enter; turn Amend on and off.
    Click the branch in the status bar: the branch picker opens.
17. In a Go file: F2 on a function used in another file renames both; remove an import and press
    Cmd+. on the error (the gutter dot should show); Cmd+Shift+O lists symbols; Cmd+F12 on an
    interface method; save and check imports are organized.
18. Select three lines and press Tab, then Shift+Tab; type `(` and `"`; Alt+Up a line; Cmd+Alt+F
    and replace a word; Cmd+= twice then Cmd+0 in an editor and a terminal. Look at the indentation
    guides and the status bar, and click its indentation and language.
19. In a terminal run a failing `go test ./...` and Cmd+click the `_test.go:NN` line; open the
    Problems tab with Cmd+Shift+M and step with F8.

For v0.5.0:

20. Run **Toggle Claude Code integration**, open a new terminal and start `claude` (if the tab
    belongs to a daemon from 0.4, type `/ide`). Ask for an edit in default permission mode: the
    proposal tab opens; Cmd+Enter accepts and Claude writes the file, Cmd+Backspace rejects. Select
    lines in an editor, then Cmd+Alt+K. Try once in an auto-accept mode and note what happens.
21. In an editor: Cmd+D three times, Cmd+Shift+L, Cmd+Alt+Down, Alt+click, Shift+Alt+drag; type and
    undo. Alt+Z on a long line; scroll a nested file and look for the pinned headers.
22. Switch macOS to Light: Athena follows; check the terminal colours under Claude Code. Ctrl+R
    outside a terminal; edit `keymap.json` with a bad entry and watch for the toast. In zsh, run a
    failing command: red dot, Cmd+Up jumps to it.
23. Quit with folds, a scrolled editor and wrap on in one tab, relaunch: all three come back.
