// vnc.rs — remote view + control of the Windows machines from the web (Remote page). The agent on each machine
// (scripts/broom-vnc.cs, started by broom-watch.ps1 as SYSTEM in the console session) keeps one WebSocket open to
// /api/vnc/agent: the machine dials out, nothing listens on it, so no guest or other LAN host can reach its screen.
// A signed-in admin opens /api/vnc/view?host=<hostname>: the server tells that agent to start, then passes its screen
// updates to the viewer and the viewer's mouse / keys to the agent. One viewer per machine (a new one replaces the
// old). The agent is known by its IP (machines::machine_at), like the other client endpoints.
//
// Agent → viewer (binary, passed as is): 1 x y w h <jpeg> a changed area, 2 w h the screen size (u16 LE).
// Viewer → agent (text JSON, passed as is): {"op":"ack"} once an update is drawn (the agent sends the next one only
// then: the stream paces itself to the network), mouse / wheel / key events, {"op":"start","q":..} to change the
// quality. The server adds {"op":"start","q":..} when a viewer comes and {"op":"stop"} when it goes.
use axum::{
    Json, Router,
    extract::{
        ConnectInfo, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::Deserialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;

use crate::SharedState;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/vnc/agent", get(agent))
        .route("/api/vnc/view", get(view))
        .route("/api/vnc/agents", get(agents))
}

/// Messages queued towards one side. Full = that side is not reading: updates are dropped (the agent waits for an
/// ack anyway), so a stuck viewer or a flooding agent never grows the server's memory.
const QUEUE: usize = 16;
/// The server pings each agent this often; nothing heard for 3× as long → the connection is dead (machine off).
const PING: Duration = Duration::from_secs(30);
/// A viewer message larger than this is dropped (input events are tiny; the clipboard text the admin pastes can be
/// larger, so the limit is generous but bounded).
const MAX_INPUT: usize = 256 << 10;

struct Agent {
    id: u64,
    host: String,
    to_agent: mpsc::Sender<Message>,
    /// The viewer now watching: (id, its queue).
    viewer: Option<(u64, mpsc::Sender<Message>)>,
}

/// Connected agents by MAC (lower case).
static AGENTS: LazyLock<Mutex<HashMap<String, Agent>>> = LazyLock::new(Default::default);
static NEXT: AtomicU64 = AtomicU64::new(1);

/// An agent connects: it replaces an older connection of the same machine (whose viewer is closed).
fn register(mac: &str, host: &str, to_agent: mpsc::Sender<Message>) -> u64 {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    AGENTS.lock().insert(
        mac.to_string(),
        Agent {
            id,
            host: host.to_string(),
            to_agent,
            viewer: None,
        },
    );
    id
}

/// Agent connection `id` ended: gone from the list unless a newer one took its place. Its viewer's queue is dropped
/// with it → the viewer is closed.
fn unregister(mac: &str, id: u64) {
    let mut a = AGENTS.lock();
    if a.get(mac).is_some_and(|x| x.id == id) {
        a.remove(mac);
    }
}

/// An update from agent connection `id` → its viewer, if one watches. false = no viewer.
fn to_viewer(mac: &str, id: u64, m: Message) -> bool {
    let a = AGENTS.lock();
    match a
        .get(mac)
        .filter(|x| x.id == id)
        .and_then(|x| x.viewer.as_ref())
    {
        Some((_, v)) => {
            let _ = v.try_send(m);
            true
        }
        None => false,
    }
}

/// The machine named `host` (its hostname, any case) has an agent connected → (MAC, hostname, its queue).
fn find(host: &str) -> Option<(String, String, mpsc::Sender<Message>)> {
    AGENTS
        .lock()
        .iter()
        .find(|(_, a)| a.host.eq_ignore_ascii_case(host))
        .map(|(mac, a)| (mac.clone(), a.host.clone(), a.to_agent.clone()))
}

/// Viewer `vid` starts watching `mac` (an older viewer's queue is dropped → it is closed). false = agent gone.
fn attach(mac: &str, vid: u64, q: mpsc::Sender<Message>) -> bool {
    match AGENTS.lock().get_mut(mac) {
        Some(a) => {
            a.viewer = Some((vid, q));
            true
        }
        None => false,
    }
}

/// Viewer `vid` stops: true = it was still the one watching (the agent can stop), false = replaced meanwhile.
fn detach(mac: &str, vid: u64) -> bool {
    let mut g = AGENTS.lock();
    let Some(a) = g.get_mut(mac) else {
        return false;
    };
    if a.viewer.as_ref().is_some_and(|v| v.0 == vid) {
        a.viewer = None;
        return true;
    }
    false
}

/// Has machine `mac` an agent connected (Machines page: Remote button).
pub fn connected(mac: &str) -> bool {
    AGENTS.lock().contains_key(&mac.to_lowercase())
}

/// GET /api/vnc/agents — hostnames of the machines with an agent connected, sorted.
async fn agents() -> Json<Vec<String>> {
    let mut v: Vec<String> = AGENTS.lock().values().map(|a| a.host.clone()).collect();
    v.sort_by_key(|h| h.to_lowercase());
    Json(v)
}

/// GET /api/vnc/agent (public, WebSocket) — a machine's agent. Only a registered machine (by its IP).
async fn agent(
    ws: WebSocketUpgrade,
    State(st): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    let Some(m) = crate::machines::machine_at(&st, &peer.ip().to_string()) else {
        return (StatusCode::FORBIDDEN, "not a registered machine").into_response();
    };
    let (mac, host) = (m.mac.to_lowercase(), crate::machines::who(&m));
    ws.max_message_size(16 << 20)
        .on_upgrade(move |s| run_agent(s, mac, host))
}

async fn run_agent(ws: WebSocket, mac: String, host: String) {
    let (mut tx, mut rx) = ws.split();
    let (to_agent, mut queue) = mpsc::channel::<Message>(QUEUE);
    let id = register(&mac, &host, to_agent);
    tracing::info!("remote: agent of {host} connected");
    let writer = async {
        let mut ping = tokio::time::interval(PING);
        loop {
            let m = tokio::select! {
                m = queue.recv() => match m { Some(m) => m, None => break },
                _ = ping.tick() => Message::Ping(Default::default()),
            };
            if tx.send(m).await.is_err() {
                break;
            }
        }
    };
    let reader = async {
        while let Ok(Some(Ok(m))) = tokio::time::timeout(PING * 3, rx.next()).await {
            match m {
                Message::Binary(_) => {
                    to_viewer(&mac, id, m);
                }
                Message::Close(_) => break,
                _ => {} // pong / text: nothing to do
            }
        }
    };
    tokio::select! {
        _ = writer => {}
        _ = reader => {}
    }
    unregister(&mac, id);
    tracing::info!("remote: agent of {host} disconnected");
}

#[derive(Deserialize)]
struct ViewQuery {
    host: String,
    /// JPEG quality 20–95 (default 60).
    #[serde(default)]
    q: Option<u8>,
}

/// The browser's Origin names this server (a WebSocket is not covered by the same-origin policy: a page elsewhere
/// could open one with the admin's cookie if SameSite let it through). No Origin = not a browser.
fn same_origin(h: &HeaderMap) -> bool {
    let Some(origin) = h.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let host = h.get(header::HOST).and_then(|v| v.to_str().ok());
    let o = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"));
    o.is_some() && o == host
}

/// GET /api/vnc/view?host=<hostname>&q= (admin, WebSocket) — watch and control that machine.
async fn view(
    ws: WebSocketUpgrade,
    Query(q): Query<ViewQuery>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    if !same_origin(&headers) {
        return (StatusCode::FORBIDDEN, "cross-site request").into_response();
    }
    let Some((mac, host, to_agent)) = find(q.host.trim()) else {
        return (
            StatusCode::NOT_FOUND,
            format!("{}: no agent connected", q.host.trim()),
        )
            .into_response();
    };
    let quality = q.q.unwrap_or(60).clamp(20, 95);
    ws.on_upgrade(move |s| run_view(s, mac, host, to_agent, quality, peer))
}

fn start_msg(q: u8) -> Message {
    Message::Text(format!("{{\"op\":\"start\",\"q\":{q}}}").into())
}

async fn run_view(
    ws: WebSocket,
    mac: String,
    host: String,
    to_agent: mpsc::Sender<Message>,
    quality: u8,
    peer: SocketAddr,
) {
    let (vq, mut queue) = mpsc::channel::<Message>(QUEUE);
    let vid = NEXT.fetch_add(1, Ordering::Relaxed);
    if !attach(&mac, vid, vq) || to_agent.send(start_msg(quality)).await.is_err() {
        return;
    }
    tracing::info!("remote: {host} opened from {}", peer.ip());
    let (mut tx, mut rx) = ws.split();
    let down = async {
        while let Some(m) = queue.recv().await {
            if tx.send(m).await.is_err() {
                return;
            }
        }
        let _ = tx.send(Message::Close(None)).await; // agent gone or another viewer took over
    };
    let up = async {
        while let Some(Ok(m)) = rx.next().await {
            match m {
                Message::Text(t) if t.len() <= MAX_INPUT => {
                    if to_agent.send(Message::Text(t)).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    };
    tokio::select! {
        _ = down => {}
        _ = up => {}
    }
    if detach(&mac, vid) {
        let _ = to_agent.try_send(Message::Text("{\"op\":\"stop\"}".into()));
    }
    tracing::info!("remote: {host} closed");
}

#[cfg(test)]
#[path = "../tests/unit/vnc.rs"]
mod tests;
