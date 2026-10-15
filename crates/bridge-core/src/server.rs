//! WebSocket server: one task per connection, protocol v1.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse, Request, Response as HsResponse,
};
use tokio_tungstenite::tungstenite::http::{StatusCode, header};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::protocol::frame::{CloseFrame, coding::CloseCode};
use tokio_tungstenite::tungstenite::{Bytes, Message};

use crate::hub::Hub;
use crate::protocol::{
    APP_NAME, BridgeError, ErrorCode, Event, MAX_IN_FLIGHT, MAX_MESSAGE_BYTES, PROTOCOL_VERSION,
    RequestBody, Response, WS_PATH, parse_request, validate_apdus,
};
use crate::worker::{Job, WorkerHandle};

const PING_INTERVAL: Duration = Duration::from_secs(30);
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// A client that stops reading (e.g. a hung tab) fills the socket buffer; give up on it after this.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct ServerCtx {
    pub hub: Arc<Hub>,
    pub worker: WorkerHandle,
    pub version: String,
    pub port: u16,
}

/// Accept connections until `shutdown` flips, then give open connections a moment to close cleanly.
pub async fn serve(listener: TcpListener, ctx: ServerCtx, mut shutdown: watch::Receiver<bool>) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let ctx = ctx.clone();
                    let shutdown = shutdown.clone();
                    connections.spawn(connection(stream, ctx, shutdown));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            _ = shutdown.changed() => break,
        }
    }
    let _ = tokio::time::timeout(Duration::from_millis(500), async {
        while connections.join_next().await.is_some() {}
    })
    .await;
}

fn reject(status: StatusCode, msg: &str) -> ErrorResponse {
    let mut r = ErrorResponse::new(Some(msg.to_string()));
    *r.status_mut() = status;
    r
}

/// Short form of an Origin header for display: host[:port], or "local app" when absent.
fn origin_label(origin: Option<&str>) -> String {
    match origin {
        None | Some("null") | Some("") => "local app".into(),
        Some(o) => o
            .split_once("://")
            .map_or(o, |(_, rest)| rest)
            .trim_end_matches('/')
            .to_string(),
    }
}

// The handshake callback's error type is fixed by tungstenite.
#[allow(clippy::result_large_err)]
async fn connection(stream: TcpStream, ctx: ServerCtx, mut shutdown: watch::Receiver<bool>) {
    let _ = stream.set_nodelay(true);
    let allowed_hosts = [
        format!("127.0.0.1:{}", ctx.port),
        format!("localhost:{}", ctx.port),
    ];
    let mut origin: Option<String> = None;
    let callback = |req: &Request, resp: HsResponse| {
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        if !allowed_hosts.iter().any(|a| a.eq_ignore_ascii_case(host)) {
            tracing::warn!(host, "rejected connection: bad Host header");
            return Err(reject(StatusCode::FORBIDDEN, "Forbidden Host"));
        }
        if req.uri().path() != WS_PATH {
            return Err(reject(StatusCode::NOT_FOUND, "Not found"));
        }
        origin = req
            .headers()
            .get(header::ORIGIN)
            .and_then(|o| o.to_str().ok())
            .map(str::to_string);
        Ok(resp)
    };
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES));
    let ws = match tokio_tungstenite::accept_hdr_async_with_config(stream, callback, Some(config))
        .await
    {
        Ok(ws) => ws,
        Err(e) => {
            tracing::debug!(error = %e, "handshake failed");
            return;
        }
    };

    let client_id = ctx.hub.add_client(origin_label(origin.as_deref()));
    let (mut sink, mut stream) = ws.split();
    let (service, readers, mut events) = ctx.hub.snapshot_and_subscribe();
    let hello = Event::Hello {
        protocol: PROTOCOL_VERSION,
        app: APP_NAME.into(),
        version: ctx.version.clone(),
        service,
        readers,
    };

    // Responses from request tasks.
    let (out_tx, mut out_rx) = mpsc::channel::<Response>(MAX_IN_FLIGHT * 2);
    let in_flight = Arc::new(AtomicUsize::new(0));
    let mut ping = tokio::time::interval_at(Instant::now() + PING_INTERVAL, PING_INTERVAL);
    let mut last_seen = Instant::now();

    let mut close: Option<CloseFrame> = None;
    if send_json(&mut sink, &hello).await.is_ok() {
        loop {
            tokio::select! {
                msg = stream.next() => {
                    let Some(Ok(msg)) = msg else { break };
                    last_seen = Instant::now();
                    match msg {
                        Message::Text(text) => {
                            if let Some(resp) = handle_request(&text, &ctx, &out_tx, &in_flight)
                                && send_json(&mut sink, &resp).await.is_err()
                            {
                                break;
                            }
                        }
                        Message::Binary(_) => {
                            let resp = Response::err(Value::Null, BridgeError::bad_request("Binary frames are not supported"));
                            if send_json(&mut sink, &resp).await.is_err() {
                                break;
                            }
                        }
                        Message::Close(_) => break,
                        _ => {}
                    }
                }
                Some(resp) = out_rx.recv() => {
                    if send_json(&mut sink, &resp).await.is_err() {
                        break;
                    }
                }
                event = events.recv() => match event {
                    Ok(event) => {
                        if send_json(&mut sink, &event).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(missed = n, "client too slow; closing");
                        close = Some(CloseFrame { code: CloseCode::Again, reason: "Too slow; reconnect".into() });
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                _ = ping.tick() => {
                    if last_seen.elapsed() > IDLE_TIMEOUT {
                        tracing::info!("client idle; closing");
                        break;
                    }
                    if send(&mut sink, Message::Ping(Bytes::new())).await.is_err() {
                        break;
                    }
                }
                _ = shutdown.changed() => {
                    close = Some(CloseFrame { code: CloseCode::Away, reason: "Bridge shutting down".into() });
                    break;
                }
            }
        }
    }
    let _ = send(&mut sink, Message::Close(close)).await;
    let _ = tokio::time::timeout(SEND_TIMEOUT, sink.close()).await;
    ctx.hub.remove_client(client_id);
}

async fn send_json<S, T>(sink: &mut S, value: &T) -> Result<(), ()>
where
    S: SinkExt<Message> + Unpin,
    T: serde::Serialize,
{
    let text = serde_json::to_string(value).map_err(|_| ())?;
    send(sink, Message::text(text)).await
}

/// Send one message; fails if the socket is gone or the client has not read for `SEND_TIMEOUT`.
async fn send<S>(sink: &mut S, msg: Message) -> Result<(), ()>
where
    S: SinkExt<Message> + Unpin,
{
    match tokio::time::timeout(SEND_TIMEOUT, sink.send(msg)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(()),
        Err(_) => {
            tracing::info!("client stopped reading; closing");
            Err(())
        }
    }
}

/// Handle one request. Immediate answers are returned; card jobs answer later through `out`.
fn handle_request(
    text: &str,
    ctx: &ServerCtx,
    out: &mpsc::Sender<Response>,
    in_flight: &Arc<AtomicUsize>,
) -> Option<Response> {
    let req = match parse_request(text) {
        Ok(r) => r,
        Err((id, e)) => return Some(Response::err(id, e)),
    };
    let id = req.id;
    let (job, reply): (Job, ReplyFuture) = match req.body {
        RequestBody::Ping => return Some(Response::ok(id, json!({}))),
        RequestBody::ListReaders => {
            let (service, readers) = ctx.hub.status_snapshot();
            return Some(Response::ok(
                id,
                json!({ "service": service, "readers": readers }),
            ));
        }
        RequestBody::Transmit {
            reader,
            apdus,
            stop_on_error,
            unsafe_write,
        } => {
            let apdus = match validate_apdus(&apdus, unsafe_write) {
                Ok(a) => a,
                Err(e) => return Some(Response::err(id, e)),
            };
            if let Some(e) = check_reader(ctx, &reader) {
                return Some(Response::err(id, e));
            }
            let (tx, rx) = oneshot::channel();
            let reply = Box::pin(async move {
                received(rx)
                    .await
                    .map(|responses| json!({ "responses": responses }))
            });
            (
                Job::Transmit {
                    reader,
                    apdus,
                    stop_on_error,
                    reply: tx,
                },
                reply,
            )
        }
        RequestBody::Reset { reader } => {
            if let Some(e) = check_reader(ctx, &reader) {
                return Some(Response::err(id, e));
            }
            let (tx, rx) = oneshot::channel();
            let reply =
                Box::pin(async move { received(rx).await.map(|atr| json!({ "atr": atr })) });
            (Job::Reset { reader, reply: tx }, reply)
        }
    };

    if in_flight.fetch_add(1, Ordering::SeqCst) >= MAX_IN_FLIGHT {
        in_flight.fetch_sub(1, Ordering::SeqCst);
        return Some(Response::err(
            id,
            BridgeError::new(ErrorCode::Busy, "Too many requests in flight"),
        ));
    }
    if let Err(e) = ctx.worker.submit(job) {
        in_flight.fetch_sub(1, Ordering::SeqCst);
        return Some(Response::err(id, e));
    }
    let out = out.clone();
    let in_flight = in_flight.clone();
    tokio::spawn(async move {
        let result = reply.await;
        in_flight.fetch_sub(1, Ordering::SeqCst);
        let resp = match result {
            Ok(v) => Response::ok(id, v),
            Err(e) => Response::err(id, e),
        };
        let _ = out.send(resp).await;
    });
    None
}

fn check_reader(ctx: &ServerCtx, reader: &str) -> Option<BridgeError> {
    let (service, _) = ctx.hub.status_snapshot();
    if service != crate::protocol::ServiceStatus::Ready {
        return Some(BridgeError::new(
            ErrorCode::PcscUnavailable,
            "Smart card service unavailable",
        ));
    }
    if !ctx.hub.has_reader(reader) {
        return Some(BridgeError::new(
            ErrorCode::NoSuchReader,
            format!("No reader named {reader:?}"),
        ));
    }
    None
}

type ReplyFuture = Pin<Box<dyn Future<Output = Result<Value, BridgeError>> + Send>>;

async fn received<T>(rx: oneshot::Receiver<Result<T, BridgeError>>) -> Result<T, BridgeError> {
    rx.await.unwrap_or_else(|_| {
        Err(BridgeError::new(
            ErrorCode::PcscUnavailable,
            "Bridge is shutting down",
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::origin_label;

    #[test]
    fn origin_labels() {
        assert_eq!(
            origin_label(Some("https://topps.drey.dev")),
            "topps.drey.dev"
        );
        assert_eq!(
            origin_label(Some("http://localhost:5173")),
            "localhost:5173"
        );
        assert_eq!(origin_label(None), "local app");
        assert_eq!(origin_label(Some("null")), "local app");
    }
}
