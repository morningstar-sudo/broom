// agentauth.rs — the Remote agent and the server prove themselves to each other, and every command the server sends
// an agent is signed. Without it any LAN host (a guest is an administrator on their own machine) could pretend to be
// the server to the other machines' agents (ARP spoofing) and type on their screens, or pretend to be a machine's
// agent and watch what the admin types into it.
// Each machine has its own 32-byte key, made here and handed ONCE to its Windows stage (GET /api/agent-key: the
// machine at that IP by its MAC as ARP sees it, registered, PXE-booted in the last 30 min), which keeps it on BROOMWIN
// (broom\agent.key, kept across resets); broom-watch passes it to the agent. Again only after Re-issue (Devices).
// The key itself never crosses the network again:
//   server → `auth <ns>`; agent → `proof <na> <HMAC(K, "agent:ns:na")>`; server → `ok <HMAC(K, "server:ns:na")>`;
//   session key S = HMAC(K, "session:ns:na"); every text to the agent is `<n> <HMAC(S, "n json")> <json>`, n rising
//   (a command injected or replayed by a man in the middle is dropped by the agent).
// The screen updates the agent sends are not signed: faking them misleads the viewer, it controls nothing.
use axum::{
    Json, Router,
    extract::{ConnectInfo, State, ws::Message},
    http::StatusCode,
    routing::{get, post},
};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::time::Duration;

use crate::SharedState;
use crate::api::{ApiError, ise, ok};

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/agent-key", get(agent_key))
        .route("/api/vnc/rekey", post(rekey))
}

/// HMAC-SHA256 (the agent side is .NET's HMACSHA256).
pub(crate) fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    use hmac::{KeyInit, Mac};
    let mut m = hmac::Hmac::<sha2::Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    m.update(msg);
    m.finalize().into_bytes().into()
}

use crate::auth::hex;

/// 64 hex digits → 32 bytes.
fn unhex32(h: &str) -> Option<[u8; 32]> {
    blake3::Hash::from_hex(h).ok().map(|h| *h.as_bytes())
}

/// A machine's agent key (config `agentkey:<mac>`).
#[derive(Serialize, Deserialize, Default, Clone)]
struct KeyRec {
    key: String,
    /// Unix time it was made.
    made: u64,
    /// Unix time its stage got it; None = waiting for the next boot.
    #[serde(default)]
    delivered: Option<u64>,
}

fn rec_key(mac: &str) -> String {
    format!("agentkey:{}", mac.to_lowercase())
}

fn load(st: &SharedState, mac: &str) -> Option<KeyRec> {
    serde_json::from_str(&st.db.get_config(&rec_key(mac), "")).ok()
}

fn store(st: &SharedState, mac: &str, r: &KeyRec) -> Result<(), String> {
    st.db
        .set_config(&rec_key(mac), &serde_json::to_string(r).unwrap())
}

fn fresh() -> KeyRec {
    KeyRec {
        key: hex(&crate::auth::random32()),
        made: crate::now_secs(),
        delivered: None,
    }
}

/// The key machine `mac`'s agent must prove (None = it has none yet: its next boot brings one).
pub(crate) fn key_of(st: &SharedState, mac: &str) -> Option<[u8; 32]> {
    load(st, mac).and_then(|r| unhex32(&r.key))
}

/// For the machine's detail (Devices): {made, delivered}.
pub(crate) fn info(st: &SharedState, mac: &str) -> serde_json::Value {
    load(st, mac).map_or(
        serde_json::Value::Null,
        |r| serde_json::json!({"made": r.made, "delivered": r.delivered}),
    )
}

/// A key is handed out only this long after the machine's PXE boot (its stage asks right then).
const WINDOW_S: u64 = 30 * 60;

/// GET /api/agent-key (public: the Windows stage, every boot) → the key of the machine at the peer, hex, when it has
/// one waiting (made on its first request, again after Re-issue); 204 when its stage already has it.
async fn agent_key(
    State(st): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Result<(StatusCode, String), ApiError> {
    let ip = peer.ip().to_canonical().to_string();
    let refuse = |why: String| {
        tracing::warn!("agent key request from {ip} refused ({why})");
        Err((StatusCode::FORBIDDEN, "no key for this machine".to_string()))
    };
    let Some(mac) = crate::license::arp_mac(&ip) else {
        return refuse("no ARP entry: not on this LAN".into());
    };
    let Some(m) = crate::machines::by_mac(&st, &mac)? else {
        return refuse(format!("{mac} is not a registered machine"));
    };
    let who = crate::machines::who(&m);
    let pxe = st.pxe_seen.lock().get(&mac).copied();
    if !pxe.is_some_and(|t| crate::now_secs().saturating_sub(t) <= WINDOW_S) {
        return refuse(format!("{who}: no PXE boot in the last 30 min"));
    }
    // Handed over once, as one step: two requests at the same moment can't both get it.
    let key = crate::db::edit_json(&*st.db, &rec_key(&mac), |r: &mut Option<KeyRec>| {
        if r.as_ref().is_some_and(|x| x.delivered.is_some()) {
            return Ok::<_, ApiError>(None); // its stage has it
        }
        let rec = r.get_or_insert_with(fresh);
        rec.delivered = Some(crate::now_secs());
        Ok(Some(rec.key.clone()))
    })
    .map_err(ise)??;
    let Some(key) = key else {
        return Ok((StatusCode::NO_CONTENT, String::new()));
    };
    tracing::info!("remote: agent key handed to {who} ({mac}, {ip})");
    Ok((StatusCode::OK, key))
}

#[derive(Deserialize)]
struct MacBody {
    mac: String,
}

/// POST /api/vnc/rekey {mac} (owner) — a new key for that machine, handed to its stage at its next boot (it lost the
/// old one: new disk / BROOMWIN wiped; or the old one may have leaked). Until then its agent is refused.
async fn rekey(
    State(st): State<SharedState>,
    Json(b): Json<MacBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    store(&st, &b.mac, &fresh()).map_err(ise)?;
    tracing::warn!(
        "remote: agent key of {} re-issued — the machine gets it at its next boot",
        b.mac
    );
    Ok(ok())
}

/// Server side of the handshake on a new agent connection. Some(session key) = the agent proved its key (and got the
/// server's proof back); None = refused (logged).
pub(crate) async fn handshake<T, R>(
    tx: &mut T,
    rx: &mut R,
    key: &[u8; 32],
    host: &str,
) -> Option<[u8; 32]>
where
    T: Sink<Message> + Unpin,
    R: Stream<Item = Result<Message, axum::Error>> + Unpin,
{
    let ns = crate::auth::random_token();
    tx.send(Message::Text(format!("auth {ns}").into()))
        .await
        .ok()?;
    let reply = loop {
        match tokio::time::timeout(Duration::from_secs(10), rx.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => break t.to_string(),
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
            _ => {
                tracing::warn!(
                    "remote: agent of {host} did not authenticate (an older agent? machines get the new one at their next boot)"
                );
                return None;
            }
        }
    };
    let Some(na) = check_proof(key, &ns, &reply) else {
        tracing::warn!(
            "remote: agent of {host} has the wrong key — refused (re-issue its key on Devices if the machine was reset)"
        );
        return None;
    };
    let ok = hex(&hmac_sha256(key, format!("server:{ns}:{na}").as_bytes()));
    tx.send(Message::Text(format!("ok {ok}").into()))
        .await
        .ok()?;
    Some(hmac_sha256(key, format!("session:{ns}:{na}").as_bytes()))
}

/// The agent's reply `proof <na> <HMAC(K, "agent:ns:na")>` to challenge `ns`: its nonce `na` when the proof is right.
pub(crate) fn check_proof(key: &[u8; 32], ns: &str, reply: &str) -> Option<String> {
    let mut f = reply.split(' ');
    let (word, na, proof) = (f.next()?, f.next()?, f.next()?);
    if word != "proof" || na.len() != 32 || !na.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let want = hex(&hmac_sha256(key, format!("agent:{ns}:{na}").as_bytes()));
    crate::auth::same(&want, proof).then(|| na.to_string())
}

/// A text for the agent, signed with the session key: `<n> <HMAC(S, "n json")> <json>`.
pub(crate) fn signed(s: &[u8; 32], n: u64, json: &str) -> String {
    let tag = hex(&hmac_sha256(s, format!("{n} {json}").as_bytes()));
    format!("{n} {tag} {json}")
}

#[cfg(test)]
#[path = "../tests/unit/agentauth.rs"]
mod tests;
