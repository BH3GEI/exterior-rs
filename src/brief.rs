//! brief: the only way exterior talks to Telegram.
use serde_json::Value;
use std::process::Command;

fn conf() -> (String, String) {
    let c = crate::store::config();
    let url = c["brief"]["url"].as_str().unwrap_or("").trim_end_matches('/').to_string();
    let key = c["brief"]["key"].as_str().and_then(crate::crypt::open).unwrap_or_default();
    (url, key)
}

pub fn configured() -> bool { let (u, k) = conf(); !u.is_empty() && !k.is_empty() }

pub fn key() -> String { conf().1 }

/// brief's Telegram topics are chats named "tgbot:<topic id>".
pub const TOPIC_PREFIX: &str = "tgbot:";
/// brief prefixes the name of a topic marked done with this.
pub const DONE_MARK: &str = "✓ ";

pub fn topic_id(chat: &str) -> &str { chat.trim_start_matches(TOPIC_PREFIX) }

/// brief marks messages the user sent himself with this sender name (its API convention, used by all its adapters).
const SELF_SENDER: &str = "我";

pub fn from_user(m: &Value) -> bool { m["sender"] == SELF_SENDER }

/// One API call. Ok(json) on 2xx, Err((status, json)) otherwise (status 0 = no answer at all).
pub fn call(method: &str, path: &str, body: Option<&Value>) -> Result<Value, (u16, Value)> {
    let (base, key) = conf();
    if base.is_empty() || key.is_empty() { return Err((0, Value::String(crate::text::BRIEF_NOT_SET.into()))); }
    let mut c = Command::new("curl");
    c.args(["-s", "-m", "70", "-w", "\n%{http_code}", "-X", method, "-H", "@-"]).arg(format!("{base}{path}"));
    // the body goes through a file: one command-line argument can hold at most 128 KB (a picture is more)
    let file = body.map(|b| {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let f = crate::store::path(&format!("tmp/body-{}-{}.json", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)));
        let _ = std::fs::create_dir_all(crate::store::path("tmp"));
        (std::fs::write(&f, b.to_string()), f)
    });
    if let Some((w, f)) = &file {
        if let Err(e) = w { return Err((1, Value::String(e.to_string()))); }
        c.args(["-H", "Content-Type: application/json", "--data-binary"]).arg(format!("@{f}"));
    }
    let gone = || if let Some((_, f)) = &file { let _ = std::fs::remove_file(f); };
    // the key goes in through stdin, so it never shows up in the process list
    c.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
    // curl that cannot start is not a network error: retrying will not help
    let mut ch = match c.spawn() { Ok(ch) => ch, Err(e) => { gone(); return Err((1, Value::String(e.to_string()))); } };
    { use std::io::Write; let mut i = ch.stdin.take().unwrap(); let _ = writeln!(i, "Authorization: Bearer {key}"); }
    let out = ch.wait_with_output();
    gone();
    let out = match out { Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(), Err(_) => return Err((0, Value::Null)) };
    let (body, code) = out.rsplit_once('\n').unwrap_or(("", "0"));
    let code: u16 = code.trim().parse().unwrap_or(0);
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    if (200..300).contains(&code) { Ok(v) } else { Err((code, v)) }
}

/// Download a file (e.g. /api/media/<id>) as bytes.
pub fn download(path: &str) -> Result<Vec<u8>, String> {
    let (base, key) = conf();
    let mut c = Command::new("curl");
    c.args(["-s", "-f", "-m", "180", "-H", "@-"]).arg(format!("{base}{path}"));
    c.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
    let mut ch = c.spawn().map_err(|e| e.to_string())?;
    { use std::io::Write; let mut i = ch.stdin.take().unwrap(); let _ = writeln!(i, "Authorization: Bearer {key}"); }
    let o = ch.wait_with_output().map_err(|e| e.to_string())?;
    if o.status.success() && !o.stdout.is_empty() { Ok(o.stdout) } else { Err(crate::text::download_failed(o.status.code())) }
}

/// Standard base64, for /api/send_file.
pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 { out.push(if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' }); }
    }
    out
}

pub fn get(path: &str) -> Option<Value> { call("GET", path, None).ok() }
pub fn post(path: &str, body: Value) -> Result<Value, (u16, Value)> { call("POST", path, Some(&body)) }

/// Until when Telegram told us to hold off (429 with retry_after). One group shares one limit, so this is global.
static HOLD: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// The queue: the earliest moment the next post to Telegram may go out. Every post takes the next free slot,
/// tg_gap_ms after the previous one, so we stay under Telegram's per-group limit instead of running into it.
static NEXT: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// Take a slot in the queue and wait for it. `wait` false: only take a slot that is free right now.
fn slot(wait: bool) -> bool {
    use std::time::{Duration, Instant};
    let gap = Duration::from_millis(crate::params::num("tg_gap_ms"));
    let at = {
        let mut n = NEXT.lock().unwrap();
        let now = Instant::now();
        let at = n.filter(|t| *t > now).unwrap_or(now);
        if !wait && at > now { return false; }
        *n = Some(at + gap);
        at
    };
    let now = Instant::now();
    if at > now { std::thread::sleep(at - now); }
    true
}

/// Telegram is rate-limiting us right now.
#[cfg(test)]
fn held() -> bool { HOLD.lock().unwrap().map(|u| u > std::time::Instant::now()).unwrap_or(false) }

/// Post something that ends up in Telegram (send, edit). On 429 every later post waits out retry_after.
/// `wait`: wait and retry (for anything that must arrive); otherwise give up at once (live updates,
/// the next one carries the same text anyway). A message that must arrive waits as long as Telegram says.
pub fn tg_post(path: &str, body: Value, wait: bool) -> Result<Value, (u16, Value)> {
    use std::time::{Duration, Instant};
    loop {
        let until = *HOLD.lock().unwrap();
        if let Some(u) = until { let now = Instant::now(); if u > now {
            if !wait { return Err((429, serde_json::json!({"error": "rate_limited", "held": true}))); }
            std::thread::sleep(u - now);
        } }
        if !slot(wait) { return Err((429, serde_json::json!({"error": "queued", "held": true}))); }
        match post(path, body.clone()) {
            Err((429, v)) => {
                let ra = v["retry_after"].as_u64().unwrap_or(5).max(1) + 1;
                let u = Instant::now() + Duration::from_secs(ra);
                { let mut h = HOLD.lock().unwrap(); if h.map(|x| x < u).unwrap_or(true) { *h = Some(u); } }
                crate::store::log(&format!("{path}: Telegram rate limit, holding {ra}s{}", if wait { ", will retry" } else { "" }));
                if !wait { return Err((429, v)); }
            }
            r => return r,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// A fake brief: the first `n` posts get 429 retry_after=1, the rest 200.
    fn fake(n: usize) -> u16 {
        let srv = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = srv.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || for (i, mut r) in srv.incoming_requests().enumerate() {
            let mut got = String::new();
            let _ = std::io::Read::read_to_string(r.as_reader(), &mut got);
            let (code, body) = if i < n { (429, r#"{"error":"rate_limited","retry_after":1}"#.to_string()) } else { (200, format!(r#"{{"id":7,"len":{}}}"#, got.len())) };
            let _ = r.respond(tiny_http::Response::from_string(body).with_status_code(code));
        });
        port
    }

    #[test]
    fn rate_limit() {
        let _g = crate::store::TEST.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("exterior-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("EXTERIOR_VAR", &dir);
        let port = fake(2);
        crate::store::set_config(&serde_json::json!({"brief": {"url": format!("http://127.0.0.1:{port}"), "key": crate::crypt::seal("k")}}));
        // a live update gives up at once and puts everyone on hold
        assert_eq!(tg_post("/api/edit", serde_json::json!({}), false).unwrap_err().0, 429);
        assert!(held());
        // a final answer waits out the hold (2 s each: retry_after + 1) and gets through
        let t = Instant::now();
        assert_eq!(tg_post("/api/send", serde_json::json!({}), true).unwrap()["id"], 7);
        assert!(t.elapsed() >= Duration::from_secs(3), "waited {:?}", t.elapsed());
        assert!(!held());
        // a picture is bigger than one command-line argument can hold: it still goes through, whole
        let big = serde_json::json!({"data": "A".repeat(400_000)});
        assert_eq!(call("POST", "/api/send_file", Some(&big)).unwrap()["len"], big.to_string().len());
    }
}
