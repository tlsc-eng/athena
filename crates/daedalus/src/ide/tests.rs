//! A fake Claude Code replaying the frames 2.1.295 sends, against a real server on loopback.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderName;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{WebSocketStream, client_async};

use super::*;

const INITIALIZE: &str = r#"{"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"roots":{},"elicitation":{}},"clientInfo":{"name":"claude-code","version":"2.1.295"}},"jsonrpc":"2.0","id":0}"#;
const INITIALIZED: &str = r#"{"method":"notifications/initialized","jsonrpc":"2.0"}"#;
const IDE_CONNECTED: &str = r#"{"method":"ide_connected","params":{"pid":4242},"jsonrpc":"2.0"}"#;
const TAB: &str = "\u{273b} [Claude Code] main.rs (a1b2c3) \u{29c9}";

struct Fixture {
    dir: PathBuf,
    server: Option<Server>,
    events: async_channel::Receiver<Event>,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("athena-ide-t-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (server, events) = Server::start(Config {
            lock_dir: dir.join("ide"),
            env_file: Some(dir.join("ide.env")),
            folders: vec![PathBuf::from("/p")],
        })
        .unwrap();
        Self {
            dir,
            server: Some(server),
            events,
        }
    }

    fn server(&self) -> &Server {
        self.server.as_ref().unwrap()
    }

    fn lock(&self) -> Value {
        let path = self.dir.join(format!("ide/{}.lock", self.server().port()));
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    fn token(&self) -> String {
        self.lock()["authToken"].as_str().unwrap().to_string()
    }

    async fn dial(&self, headers: &[(&str, &str)]) -> Result<WebSocketStream<TcpStream>, WsError> {
        let port = self.server().port();
        let mut request = format!("ws://127.0.0.1:{port}")
            .into_client_request()
            .unwrap();
        for (k, v) in headers {
            let name = HeaderName::from_bytes(k.as_bytes()).unwrap();
            request
                .headers_mut()
                .insert(name, v.parse().expect("header value"));
        }
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        client_async(request, stream).await.map(|(ws, response)| {
            let protocol = response.headers().get("sec-websocket-protocol");
            assert_eq!(protocol.and_then(|v| v.to_str().ok()), Some("mcp"));
            ws
        })
    }

    /// Connects and replays Claude Code's handshake, returning the client id the window saw.
    async fn connect(&self) -> (Client, u64) {
        let token = self.token();
        let ws = self
            .dial(&[
                ("x-claude-code-ide-authorization", &token),
                ("sec-websocket-protocol", "mcp"),
                ("user-agent", "claude-cli/2.1.295"),
            ])
            .await
            .expect("handshake");
        let mut client = Client { ws, next_id: 1 };
        client.send(INITIALIZE).await;
        let init = client.response(0).await;
        assert!(
            init["result"]["capabilities"]["tools"].is_object(),
            "{init}"
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "athena");
        client.send(INITIALIZED).await;
        let Event::Client {
            client: id,
            pid: None,
        } = self.event().await
        else {
            panic!("expected a new client")
        };
        client.send(IDE_CONNECTED).await;
        let Event::Client { client: same, pid } = self.event().await else {
            panic!("expected the client's pid")
        };
        assert_eq!((same, pid), (id, Some(4242)));
        (client, id)
    }

    async fn event(&self) -> Event {
        tokio::time::timeout(Duration::from_secs(5), self.events.recv())
            .await
            .expect("no event in time")
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        drop(self.server.take());
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Client {
    ws: WebSocketStream<TcpStream>,
    next_id: u64,
}

impl Client {
    async fn send(&mut self, frame: &str) {
        self.ws.send(Message::text(frame)).await.unwrap();
    }

    async fn next(&mut self) -> Value {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(5), self.ws.next())
                .await
                .expect("no frame in time")
                .expect("connection closed")
                .unwrap();
            if let Message::Text(text) = msg {
                return serde_json::from_str(&text).unwrap();
            }
        }
    }

    async fn response(&mut self, id: u64) -> Value {
        loop {
            let frame = self.next().await;
            if frame["id"] == id {
                return frame;
            }
        }
    }

    async fn call(&mut self, tool: &str, arguments: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let frame = json!({
            "method": "tools/call",
            "params": {"name": tool, "arguments": arguments},
            "jsonrpc": "2.0",
            "id": id,
        });
        self.send(&frame.to_string()).await;
        id
    }

    async fn texts(&mut self, id: u64) -> Vec<String> {
        let frame = self.response(id).await;
        frame["result"]["content"]
            .as_array()
            .unwrap_or_else(|| panic!("no content: {frame}"))
            .iter()
            .map(|c| c["text"].as_str().unwrap().to_string())
            .collect()
    }
}

fn open_diff_args() -> Value {
    json!({
        "old_file_path": "/p/src/main.rs",
        "new_file_path": "/p/src/main.rs",
        "new_file_contents": "fn main() {}\n",
        "tab_name": TAB,
    })
}

fn run(test: impl AsyncFnOnce()) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(test());
}

#[test]
fn the_handshake_answers_tools_and_unknown_tools_get_an_error() {
    run(async || {
        let f = Fixture::new("hello");
        let (mut client, _) = f.connect().await;
        client
            .send(r#"{"method":"tools/list","jsonrpc":"2.0","id":7}"#)
            .await;
        let list = client.response(7).await;
        let mut names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "closeAllDiffTabs",
                "close_tab",
                "getDiagnostics",
                "openDiff"
            ]
        );
        let id = client.call("executeCode", json!({"code": "1"})).await;
        let reply = client.response(id).await;
        assert!(reply["error"].is_object(), "{reply}");
    });
}

#[test]
fn a_wrong_or_missing_token_and_any_origin_are_refused_before_the_upgrade() {
    run(async || {
        let f = Fixture::new("auth");
        let status = |r: Result<WebSocketStream<TcpStream>, WsError>| match r {
            Err(WsError::Http(response)) => response.status().as_u16(),
            Err(e) => panic!("unexpected error {e}"),
            Ok(_) => panic!("connection was accepted"),
        };
        let wrong = f.dial(&[("x-claude-code-ide-authorization", "0000")]).await;
        assert_eq!(status(wrong), 401);
        assert_eq!(status(f.dial(&[]).await), 401);
        let token = f.token();
        let origin = f
            .dial(&[
                ("x-claude-code-ide-authorization", &token),
                ("origin", "https://example.com"),
            ])
            .await;
        assert_eq!(status(origin), 403);
        assert!(
            f.events.is_empty(),
            "a refused connection reaches the window"
        );
    });
}

/// Whether the server closes `stream` within `wait`.
async fn closed(stream: &mut TcpStream, wait: Duration) -> bool {
    use tokio::io::AsyncReadExt;
    let mut buf = [0; 64];
    match tokio::time::timeout(wait, stream.read(&mut buf)).await {
        Ok(Ok(n)) => n == 0,
        Ok(Err(_)) => true,
        Err(_) => false,
    }
}

#[test]
fn connections_waiting_to_authenticate_are_capped_and_time_out() {
    run(async || {
        let f = Fixture::new("idle");
        let port = f.server().port();
        let mut idle = Vec::new();
        for _ in 0..MAX_HANDSHAKES {
            idle.push(TcpStream::connect(("127.0.0.1", port)).await.unwrap());
        }
        let mut extra = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        assert!(
            closed(&mut extra, HANDSHAKE_TIMEOUT / 2).await,
            "one over the cap is dropped at once"
        );
        assert!(
            !closed(&mut idle[0], Duration::from_millis(50)).await,
            "the ones before it wait for their handshake"
        );
        for stream in &mut idle {
            assert!(closed(stream, HANDSHAKE_TIMEOUT * 3).await, "timed out");
        }
        f.connect().await;
    });
}

#[test]
fn an_accepted_diff_returns_file_saved_with_the_contents_and_a_rejected_one_says_so() {
    run(async || {
        let f = Fixture::new("diff");
        let (mut client, id) = f.connect().await;

        let call = client.call("openDiff", open_diff_args()).await;
        let Event::OpenDiff {
            key,
            path,
            contents,
        } = f.event().await
        else {
            panic!("expected openDiff")
        };
        assert_eq!(
            key,
            DiffKey {
                client: id,
                tab: TAB.into()
            }
        );
        assert_eq!(path, Path::new("/p/src/main.rs"));
        assert_eq!(contents, "fn main() {}\n");
        assert!(f.server().is_pending(&key));
        assert!(
            f.server()
                .resolve(&key, Verdict::Accepted(contents.clone()))
        );
        assert_eq!(client.texts(call).await, ["FILE_SAVED", "fn main() {}\n"]);
        assert!(!f.server().is_pending(&key));

        let call = client.call("openDiff", open_diff_args()).await;
        let Event::OpenDiff { key, .. } = f.event().await else {
            panic!("expected openDiff")
        };
        f.server().resolve(&key, Verdict::Rejected);
        assert_eq!(client.texts(call).await, ["DIFF_REJECTED", TAB]);
    });
}

#[test]
fn close_tab_from_claude_withdraws_the_pending_diff() {
    run(async || {
        let f = Fixture::new("close");
        let (mut client, _) = f.connect().await;
        let open = client.call("openDiff", open_diff_args()).await;
        let Event::OpenDiff { key, .. } = f.event().await else {
            panic!("expected openDiff")
        };
        let close = client.call("close_tab", json!({"tab_name": TAB})).await;
        assert_eq!(client.texts(close).await, ["TAB_CLOSED"]);
        assert_eq!(client.texts(open).await, ["DIFF_REJECTED", TAB]);
        let Event::CloseDiff { key: closed } = f.event().await else {
            panic!("expected the tab to close")
        };
        assert_eq!(closed, key);
        assert!(!f.server().resolve(&key, Verdict::Rejected));

        let open = client.call("openDiff", open_diff_args()).await;
        f.event().await;
        let all = client.call("closeAllDiffTabs", json!({})).await;
        assert_eq!(client.texts(all).await, ["CLOSED_1_DIFF_TABS"]);
        assert_eq!(client.texts(open).await[0], "DIFF_REJECTED");
    });
}

#[test]
fn diagnostics_echo_the_requested_uri_and_list_every_file_otherwise() {
    run(async || {
        let f = Fixture::new("diag");
        let (mut client, _) = f.connect().await;
        let uri = "file:///p/src/main.rs";
        let call = client.call("getDiagnostics", json!({"uri": uri})).await;
        let Event::Diagnostics { path, reply } = f.event().await else {
            panic!("expected a diagnostics request")
        };
        assert_eq!(path.as_deref(), Some(Path::new("/p/src/main.rs")));
        let diagnostic = Diagnostic {
            message: "unused variable".into(),
            severity: "Warning",
            source: Some("rustc".into()),
            code: Some(json!("unused_variables")),
            range: Range {
                start: Position {
                    line: 2,
                    character: 4,
                },
                end: Position {
                    line: 2,
                    character: 5,
                },
            },
        };
        let _ = reply.send(vec![FileDiagnostics {
            path: PathBuf::from("/p/src/main.rs"),
            diagnostics: vec![diagnostic.clone()],
        }]);
        let text = client.texts(call).await.remove(0);
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            parsed,
            json!([{"uri": uri, "diagnostics": [{
                "message": "unused variable", "severity": "Warning", "source": "rustc",
                "code": "unused_variables",
                "range": {"start": {"line": 2, "character": 4}, "end": {"line": 2, "character": 5}},
            }]}])
        );

        let call = client.call("getDiagnostics", json!({})).await;
        let Event::Diagnostics { path: None, reply } = f.event().await else {
            panic!("expected a request for every file")
        };
        let _ = reply.send(vec![FileDiagnostics {
            path: PathBuf::from("/p/b.go"),
            diagnostics: vec![diagnostic],
        }]);
        let parsed: Value = serde_json::from_str(&client.texts(call).await[0]).unwrap();
        assert_eq!(parsed[0]["uri"], "file:///p/b.go");

        // A window too busy to answer in time still gets Claude an answer for the file it asked about.
        let call = client.call("getDiagnostics", json!({"uri": uri})).await;
        let Event::Diagnostics { reply: _held, .. } = f.event().await else {
            panic!("expected a diagnostics request")
        };
        let parsed: Value = serde_json::from_str(&client.texts(call).await[0]).unwrap();
        assert_eq!(parsed, json!([{"uri": uri, "diagnostics": []}]));
    });
}

#[test]
fn notifications_reach_only_the_clients_named() {
    run(async || {
        let f = Fixture::new("notify");
        let (mut first, a) = f.connect().await;
        let (mut second, b) = f.connect().await;
        assert_eq!(f.server().clients(), [(a, Some(4242)), (b, Some(4242))]);
        let selection = json!({
            "text": "fn main",
            "filePath": "/p/src/main.rs",
            "fileUrl": "file:///p/src/main.rs",
            "selection": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 7}, "isEmpty": false},
        });
        f.server()
            .notify(&[b], "selection_changed", selection.clone());
        let frame = second.next().await;
        assert_eq!(frame["method"], "selection_changed");
        assert_eq!(frame["params"], selection);
        f.server().notify(
            &[a],
            "at_mentioned",
            json!({"filePath": "/p/x", "lineStart": 1, "lineEnd": 2}),
        );
        let frame = first.next().await;
        assert_eq!(frame["method"], "at_mentioned");
        assert_eq!(frame["params"]["lineEnd"], 2);
    });
}

#[test]
fn a_client_that_goes_away_closes_its_tabs() {
    run(async || {
        let f = Fixture::new("gone");
        let (mut client, id) = f.connect().await;
        client.call("openDiff", open_diff_args()).await;
        let Event::OpenDiff { key, .. } = f.event().await else {
            panic!("expected openDiff")
        };
        drop(client);
        let mut closed = false;
        loop {
            match f.event().await {
                Event::CloseDiff { key: k } => closed = k == key,
                Event::Disconnected { client } => {
                    assert_eq!(client, id);
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(closed);
        assert!(f.server().clients().is_empty());
    });
}

#[test]
fn stopping_rejects_pending_diffs_removes_the_lock_and_keeps_the_port_for_next_time() {
    let mut f = Fixture::new("stop");
    let lock = f.dir.join(format!("ide/{}.lock", f.server().port()));
    let port = f.server().port();
    assert!(lock.exists());
    assert_eq!(f.lock()["workspaceFolders"], json!(["/p"]));
    f.server
        .as_mut()
        .unwrap()
        .set_folders(vec![PathBuf::from("/p"), PathBuf::from("/q")]);
    assert_eq!(f.lock()["workspaceFolders"], json!(["/p", "/q"]));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (mut client, call) = runtime.block_on(async {
        let (mut client, _) = f.connect().await;
        let call = client.call("openDiff", open_diff_args()).await;
        f.event().await;
        (client, call)
    });
    drop(f.server.take());
    assert!(!lock.exists());
    let texts = runtime.block_on(client.texts(call));
    assert_eq!(texts[0], "DIFF_REJECTED");

    let (again, _events) = Server::start(Config {
        lock_dir: f.dir.join("ide"),
        env_file: Some(f.dir.join("ide.env")),
        folders: Vec::new(),
    })
    .unwrap();
    assert_eq!(again.port(), port, "the port shells were given is reused");
    again.turn_off();
    assert!(!f.dir.join("ide.env").exists());
}

#[test]
fn file_uris_with_spaces_and_unicode_decode_whether_encoded_or_not() {
    let path = Path::new("/p/my dir/日本#1.rs");
    assert_eq!(
        file_url(path),
        "file:///p/my%20dir/%E6%97%A5%E6%9C%AC%231.rs"
    );
    assert_eq!(session::uri_path(&file_url(path)), path);
    assert_eq!(session::uri_path("file:///p/my dir/日本#1.rs"), path);

    let dir = std::env::temp_dir().join(format!("athena-ide-uri-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let literal = dir.join("100%20done.txt");
    std::fs::write(&literal, "").unwrap();
    let uri = format!("file://{}", literal.display());
    assert_eq!(session::uri_path(&uri), literal, "only the raw name exists");
    std::fs::remove_dir_all(&dir).unwrap();
}
