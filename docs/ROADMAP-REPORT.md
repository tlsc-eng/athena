# Roadmap report: v0.2.0 and v0.3.0

What happened while you were away, for the roadmap in `piped-orbiting-stearns.md` (Phases A–E).

- Release: v0.2.0 — https://github.com/tlsc-eng/athena/releases/tag/v0.2.0 (tap `f65669b`,
  installed here with `brew upgrade --cask athena`)
- Release: v0.3.0 — https://github.com/tlsc-eng/athena/releases/tag/v0.3.0 (tap `5f58c02`,
  installed here with `brew upgrade --cask athena`)

The screen was locked for most of the work after v0.2.0. Everything below marked
**unverified on screen** is covered by unit tests, logs or synthetic-key runs, but nobody has
looked at it. Start with [What to check when you're back](#what-to-check-when-youre-back).

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

Backlog candidates from the plan: rename symbol, document outline, problems panel, diff viewer
and commit UI, multi-cursor, word wrap, light theme, keymap file.

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
