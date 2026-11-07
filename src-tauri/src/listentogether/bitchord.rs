//! The BitChord party server as a second Listen Together backend.
//!
//! BitChord's desktop player talks a different Listen Together protocol from limusic's relay: a
//! REST membership step (`POST /api/parties[/{code}/join]`) whose token rides the WebSocket
//! handshake in a subprotocol (`bitchord.v1` + `bitchord.token.<token>`), JSON frames for state
//! (`welcome`/`state`/`queue`/`members`/`pong`), and `control` frames for playback intents, with a
//! server-authoritative position anchor the client extrapolates from (the wire contract is
//! BitChord's `app/src/party/models.ts`, "rename a field on both sides, never paper over it").
//!
//! This module implements that protocol as a client and maps it onto the same [`SyncCommand`]
//! bridge the limusic relay drives, so a BitChord room behaves like a limusic room from the app's
//! point of view: the same `lt-state` snapshot, the same guest playback path, the same UI. Room
//! codes are BitChord's six alphanumeric characters; identity is a per-process id (BitChord hashes
//! the signed-in YouTube account — limusic has no such id to hash, so a random one stands in;
//! it is stable for the session and carries nothing sensitive).
//!
//! What maps, and what doesn't:
//! - play/pause/seek/track/queue → `control` frames (any member may send; the server enforces
//!   `hostOnlyControl`), so guests in an unlocked room can drive playback like they can in
//!   BitChord, and `is_guest()` only locks a *host-only* room.
//! - kick → `control('kick')`; suggestions → `queueAdd` (BitChord has no approval step, so the
//!   approve/reject commands are no-ops here); transfer-host → a notice (BitChord's host is the
//!   room's creator; the protocol has no hand-over).

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use listen_protocol::{Playback, Track, User};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::{Inner, Role, SyncCommand};

/// The BitChord party server a user gets when they set none of their own: the public instance the
/// BitChord web app ships configured with (its `VITE_PARTY_SERVER_URL`). The HTTPS origin serves
/// REST; the socket is the ws(s) rewrite of it.
pub const BITCHORD_DEFAULT_SERVER: &str = "https://api.bitchord.kushagrasingh.in";

const PROTOCOL_TAG: &str = "bitchord.v1";
const PING_INTERVAL_MS: u64 = 15_000;
const REPORT_INTERVAL_MS: u64 = 10_000;
const PING_BURST: usize = 4;
const PING_BURST_GAP_MS: u64 = 300;
/// After this many failed socket opens, ask the server whether the membership still exists; a
/// party it has forgotten is left rather than redialled forever.
const CHECK_AFTER_FAILURES: u32 = 3;
const MAX_MEMBERS: i64 = 5;

// ---- Wire shapes (the subset of models.ts this client reads; loose on purpose, exactly like the
// original's `onFrame`, so an older server that drops a field degrades instead of erroring). ----

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key)?.as_str().map(|x| x.to_string())
}
fn n(v: &Value, key: &str) -> Option<i64> {
    v.get(key)?.as_i64()
}
fn b(v: &Value, key: &str) -> Option<bool> {
    v.get(key)?.as_bool()
}

/// `PartyTrack` → the shared `Track` the bridge and the UI already speak. Only a shareable id (an
/// 11-char YouTube id) may cross — same rule as the limusic relay's [`shareable`](super::shareable),
/// because the player resolves the id straight to a stream.
fn track_of(raw: &Value) -> Option<Track> {
    let id = s(raw, "videoId")?;
    if !super::shareable(&Track {
        id: id.clone(),
        title: String::new(),
        artist: String::new(),
        thumbnail: None,
        duration_ms: 0,
        queued_by: None,
    }) {
        return None;
    }
    Some(Track {
        id,
        title: s(raw, "title").unwrap_or_default(),
        artist: s(raw, "artist").unwrap_or_default(),
        thumbnail: s(raw, "thumbnailUrl"),
        duration_ms: n(raw, "durationMs").unwrap_or(0),
        queued_by: None,
    })
}

fn bc_track_of(t: &Track) -> Value {
    serde_json::json!({
        "videoId": t.id,
        "title": t.title,
        "artist": t.artist,
        "thumbnailUrl": t.thumbnail,
        "durationMs": t.duration_ms,
        "fromAutoplay": false,
    })
}

fn tracks_of(raw: &Value) -> Vec<Track> {
    raw.get("items")
        .and_then(|i| i.as_array())
        .map(|a| a.iter().filter_map(track_of).collect())
        .unwrap_or_default()
}

/// A member as the snapshot's `users` list already speaks it.
fn member_user(raw: &Value) -> Option<User> {
    Some(User {
        user_id: s(raw, "memberId")?,
        username: s(raw, "displayName").unwrap_or_default(),
        is_host: b(raw, "isHost").unwrap_or(false),
        is_connected: b(raw, "connected").unwrap_or(false),
    })
}

// ---- Identity -------------------------------------------------------------------------------

/// A stable-enough identity for the room: a fresh random id per process, hashed the way BitChord
/// hashes its account id. Two limusic clients joining the same room are distinct members; the same
/// client rejoining after a restart is a new seat (the old one times out server-side).
fn identity_hash() -> String {
    use rand::Rng;
    let raw: [u8; 16] = rand::thread_rng().gen();
    let mut out = String::with_capacity(32);
    for byte in raw {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// `sanitizeServerUrl`: forces https on scheme-less input, drops trailing slashes.
pub fn sanitize_server_url(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    let with_scheme = if t.starts_with("http://") || t.starts_with("https://") {
        t.to_string()
    } else {
        format!("https://{t}")
    };
    Some(with_scheme.trim_end_matches('/').to_string())
}

/// The server a party is made on: the custom one if set, else BitChord's public instance.
fn idle_server(inner: &Inner) -> String {
    if inner.server_url.is_empty() {
        BITCHORD_DEFAULT_SERVER.to_string()
    } else {
        inner.server_url.clone()
    }
}

// ---- REST ----------------------------------------------------------------------------------

async fn rest_json(
    base: &str,
    path: &str,
    method: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> Result<Value, String> {
    let url = format!("{base}{path}");
    let method = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|_| "bad method".to_string())?;
    let mut req = crate::http::client()
        .request(method, &url)
        .header("Content-Type", "application/json");
    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    if let Some(body) = body {
        req = req.json(&body);
    }
    let resp = req
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|_| format!("Couldn't reach the party server at {base}"))?;
    let status = resp.status();
    let json: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let code = s(&json, "error").unwrap_or_else(|| format!("http_{}", status.as_u16()));
        let msg = s(&json, "message").unwrap_or_else(|| match status.as_u16() {
            422 => "This account can't be used to jam.".to_string(),
            _ => format!("The party server said {}.", status.as_u16()),
        });
        // The membership probe reads the status code off the message (below), so keep it in there.
        return Err(if msg.is_empty() {
            code
        } else {
            format!("{} ({} {})", msg, status.as_u16(), code)
        });
    }
    Ok(json)
}

// ---- The connection -------------------------------------------------------------------------

/// One BitChord room session. `Create` or `Join(code)`; the token comes from the REST step.
pub enum BcMode {
    Create,
    Join(String),
}

/// The full run loop for a BitChord room: REST membership, then the socket, then reconnects.
/// Structure mirrors `LtSession::run` so cancellation, reconnect backoff and state resets behave
/// identically from the UI's point of view.
pub async fn run(session: Arc<super::LtSession>, gen: u64, mode: BcMode) {
    // REST membership first: the token the socket needs only exists after it.
    let base = idle_server(&session.inner.lock().await);
    let device = identity_hash();
    let display = session.inner.lock().await.bc_display_name.clone();
    let body = serde_json::json!({
        "userId": format!("bc{}", &device[..28.min(device.len())]),
        "deviceId": device,
        "displayName": display,
        "avatarUrl": Value::Null,
        "maxMembers": MAX_MEMBERS,
        "autoplayEnabled": true,
    });
    let membership = match &mode {
        BcMode::Create => rest_json(&base, "/api/parties", "POST", None, Some(body)).await,
        BcMode::Join(code) => {
            rest_json(&base, &format!("/api/parties/{code}/join"), "POST", None, Some(body)).await
        }
    };
    if session.gen.load(Ordering::SeqCst) != gen {
        return; // cancelled while the request was in flight
    }
    let membership = match membership {
        Ok(m) => m,
        Err(e) => {
            session.close_locally(&e).await;
            return;
        }
    };
    let Some(token) = s(&membership, "token") else {
        session.close_locally("The party server answered without a membership token.").await;
        return;
    };
    let Some(party) = membership.get("party").cloned() else {
        session.close_locally("The party server answered without a room.").await;
        return;
    };
    let code = s(&membership, "code").unwrap_or_default();
    let me = membership.get("you").and_then(member_user);

    {
        let mut inner = session.inner.lock().await;
        // The room's creator is its host; otherwise we arrived as a guest. BitChord has no join
        // approval step, so there is nothing to wait for beyond the REST answer.
        let am_host = me.as_ref().map(|u| u.is_host).unwrap_or(false);
        inner.role = if am_host { Role::Host } else { Role::Guest };
        inner.requesting = false;
        inner.room_code = Some(code.clone());
        if let Some(u) = &me {
            inner.my_id = Some(u.user_id.clone());
        }
        inner.bc_token = Some(token.clone());
        inner.bc_base = Some(base.clone());
        absorb_party(&mut inner, &party);
    }
    // Seed local playback the same way a limusic guest's ApplyState would.
    session.bc_apply_snapshot(&party).await;
    session.emit_state().await;

    // The socket loop proper.
    let mut attempt: u32 = 0;
    let mut failures: u32 = 0;
    loop {
        if session.gen.load(Ordering::SeqCst) != gen {
            return;
        }
        {
            let mut inner = session.inner.lock().await;
            inner.connecting = true;
            inner.status_connected = false;
        }
        session.emit_state().await;

        let url = ws_url(&base, &code);
        let protocols = vec![PROTOCOL_TAG.to_string(), format!("bitchord.token.{token}")];
        match super::connect_ws_protocols(&url, &protocols).await {
            Ok(ws) => {
                if session.gen.load(Ordering::SeqCst) != gen {
                    return;
                }
                attempt = 0;
                failures = 0;
                session.bc_socket_loop(ws, gen).await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "bitchord party connect failed");
            }
        }
        if session.gen.load(Ordering::SeqCst) != gen {
            return;
        }
        // Reconnect only while the membership still exists. After a few failed opens the server is
        // asked directly (a browser can't read a handshake failure either, same problem).
        failures += 1;
        if failures >= CHECK_AFTER_FAILURES {
            failures = 0;
            let check =
                rest_json(&base, &format!("/api/parties/{code}"), "GET", Some(&token), None).await;
            if let Err(msg) = check {
                if msg.contains("404") || msg.contains("401") {
                    session.close_locally("That party has ended.").await;
                    return;
                }
                // Unreachable: keep trying.
            }
        }
        attempt += 1;
        if attempt > 15 {
            session.close_locally("Lost connection to the party.").await;
            return;
        }
        {
            let mut inner = session.inner.lock().await;
            inner.status_connected = false;
            inner.connecting = true;
        }
        session.emit_state().await;
        tokio::time::sleep(super::backoff_delay(attempt)).await;
    }
}

/// `https://host` → `wss://host/ws/parties/<code>` (the BitChord client's `replace(/^http/, 'ws')`).
fn ws_url(base: &str, code: &str) -> String {
    let ws = base.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
    format!("{ws}/ws/parties/{code}")
}

/// Pull the room's members/current track/queue out of a `PartySnapshot` into the shared `Inner`,
/// mapping the BitChord queue (full list + index) onto the mirror the UI reads (upcoming only).
fn absorb_party(inner: &mut Inner, party: &Value) {
    inner.users = party
        .get("members")
        .and_then(|m| m.as_array())
        .map(|a| a.iter().filter_map(member_user).collect::<Vec<_>>())
        .unwrap_or_default();
    if let Some(hoc) = party.get("hostOnlyControl").and_then(|v| v.as_bool()) {
        inner.bc_host_only = hoc;
    }
    if let Some(q) = party.get("queue") {
        let items = tracks_of(q);
        let index = n(q, "index").unwrap_or(-1);
        if index < 0 || items.is_empty() {
            inner.bc_queue_index = 0;
            inner.queue = Vec::new();
            inner.current_track = None;
        } else {
            inner.bc_queue_index = (index as usize).min(items.len() - 1);
            inner.queue = upcoming(&items, inner.bc_queue_index);
            inner.current_track = Some(items[inner.bc_queue_index].clone());
        }
    }
}

/// The upcoming slice of a full BitChord queue: everything after the playing index.
fn upcoming(items: &[Track], index: usize) -> Vec<Track> {
    items.get(index + 1..).map(|s| s.to_vec()).unwrap_or_default()
}

impl super::LtSession {
    /// The BitChord socket: heartbeats, frames, silence detection. Same shape as the relay's read
    /// loop, with the protocol's own keepalive (a ping burst on arrival, then ping/report timers).
    /// Takes the Arc because the presence-report task below outlives this call.
    async fn bc_socket_loop(self: &Arc<super::LtSession>, ws: WebSocketStream<MaybeTlsStream<TcpStream>>, gen: u64) {
        let (mut sink, mut read) = ws.split();
        let (otx, mut orx) = mpsc::unbounded_channel::<String>();
        {
            let mut inner = self.inner.lock().await;
            inner.bc_outbound = Some(otx.clone());
            inner.connecting = false;
            inner.status_connected = true;
        }
        self.emit_state().await;

        // Writer: raw JSON frames, already shaped.
        let writer = tokio::spawn(async move {
            while let Some(m) = orx.recv().await {
                if sink.send(Message::Text(m)).await.is_err() {
                    break;
                }
            }
        });

        // Heartbeat: a burst on arrival makes the first seconds accurate, then a trickle keeps up
        // with drift, exactly as the original's `onopen` schedules them.
        let hb = {
            let tx = otx.clone();
            tokio::spawn(async move {
                for i in 0..PING_BURST {
                    if i > 0 {
                        tokio::time::sleep(Duration::from_millis(PING_BURST_GAP_MS)).await;
                    }
                    let _ = tx.send(bc_ping());
                }
                loop {
                    tokio::time::sleep(Duration::from_millis(PING_INTERVAL_MS)).await;
                    let _ = tx.send(bc_ping());
                }
            })
        };
        // Presence report: the room's own position carried forward locally, as the original's
        // `report` does. Nothing from the local player is read, so a guest behind never lies.
        let report = {
            let tx = otx.clone();
            let me = self.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(REPORT_INTERVAL_MS)).await;
                    if let Some(frame) = me.bc_report_frame().await {
                        let _ = tx.send(frame);
                    }
                }
            })
        };
        let mut last_heard = std::time::Instant::now();
        loop {
            let next = tokio::select! {
                biased;
                next = read.next() => next,
                _ = tokio::time::sleep(Duration::from_millis(500)) => {
                    if self.gen.load(Ordering::SeqCst) != gen {
                        break;
                    }
                    if last_heard.elapsed() > super::SILENCE_LIMIT {
                        tracing::warn!("bitchord party: server went silent, reconnecting");
                        break;
                    }
                    continue;
                }
            };
            if self.gen.load(Ordering::SeqCst) != gen {
                break;
            }
            let Some(next) = next else { break };
            last_heard = std::time::Instant::now();
            match next {
                Ok(Message::Text(t)) => {
                    if self.handle_bc_frame(&t).await {
                        break; // fatal (kicked / bad token / no such party)
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
        hb.abort();
        report.abort();
        {
            let mut inner = self.inner.lock().await;
            inner.bc_outbound = None;
        }
        drop(otx);
        let mut writer = writer;
        if tokio::time::timeout(Duration::from_secs(2), &mut writer).await.is_err() {
            writer.abort();
        }
    }

    /// One server frame, handled the loose way the original's `onFrame` handles them. Returns
    /// `true` when the connection should close (kicked, bad token, no such party).
    async fn handle_bc_frame(&self, text: &str) -> bool {
        let Ok(frame) = serde_json::from_str::<Value>(text) else {
            return false;
        };
        let kind = frame.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            "welcome" => {
                let Some(party) = frame.get("party") else { return false };
                let mut inner = self.inner.lock().await;
                inner.connecting = false;
                inner.status_connected = true;
                if let Some(id) = frame.get("you").and_then(|you| s(you, "memberId")) {
                    inner.my_id = Some(id);
                }
                absorb_party(&mut inner, party);
                drop(inner);
                self.bc_apply_snapshot(party).await;
                self.emit_state().await;
                false
            }
            "pong" => {
                if let (Some(c), Some(m)) = (n(&frame, "clientMs"), n(&frame, "serverMs")) {
                    let recv = now_ms();
                    let rtt = (recv - c).max(0);
                    // NTP over the frame pair; the smallest-RTT sample wins (the original's clock:
                    // best of the last window).
                    let offset = m - (c + recv) / 2;
                    let mut inner = self.inner.lock().await;
                    let better = inner.bc_offset_ms.is_none() || rtt < inner.bc_rtt_ms;
                    if better {
                        inner.bc_offset_ms = Some(offset);
                        inner.bc_rtt_ms = rtt;
                    }
                }
                false
            }
            "state" => {
                let Some(p) = frame.get("playback") else { return false };
                let seq = n(p, "seq").unwrap_or(0);
                let mut inner = self.inner.lock().await;
                // Older than what's applied: two controllers pressing at once settle instead of
                // fighting.
                if seq < inner.bc_mirror.seq {
                    return false;
                }
                let track = p.get("track").filter(|t| !t.is_null()).and_then(track_of);
                let queue_seq = n(p, "queueSeq").unwrap_or(inner.bc_queue_seq);
                let is_playing = b(p, "isPlaying").unwrap_or(false);
                let position_ms = n(p, "positionMs").unwrap_or(0);
                let anchor_ms = n(p, "anchorMs").unwrap_or(0);
                let effective = n(p, "effectivePositionMs").unwrap_or(position_ms);
                let prev_track_id = inner.bc_mirror.track_id.clone();
                let prev_playing = inner.bc_mirror.is_playing;
                inner.bc_mirror = super::BcMirror {
                    seq,
                    track_id: track.as_ref().map(|t| t.id.clone()),
                    is_playing,
                    position_ms,
                    anchor_ms,
                    effective_position_ms: effective,
                };
                // A missed queue frame is healed on the next heartbeat rather than lived with.
                let heal_queue = queue_seq > inner.bc_queue_seq;
                if heal_queue {
                    inner.bc_queue_seq = queue_seq;
                }
                let track_changed =
                    prev_track_id.as_deref() != inner.bc_mirror.track_id.as_deref();
                if track_changed {
                    inner.current_track = track.clone();
                    if track.is_none() {
                        inner.queue.clear();
                    }
                }
                let pos = live_position_of(&inner);
                let role = inner.role;
                let heal_frame = if heal_queue {
                    Some(serde_json::json!({ "type": "syncQueue" }).to_string())
                } else {
                    None
                };
                drop(inner);
                if let Some(f) = heal_frame {
                    self.send_raw(f).await;
                }
                if role == Role::Guest {
                    if track_changed {
                        let queue = self.inner.lock().await.queue.clone();
                        let _ = self.sync_tx.send(SyncCommand::ChangeTrack {
                            track,
                            position_ms: pos,
                            playing: is_playing,
                            queue,
                        });
                    } else if is_playing != prev_playing {
                        let _ = self.sync_tx.send(if is_playing {
                            SyncCommand::Play { position_ms: pos }
                        } else {
                            SyncCommand::Pause { position_ms: pos }
                        });
                    } else if is_playing {
                        // Drift correction, the original reconcile's rule: only real drift moves
                        // the player (twice past 1.2 s would, in the original; one solid 1.2 s
                        // jump lands here), never a heartbeat's rounding.
                        let last = self.inner.lock().await.bc_last_applied_ms;
                        if last >= 0 && (pos - last).abs() > 1200 {
                            let _ = self.sync_tx.send(SyncCommand::Seek { position_ms: pos });
                        }
                    }
                    let mut inner = self.inner.lock().await;
                    inner.bc_last_applied_ms = pos;
                }
                self.emit_state().await;
                false
            }
            "queue" => {
                let Some(q) = frame.get("queue") else { return false };
                let seq = n(q, "seq").unwrap_or(0);
                let mut inner = self.inner.lock().await;
                if seq < inner.bc_queue_seq {
                    return false;
                }
                inner.bc_queue_seq = seq;
                let items = tracks_of(q);
                let index = n(q, "index").unwrap_or(-1);
                if index < 0 || items.is_empty() {
                    inner.bc_queue_index = 0;
                    inner.queue = Vec::new();
                    inner.current_track = None;
                } else {
                    inner.bc_queue_index = (index as usize).min(items.len() - 1);
                    inner.queue = upcoming(&items, inner.bc_queue_index);
                    inner.current_track = Some(items[inner.bc_queue_index].clone());
                }
                let role = inner.role;
                let upcoming = inner.queue.clone();
                drop(inner);
                // Mirror the host's upcoming queue, the way a limusic SyncQueue does. The playing
                // row is the state frames' business, not this one's.
                if role == Role::Guest {
                    let _ = self.sync_tx.send(SyncCommand::SyncQueue { queue: upcoming });
                }
                self.emit_state().await;
                false
            }
            "members" => {
                {
                    let mut inner = self.inner.lock().await;
                    if let Some(list) = frame.get("members").and_then(|m| m.as_array()) {
                        inner.users = list.iter().filter_map(member_user).collect();
                    }
                    if let Some(hoc) = frame.get("hostOnlyControl").and_then(|v| v.as_bool()) {
                        inner.bc_host_only = hoc;
                    }
                }
                self.emit_state().await;
                false
            }
            "activity" => false, // room chatter the UI doesn't show yet
            "error" => {
                let reason = s(&frame, "error").unwrap_or_default();
                let message = s(&frame, "message").unwrap_or_else(|| reason.clone());
                self.emit_notice(&message).await;
                // Terminal: the party no longer knows this device.
                if reason == "bad_token" || reason == "no_such_party" {
                    self.close_locally("").await;
                    return true;
                }
                false
            }
            "bye" => {
                let kicked = frame.get("reason").and_then(|v| v.as_str()) == Some("kicked");
                self.close_locally(if kicked {
                    "The host removed you from the party."
                } else {
                    ""
                })
                .await;
                true
            }
            _ => false,
        }
    }

    /// A fresh snapshot (welcome / join answer) drives the same bridge path a limusic
    /// `ApplyState` does: land on the room's current track at its live position.
    async fn bc_apply_snapshot(&self, party: &Value) {
        let Some(p) = party.get("playback") else { return };
        let Some(track) = p.get("track").filter(|t| !t.is_null()).and_then(track_of) else {
            return; // nothing playing in the room yet
        };
        {
            let mut inner = self.inner.lock().await;
            inner.bc_mirror = super::BcMirror {
                seq: n(p, "seq").unwrap_or(0),
                track_id: Some(track.id.clone()),
                is_playing: b(p, "isPlaying").unwrap_or(false),
                position_ms: n(p, "positionMs").unwrap_or(0),
                anchor_ms: n(p, "anchorMs").unwrap_or(0),
                effective_position_ms: n(p, "effectivePositionMs").unwrap_or(0),
            };
            inner.current_track = Some(track.clone());
        }
        let pos = live_position_of(&self.inner.lock().await);
        let playing = self.inner.lock().await.bc_mirror.is_playing;
        let queue = self.inner.lock().await.queue.clone();
        let _ = self.sync_tx.send(SyncCommand::ChangeTrack {
            track: Some(track),
            position_ms: pos,
            playing,
            queue,
        });
    }

    /// The presence report frame: the room's own position carried forward — no local player reads,
    /// exactly like the original's `report`.
    async fn bc_report_frame(&self) -> Option<String> {
        let inner = self.inner.lock().await;
        if inner.role == Role::None || inner.bc_mirror.track_id.is_none() {
            return None;
        }
        let pos = live_position_of(&inner);
        Some(
            serde_json::json!({
                "type": "report",
                "positionMs": pos,
                "isPlaying": inner.bc_mirror.is_playing,
            })
            .to_string(),
        )
    }

    /// Send a playback intent as BitChord `control` frames. Every member sends (the server decides
    /// who may); the mapping mirrors BitChord's `party` control builders. The queue rides along on
    /// a track change because BitChord's `setTrack` doesn't carry one.
    pub async fn bc_broadcast_playback(&self, p: Playback) {
        use listen_protocol::PlaybackKind as K;
        let mut frames: Vec<Value> = Vec::new();
        match p.kind {
            K::Play => frames.push(serde_json::json!({
                "type": "control", "action": "play", "positionMs": p.position_ms
            })),
            K::Pause => frames.push(serde_json::json!({
                "type": "control", "action": "pause", "positionMs": p.position_ms
            })),
            K::Seek => frames.push(serde_json::json!({
                "type": "control", "action": "seek", "positionMs": p.position_ms
            })),
            K::ChangeTrack => {
                if let Some(t) = &p.track {
                    frames.push(serde_json::json!({
                        "type": "control", "action": "setTrack",
                        "track": bc_track_of(t),
                        "positionMs": p.position_ms,
                        "isPlaying": p.playing,
                    }));
                    // The shared queue follows the track: current first, then the upcoming slice.
                    let items = {
                        let inner = self.inner.lock().await;
                        let mut items = vec![bc_track_of(t)];
                        items.extend(inner.queue.iter().map(bc_track_of));
                        items
                    };
                    frames.push(serde_json::json!({
                        "type": "control", "action": "setQueue",
                        "queue": items, "queueIndex": 0,
                    }));
                }
            }
            K::SyncQueue => {
                let (items, index) = {
                    let inner = self.inner.lock().await;
                    let mut items: Vec<Value> = Vec::new();
                    if let Some(t) = &inner.current_track {
                        items.push(bc_track_of(t));
                    }
                    items.extend(inner.queue.iter().map(bc_track_of));
                    let index = if inner.current_track.is_some() { 0 } else { -1 };
                    (items, index)
                };
                frames.push(serde_json::json!({
                    "type": "control", "action": "setQueue",
                    "queue": items, "queueIndex": index,
                }));
            }
            K::SetVolume => {} // BitChord rooms don't carry volume
        }
        for f in frames {
            self.send_raw(f.to_string()).await;
        }
    }

    /// A guest's track suggestion: BitChord rooms have no approval step, so it goes straight in as
    /// a queue add (append; the room's queue is the shared timeline).
    pub async fn bc_suggest(&self, track: &Track) {
        if !super::shareable(track) {
            return;
        }
        let frame = serde_json::json!({
            "type": "control", "action": "queueAdd",
            "tracks": [bc_track_of(track)], "playNext": false,
        });
        self.send_raw(frame.to_string()).await;
    }

    /// The host's kick, over the BitChord protocol.
    pub async fn bc_kick(&self, member_id: &str) {
        let frame =
            serde_json::json!({ "type": "control", "action": "kick", "memberId": member_id });
        self.send_raw(frame.to_string()).await;
    }

    /// Tell the server we're leaving (the socket close alone leaves the seat held until its grace
    /// lapses — the original releases the slot explicitly).
    pub async fn bc_release_slot(&self) {
        let (base, token, code) = {
            let inner = self.inner.lock().await;
            match (inner.bc_base.clone(), inner.bc_token.clone(), inner.room_code.clone()) {
                (Some(b), Some(t), Some(c)) => (b, t, c),
                _ => return,
            }
        };
        let _ = rest_json(&base, &format!("/api/parties/{code}/leave"), "POST", Some(&token), None)
            .await;
    }
}

// ---- Frame builders + math -----------------------------------------------------------------

fn bc_ping() -> String {
    serde_json::json!({ "type": "ping", "clientMs": now_ms() }).to_string()
}

/// The room's live position in ms: the anchor carried forward by the (clock-corrected) time since
/// it was set, clamped to the track — BitChord's `partyPositionMs` verbatim. Before the first pong
/// it is the server's own reading at send time: a starting point, not a thing to seek to.
fn live_position_of(inner: &Inner) -> i64 {
    let track_ms = inner.current_track.as_ref().map(|t| t.duration_ms).unwrap_or(0);
    let position_ms = inner.bc_mirror.position_ms;
    if !inner.bc_mirror.is_playing {
        return position_ms;
    }
    let server_now = match inner.bc_offset_ms {
        Some(off) => now_ms() + off,
        None => return inner.bc_mirror.effective_position_ms,
    };
    let mut pos = position_ms + (server_now - inner.bc_mirror.anchor_ms).max(0);
    if track_ms > 0 {
        pos = pos.min(track_ms);
    }
    pos
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
