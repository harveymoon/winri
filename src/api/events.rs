//! Server-Sent Events fan-out for `GET /events`.
//!
//! Clients subscribe with a single HTTP request that stays open
//! (`text/event-stream`). Each "interesting" state change pushes one
//! `data: <json>\n\n` line. The JSON shape is identical to `GET /state`
//! so clients can replace polling without rewriting their parser.
//!
//! Triggers fan in through [`publish`] which is called from
//! `crate::api::publish_state`. We diff-suppress: if the serialized
//! state is byte-identical to the last sent payload we drop the event,
//! so the 60Hz animation-tick path doesn't drown subscribers in
//! identical frames while the strip is at rest.
//!
//! Each subscriber owns an unbounded `mpsc` channel. When `publish`
//! fires it pushes the JSON to every channel; the per-connection
//! writer thread drains its channel and writes to the TCP stream.
//! When a write fails (client disconnected) the writer drops its
//! sender, the parking-lot mutex notices the dropped end on its next
//! cleanup pass, and the subscription is removed.

use std::sync::{Mutex, OnceLock, mpsc};

/// One subscriber to the SSE event stream.
struct Subscriber {
    /// Sender we hand each new event to. `None` once the writer thread
    /// detached (TCP write failed, client closed). We GC on the next
    /// publish.
    sender: Option<mpsc::Sender<String>>,
}

struct Registry {
    subs: Vec<Subscriber>,
    /// Last JSON we broadcast. Diff-suppress key: identical-to-last
    /// payloads are dropped, so animation-tick noise is filtered.
    last_payload: Option<String>,
}

fn registry() -> &'static Mutex<Registry> {
    static REG: OnceLock<Mutex<Registry>> = OnceLock::new();
    REG.get_or_init(|| {
        Mutex::new(Registry {
            subs: Vec::new(),
            last_payload: None,
        })
    })
}

/// Register a new SSE subscriber. Returns the receiver end of the
/// per-connection event channel. The caller is responsible for
/// draining it and writing each item to its socket as
/// `data: <item>\n\n`. Dropping the receiver detaches the subscriber.
pub fn subscribe() -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    let mut reg = registry().lock().expect("events registry poisoned");
    reg.subs.push(Subscriber { sender: Some(tx) });
    rx
}

/// Push a new event to every subscriber. Drops subscribers whose
/// receiver has gone away (TCP closed, writer thread exited). The
/// caller passes the already-serialized JSON string.
pub fn publish(payload_json: String) {
    let mut reg = registry().lock().expect("events registry poisoned");

    if reg.last_payload.as_deref() == Some(payload_json.as_str()) {
        return;
    }
    reg.last_payload = Some(payload_json.clone());

    // Send to every live subscriber; mark dead ones for GC.
    let mut any_dead = false;
    for sub in reg.subs.iter_mut() {
        let Some(tx) = sub.sender.as_ref() else {
            continue;
        };
        if tx.send(payload_json.clone()).is_err() {
            sub.sender = None;
            any_dead = true;
        }
    }
    if any_dead {
        reg.subs.retain(|s| s.sender.is_some());
    }
}

/// Current subscriber count. Exposed for diagnostics / a future
/// `/debug/events` endpoint.
#[allow(dead_code)]
pub fn subscriber_count() -> usize {
    registry()
        .lock()
        .map(|r| r.subs.iter().filter(|s| s.sender.is_some()).count())
        .unwrap_or(0)
}
