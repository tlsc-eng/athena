# Companion protocol (v1)

The wire protocol between Daedalus on the Mac and the iOS companion app. The Rust types live in `crates/daedalus/src/companion/protocol.rs`. The files in `design/protocol/` are golden examples of every message; the iOS app decodes the same files. Regenerate them with `UPDATE_FIXTURES=1 cargo test -p daedalus companion::protocol`.

## Endpoints

The server listens on `ws://<tailscale-addr>:47123`.

| Path | Auth header | Purpose |
| --- | --- | --- |
| `GET /v1/pair` | `X-Daedalus-Pair: <code>` | One pairing exchange, then the socket closes. |
| `GET /v1/connect` | `Authorization: Bearer <device token>` | The long-lived session. |

A request with an `Origin` header is refused with 403. The server pings every 20 s and drops a peer that sends no pong for 45 s. `/v1/connect` answers 401 before the upgrade for a bad or revoked token and 429 when the device already has 4 connections, and a live socket is closed with code 1008 (Policy) when its device is revoked.

## Framing

- Text frames carry one JSON object with a snake_case `"type"`. An unknown `type` from the phone makes the desktop close the socket, and the phone skips a message from the desktop whose `type` it does not know. The phone sends the transcript, act and usage messages only when `welcome.features` lists `transcript`, `usage` and `act`, and reads `terminal.dialog` only when it lists `dialog`, and sends `answer` only when it lists `answer`.
- Binary frames carry terminal bytes: `[kind u8][pane u64 big-endian][payload]`. Kind `1` is output (desktop to phone), kind `2` is input (phone to desktop). The payload is at most 64 KiB, and a frame with a shorter header, an unknown kind or a larger payload is invalid.
- Optional fields are omitted when absent.
- Replies to `spawn_claude`, `prompt`, `interrupt`, `answer` and `usage_refresh` come when the desktop has finished, not in request order, and `snapshot` and ping frames keep flowing in between. The phone matches a reply to its request by `id`. The desktop holds at most 8 such requests per connection and answers a ninth at once with `error` `busy`, without acting on it.

## Session flow

1. The phone sends `hello` first. If the `proto` major differs, the server closes with code 4001.
2. The server answers `welcome`, then a `snapshot`, and another whenever state changes (at most every 250 ms).
3. `attach` starts a pane: `attached`, binary output frames for the replay tail, `replay_done`, then live frames. A pane that is not in the latest snapshot gets an `error`, and so does a ninth pane: a connection streams at most 8 panes at once.
4. Input is a binary frame of kind `2`. It is ignored for a pane that is not attached, and the server forwards it to the terminal in order.

## Phone to desktop (`/v1/connect`)

`hello`: first message. `proto` is the protocol version.

```json
{"type":"hello","proto":1,"app":"0.1.0","device":"Jason's iPhone"}
```

`attach`: replay up to `tail` bytes (capped at 1 MiB), then stream live output. When the replay is cut, it starts after the first newline of the newest `tail` bytes, or at the cut if there is none. Attaching a pane that is already attached replaces the earlier attachment and replays again.

```json
{"type":"attach","pane":123,"tail":262144}
```

`detach`: stop streaming a pane. The server sends nothing back, and closing the socket detaches every pane.

```json
{"type":"detach","pane":123}
```

`fit`: size a pane's terminal to the phone, so the program redraws at the phone's width. `rows` is clamped to 8..=500 and `cols` to 20..=500. A pane that is not in the latest snapshot gets `pane is not open`. `id` is echoed in `fitted` or `error`. Send it only when `features` lists `fit`, because an unknown message closes the socket.

```json
{"type":"fit","id":"r20","pane":123,"rows":44,"cols":57}
```

`unfit`: end the phone's lease on a pane. The server sends nothing back. The lease also ends on `detach` of that pane and when the socket closes. The desk can end it too, which shows as `terminal.rows` and `terminal.cols` changing in the snapshot.

```json
{"type":"unfit","pane":123}
```

`spawn_claude`: start `claude` in an open project root. `project` must equal a root in the snapshot exactly, as a folder inside a project is refused. `id` is echoed in `spawned` or `error`, and the answer comes once the new pane is in a snapshot, so an `attach` can follow at once.

```json
{"type":"spawn_claude","id":"r1","project":"/Users/jason/code/daedalus"}
```

`refresh`: ask for a fresh `snapshot`.

```json
{"type":"refresh"}
```

`transcript_open`: start following a session's conversation. The desktop answers with a `transcript` of kind `page` holding the newest `limit` items (at most 200), then sends `append` messages as the file grows. `session` must be an id in the latest snapshot, and a connection follows at most 4 sessions. Opening a session that is already followed sends a fresh `page`. Failures come back as `error` with the `id`.

```json
{"type":"transcript_open","id":"r5","session":"5d1f0c2e-8a47-4b9e-9c1a-3f6e2b7a9d10","limit":60}
```

`transcript_more`: load items older than `before`, a `pos` from an earlier `transcript`. The answer is a `transcript` of kind `older`. `limit` is at most 200, and a session that is not followed gets an `error` with the `id`, and so does a request that arrives while the desktop is re-reading that file (`busy`).

```json
{"type":"transcript_more","id":"r6","session":"5d1f0c2e-8a47-4b9e-9c1a-3f6e2b7a9d10","before":123456,"limit":60}
```

`tool_output`: the full output of one finished tool call, for when the preview on the item is not enough. `tool_id` is the item's `tool_id`. It needs the `tool_output` feature. The answer is a `tool_output` message with the `text` (ANSI stripped, at most 64 KiB, `cut` when it was longer), the `lines` of the whole output and the same `id`. The desktop reads it from the transcript file and keeps nothing. An `error` with the `id` answers a session that is not followed (`transcript is not open`), an unknown `tool_id` (`no such tool call`), a call that has not finished (`tool call has no output yet`), a file that changed since (`output is not available`), and a request that arrives while the desktop is re-reading that file (`busy`).

```json
{"type":"tool_output","id":"r7","session":"5d1f0c2e-8a47-4b9e-9c1a-3f6e2b7a9d10","tool_id":"toolu_01"}
```

```json
{"type":"tool_output","id":"r7","session":"5d1f0c2e-8a47-4b9e-9c1a-3f6e2b7a9d10","tool_id":"toolu_01","text":"Already up to date.\n","lines":1}
```

`transcript_close`: stop following a session. The server sends nothing back.

```json
{"type":"transcript_close","session":"5d1f0c2e-8a47-4b9e-9c1a-3f6e2b7a9d10"}
```

`prompt`: type `text` into a terminal pane and submit it. `text` is at most 16 KiB (`prompt is too long` beyond that), and the desktop strips C0 control characters and DEL except `\n` and `\t`, so the phone need not. The pane must be in the latest snapshot (`pane is not open` otherwise). The desktop pastes the text, waits 80 ms, checks the pane again and presses Enter. It refuses with `Claude is asking something; answer it first` when Claude is in `needs_input`, at the first check or at the second. In that case the text may already sit in the prompt. A running Claude accepts it and queues the message. Any text already in the prompt is not checked, and the new text goes after it. `id` is echoed in `sent`, once Enter is pressed, or in `error`. The desktop never logs the text.

```json
{"type":"prompt","id":"r7","pane":123,"text":"run the tests\nthen commit"}
```

`interrupt`: send Esc to a pane whose Claude is `running`. A pane that is not in the latest snapshot gets `pane is not open`, one that is not a Claude session gets `this is not a Claude session`, and Claude in any other state gets `Claude is not running`. `id` is echoed in `sent` or `error`.

```json
{"type":"interrupt","id":"r10","pane":123}
```

One `prompt` or `interrupt` per pane at a time: a second one for a pane whose first has no reply yet gets `error` `busy`, without acting on it.

`answer`: answer the dialog open in a Claude pane, one verified key at a time. `key` is the `terminal.dialog.key` the phone showed. The pane must be in the latest snapshot and Claude must be in `needs_input` (`this is not a Claude session` or `Claude is not asking anything` otherwise), and a second answer for the same pane while one runs gets `Already answering`.
- A `select` dialog takes `answers: [{"pick":["<label>"]}]`; a `review` dialog takes `answers: []` and presses "Submit answers".
- A `question` dialog takes one entry per question, in tab order, each with the `question` text as the dialog shows it. A single-select entry has one `pick` label or an `other` text, not both. A multi-select entry lists every `pick` it wants checked, plus an optional `other`.
- `other` is typed into the Other row and must be one line: any control character, `\n` and `\t` included, is refused with `answer text has control characters`, because a newline typed into the terminal would act as Enter. It is at most 2 KiB.
- At most 8 entries, 12 picks each, and labels and `question` at most 200 characters (`answer is too large`).
- A label matches a screen row ignoring whitespace runs and the curly or straight apostrophe, and a screen label cut with `…` matches by prefix. A label that matches no row, or more than one, stops the answer.
- The desktop sends one key, reads the pane again, and sends the next only when the screen shows what that key should have done. It never sends Esc, and it stops at the first surprise with `error` `Couldn't confirm the screen, open the terminal (step N: reason)`. The pane is left as it is, so a half-filled form is visible at the desk and the next `snapshot` shows it again. It gives up after 80 keys or 20 s.
- The reply `sent` comes once the dialog is gone or has changed after the last Enter. A key pressed at the desk in the middle of an answer makes the check fail, and a press in the few milliseconds before the final Enter can still pick another row.
- The desktop never logs a label, `other` or prompt text.

```json
{"type":"answer","id":"r12","pane":123,"key":"3f2a9c10","answers":[{"question":"Which colour?","pick":["Blue"]},{"question":"Which sizes?","pick":["A","B"],"other":"XXL"}]}
```

`usage_refresh`: fetch the Claude plan limits again. The desktop fetches at most once every 60 s, so a request inside that window changes nothing and the phone keeps the current `usage`. `id` is echoed in `sent` straight away, and the new figures arrive in a later `snapshot`. When the Mac's usage indicator is off, the answer is `error` with `usage indicator is off`.

```json
{"type":"usage_refresh","id":"r11"}
```

## Desktop to phone (`/v1/connect`)

`welcome`: answer to `hello`. `server` is the server id: 16 lowercase hex characters, generated once and stored in the Daedalus data directory, so it stays the same across restarts. Phones key their pairing by it. `features` lists the optional message groups the desktop handles (`transcript` for the transcript messages, `usage` for `snapshot.usage` and `usage_refresh`, `act` for `prompt` and `interrupt`, `dialog` for `terminal.dialog`, `answer` for `answer`, `subagents` for `subagents`, `fit` for `fit`, `unfit` and `terminal.fitted`, `tool_output` for the output fields on tool items and the `tool_output` request), and is omitted when empty.

```json
{"type":"welcome","proto":1,"server":"a1b2c3d4e5f60718","name":"Jason's Mac","app":"2.0.1","features":["transcript","usage","act","dialog","answer","subagents","fit","tool_output"]}
```

`snapshot`: the whole state. `session.unread` is a boolean and `session.files` is the count of changed files. `session.state` and `terminal.claude` are `running`, `needs_input`, `finished` or `idle`. The desktop omits `terminal.claude` when no Claude session runs in the pane, and never sends `idle` there. See `design/protocol/snapshot.json` for a full example with every optional field set.

```json
{"type":"snapshot","seq":7,"projects":[{"root":"/Users/jason/code/daedalus","name":"daedalus","window":1}],"sessions":[],"terminals":[{"pane":124,"project":"/Users/jason/code/daedalus","title":"zsh","rows":24,"cols":80}]}
```

`terminal.dialog` is present only while the pane runs Claude in `needs_input` and the desktop recognised the dialog on its screen, and is omitted otherwise. The desktop reads the visible text only, so a dialog it does not recognise is omitted or sent as `unknown`, never guessed. See `design/protocol/snapshot_dialog.json`.
- `kind` is `select` (a single `❯` list: permission prompt, plan approval, trust dialog), `question` (a list with an Other row, checkboxes or a tab header), `review` (the review screen before a question set is submitted) or `unknown` (it has a known footer and zero, several or unplaceable `❯` rows; show it read-only and offer "Open terminal"). A phone decoding an unrecognised `kind` treats it as `unknown`.
- `key` is 8 hex characters of FNV-1a. With tabs it hashes the kind and the tab labels, plus the summary lines for `review`. Without tabs it hashes the kind, title, prompt, body `lines` (the command or file shown), each option label and each input row's text, so the next prompt with the same labels has another key. It does not change when the highlight, a checkbox or the current tab moves. Text typed into an Other row can change it, since the row then reads as a plain option.
- `title`, `lines` (at most 12 of at most 200 characters) and `prompt` are the text above the rows. For a `question`, `title` is the question under the tab header, and `lines` and `prompt` are empty. For `select` and `review`, `prompt` is the last line ending in `?` above the rows and `lines` holds the rest.
- `options` lists at most 12 rows, in screen order, with labels of at most 200 characters. `description` is the dim line under a row, and `checked` is present only on multi-select rows (`multi` is then true). `input` marks a row that takes typed text: the Other row (`label` is `Other`, `text` empty) and the `Yes, and don’t ask again for` row, whose `text` (at most 500 characters) is the prefix currently in the field.
- `tabs` is absent without a tab header, and `answered` is true for `☒`. Which tab is current is shown only by colour, so it is not sent. `submit` is the label of the unnumbered `Next` or `Submit` row that follows a multi-select's options.
- `highlight` is the index of the row with `❯`. When the submit row is focused it equals `options.len()`. It is absent when no row can be placed (`unknown`).
- `partial` is true when scroll hints (`↑`, `↓`) show, more than 12 rows exist, or a line among the rows could not be placed, and always for `unknown`. A partial dialog is for reading only: the phone offers "Open terminal" and no answer.

```json
{"type":"snapshot","seq":9,"projects":[],"sessions":[],"terminals":[{"pane":123,"project":"/Users/jason/code/daedalus","title":"claude","program":"claude","claude":"needs_input","rows":40,"cols":120,"dialog":{"key":"5e0b2f11","kind":"question","title":"Which sizes?","tabs":[{"label":"Color","answered":true},{"label":"Size","answered":false}],"multi":true,"options":[{"label":"Red","description":"warm","checked":true},{"label":"Other","checked":false,"input":true,"text":""}],"submit":"Submit","highlight":2,"partial":false}}]}
```

`snapshot.usage` lists the Claude plan limits, one entry per Claude sign-in on the Mac, and is omitted when the Mac's usage indicator is off or nothing has been fetched yet. `status` is `ok`, `stale` (the saved sign-in expired), `no_access` (the Keychain refused), `rate_limited` or `unavailable`. `windows` holds the figures and is empty unless `status` is `ok`. A window has a `label` (`5h`, `7d` or a model name), `used` (percent, 0 to 100) and `resets_at` (epoch seconds, omitted when unknown). `message` is present only for `unavailable`. See `design/protocol/snapshot_usage.json`.

```json
{"type":"snapshot","seq":8,"projects":[],"sessions":[],"terminals":[],"usage":[{"profile":"claude","status":"ok","windows":[{"label":"5h","used":42.5,"resets_at":1767240000},{"label":"7d","used":18.0,"resets_at":1767700000},{"label":"Opus","used":7.0}]},{"profile":"claude-work","status":"unavailable","windows":[],"message":"the usage service did not answer"}]}
```

`attached`: the pane's size, sent before its replay.

```json
{"type":"attached","pane":123,"rows":40,"cols":120}
```

`fitted`: the pane has been resized, sent in answer to `fit`. `rows` and `cols` are the size that was applied, which can differ from the request after clamping. While a lease is held, `terminal.fitted` is `true` in the snapshot and `terminal.rows` and `terminal.cols` report the leased size.

```json
{"type":"fitted","id":"r20","pane":123,"rows":44,"cols":57}
```

`replay_done`: the replay tail has been sent; later frames are live.

```json
{"type":"replay_done","pane":123}
```

`exited`: the pane's process ended. `code` is absent when the exit status is unknown.

```json
{"type":"exited","pane":123,"code":0}
```

`dropped`: the server stopped streaming a pane. `lagging` means the phone read too slowly (more than 2 MiB of output waiting to be sent) or its input could not be forwarded fast enough, and `closed` means the session daemon could not be reached or hung up. Frames already queued for the pane are discarded. The phone re-attaches when the pane is visible.

```json
{"type":"dropped","pane":123,"reason":"lagging"}
```

`spawned`: answer to `spawn_claude`.

```json
{"type":"spawned","id":"r1","pane":456}
```

`error`: a failed request. `id` is present when it answers a request that had one.

```json
{"type":"error","id":"r1","message":"project is not open in a window"}
```

`notice`: a session needs attention. `kind` is `needs_input` or `stopped`, and `at` is epoch milliseconds.

```json
{"type":"notice","pane":123,"kind":"needs_input","message":"Run cargo test?","at":1767225600000}
```

`transcript`: a part of a session's conversation, in file order. `kind` says how the phone applies `items`:

- `page` replaces everything. It is sent on open, and again when the file shrinks or is replaced.
- `older` prepends.
- `append` upserts by `id`: new items, or tool items whose result has arrived.

`before` is the `pos` to pass to `transcript_more`, and is absent when the page reaches the start of the file. Each message is at most 512 KiB serialized, so the desktop drops the oldest items of a page to fit.

```json
{"type":"transcript","session":"5d1f0c2e-8a47-4b9e-9c1a-3f6e2b7a9d10","kind":"page","items":[{"id":"u1","pos":1024,"at":1767225600000,"kind":"user","text":"run the tests"}],"before":1024}
```

An item has `id`, `pos` (byte offset in the file), `at` (epoch milliseconds) and `kind`, which is `user`, `assistant`, `tool`, `question` or `note`. `note` covers compaction, interruptions and `/command` lines. The other fields are optional and omitted when absent:

| Field | Meaning |
| --- | --- |
| `text` | The message text. Caps: assistant 24 KiB, user 8 KiB. |
| `cut` | `true` when `text` was truncated. |
| `tool`, `tool_id` | The tool name and its call id, on `tool` and `question` items. |
| `summary` | A one-line description of the call, at most 200 characters. |
| `status` | `pending`, `ok` or `error`. A tool or question item stays `pending` until its result arrives. |
| `detail` | The first 1 KiB of an error result. |
| `questions` | On a `question` item: each has `question`, `header`, `multi` and `options` (`label`, `description`). |
| `answer` | On a `question` item: the result text, cut to 1 KiB. |
| `output` | On a finished `tool` item that did not fail: the first 6 non-empty lines of its output, each cut to 200 characters and 1 KiB in all, with ANSI escape sequences and control characters removed. Not sent for `Read`, `Agent`, `Task`, `TodoWrite` or `AskUserQuestion`. |
| `lines` | The line count of the whole output. For `Read` it is the number of lines read. |
| `files` | The files the call changed, at most 5. Each has `path` (relative to the session's cwd, at most 200 characters), `added`, `removed` and `created` (omitted unless `true`). |
| `more_files` | The number of changed files beyond `files`. |
| `permission` | How the call was allowed or refused: `auto`, `mode`, `rule`, `user`, `denied_auto`, `denied_hook` or `denied`. Omitted when unknown. |

`output`, `lines`, `files`, `more_files` and `permission` come from fields Claude Code writes to the transcript but does not document (`toolUseResult` and `permissionDecision`), so any of them can be absent.

`subagents`: the sub-agents Claude launched in a session with an open transcript, as the desktop reads them from the transcript and the agents' own files. The list is complete, so each message replaces the phone's copy for that session, and a `transcript` message of kind `page` clears it. It carries no prompt or result text, only the call's short `description` and tool names.

```json
{"type":"subagents","session":"5d1f0c2e-8a47-4b9e-9c1a-3f6e2b7a9d10","agents":[{"tool_id":"toolu_01A","agent_id":"a1b2c3d4e5f607182","agent_type":"builder","description":"Build the transcript parser","state":"running","started_at":1767225600000,"active_at":1767225660000,"tools":12,"tool":"Bash"},{"tool_id":"toolu_01B","agent_id":"a9f8e7d6c5b4a3921","agent_type":"Explore","description":"Find the callers","state":"done","started_at":1767225500000,"ended_at":1767225590000,"tools":7}]}
```

| Field | Meaning |
| --- | --- |
| `tool_id` | Id of the `Agent` call that launched it. It joins to `tool_id` on the chat item. |
| `agent_id` | Claude's agent id, once the launch result has arrived. |
| `agent_type` | The call's `subagent_type`, at most 64 characters, `""` when absent. |
| `description` | The call's `description`, at most 200 characters. |
| `state` | `running`, `done`, `failed` or `stopped`. A phone treats any other value as unknown. |
| `started_at` | Epoch milliseconds of the launch, or of the latest resume. |
| `ended_at` | Epoch milliseconds when it reached a terminal state. Absent while `running`. |
| `active_at` | Epoch milliseconds of the newest assistant line in the agent's own file. |
| `tools` | Tool calls so far. Omitted when 0. |
| `tool` | The latest tool name, at most 64 characters. Sent only while `running`. |

The optional fields are omitted when absent. The list holds at most 20 agents: running ones first, newest `started_at` first, then ended ones, newest `ended_at` first. Only the session's own direct sub-agents are listed.

The desktop sends `subagents` after the `page` of a `transcript_open` and after a reset page when the list is not empty, and whenever the list changes while the transcript is open, even to empty. At most one is queued per session, and a newer one replaces it. The phone sends nothing new.

`sent`: answer to `prompt`, `interrupt` or `answer` once the keys have been written to the pane, and to `usage_refresh` once the desktop has handled it.

```json
{"type":"sent","id":"r7"}
```

## Pair endpoint (`/v1/pair`)

The phone sends `pair`. The desktop asks its user to confirm, waiting up to 60 s.

```json
{"type":"pair","device":"Jason's iPhone","proto":1}
```

On approval the desktop answers `paired` and closes. The token is 64 hex characters and is shown once.

```json
{"type":"paired","token":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","server":"a1b2c3d4e5f60718","name":"Jason's Mac"}
```

Otherwise it answers `error` (without `id`) and closes.

## Pairing URL

The pairing QR code encodes one URL; the code in it is the `X-Daedalus-Pair` value for `/v1/pair`.

```
daedalus://pair?host=mac.tail1234.ts.net&ip=100.64.1.2&port=47123&code=0123456789abcdef0123456789abcdef&sid=a1b2c3d4e5f60718
```

| Key | Value |
| --- | --- |
| `host` | The Mac's MagicDNS name. Omitted when it could not be looked up. |
| `ip` | The Mac's Tailscale IPv4 address. Omitted when it has none. |
| `port` | The server's port. |
| `code` | The one-time pairing code: 32 lowercase hex characters, valid for 5 minutes and one attempt. |
| `sid` | The server id from `welcome`: 16 lowercase hex characters. |

`host` or `ip` may be omitted but never both. Values are percent-encoded.
