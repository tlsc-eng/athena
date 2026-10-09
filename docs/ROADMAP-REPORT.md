# Roadmap report: v0.2.0 to v0.10.0

What happened while you were away, for the roadmap in `piped-orbiting-stearns.md` (Phases A–E).

- Release: v0.2.0 — https://github.com/tlsc-eng/athena/releases/tag/v0.2.0 (tap `f65669b`,
  installed here with `brew upgrade --cask athena`)
- Release: v0.3.0 — https://github.com/tlsc-eng/athena/releases/tag/v0.3.0 (tap `5f58c02`,
  installed here with `brew upgrade --cask athena`)
- Release: v0.4.0 — https://github.com/tlsc-eng/athena/releases/tag/v0.4.0 (tap `7aa6afb`, installed here)
- Release: v0.5.0 — https://github.com/tlsc-eng/athena/releases/tag/v0.5.0 (tap `b75ca95`, installed here)
- Release: v0.6.0 — https://github.com/tlsc-eng/athena/releases/tag/v0.6.0 (tap `f79bd1b`, installed here)
- Release: v0.7.0 — https://github.com/tlsc-eng/athena/releases/tag/v0.7.0 (tap `e001a6b`, installed here)
- Release: v0.8.0 — https://github.com/tlsc-eng/athena/releases/tag/v0.8.0 (tap `2a0ce2a`, installed here)
- Release: v0.9.0 — https://github.com/tlsc-eng/athena/releases/tag/v0.9.0 (tap `fa17d03`, installed here)
- Release: v0.10.0 — https://github.com/tlsc-eng/athena/releases/tag/v0.10.0 (tap `8c3a65b`, installed here)

The screen was locked for most of the work after v0.2.0; it became unlocked only near the end of
v0.5. For v0.6 the lanes ran GUI QA with synthetic keys only, while you were idle, so anything
that needs a mouse click is unverified. v0.7 was the same, except that the git and code lanes and
the fix sweep could take screenshots with the screen unlocked; the workbench lane ran locked
until its last two captures. v0.8's lanes and fix sweep ran their QA apps in isolated `HOME`s
with synthetic keys while you were idle and captured them on screen, and the installed v0.7.0 got
a visual QA sweep of its own. v0.9's lanes ran their QA apps the same way, but the screen was
locked for most of them, so few GUI checks happened, and Delve never ran a program end to end on
this Mac (see the v0.9.0 incident). v0.10's lanes ran while you were active at the machine, so
its idle-gated GUI sweep never ran. Everything below marked **unverified on screen** is
covered by unit tests, logs or synthetic-key runs, but nobody has looked at it. v0.10.0 is the
roadmap's last milestone: start with the [handover](#handover-where-things-stand-after-v010),
then [What to check when you're back](#what-to-check-when-youre-back).

## Handover: where things stand after v0.10

The autonomous roadmap ends with v0.10.0; the checking is yours from here. Each release added:
- v0.2: one session daemon on cold launch, stale sessions, logs, OSC titles, find references.
- v0.3: 12 more languages, hover and completion, animations, menus, drag and drop, git colours.
- v0.4: indent and auto-pairs, rename, symbols, Problems, code actions, diff tabs, commit box.
- v0.5: Claude Code IDE integration, multi-cursor, word wrap, light theme, keymap.json.
- v0.6: settings.json, find options, inlay hints, breadcrumbs, tests, tasks, conflicts, push/pull.
- v0.7: terminal panel, tree moves, bracket colours, interface zoom, diff peek, blame, outline.
- v0.8: Claude tab, project trust answer, worktrees, GitHub CI and PRs, coverage, project settings.
- v0.9: Go debugger on Delve, semantic tokens, code lens, snippets, encodings, minimap.
- v0.10: several windows, Settings and Keyboard Shortcuts tabs, checks for Athena's JSON files.

Check by hand first, in this order (numbers are [check list](#what-to-check-when-youre-back) items):
1. The trust prompt with a real keyboard: Return does nothing, Escape is Don't Allow (42).
2. Developer Mode on and Delve installed, then debug a Go test end to end (51, 52).
3. Several windows: move, merge, close one with a running shell, quit and relaunch (58, 59).
4. Drag and drop and right-click menus: tabs, panes, tree rows, a Finder drop, the rail (3, 4, 33).
5. The light theme everywhere, and the terminal under Claude Code in it (22).
6. The Settings and Keyboard Shortcuts tabs, and the JSON checks (60 to 62).
7. Claude Code integration on, with a real Claude session: accept and reject a proposal (20).
8. **GitHub: Create pull request** on a real repository (45).

Ready-to-run QA scripts (idle-gated, isolated `HOME`, synthetic keys) are in a temporary folder
that may be gone: `qa/x10-qa/burstA.sh`…`burstE.sh` and `qa/vqa/` helpers under
`/private/tmp/claude-501/-Users-json-code-hobby-athena/60cc5e42-8a82-4a22-a06a-39cd285d51a8/scratchpad/`.

Known open gaps (details in each release's section):
- Never seen working here: Delve end to end, Return on the trust prompt, typescript-language-server
  on a real server, a real Jest run, `openDiff` in auto-accept modes, the 𝑥 glyph, render cost.
- Windows: Settings and Shortcuts tabs are not restored; moving a project ends its debug session
  and restarts its servers; a v0.9 build forgets closed windows' projects (their shells run on).
- No debugger attach; lenses sit after the line; only Japanese legacy encodings are detected; no
  selection in the large-file view; filter drivers inside submodules stay on; cross-volume tree
  moves block the UI; the diff peek covers lines; Timeline stops at 500 commits.

Incidents, whole run: a synthetic click landed in your window and `athena-trash-test.txt` was left
in your Trash (v0.2/v0.3); the trust prompt's Return/Escape run may have met a click of yours
(v0.8); a Delve probe raised the administrator-password dialog, dismissed with Escape, nothing
typed or authorised (v0.9). None in v0.10.

## v0.10.0

Plan: [plans/v0.10.md](plans/v0.10.md), two feature lanes (several windows; settings and shortcut
editors) and a QA lane (performance re-baseline and small known gaps), `v0.9.0..main`, then a
review pass and one fix sweep. This is the roadmap's last milestone. The v0.9.0 section follows
this one.

### What shipped

**Several windows** (`fad570a`, `5018242`, `6cfeef7`, `204af9f`)
- Each window has its own rail, layout and drawer. New Window (Cmd+Shift+N), Open Project in New
  Window…, Move Project to New Window (also on the rail's right-click menu), Merge All Windows and
  Close Window, in the menus and the palette. A project is open in one window at a time; opening
  it again (Open, Open Recent, `athena <folder>`, a drop) brings that window forward.
- An app-level coordinator owns what the windows share: the app socket (CLI and MCP requests go to
  the window holding the file, session or caller's project, and lists are joined from every
  window), the daemon's notices and banner clicks, the Claude Code IDE server with one lock file
  listing every window's folders, the Notifications list, and `workspace.json`.
- Moving a project keeps its terminals' shells and its editors' unsaved text. Closing a window
  that is not the last parks its projects with their tabs and running shells and lists them in
  Open Recent, where reopening reattaches them; closing the last window quits as before, and
  quitting asks each window about its unsaved files in turn.
- `workspace.json` gains a `windows` list and parked projects; the top-level fields repeat the
  first window's, so a single-window file keeps the old shape and v0.9 still sees every project.
  Fixtures written by the real `save()` of every release from v0.2 to v0.9 open as one window and
  save back byte-identical.

**Settings and Keyboard Shortcuts tabs** (`7701743`, `d174138`, `c86b898`)
- One schema (`settings/schema.rs`) describes each setting: title, description, control, default,
  VS Code spellings and whether only the user's file may set it. It generates settings.json's
  JSON Schema, and tests hold it to the commented template and to what Athena does by default.
- Cmd+, opens a Settings tab: every setting with a checkbox, number field, dropdown or a link to
  the JSON for structured ones, searchable, with Modified markers, Reset, and User and Project
  scopes; the Project scope leaves out app-wide settings. Writes go through the comment-keeping
  writer, project ones to `.athena/settings.json`.
- Cmd+K Cmd+S (outside terminals) and File > Keyboard Shortcuts open a Keyboard Shortcuts tab:
  every command with its keys, context and source, searchable. Double-click, Return or the
  right-click menu records up to two keystrokes and shows the commands already on them; Remove,
  Reset and Copy Command ID are in the menu, and keymap.json's unusable entries are listed above
  the table.
- settings.json, a project's `.athena/settings.json` and keymap.json show Athena's own problems in
  their editors as they are edited. When `vscode-json-language-server` is on the login `PATH`
  (optional, never installed), JSON files open in it and those three get generated schemas.

**QA lane** (`4d98fef`, `058f29a`, `1dd6dca`, `23b2644`, `83a4173`, `e7e870d`)
- The performance re-baseline below, recorded in
  [perf/2026-10-v0.10.md](perf/2026-10-v0.10.md).
- Small known gaps closed: a Claude Proposal diff reads the file on disk in its own encoding
  (`1dd6dca`); a long completion detail is cut at its right edge (`23b2644`); a language server
  replaced by Allow or Disallow project code exits at once instead of living on while editors held
  its code lens answers or last suggestions, and lenses come back from the restarted server
  (`83a4173`, `e7e870d`).

Screenshots from the UI lane's isolated QA run: [the Settings tab](screenshots/v0.10/01-settings.png),
[the Keyboard Shortcuts tab with an entry Athena cannot use](screenshots/v0.10/02-shortcuts.png),
and [settings.json's problems in the light theme](screenshots/v0.10/03-json-problems.png) (the
window was in the background, hence dimmed).

### Decisions made without you

- **One window per project**, as VS Code does: opening a project another window has focuses that
  window instead of opening a second copy.
- **Settings and Keyboard Shortcuts tabs are not saved.** VS Code restores its settings editor, but
  a v0.9 build reading those tab kinds set the whole `workspace.json` aside and started empty, so
  downgrade safety won. This build also drops tab kinds it does not know instead of failing the
  file, so later releases can add kinds without the same problem.
- **Closed windows' projects keep their shells** (Claude Code sessions included) until they are
  reopened, their folder is gone at launch, or you clear Open Recent, which asks first ("Clear and
  End Terminals"). Parking is not tied to Open Recent's 20 entries.
- **Moving a project starts its tools afresh in the new window**: it ends its debug session,
  rejects Claude's pending proposals for it and restarts its language servers; merging or closing
  a window also stops its debug session and test run. Each window owns its debugger and servers,
  so carrying them across was left out; the README says so.
- Daemon terminals and the Claude Code IDE server are shared app-wide. Parked projects are left
  out of the IDE lock file's folders, since nothing in Athena shows them.
- The JSON server is optional and never installed, and a missing one is not reported. Programs
  are now looked for only in absolute `PATH` folders, for every server and Delve (`4df615a`).
- App-wide settings are hidden in the Project scope, and a project write of one is refused
  (`9a22d54`).
- Keymap edits follow VS Code: a changed default becomes a new binding plus a `-` removal. Each
  edit finds its entry again by key, command and `when` before writing (`b0fa31d`).
- Cmd+K Cmd+S is bound outside terminals only, so Cmd+K in a terminal still clears at once.
- settings.json and keymap.json now share one folder watcher (`94d2bc6`), the follow-up the
  performance notes left for after this release's settings work.

### Review fixes

A review of `v0.9.0..204af9f` found 11 issues (2 high, 3 medium, 6 low, the last a bundle of four
small ones). All are fixed on main:
- High: Settings and Keyboard Shortcuts tab kinds made a v0.9 build set `workspace.json` aside and
  start empty; those tabs are no longer saved and unknown kinds are dropped on load (`1136faa`),
  including the copies the first fix missed, a window's first save and parked projects
  (`80bbdbe`).
- High: every window loaded and saved every project's breakpoints, so a breakpoint deleted in one
  window came back from another's stale copy and the entry for files outside any project
  ping-ponged; each window now loads its own projects' and writes only what it changed
  (`55ded64`, after the lane's `6cfeef7`).
- Medium: removing the last setting or keymap entry deleted the comments before it (`554a9fd`);
  Clear Recently Opened, or 20 newer closed folders, killed parked shells without asking
  (`2a9984b`); a keymap.json reload during a recording panicked or recorded onto another command
  (`dad43fa`).
- Low: a moved project's source window saved late, so a crash could list it in both windows; a new
  window that failed to open dropped the project's unsaved text; proposals with no open tab got no
  answer (all `33242e6`). Parked projects pruned at launch kept their shells (`2a9984b`). What
  Move, Merge and Close Window end was undocumented (`9863729`). The bundle: the IDE lock's
  folders lagged up to 500 ms behind (`aa943a7`), keymap edits used a stale entry index
  (`b0fa31d`), relative `PATH` entries could run a project's own binary as a server (`4df615a`),
  and project writes of app-wide settings were not refused (`9a22d54`).

After the review: windows restored without saved bounds cascade instead of opening on top of each
other (`ab73801`).

### Performance

The QA lane repeated the E0 protocol on the installed v0.9.0, with the v0.2 build as a same-day
control, and added a 30-minute load (a clone of this repository, ten terminals, one printing
Claude-like output, 2 000- and 10 000-line Go editors with gopls, inlay hints, minimap and
semantic tokens): [perf/2026-10-v0.10.md](perf/2026-10-v0.10.md). Nothing in CPU time, dirty
memory, file descriptors or the editor benchmarks was more than 20 % worse, so no performance
change was made. Dirty memory is about 8 % above the control; RSS is 27 % higher at the end of the
idle phase, which is clean pages from a binary twice v0.2's size. Six more threads are folder
watchers, two of which `94d2bc6` removes. Under the 30-minute load memory, threads and files stay
flat (footprint 83 to 84 MB), and `git_status` through MCP answers in 59 ms. These numbers
measure v0.9.0, not v0.10's window and settings code, and rendering is still unmeasured: the
hidden QA window drew no frames after launch.

### Verification

- Unit and fixture tests throughout: `workspace.json` fixtures from every release, a two-window
  split with a parked project, routing and reply-merging, breakpoints saved by two windows in
  turn, parked shells kept past 21 folders, schema against template and defaults, unsets and
  removals across 3000 random files keeping every comment, the recorder and conflicts, keymap
  edits finding their entry after an insertion, and unknown tab kinds dropped on load.
- Against an isolated `HOME`: `list_projects` and `get_open_editors` joined across two windows,
  `open_file` landing in the window holding the file, `athena <folder>` focusing the window that
  has it, the IDE lock file listing all three folders of two windows, and three saved windows
  without bounds cascading. The UI lane drew both tabs in both themes (screenshots above), and
  with the JSON server on `PATH` it flagged `window.zoom_level` over its maximum.
- **No idle-gated GUI sweep ran this milestone.** You were active at the machine throughout, so
  the QA lane's visual sweep of the check list (plan item 1) never got its 300 s idle window and
  took no captures. Nothing marked unverified in earlier releases was looked at, and none of these
  have been seen on screen: moving, merging and closing windows with the mouse, the rail's menu,
  the Clear Recently Opened prompt, recording a shortcut with a real keyboard. The new window
  failure path cannot be triggered from tests.

### Known gaps

- Settings and Keyboard Shortcuts tabs are not restored on launch.
- Moving a project ends its debug session and restarts its language servers.
- A v0.9 build opening this `workspace.json` forgets closed windows' projects; their shells keep
  running until `athena mux stop`.
- Rendering cost is still unmeasured.

## v0.9.0

Plan: [plans/v0.9.md](plans/v0.9.md), three file-disjoint lanes (debugger, language features,
big and foreign files with a minimap), `v0.8.0..main`, then a review pass and one fix sweep. The
v0.8.0 section follows this one.

### What shipped

**Debugger** (`7bf13ea`, `743912c`, `fdb4a78`, `3be09d4`, `46721e4`, `0ed8256`, `7cfcb07`)
- A new `athena-dap` crate speaks the Debug Adapter Protocol on plain threads, as `athena-lsp`
  does: one writer, a reader matching replies by `request_seq`, a watchdog (10 s per request,
  300 s for launch), the adapter in its own process group and a reaper that kills the group and
  removes the session's private scratch folder.
- Go through Delve: `dlv dap --client-addr unix:<socket>`, found on the login shell's `PATH` and
  started through it, never installed. **F5** debugs the first `"type": "go"` entry in
  `.vscode/launch.json` (request, mode auto/debug/test/exec, program, args, env, buildFlags, cwd
  and VS Code's workspace and file variables), else the open file's package, tests for a
  `_test.go` file. Right-clicking a test's ▶ offers Debug Test, and **Debug: debug test at
  cursor** runs only that test or subtest.
- Breakpoints in the gutter (click or **F9**; conditional, hit count, logpoints, disabled and
  unplaced drawn apart), moving with edits, kept per project in `breakpoints.json` beside
  `workspace.json`. A stop opens the paused line (amber band and arrow), loads locals and
  re-expands what was open, and evaluates the watch expressions; hover evaluates while paused.
- A **Debug** drawer tab (Call Stack by goroutine, Breakpoints, Variables, Watch, Debug Console),
  title-bar controls while a session runs, a **Run** menu and eleven **Debug:** palette commands.
  F5, Shift+F5, Cmd+Shift+F5, F6, F10, F11 and Shift+F11 are bound outside terminals (nothing used
  them), F9 in the editor.
- MCP tool `debug_state`: read-only status, stop reason, paused goroutine, location, 20 frames
  and 50 locals, answered from the last stop, so it never resumes or steps the program.

**Language features** (`71e55b1`, `fb485c5`, `74bb734`, `5475b1c`, `c5759a1`, `e908bbd`, `fdef526`)
- Semantic tokens (full and delta) from gopls and typescript-language-server over the tree-sitter
  colours, names only; parameters and type parameters got theme colours in both themes. Decoding
  and placement run off the UI thread; keystroke cost is unchanged within noise on 2 k- and
  10 k-line files ([perf/2026-10-v0.9-semantic.md](perf/2026-10-v0.9-semantic.md)).
- Code lenses for the lines on screen, resolved together, drawn muted after the line; a click
  runs the command through `workspace/executeCommand` or lists a lens's references.
- User snippets from `snippets/<language id>.json` and `*.code-snippets` in the app support
  folder, in VS Code's format with its variables and transforms, after the server's suggestions;
  **Snippets: configure …** in the palette.
- Inlay hints on wrapped lines, each on the row it belongs to, dropped when they would push code
  past the edge.

**Big and foreign files, minimap** (`58bb2c8`, `86fa17e`, `2ab0591`, `0f293fa`, `c17b818`, `68c6b4d`)
- Encodings through `encoding_rs`: UTF-8 with or without BOM, UTF-16 LE/BE with BOM, Shift JIS and
  EUC-JP by their kana, else Windows 1252; saved back byte for byte, refused when the bytes do not
  round-trip. Reopen / Save with Encoding from the status bar (14 encodings).
- Git views decode every version in the file's encoding, and hunk stage, unstage and revert
  write it back in that encoding only when both sides agree and decode without loss. UTF-16 files
  get gutter marks from an in-process line diff numbered as `git diff -U0` numbers them.
- Files over 50 MB open in a read-only large-file view (positional reads, a sparse background
  line index, find over the whole file in 4 MB blocks); over 2 GB and binary are still refused.
- A minimap (on by default, `editor.minimap.enabled`, **Toggle Minimap**), coloured from cached
  highlight spans: about 4 µs a cached frame and 0.8 ms to recompute 300 rows on a 10,000-line
  file in release.

Screenshots from the QA builds: [semantic colours, a gopls code lens, inlay hints on a wrapped line
and the minimap](screenshots/v0.9/01-semantic-lens.png), [a user snippet in the suggestions with
its body beside the list](screenshots/v0.9/02-snippet.png), and [two breakpoints in the gutter
before any launch](screenshots/v0.9/03-breakpoints.png) (the window was in the background, hence
dimmed; no session ran, see Verification).

### Decisions made without you

- **Delve dials in.** `dlv dap` has no stdio mode, so the client listens on a Unix socket in the
  session's scratch folder and Delve connects with `--client-addr`. Delve is started as
  `$SHELL -lc 'exec "$0" "$@"' dlv …` so goenv-style shims work; `$SHELL` is used only when it is
  sh, bash, zsh, ksh or dash, since fish and nu cannot run that line, and `/bin/zsh` otherwise
  (`f7ff64b`).
- **Debugging and lens commands sit behind the project trust answer.** Debugging builds and runs
  project code, so it uses the v0.8 "project code" answer, asked before launch.json is even read,
  with the Cancel-first button order. The review extended the gate to code lens and code action
  server commands (`47f6a92`); reference lenses still list at once.
- **Delve's binary goes in the scratch folder**, not the project, and the program's output comes
  as DAP events (`outputMode remote`) for the Debug Console. Attach is refused for now.
- **Developer Mode is not touched.** Athena never runs `DevToolsSecurity`; a launch macOS holds for
  a password says in the Debug Console how to stop it asking, and the README documents it.
- Breakpoints and watch expressions live in their own `breakpoints.json` beside `workspace.json`,
  with paths relative to each project, written through a temp file and rename (`39cf51b`).
- A Delve `continued` event arrives before every step's reply (`sendStepResponse` in Delve
  1.27.1), which cleared the stack on each F10; steps now ignore it until the next stop
  (`7cfcb07`).
- **Code lenses are drawn after the line, not above it** as the plan said (and VS Code does): they
  reuse the merge-conflict actions' drawing after the text, so no row is added. See Known gaps.
- Semantic highlighting, code lens and the minimap are on by default, as in VS Code; VS Code's
  `"configuredByTheme"` reads as on, and gopls gets `semanticTokens: true` unless its settings set
  it. Only names take semantic colours, so keywords, strings and comments keep tree-sitter's finer
  classes.
- Snippet transforms follow VS Code's own format parser (`430f623`) rather than Rust's
  `Regex::replace`; variables inside a choice stay literal, as VS Code keeps them.
- **The large-file view reads with `pread`, not a memory map**, so a file truncated underneath it
  cannot fault the app. The 50 MB editor limit stays; the view has no selection or copy.
- Detection is deliberately narrow: only Japanese multibyte encodings are guessed (by kana), and
  everything else that is not UTF-8 or BOM'd UTF-16 opens as Windows 1252, which round-trips any
  byte, so nothing is lost before you pick the right encoding.
- Files with a `working-tree-encoding` attribute are refused for hunk operations like filtered
  files (`530fd73`); a hunk is staged with the byte order mark the index's copy has (`d314251`).

### Review fixes

A review of `v0.8.0..7cfcb07` found 15 issues (no critical or high: 8 medium, 3 low-medium,
4 low) plus six minor ones and three follow-ups the debugger lane left. All are fixed on main:
- Code lens and code action server commands (go generate, run test, go mod tidy) ran project
  code without the trust answer (`47f6a92`).
- A semantic tokens delta whose edit end overflowed panicked (`3958c15`).
- A failed launch or a finishing stop could end a newer session (`39cf51b`); closing a project did
  not stop its session (`39cf51b`).
- Quitting left Delve and its scratch folder behind while clones of the client lived (`f7ff64b`,
  `39cf51b`); `$SHELL` under fish or nu broke Delve's start (`f7ff64b`).
- A wrapped minimap line cost characters times breaks: 57 s for a 1 MB line in a debug build, now
  about 90 ms (`47bdc8b`).
- The Debug Console had no line cap (a `\r` progress bar grew without end) and laid out 5000 lines
  per event; now 4 KB lines, events batched per update and a `uniform_list` (`39cf51b`).
- Low-medium: invalid UTF-8 from Delve ended output forwarding (`f7ff64b`); `breakpoints.json` was
  written in place, so a truncated file reset to defaults and the next save erased it; it is now
  renamed into place and a corrupt one kept aside (`39cf51b`); snippet transforms went to
  `Regex::replace` (`$1_test` read as a group name, no `/upcase`, flags ignored) (`430f623`).
- Low: `working-tree-encoding` files were peeked and staged in a guessed encoding (`530fd73`);
  session generations restarted at 0 (`39cf51b`); Restart skipped the trust check, disallowing did
  not stop a session, and `debug_state` did not shorten names (`39cf51b`); `killpg` could hit a
  recycled group after the leader was reaped (`f7ff64b`).
- Minor: stale semantic colours after a null or failed reply (`f62ce86`); the minimap kept old
  colours when tokens were re-sent for the same text (`5ea3706`); the large-file view ended lines
  only at LF (`5b4f7d2`); a clipboard with a comma split a snippet choice (`430f623`); a doc
  comment had drifted (`47f6a92`); staging a peeked hunk failed with a misleading message when
  the index's copy differed in its byte order mark (`d314251`).
- Follow-ups from the debugger lane (`39cf51b`): a breakpoint Delve verifies on another line now
  moves there; a panic shows the first frame under the project root rather than
  `runtime/panic.go`; Go test rows in the Tests tab get a Debug link.

Within the lanes, before the review: newly opened editors showed no breakpoints and steps cleared
the paused marks (`7cfcb07`); the minimap was not recoloured on a language change and pointer
hovers over it asked the server about the text beneath (`0f293fa`); semantic tokens were appended
out of order for the minimap (`e908bbd`); inlay hints pushed unwrapped rows past the edge while
wrapping (`fdef526`).

### Verification

- Unit tests throughout, with fake adapters (an in-process socket pair, `/bin/sleep` and `nc`
  dial-in scripts, one that exits early, one writing a `\377` byte), launch.json parsing and
  substitution, breakpoint edit-following, session hand-over between two sessions, a corrupt
  `breakpoints.json` set aside, a 5000-update `\r` line capped, and a `debug_state` reply with
  5000-character names kept under one 64 KiB app frame. The fix commits report the full gate.
- **Delve was never run end to end on this Mac.** The Delve integration test (a breakpoint in a
  temp module, a local and a struct field, an evaluate, a step, running to the end) skips when
  Developer Mode is off, because launching then asks for an administrator password, and it is off
  here. The debugger lane's QA app got as far as breakpoints in the gutter (screenshot above),
  a capture after a relaunch with the same two breakpoints, and `debug_state` answering `not_debugging` through
  `athena mcp-stdio`; the captures named "paused" and "stepped" show the same unpaused window, so
  no stop, step, Debug tab, hover value or title-bar control has been seen on screen.
- gopls 0.23 integration tests: semantic tokens classify a function, a parameter and a read-only
  constant before and after an edit; code lenses (go generate, and the test lens when enabled)
  resolve and run. The language lane's QA app showed them on screen in both themes (screenshots
  above), with a snippet suggested and expanded and inlay hints on a wrapped line.
- Encodings: round trips for UTF-16 LE/BE, Shift JIS, EUC-JP, all 255 non-zero Latin-1 bytes and
  UTF-8 with BOM, edit-and-save byte comparisons, refusals; Shift JIS and UTF-16 diffs staged
  against real repositories with the index holding exactly the bytes on disk; UTF-16 gutter hunks
  matching `git diff -U0`. Large files: a sparse 100 MB file (index in steps, paging around a hole,
  find both ways and wrapping, a mixed CR/CRLF/LF file), a 2 GB + 1 byte refusal. Minimap
  geometry, block and budget tests. None of the files lane's work was looked at on screen.
- Unverified: most GUI checks, because the screen was locked for most of the lanes' QA (the
  encoding picker, the large-file view, the minimap's dragging, the trust prompts for Debug and
  for a lens command, the Run menu); typescript-language-server was not installed, so its
  semantic tokens and code lenses are untested against a real server.

### Known gaps

- Code lenses are drawn after their line, not on a row above it as in VS Code.
- Cyrillic, Chinese and Korean files are not detected (only Japanese is) and open as Windows 1252;
  **Reopen with Encoding** fixes them per file.
- The large-file view has no selection or copy.
- typescript-language-server's semantic tokens and reference/implementation lenses have not run
  against a real server.
- The debugger has not run end to end here (see Verification); attach is not supported.

### Incident

- During the debugger lane's QA, a Delve probe started a launch while the screen was locked, and
  macOS put up its administrator-password dialog (Delve taking control of a process without
  Developer Mode). An agent dismissed the dialog with Escape. Nothing was typed into it and
  nothing was authorised, and Developer Mode was left as it was.

## v0.8.0

Plan: [plans/v0.8.md](plans/v0.8.md), three file-disjoint lanes (Claude workspace, worktrees and
GitHub and tests, language features), `v0.7.0..main`, then a review pass, a visual QA sweep of the
installed v0.7.0 and one fix sweep for both. The v0.7.0 section follows this one.

### What shipped

**Claude workspace** (`f5a9b08`, `eb589cb`, `592d45a`, `6c3224a`, `23b510f`, docs `6677482`)
- A **Claude** drawer tab (palette: **Claude: Show sessions**) listing the active project's recent
  Claude Code sessions from `~/.claude` and every `~/.claude-*` profile, newest first: title, todo
  progress, message count, tokens and an estimated cost. A session expands to its todos and plan
  and to every file it changed with +/− counts; a file opens its "Claude's Edits" diff, **Review
  All** steps through them (palette: **Claude: Review next / previous changed file**, which also
  starts a review of the newest session), and **Revert** puts one file back as it was before the
  session after a confirm, keeping a copy in `discarded/`.
- A new `PostToolUse` hook on `TodoWrite|ExitPlanMode` (`athena notify --event claude-plan`)
  feeds the todos and plan, and the terminal tab's badge shows "3/7" with the list as its
  tooltip. The installer merges it beside the existing hooks and keeps the user's own; **existing
  installs must run Enable Claude Code hooks for this project again** to get it.
- **Resume** opens a terminal tab in the project running `claude --resume <id>`, with
  `CLAUDE_CONFIG_DIR` set for a profile other than `~/.claude`.
- Token use comes from the transcripts (each streamed reply counted once, subagents included,
  read on incrementally); cost is an estimate from a built-in list-price table, which
  `"claude": {"prices": …}` in settings.json overrides per model id. Unknown models show "cost n/a".
- Eight MCP tools, additive: `lsp_definition`, `lsp_references`, `document_symbols` (from the live
  server, unsaved edits flushed first, for files open in Athena only), `get_open_editors`,
  `read_buffer` (256 KiB by default, 512 KiB at most), `run_tests` / `get_test_results` (a Tests
  panel run, `TestX/sub` names a subtest) and `git_status` (at most 2000 files). Every list is
  capped below the 1 MiB frame.

**Worktrees, GitHub and tests** (`7a0b99c`, `a458544`, `83d512f`, `e0f65d9`, `f554b59`)
- Worktrees: **Git: Open / Create / Delete worktree…** and **Create worktree…** in the branch
  picker. A new one goes beside the main worktree as `<repo>-<branch>` and opens as a project;
  the rail groups linked worktrees right after their main repository with a connector line.
  Deleting refuses a worktree open as a project or holding a terminal, asks **Delete Anyway** for
  one with changes, and keeps the branch.
- GitHub through `gh`, only when installed and signed in: a status-bar CI dot for the current
  branch's latest run (click opens it), **GitHub: Create pull request** (`gh pr create --web`, so
  the browser opens and nothing is written; unpushed branches are stopped first) and **GitHub:
  View pull request checks** in the palette. gh never prompts.
- Go subtests: `t.Run("name", …)` with a literal name gets its own ▶, nested ones too, run with an
  escaped `-run '^TestX$/^name$'` pattern per level.
- Go coverage: **Tests: run all tests with coverage** tints line numbers green or red from
  `-coverprofile`; **Tests: toggle coverage** hides them, and an edit drops a file's tints.

**Language features** (`665dd02`, `32a074b`, `2c2a6e1`, `7fa5f84`, `73d873b`, `55d93d8`)
- Tree renames and drag moves send `workspace/willRenameFiles` and apply the import updates the
  server answers with (asking when they reach more than one file), then `didRenameFiles`.
- Completion documentation beside the list, asked for with `completionItem/resolve` when the
  server leaves it out, and resolved `additionalTextEdits` (TypeScript auto-imports) applied on
  accept.
- Project settings: `.athena/settings.json` laid over the global file, and the keys Athena knows
  from `.vscode/settings.json` (`.athena` wins). Server settings from a project (`lsp`, `gopls`,
  `go.toolsEnvVars`, `typescript.*`) wait for the project's trust answer.
- One trust answer now covers all project code: its ESLint and Biome, its own TypeScript, and
  project server settings. Until allowed, typescript-language-server is pinned to a global
  TypeScript; gopls runs with `GOTOOLCHAIN=local` unless its settings say otherwise.
- **Show type hierarchy** (palette), expand / shrink selection (Ctrl+Shift+Cmd+Right / Left),
  **Format selection** (Cmd+K Cmd+F) and linked editing for JSX/TSX tags (`editor.linked_editing`,
  off by default).
- `editor.lightbulb` (`quickfix` by default, `all`, `off`) and `editor.codeActionsOnSave`
  (`source.organizeImports`, now for TypeScript too, and `source.fixAll.eslint`).

Screenshots from the QA builds: [the Claude tab with a session's file, tokens and cost](screenshots/v0.8/01-claude.png),
[Go subtest marks, coverage tints and a worktree in the rail](screenshots/v0.8/02-tests.png), and
[completion documentation beside the list over a type hierarchy](screenshots/v0.8/03-lsp.png).

### Decisions made without you

- **Risky prompts list Cancel first.** In gpui 0.2.2's macOS alert the first button takes Return
  unless it is a Cancel button, which takes Escape instead. With the action first (as the lanes
  had it), Return allowed project code, deleted a dirty worktree, updated imports or reverted a
  Claude edit. Now the trust prompt, Delete Anyway, Update Imports and Revert list the cancel
  button first and the action second: Escape cancels, Return answers neither, only a click or
  Space on the focused action confirms. Save prompts keep their order (`1e9c329`).
- **One "project code" answer.** The v0.7 linter answer (`"linters"` in `workspace.json`) now also
  covers the project's TypeScript and its server settings, since all three run or choose code
  from the repository. The question names only what the project brings, and a settings-only
  project is asked "Use proj's language server settings?" (`fba1e34`). The palette commands are
  now **Allow / Disallow project code (linters, TypeScript, project settings)** (`aa8922c`), and
  any answer restarts every language server of the project (`ddebe6d`, `ce7afe1`).
- `claude.prices` is app-wide like the theme: a project's `.athena/settings.json` cannot set it
  (`cc97129`).
- Resume types the plain `claude --resume <id>`; `--resume=<id>` could not be checked against the
  installed claude. Instead an id must start with an ASCII letter or digit (`5425283`).
- gh asks about github.com only, so another host's expired token does not hide the features; a
  pull request is created only through the browser form, never by a silent `gh pr create`.
- Transcripts are read up to 8 MiB a line; TodoWrite lists keep 200 items and plans 64 KiB.
- Worktree delete keeps the branch; the main worktree is never offered.
- Linked editing is off and the lightbulb shows only for quick fixes by default, as in VS Code.
- A Go-and-JavaScript `run_tests` from Claude in a project that is not allowed still runs go test
  and says what it skipped, rather than refusing the whole run.

### Review fixes

A review of `v0.7.0..main` before the fixes found 15 issues (no critical or high: 10 medium,
5 low) plus four minor ones. All are fixed on main:
- typescript-language-server ran unpinned for an untrusted project whose `node_modules/typescript`
  was linked outside it (pnpm) or sat in a parent folder (`e276286`).
- Claude's `run_tests` resolved a path against every open project, climbed to `/` looking for
  `go.mod` or `package.json`, and ran Vitest or Jest in a denied project (`bc09b57`).
- Return picked the risky action in the trust, worktree delete, update imports and revert prompts
  (`1e9c329`).
- Reverting a Claude edit replaced a symlink with a plain file, used a fixed temp name and failed
  when the folder was gone (`200014d`).
- A tree rename onto a taken name rewrote imports and then failed, and a second Enter applied the
  edits twice (`8198f56`).
- A resolved auto-import was applied twice, and a late one was dropped once the user typed into
  the snippet (`5cfa069`).
- A transcript named `--dangerously-skip-permissions.jsonl` made Resume type that flag
  (`5425283`).
- A worktree whose folder was deleted could never be removed (`df3eb94`).
- The Claude tab's 10 s refresh reread every snapshot and every subagent transcript from the start
  (`b38bb5c`).
- gopls kept the settings a project chose while trusted after trust was withdrawn (`ce7afe1`).
- Lows: looped subtests (`x`, `x#01`) ran only the first, a false pass (`20fcc85`); deleting a
  worktree with a project or terminal inside it (`9d9ab97`); `document_symbols` and locations
  could exceed a frame (`4364935`); transcript lines, token sums, profile folder names, todos and
  plans were unbounded (`4437244`); revert ignored a dirty buffer, which the next Cmd+S wrote back
  (`427533e`).
- Minor: a reversed linked editing range panicked (`b098222`); `gh auth status` failed on another
  host's token, gh's git could run a repository's fsmonitor, and a signed-out gh was never checked
  again (`32296f8`); a coverprofile block with a huge line range was expanded line by line
  (`4926b66`).

Within the lanes, before the review fixes: worktrees grouped only when opened, not at launch,
the branch picker offered the current branch's remote as "in worktree", and the gh notices were
cut off (`f554b59`); `run_tests` could not name a Go subtest after the rebase (`23b510f`); the
completion documentation panel was cut to the list's height (`cc97129`); the trust question
talked about TypeScript for projects without one (`fba1e34`).

### Visual QA sweep of v0.7.0

The installed v0.7.0 was walked through on screen in an isolated `HOME`, dark and light, from
launch to relaunch (screenshots kept with the QA notes, not in the repository); seven findings,
all fixed on main:
- V1 a toast's long body was clipped instead of wrapping (`cae4a46`).
- V2 a Markdown list mixing plain and task items lost the plain items' bullets, and V3 code
  blocks had no tab size (`fcacdf0`, tab size 4).
- V4 Cmd+Shift+F seeded the query from the selection but left the caret at its end, so typing
  appended (`d96f828`).
- V5 after a relaunch, terminal tabs not yet shown read "Terminal", and an open drawer came back
  closed (`fcb0b08`).
- V6 the terminal's prompt dot sat 1 px from the first character and against the pane edge; the
  left padding is now 14 px with the dot centred (`38dc12a`).
- V7 the outline and completion list marked variables and constants with bare `x` and `c`; they
  now show 𝑥 and ≡ in their theme tints (`70adc00`).

### Verification

- Unit tests throughout, with fixture transcripts and snapshot stores, fake `gh` scripts on a
  `PATH` (not installed, signed out, success, failure, the github.com host argument, fsmonitor off
  inside gh), temp-repository worktree tests (including a deleted folder), coverprofile and
  subtest pattern tests checked against go 1.27, scripted language servers for every new request,
  gopls 0.23 integration tests (selection ranges, type hierarchy, no rename participation or range
  formatting), and a model of gpui's alert key mapping for the button order. Most fix commits
  report the full gate passing.
- The Claude lane's QA app, driven by the `notify` CLI in an isolated `HOME`: the Claude tab, the
  review stepping through files from the palette (screenshot above). The MCP tools were run end to
  end through `athena mcp-stdio` against that window with gopls.
- The git lane's QA app with a fake gh on the login `PATH`, by screenshot: the palette's GitHub
  rows, the checks list, coverage tints, the branch picker, creating and deleting worktrees (and
  the refusal), the signed-out palette rows and the "Publish the branch first" notice.
- The language lane's QA app with gopls: expand and shrink selection, Format selection's notice,
  the type hierarchy, completion documentation and the trust prompt answered with Don't Allow,
  Escape and Space (before the button order changed).
- After the fix sweep, seen on screen in an isolated QA build: V1 (a long push error wraps in the
  toast), V4 (typing replaced the seeded query), V5 (restored tabs named after their folder and
  the drawer reopened on the panel) and V6 (the dot's gaps). V2 and V3 are covered by a page test
  and V7 by a unit test; neither was looked at on screen.
- Unverified: Return and Escape on the reordered trust prompt with a real keyboard (see the
  incident below); typescript-language-server was not installed here, so completion resolve,
  auto-imports, willRenameFiles import updates, linked editing, TypeScript organize imports on save
  and the held-back TypeScript are covered by scripted-server tests only; `gh pr create --web` and
  `gh pr checks` against real GitHub (only fake scripts were run).

### Known gaps

- Return on the trust dialog is not yet confirmed with a real keyboard: the synthetic-key run is
  in doubt (see the incident).
- The 𝑥 variable glyph (U+1D465) is drawn through a font fallback that has not been checked on
  screen; if no fallback font has it, it shows as a box.
- **GitHub: Create pull request** has never run against real GitHub.
- typescript-language-server features are untested against a real server (see Verification).
- Cost shows "cost n/a" for model ids missing from the built-in table (seen for a session in the
  QA screenshot); `claude.prices` fills them in.
- A completion detail longer than its column is cut at its left edge rather than its right
  (visible for `Printf` in the third screenshot above).

### Incident

- During the f8-trust QA run, which pressed Return and then Escape on the trust prompt with
  synthetic keys while you were idle, the dialog may have been answered by a click of yours while
  you were active at the machine. The run's result therefore says nothing reliable about Return;
  it needs repeating by hand (check list item 42).

## v0.7.0

Plan: [plans/v0.7.md](plans/v0.7.md), three file-disjoint lanes (workbench, git review, code
understanding), `v0.6.0..main`, then a review pass, a fix sweep and a paint performance
follow-up. The v0.6.0 section follows this one.

### What shipped

**Workbench** (`c59c9fe`, `0811914`, `9d0e6ec`, `9aa0d2c`, `a2df79a`, `20b7748`, `bcb59d9`,
`08ccd4d`, `6e38a6b`)
- A terminal panel in the drawer, as in VS Code: ``Ctrl+` `` shows, focuses or hides it,
  ``Ctrl+Shift+` `` opens another, several panel terminals are listed as sub-tabs on the right, and a
  terminal moves between the panel and the editor area keeping its shell (View menu, palette,
  right-click menus, or dragging a sub-tab onto a pane and a terminal tab onto the panel). Panel
  terminals are stored in the project's new `panel` in `workspace.json` and reattach like tabs.
  MCP's `list_terminals`, `run_in_terminal` and the Cmd+Alt+K at-mention see them (`2be737b`).
- Dragging tree rows moves entries into folders, Option copies ("name copy.ext" on a clash, in
  the background). A move asks first as VS Code does, with "Move and Don't Ask Again" writing
  `"explorer": {"confirmDragAndDrop": false}`; a name clash asks to replace and the replaced
  entry goes to the Trash. Open tabs follow the move and git status refreshes.
- Bracket pair colours by depth (three colours per theme, 4.5:1 contrast), driven by the parse
  tree so a view mid-file needs no scan from the top; `editor.bracketPairColorization.enabled`
  (or `bracket_pair_colorization`, also per language) turns them off.
- Interface zoom on Cmd+Option+= / - / 0, in 10% steps from -3 to +5, scaling the chrome (tabs,
  rows, bars, menus, buttons, toasts) and not the code; kept in `workspace.json` and in
  `"window": {"zoom_level": …}` once settings.json has it.
- Toasts no longer sit under web previews: a preview whose pane a toast overlaps hides until the
  last toast fades.

**Git review** (`cabc7c9`, `cf40362`, `23cdd7b`, `b802e47`, `8aabe35`, `906af9b`, `bfb7627`,
`f3ee854`, `93acef4`)
- Quick diff peek: clicking a gutter change bar, or Alt+F3 / Shift+Alt+F3, opens the index's
  lines for that change under it with Stage, Revert, Previous, Next and Close.
- Timeline drawer tab (**Git: Open timeline**): the active file's commits through renames; a
  click opens what the commit did to the file, Compare with Current diffs it with the working tree.
- Whole-file blame gutter (**Git: Toggle file blame**): an age bar per line, author and age per
  stretch, a hover card, and a click that opens the commit's diff.
- Diff views: drag to select on either side, Cmd+C / Cmd+A, Hide Unchanged (three lines of
  context, on by default over 500 rows) and a right-click menu with Copy, Select All, the
  change's Stage / Unstage / Revert and Open File.
- A multi-line commit box (Enter breaks the line, Cmd+Enter commits, grows to six rows), and
  Select for Compare / Compare with Selected in the tree.
- New diff bases (a commit against its parent, a revision against the working tree, two files)
  that restore across launches.

**Code understanding** (`948cf9b`, `46e0509`, `43f1622`, `8ab13bb`, `1769f13`, `b94d491`)
- Outline view behind an Explorer / Outline switch at the top of the tree area: the active
  editor's symbols, following the cursor, with a filter.
- Call hierarchy on Shift+Alt+H in the References tab: incoming calls by default, Outgoing in
  the header, a chevron or double click loading the next level.
- ESLint (`vscode-eslint-language-server`) and Biome (`biome lsp-proxy`) started only from the
  project's own `node_modules/.bin`, with diagnostics per server labelled `eslint(no-var)` in
  Problems, their fixes in Cmd+., and `eslint.fixOnSave` (off by default). After the review they
  run only once the project is allowed (see Decisions).
- Built-in language defaults (Markdown wraps and keeps trailing whitespace, Go formats on save)
  now sit between the global `"editor"` block and the user's `"[lang]"` block, as in VS Code.
  This closes the v0.6 known gap about global trimming hitting Markdown.
- Markdown inline highlighting (emphasis, strong, code spans, links) through tree-sitter-md's
  inline grammar, a Mermaid line highlighter for `.mmd` / `.mermaid`, and Rust ALL_CAPS
  constants coloured again (the upstream query's predicate had a stray quote).

Screenshots from the QA builds: [a panel terminal in the drawer's Terminal tab](screenshots/v0.7/01-panel.png),
[file blame, the Timeline tab and a commit's diff, with a two-file compare tab](screenshots/v0.7/02-git.png),
and [the Explorer / Outline switch, bracket pair colours and the call hierarchy](screenshots/v0.7/03-code.png).

### Decisions made without you

- **Project linters ask first.** The lane shipped ESLint and Biome starting on their own once
  found in the project; the review called that running a repository's code unprompted (H3). Now
  the first file that would start one asks once per project root, "Run ESLint and Biome from
  proj?" (naming only the linters it installs) with Allow / Don't Allow (Escape is Don't
  Allow), and nothing starts until allowed. The answer lives on the project in `workspace.json`
  (`"linters": "allowed" | "denied"`, absent meaning not asked), since it belongs to one folder
  rather than to every project the way settings.json does; the palette's Allow / Disallow project linters change it. When two saved roots turn out to be one
  folder, the strictest answer wins: denied over allowed over not asked (`b62e3fa`, `588a8e7`).
- Project linters run without `SSH_AUTH_SOCK`, `NODE_OPTIONS` and `NODE_PATH`, ESLint's
  `nodePath` is pinned to the project's own `node_modules`, and every language server gets its
  own process group so the reaper kills Biome's native child too. `biome stop` is not run, since it would stop a daemon
  another editor shares; the daemon is started with `--stop-on-disconnect` instead (`da7c0cb`).
- **Git reads never run filter drivers or textconv.** The review suggested
  `--attr-source=<empty tree>`; a scratch repository with the attribute in `.git/info/attributes`
  still ran all three drivers that way, and it would drop `eol=crlf` conversion. Instead every
  reading git gets each configured driver's commands emptied (and `required=false`) through
  `GIT_CONFIG_KEY_n`, with names from `git config --list -z`, plus `--no-textconv` and
  `log.showSignature=false`. Commands that write (stage a whole file, discard, switch, stash,
  pull, commit) still run the filters, since skipping them would store or check out the wrong
  bytes for git-crypt or LFS; one-hunk stage, unstage and revert, and the peek, are refused for a
  file behind a filter (`5c16bb2`, `e5a2ff3`).
- The gutter now compares the saved file with the index rather than `HEAD`, as VS Code's quick
  diff does, so staging a change clears its bar and the peek always shows what a bar stands for.
- The peek is an overlay anchored under the change, not a view zone that pushes lines down; that
  would need row changes in the display map. It covers the lines below it (see Known gaps).
- Interface zoom took Cmd+Option+= / - / 0 because none of them was bound and Cmd+= stays the
  code font zoom (`editor.font_size`). With macOS Accessibility Zoom's shortcuts on, the system
  takes them first.
- Bracket pair colours are on by default, as in VS Code; highlighters without a tree count from
  the top of the file, up to 5000 lines.
- A tree drag that would replace a file with unsaved changes is refused with a notice rather than
  discarding them; the replaced entry's tabs close as the move lands. Option-copy runs in the
  background; a move runs on the UI thread.
- Hide Unchanged starts on for diffs over 500 rows; Amend now fills in the whole last message.
- ``Ctrl+Shift+` `` is also bound as Ctrl+~, which is what macOS reports for it.
- Notifications remember the daemon session rather than the tab id, since a terminal's id changes
  when it moves between the panel and the editor area (`9e4c536`).
- A blamed commit's diff compares against blame's own `previous` commit, and timeline log records
  are split on NUL only, so a subject holding `\x1e` cannot fake a row (`5c16bb2`).

### Review fixes

A review of `v0.6.0..a12ca42` found 16 issues (3 high, 9 medium, 4 low; one low was part of a
high). All are fixed on main:
- Dragging `/p/pkg/pkg` onto `/p` made `/p/pkg`, the dragged folder's own parent, the
  destination, and Replace sent it to the Trash with the dragged folder inside (high). Such a
  destination is refused, checked by device and inode before anything is trashed (`933cfc2`).
- A slow call hierarchy answer for an earlier Shift+Alt+H indexed into the new, smaller tree and
  panicked (high); clicks on rows drawn before the tree changed did the same (`5c651ee`).
- Project ESLint and Biome ran with no trust prompt (high) (`b62e3fa`, `588a8e7`).
- A replacing drag left the replaced file's tabs open on the moved file, where a dirty "keep
  mine" wrote the old edits over it; a move across volumes failed with EXDEV after the replaced
  entry was already in the Trash (`9503880`).
- Cmd+W with focus in a panel terminal closed the editor area's active tab, possibly a Claude
  Code terminal, and Move Terminal into Panel pulled the wrong terminal down (`fdd025c`).
- Killing a panel terminal, tab or project whose terminal had never been shown left its shell
  running in the daemon with nothing to reach it (`7724a21`).
- Git reads ran the repository's filter drivers and textconv (`5c16bb2`, `e5a2ff3`).
- Project linters inherited `SSH_AUTH_SOCK`, `NODE_OPTIONS` and `NODE_PATH`; Biome's native child
  outlived its wrapper; ESLint could resolve its library above the project (`da7c0cb`).
- Markdown inline highlighting re-parsed a whole paragraph per paint, and bracket colouring walked
  every sibling before the view per frame (`1cae47a`, numbers below).
- Whole-file blame re-ran after each typing pause with dropped runs left going, a timeout read as
  "no committed lines", and the timeline showed the previous file's commits while loading and
  kept unneeded logs running; runs can now be cancelled and kill git (`5c16bb2`).
- Lows: a notification lost its terminal once the terminal moved (`9e4c536`); a crashed ESLint's
  late pull answer overwrote its restarted instance's (`c902a66`); merging duplicate project roots
  dropped the active copy's panel terminals (`4892bb5`); a blamed commit diffed against `rev^`
  rather than blame's `previous`, and a subject with `\x1e` forged timeline rows (`5c16bb2`);
  zoom steps could overflow from a hand-edited level, and a doc comment had drifted (`0d6168f`).

Before the review, within the lanes: an aborted tree drag over its own row and a panel terminal
dropped on a pane that had gone now do nothing (`5873086`); MCP and the at-mention did not see
panel terminals (`2be737b`); the drawer's tab row pushed the Terminal tab's Kill button off the
edge at +2 zoom (`c96b183`); the commit box sat 4 px taller than the branch pill and diff toolbar
labels cut off mid-word (`f3ee854`); the Outline and call hierarchy rows ignored interface zoom
(`a12ca42`).

### Performance

From [perf/2026-10-v0.7-paint.md](perf/2026-10-v0.7-paint.md) (release build, Apple M5, an
ignored `paint_cost` test kept in the tree):

| Case | before | after |
|---|---|---|
| Markdown, 60 lines of a 20 k-line paragraph, per paint | 318 ms | 2.1 ms |
| JSON 300 k array, open brackets, each frame while scrolling | 13.5 ms | 0.002 ms |
| JSON 300 k array, open brackets, first frame after a parse | 13.5 ms | 13.4–16.5 ms |

The inline grammar now parses only the painted lines and 20 either side, and nodes with 64 or
more children keep the bracket tokens read so far until the tree changes.

### Verification

- Unit tests throughout, plus a gopls integration test for call hierarchy, a fixture-gated
  integration test against eslint 9.39 with vscode-langservers-extracted 4.10 and biome 1.9.4,
  real-repository tests for history, blame and the diff bases, a scratch repository whose
  clean / smudge / process / textconv drivers touch marker files (none ran through any read;
  `git add` still ran clean), a cancel test killing a 30 s git within 2 s, temp-dir tests with a
  recording Trash for the drag fixes, and a regression test that panicked before the call
  hierarchy fix. Most fix commits report the full gate passing.
- GUI QA in isolated `HOME`s with synthetic keys only, while you were idle. The screen was locked
  for most of the workbench lane: ``Ctrl+` `` and ``Ctrl+Shift+` `` made two panel terminals whose sessions
  survived a quit and relaunch, keymap-bound moves took one panel → pane → panel with its session
  kept, and Cmd+Option+- / = / 0 moved the saved level; only the two captures at the end (the
  panel above, and +2 zoom showing the Kill button pushed off, before `c96b183`) were seen.
- The git lane's QA app was captured on screen: the peek with its Stage / Revert row, the blame
  gutter, the Timeline tab, Hide Unchanged, Select All in a diff, and the Changes tab before and
  after the commit box alignment fix.
- The code lane's QA app: palette Focus outline, typing "sto" and Enter moved the cursor to
  `Server.Stop`; the call hierarchy screenshot above; MCP `get_diagnostics` listed ESLint and
  Biome problems for a JS file (before the trust prompt existed).
- After the fixes: Cmd+W in a focused panel terminal removed the panel terminal from
  `workspace.json` and left the editor area's tabs alone; with biome 2.5.15 in a project,
  "denied" started nothing, "allowed" started `lsp-proxy`, and no answer showed the sheet with no
  biome process running before it was answered.
- **Unverified (they need a mouse)**: dragging tree rows (move, Option-copy, the confirmation,
  Replace and the Trash, a drag to another volume), dragging terminals between the panel and
  panes, the right-click menus (terminal tab, panel row, tree compare items, diff, Timeline row),
  clicking a gutter change bar, hovering and clicking the blame gutter, selecting in a diff by
  drag, opening a Hide Unchanged fold, the Explorer / Outline switch by click, call hierarchy
  chevrons, the "Move and Don't Ask Again" button, and the zoomed drawer after `c96b183`. Bracket
  colours were seen only in the screenshots.

### Known gaps

- Filter drivers configured inside a submodule are not switched off: git status looks into
  submodules, where the parent's overrides do not reach.
- A tree move across volumes copies on the UI thread, so a large folder blocks the window until
  it is done (Option-copy runs in the background).
- When two saved roots merge, the panel terminals of the copy that loses stay running in the
  daemon with no tab. `athena mux status` shows them; the only way to end them today is
  `athena mux stop`, which hangs up every shell, not just these.
- The quick diff peek covers the lines below the change instead of pushing them down.
- The Timeline lists at most the last 500 commits of a file.
- ESLint was never run in the app after the trust prompt landed (the QA project had Biome only);
  it is covered by the integration test and the lane's earlier MCP check.
- The first frame after each tree change deep in a huge array still reads every element above
  the view once (about 13 ms at the middle of 300 k elements), twice per keystroke. Emphasis or a
  code span that opens more than 20 lines above the view paints as plain text.
- Interface zoom keys are taken by macOS when Accessibility Zoom's keyboard shortcuts are on.

## v0.6.0

Plan: [plans/v0.6.md](plans/v0.6.md), three file-disjoint lanes (search and saving, language
server settings and UI, tests, tasks, conflicts and git remotes), `v0.5.0..main`, then a review
pass. The v0.5.0 section follows this one.

### What shipped

**Search and saving** (`599dbf8`, `95169e6`, `339090a`, `6eafaa3`)
- The editor find bar gets VS Code's Match Case, Match Whole Word and Use Regular Expression
  toggles (`Aa`, `ab`, `.*`, Cmd+Alt+C/W/R). Regex mode shows an invalid pattern inline and fills
  in `$1`, `$&`, `\n` and `\t` on Replace; the toggles survive closing the bar and Cmd+D follows
  them while it is open.
- Project search gets the same toggles, Files to include / Files to exclude globs (VS Code's
  rules), `$1` replacement in regex mode, a Replace Preview diff tab when a match is clicked with
  replace text present, and its query, fields and results kept per project while the app runs.
- Save tidying: trim trailing whitespace and insert a final newline, as one undo step;
  `.editorconfig` (indentation on open, line endings, trimming and the final newline on save); and
  the status bar's LF/CRLF converts every line break. Auto save now waits for a pending format.

**Language servers and the editor** (`35fe4ed`, `b95bbd0`, `44d0771`, `db30fb7`, `bbfbe02`,
`7470bfa`, `ba2cf52`, `e40e351`, `807a1c1`, `55e8291`, `41c56d9`)
- `settings.json` in the data folder: JSONC, reloaded on save, problems in a toast. It holds
  `editor.*` (format on save, trimming, final newline, word wrap, font size, tab size, auto save
  delay, inlay hints), `"[lang]"` blocks by VS Code's language id, `theme`, `ide_integration`,
  `git.autofetch` and `lsp.<program>`, which goes to the server as `initializationOptions`, as
  `workspace/configuration` answers and through `didChangeConfiguration`. VS Code's spellings
  (`files.trimTrailingWhitespace`, `editor.tabSize`) are read too. Open Settings (JSON) is on
  Cmd+, and in the Athena menu; palette toggles write their key into the file and `workspace.json`
  stays the fallback.
- Inlay hints drawn inside the line, with Toggle inlay hints turning on a curated gopls and
  typescript-language-server set.
- `textDocument/documentHighlight` marks other uses of the symbol at the cursor (reads and writes
  in different colours) after a 250 ms rest.
- Breadcrumbs under the tab strip: folders, the file and the symbols holding the cursor, each
  with a dropdown.
- Completion snippets keep their tab stops: Tab / Shift+Tab, mirrored placeholders typed
  together, `$0` or Escape to finish.
- Grammars for go.mod / go.work, go.sum, Makefiles, SQL and Protocol Buffers (all MIT tree-sitter
  crates; Athena carries its own queries for go.mod, go.sum and proto).

**Tests, tasks, conflicts and git** (`a9dbd91`, `fb42753`, `2fe55e2`, `b976a73`, `564e2bf`,
`578c017`, `5756c89`, `34735a6`)
- A new `athena-testing` crate runs `go test -json`, Vitest and Jest (JSON reporter) in their own
  process group with a time limit and reads the reports. Test files get a green ▶ in the gutter
  per test or group, which turns into a pass, fail or skip dot; a Tests drawer tab lists suites
  and tests with output, clickable `file:line`, Run All, Re-run Failed and Stop; six palette
  commands.
- Run task… lists `package.json` scripts (npm, pnpm, yarn or bun by lockfile) and Makefile
  targets and types the command into a new terminal tab.
- Merge conflicts: tinted blocks, VS Code's Accept Current / Incoming / Both and Compare Changes
  row, a conflict count in the Changes tab and a Stage toast once none are left.
- Git: fetch, `pull --ff-only`, push and Publish Branch, ahead/behind in a status bar sync cell,
  stash, stash with untracked files and pop in the branch picker, the sync cell's menu and the
  palette, and autofetch every three minutes when `git.autofetch` is on.

Screenshots from the lanes' QA builds: [the Search tab with its toggles](screenshots/v0.6/01-search.png),
[run marks in a TypeScript and a Go test file with the Tests tab after a run](screenshots/v0.6/02-tests.png)
(the status bar shows ↓1 ↑1), and [breadcrumbs, inlay hints and a highlighted variable](screenshots/v0.6/03-lsp.png).

### Decisions made without you

- **Trimming and the final newline are off by default for every language**, as in VS Code. The
  lane first shipped them on for Go, TS/JS, Rust, Python, YAML and JSON, but the review found that
  trimming then ate blanks that are part of the text inside raw strings, template literals and
  docstrings. Off matches VS Code, which this release follows for defaults. Now settings.json or `.editorconfig` turns them on, and when
  on, a line whose break lies inside a string literal (by the tree-sitter parse) keeps its blanks
  (`f1082c2`).
- **Markdown is no longer special-cased** for saving. The lane had given it the final newline but
  not trimming, since two trailing spaces are a line break there; with both off by default there
  is nothing to special-case, and a `"[markdown]"` block can say what you want.
- **Autofetch is off**, as VS Code's `git.autofetch` is; when on it fetches the active project
  every three minutes while the window is in front, and its failures stay quiet.
- **Pull is `git pull --ff-only`**: a diverged branch is refused with git's own message rather than
  merged or rebased behind your back. Publish Branch asks first and names the remote (origin, or
  the only one).
- **Inlay hints are off in practice**: gopls and typescript-language-server send none unless
  configured, as with VS Code's Go extension. Toggle inlay hints writes `editor.inlay_hints` and
  adds a curated set unless your `lsp` entry chooses its own; `false` hides every hint.
- The find toggles are only on Cmd+Alt+C/W/R. The plan's bare Alt+C/W/R took the keys macOS uses
  to type ç, ∑ and ® into a query (`18680b1`).
- Replace All refuses while results are cut short at 2000, instead of rewriting files nobody saw
  (`f6a41dd`).
- Git remote commands never wait on a prompt, and only one fetch, pull, push or stash runs at a
  time (`c4cf875`, `ab7329a`).
- Run task types a command line into a terminal, since the terminal spawns an interactive shell
  rather than an argv; names that would act as keystrokes or options are dropped, `make` gets `--`
  (`a76c419`).
- The settings writer refuses a file `serde_json` cannot read and edits the member the parser
  actually keeps (the last of a duplicated key), so a toggle never damages or silently misses
  settings.json (`c4d269f`).
- Compare Changes and Replace Preview tabs are never saved in `workspace.json`.

### Review fixes

A review of `v0.5.0..807a1c1` found 15 issues (1 high, 12 medium or medium-low, 2 low) plus four
low notes. All are fixed on main:
- Accepting a conflict whose block held a lone CR panicked (high); an unclosed `<<<<<<<` hid the
  real block after it and offered "All conflicts resolved, Stage" (`75cdb7c`).
- The settings writer rewrote invalid files and edited the first of a duplicated key while the
  parser keeps the last (`c4d269f`).
- Replace All with truncated results rewrote files never shown (`f6a41dd`).
- Test output only arrived at EOF, so a leftover child holding the pipe blanked the panel; test
  processes outlived Athena; a timed-out run discarded its written report (`c99c083`).
- Bare Alt+C/W/R blocked typing ç, ∑, ® in the find bars (`18680b1`).
- ssh could prompt on `/dev/tty` or hang, and a timed-out ssh was orphaned (`c4cf875`).
- Remote ops and stash had no busy guard, so Publish or a double click could run two at once
  (`ab7329a`).
- Trimming on by default ignored string literals, and an auto save trimmed the caret line of
  another tab on the same file (`f1082c2`).
- Task names with control characters or a leading `-` were injected into the terminal
  (`a76c419`).
- `$2` for a missing group deleted text, whole-word search was quadratic on long words, and a
  refused save had already tidied the buffer (`a86c92c`).
- Lows: Pop stash popped by a stale index (`37770fb`), a deleted upstream showed ↓0 ↑0
  (`6a78412`), JS test titles with escapes never matched `-t` (`5f33d6e`), an unclosed snippet
  placeholder left a phantom tab stop (`8f39d22`).

Before the review, from the lanes' own QA: the conflict actions row painted over the palette and
ran into the next pane, and the sync cell flashed "Publish" at launch (`578c017`); a dependency's
compile error showed only "[build failed]" (`5756c89`); editors shown before their server was
ready never asked for inlay hints (`55e8291`); the settings template's examples sat outside the
braces (`41c56d9`).

### Verification

- Unit tests throughout, plus gopls integration tests for inlay hints (by default, configured, and
  after `didChangeConfiguration`) and documentHighlight, real `go test -json` and Vitest fixtures,
  git remote ops against a local bare remote, fake ssh scripts and an unreachable remote for the
  no-prompt rules, 3000 generated settings documents checked against the parser, and process
  group tests for the test runner (orphans, timeouts, `kill_now`).
- GUI QA: each lane ran its debug build in an isolated `HOME` with a visible window, **driven by
  synthetic keys only, and only while you were idle** (the scripts stopped as soon as HID input
  appeared). Covered that way: find toggles, invalid and valid regex, saving with
  `.editorconfig`, project search with regex, run marks in Go and TypeScript files, a palette test
  run with results in the Tests tab, Run task…, the branch picker's stash rows, a palette fetch,
  documentHighlight, breadcrumbs, snippet completion and Toggle inlay hints. These runs were on
  the lane builds, before the review fixes (the search run still used bare Alt+W, and
  typescript-language-server was not installed in its `HOME`). Only the three screenshots above
  are kept; the rest of the QA captures were scratch files.
- **Unverified (they need a mouse click)**: clicking a gutter ▶, the conflict Accept / Compare
  row and the Stage toast, breadcrumb dropdowns, the status bar's sync cell menu and LF/CRLF
  menu, the Publish Branch confirmation, clicking a Search match for Replace Preview, the Search
  details `⋯` button, the Replace All prompt, rows, hover Run / Go to Test and `file:line` links
  in the Tests tab, and picking a stash to pop.

### Known gaps

- Per-language defaults do not beat global settings yet, as they do in VS Code: Athena has none
  for saving, so a global `trim_trailing_whitespace: true` also trims Markdown unless a
  `"[markdown]"` block says otherwise. Relatedly, Markdown always wraps unless its tab is toggled;
  a `"[markdown]"` `word_wrap: false` does not stop it.
- Go subtests (`t.Run`) appear in the results but cannot be run on their own; their parent runs.
- Jest is read only through the JSON report shape it shares with Vitest; the fixture is a real
  Vitest report, and no real Jest run was checked.
- typescript-language-server settings (the inlay hint preferences, `lsp` passthrough) are
  untested against a real server; gopls is covered by integration tests.
- Lines that wrap are drawn without inlay hints.

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

For v0.6.0:

24. Open Settings (JSON) (Cmd+comma), set `"editor": {"trim_trailing_whitespace": true}` and save:
    no toast. Save a Go file with a trailing space inside a raw string and one after code: only
    the second goes. Break the file (drop a comma) and run Toggle inlay hints: a "settings.json was
    not updated" toast, the file left as it was.
25. In an editor, Cmd+F with Cmd+Alt+C / W / R, then type `ç` with Alt+C into the query. Regex
    `(\w+)_(\w+)` replaced with `$2_$1`.
26. Cmd+Shift+F, open the `⋯` details, include `*.go`; type replace text and click a match: the
    Replace Preview tab. Search something with over 2000 hits and click Replace All: refused.
27. In a Go test file click a gutter ▶, then the Tests tab: Re-run Failed, a `file:line` link,
    hover Run / Go to Test. Same in a Vitest project and, if you have one, a Jest project.
28. Make a merge conflict, open the file: click Accept Current / Incoming / Both and Compare
    Changes; save with no markers left and press Stage on the toast.
29. Click the status bar's sync cell: Fetch, Pull, Push (or Publish Branch on a new branch, which
    should ask first); Stash, then pick the stash from Pop Stash…. Click LF and convert to CRLF.
30. Click each breadcrumb (a folder, the file, a symbol). Run Run task… with a Makefile target.
31. Turn on `"git": {"autofetch": true}` and leave the window in front for three minutes.

For v0.7.0:

32. ``Ctrl+` `` in a project: the Terminal tab opens on a shell; ``Ctrl+Shift+` `` adds a second and the
    sub-tab list appears. Drag a sub-tab onto a pane, then use the terminal tab's right-click Move
    Terminal into Panel. With the panel focused press Cmd+W: only the panel terminal closes.
    Quit and relaunch: the panel terminals come back with their scrollback. Run `list_terminals`
    from Claude Code and check the panel terminal is listed.
33. Drag a file onto a folder in the tree: the "Are you sure" question, then Move. Option-drag a
    file into its own folder: "name copy.ext". Drag onto a folder holding the same name and
    Replace: the old one is in the Trash and its tab closed. Try "Move and Don't Ask Again" and
    look at settings.json. If you have an external disk, drag a project file onto it in a second
    project.
34. Open a nested JSON or Rust file: brackets in three colours by depth, none inside strings.
    Set `"editor.bracketPairColorization.enabled": false`: they go back to plain.
35. Cmd+Option+= twice, look over the tree, drawer (the Terminal tab's Kill button), menus and
    palette, then Cmd+Option+0.
36. Edit a tracked file: click its gutter bar for the peek, Stage one change, Revert another,
    step with Alt+F3. In a git-lfs or git-crypt repository, the peek and one-hunk Stage are
    refused with the whole-file message, while staging the whole file works.
37. Git: Toggle file blame: hover a stretch, click it for the commit's diff. Git: Open timeline:
    click a commit, then Compare with Current; switch tabs and watch the list follow.
38. In a diff: drag-select and Cmd+C, toggle Hide Unchanged and click a fold bar, right-click a
    change. Select for Compare on one file and Compare with Selected on another. Type a commit
    message of three lines with Enter and commit with Cmd+Enter; turn Amend on.
39. Click Outline at the top of the tree, filter, and click a symbol. Shift+Alt+H on a Go
    function, flip to Outgoing and expand a row.
40. In a project with `eslint` and `vscode-langservers-extracted` in `node_modules`, open a JS
    file: the Allow / Don't Allow sheet. Allow, then check Problems shows `eslint(…)` rows and
    Cmd+. offers its fixes; run Disallow project linters and watch them go.
41. Open a Markdown file with emphasis, code spans and links, and a `.mmd` file: both
    highlighted. A global `"word_wrap": false` still leaves Markdown wrapped.

For v0.8.0:

42. Open a project with ESLint, Biome or its own TypeScript in `node_modules` on a fresh answer:
    when the trust prompt shows, press Return with a real keyboard (nothing should happen), then
    Escape (Don't Allow; nothing starts). Run **Allow project code** from the palette and check
    the linters start; **Disallow project code** stops them.
43. Run **Enable Claude Code hooks for this project** again, start Claude and have it make a todo
    list: the tab badge shows "1/3" and the list as its tooltip. Open the Claude tab: the session
    with its files, tokens and cost (a model without a price says "cost n/a"; add it under
    `claude.prices`). Review All, then Revert one file with unsaved edits in its editor (refused),
    save, and Revert again. Resume a session from another profile and check `CLAUDE_CONFIG_DIR`.
44. From Claude: `lsp_definition` on a file open in Athena and on one that is not (asks to
    `open_file` it), `read_buffer` on a dirty file, `run_tests` with a `TestX/sub` name, and
    `git_status`.
45. In a GitHub repository with `gh` signed in: the CI dot and its click, **GitHub: View pull
    request checks**, and **GitHub: Create pull request** on a pushed branch (the browser form
    should open; finish or close it there). Sign out of gh: the dot goes and the palette rows say
    "Needs gh auth login".
46. **Git: Create worktree…** with a new branch: it opens as a project grouped under its repo in
    the rail. Make a change there and **Git: Delete worktree…** from the main project: Escape
    cancels, Delete Anyway deletes, the branch remains. Try with a terminal `cd`'d into it.
47. In a Go test with `t.Run("adds two", …)` click its ▶; run **Tests: run all tests with
    coverage**, look at the tints, edit the file and see them go.
48. With typescript-language-server installed: completion documentation beside the list, an
    auto-import on accept, renaming a `.ts` file in the tree (Update Imports when it reaches
    several files), `"editor": {"linked_editing": true}` on a TSX tag, and organize imports on
    save with `"codeActionsOnSave": {"source.organizeImports": "explicit"}`.
49. Ctrl+Shift+Cmd+Right three times and Left once in a Go file; Cmd+K Cmd+F on a selection;
    **Show type hierarchy** on an interface. Open the Outline on a file with variables and
    constants: 𝑥 and ≡ draw as glyphs, not boxes.
50. Quit with the terminal panel open and two terminal tabs, relaunch: both tabs have their
    folder or program as title and the panel is open. A Markdown list mixing `- [ ]` items and
    plain ones keeps the plain bullets.

For v0.9.0:

51. Enable Developer Mode once (`sudo DevToolsSecurity -enable`) and install Delve (`go install
    github.com/go-delve/delve/cmd/dlv@latest`). In a Go project, set a breakpoint in a test,
    right-click its ▶ and pick Debug Test: the trust prompt (Escape cancels, then Allow and
    Debug), the stop on the amber line, the Debug tab's Call Stack and Variables, hover over a
    variable, then F10 (with Fn), Shift+F11 and F5 to the end. Try the Debug link in the Tests tab
    and the title bar's buttons. While paused, ask Claude for `debug_state`.
52. Add a conditional breakpoint and a logpoint from the gutter's right-click menu, quit and
    relaunch: they come back. Add a `.vscode/launch.json` with `"program": "${workspaceFolder}"`
    and press F5 in a non-test file.
53. Open a Shift JIS file and a UTF-16 LE file with a BOM: the status bar names each; edit, save
    and check `git diff` shows only your change. Stage one hunk from the peek. Reopen a Latin-1
    file as Windows 1251 and back, and Save with Encoding as UTF-8.
54. Open a file over 50 MB (`seq 1 15000000 > big.log` makes one of about 170 MB): the banner,
    scrolling, Cmd+F for `14999999`.
55. Look at the minimap in a long file: drag the slider, click below it, and run **Toggle
    minimap**. Narrow the pane below 520 px: it hides.
56. In a Go file with a `//go:generate` line: the "run go generate" lens; click it in a project
    not yet asked (the trust prompt), then in an allowed one. Check parameters and constants take
    their semantic colours in both themes.
57. **Snippets: configure snippets for this language** in a Go file, add one using
    `$TM_FILENAME_BASE` and `${1:/upcase}`, save, then type its prefix. Turn on word wrap and
    inlay hints on a long call.

For v0.10.0:

58. Cmd+Shift+N and open a second project in the new window; then open the first project again
    with Cmd+O or `athena <folder>`: its window comes forward. Right-click a project in the
    rail, Move Project to New Window: its terminals keep running and unsaved text stays. Merge All
    Windows.
59. With two windows, start `top` in one and close it with the red button: Open Recent lists its
    projects, and reopening one finds `top` still running. Cmd+Shift+N, open another project,
    start a shell there and close that window, then Clear Recently Opened: Cancel / Clear and End
    Terminals. Quit with two windows and relaunch: both come back where they were.
60. Cmd+,: search "tab size", change it under User and under Project; watch Modified, Reset and
    "Also set …", and check settings.json keeps its comments. The Project scope has no Color Theme.
61. Cmd+K Cmd+S outside a terminal: double-click Split right, press Cmd+Shift+D (it should say it
    is also bound to Split down), Escape; record Cmd+K Cmd+T for New terminal and press Return,
    then look at keymap.json. Remove and Reset from the right-click menu. Add a bad entry to
    keymap.json: it is listed above the table and marked in its editor.
62. In settings.json type `"tab_sise": 2` and `"window": {"zoom_level": 9}`: warnings on both
    keys as you type. With `npm install -g vscode-langservers-extracted`, quit and relaunch Athena
    and reopen it: the JSON server's messages appear too.
