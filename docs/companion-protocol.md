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

- Text frames carry one JSON object with a snake_case `"type"`. An unknown `type` is a decode error.
- Binary frames carry terminal bytes: `[kind u8][pane u64 big-endian][payload]`. Kind `1` is output (desktop to phone), kind `2` is input (phone to desktop). The payload is at most 64 KiB, and a frame with a shorter header, an unknown kind or a larger payload is invalid.
- Optional fields are omitted when absent.

## Session flow

1. The phone sends `hello` first. If the `proto` major differs, the server closes with code 4001.
2. The server answers `welcome`, then a `snapshot`, and another whenever state changes (at most every 250 ms).
3. `attach` starts a pane: `attached`, binary output frames for the replay tail, `replay_done`, then live frames.

## Phone to desktop (`/v1/connect`)

`hello`: first message. `proto` is the protocol version.

```json
{"type":"hello","proto":1,"app":"0.1.0","device":"Jason's iPhone"}
```

`attach`: replay up to `tail` bytes (capped at 1 MiB), then stream live output.

```json
{"type":"attach","pane":123,"tail":262144}
```

`detach`: stop streaming a pane.

```json
{"type":"detach","pane":123}
```

`spawn_claude`: start `claude` in an open project root. `id` is echoed in `spawned` or `error`.

```json
{"type":"spawn_claude","id":"r1","project":"/Users/jason/code/daedalus"}
```

`refresh`: ask for a fresh `snapshot`.

```json
{"type":"refresh"}
```

## Desktop to phone (`/v1/connect`)

`welcome`: answer to `hello`. `server` is the server id: 16 lowercase hex characters, generated once and stored in the Daedalus data directory, so it stays the same across restarts. Phones key their pairing by it.

```json
{"type":"welcome","proto":1,"server":"a1b2c3d4e5f60718","name":"Jason's Mac","app":"2.0.1"}
```

`snapshot`: the whole state. `session.unread` is a boolean and `session.files` is the count of changed files. `session.state` and `terminal.claude` are `running`, `needs_input`, `finished` or `idle`. The desktop omits `terminal.claude` when no Claude session runs in the pane, and never sends `idle` there. See `design/protocol/snapshot.json` for a full example with every optional field set.

```json
{"type":"snapshot","seq":7,"projects":[{"root":"/Users/jason/code/daedalus","name":"daedalus","window":1}],"sessions":[],"terminals":[{"pane":124,"project":"/Users/jason/code/daedalus","title":"zsh","rows":24,"cols":80}]}
```

`attached`: the pane's size, sent before its replay.

```json
{"type":"attached","pane":123,"rows":40,"cols":120}
```

`replay_done`: the replay tail has been sent; later frames are live.

```json
{"type":"replay_done","pane":123}
```

`exited`: the pane's process ended. `code` is absent when the exit status is unknown.

```json
{"type":"exited","pane":123,"code":0}
```

`dropped`: the server stopped streaming a pane (`lagging` or `closed`). The phone re-attaches when the pane is visible.

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
