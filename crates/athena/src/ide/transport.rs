use std::io;
use std::task::Poll;

use futures::{Sink, SinkExt, Stream, StreamExt, future, stream};
use rmcp::RoleServer;
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

use super::lock::tokens_match;

pub const AUTH_HEADER: &str = "x-claude-code-ide-authorization";

/// Lets in only a client holding the lock file's token; anything carrying an `Origin` is a
/// browser page, which has no business here even with the token.
// The error type is tungstenite's handshake callback contract.
#[allow(clippy::result_large_err)]
pub fn admit(
    request: &Request,
    mut response: Response,
    token: &str,
) -> Result<Response, ErrorResponse> {
    let headers = request.headers();
    if headers.contains_key("origin") {
        return Err(refuse(StatusCode::FORBIDDEN));
    }
    let given = headers.get(AUTH_HEADER).map(HeaderValue::as_bytes);
    if !tokens_match(given.unwrap_or_default(), token.as_bytes()) {
        return Err(refuse(StatusCode::UNAUTHORIZED));
    }
    let wants_mcp = headers
        .get_all("sec-websocket-protocol")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|p| p.trim() == "mcp");
    if wants_mcp {
        response
            .headers_mut()
            .insert("sec-websocket-protocol", HeaderValue::from_static("mcp"));
    }
    Ok(response)
}

fn refuse(status: StatusCode) -> ErrorResponse {
    let mut response = ErrorResponse::new(None);
    *response.status_mut() = status;
    response
}

/// JSON-RPC over WebSocket text frames, as rmcp's sink and stream. The stream ends at a close,
/// calling `on_end` first: rmcp waits for unfinished calls before it lets go of a connection.
pub fn json_rpc<S>(
    ws: WebSocketStream<S>,
    on_end: impl FnOnce() + Send + Unpin + 'static,
) -> (
    impl Sink<TxJsonRpcMessage<RoleServer>, Error = WsError> + Send + Unpin + 'static,
    impl Stream<Item = RxJsonRpcMessage<RoleServer>> + Send + Unpin + 'static,
)
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let (sink, stream) = ws.split();
    let sink = sink.with(|msg: TxJsonRpcMessage<RoleServer>| {
        future::ready(
            serde_json::to_string(&msg)
                .map(Message::text)
                .map_err(|e| WsError::Io(io::Error::other(e))),
        )
    });
    let stream = stream
        .take_while(|m| future::ready(!matches!(m, Err(_) | Ok(Message::Close(_)))))
        .filter_map(|m| {
            future::ready(match m {
                Ok(Message::Text(text)) => serde_json::from_str(&text)
                    .inspect_err(|e| {
                        tracing::debug!("ide: dropped a frame that is not JSON-RPC: {e}")
                    })
                    .ok(),
                _ => None,
            })
        });
    let mut on_end = Some(on_end);
    let end = stream::poll_fn(move |_| {
        if let Some(f) = on_end.take() {
            f();
        }
        Poll::Ready(None)
    });
    (sink, stream.chain(end))
}
