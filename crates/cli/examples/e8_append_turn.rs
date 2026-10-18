//! Append ONE durable user+assistant turn to a seeded session through the
//! same store API the daemon uses. Used by the Edition-8 scale probe to
//! feed a real 1,000-turn/1,000-compaction session: the turn rows are
//! durable fixture data; every compaction is the REAL daemon route.
//!
//! Usage:
//!   cargo run -p faktor-cli --example e8_append_turn -- \
//!     --data-dir DIR --session 1 --turn 42 --text "turn 42 ..."
//!
//! Prints `{"userSeq":N,"assistantSeq":N+1}`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use faktor_core::id::SessionId;
use faktor_store::Store;

fn main() {
    let mut flags: HashMap<String, String> = HashMap::new();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let Some(name) = flag.strip_prefix("--") else {
            panic!("unexpected argument {flag:?}");
        };
        let value = args
            .next()
            .unwrap_or_else(|| panic!("--{name} needs a value"));
        flags.insert(name.to_string(), value);
    }
    let data_dir = PathBuf::from(flags.get("data-dir").expect("--data-dir"));
    let session: u64 = flags
        .get("session")
        .expect("--session")
        .parse()
        .expect("numeric --session");
    let turn: u64 = flags
        .get("turn")
        .expect("--turn")
        .parse()
        .expect("numeric --turn");
    let text = flags.get("text").cloned().unwrap_or_default();
    let store = Store::open(data_dir.join("store"), false).expect("open store");
    let session_id = SessionId::new(session);
    let newest = store
        .messages_window_with_parts(session_id, None, None, 1, 64 * 1024 * 1024)
        .expect("read newest message")
        .first()
        .map(|(row, _)| row.seq.max(0) as u64)
        .unwrap_or(0);
    let user_seq = newest + 1;
    let assistant_seq = newest + 2;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0);
    store
        .put_message(
            session_id,
            user_seq as i64,
            "user",
            serde_json::json!({ "text": text, "files": [] }),
        )
        .expect("put user");
    let assistant = store
        .put_message(
            session_id,
            assistant_seq as i64,
            "assistant",
            serde_json::json!({ "parts": [] }),
        )
        .expect("put assistant");
    store
        .put_part(
            assistant,
            "text",
            serde_json::json!({
                "text": format!("scale assistant answer for {text}"),
            }),
        )
        .expect("put assistant part");
    println!(
        "{{\"userSeq\":{user_seq},\"assistantSeq\":{assistant_seq},\"stamp\":{stamp},\"turn\":{turn}}}"
    );
}
