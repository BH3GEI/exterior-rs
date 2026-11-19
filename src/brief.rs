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
    if let Some(b) = body { c.args(["-H", "Content-Type: application/json", "--data-binary"]).arg(b.to_string()); }
    // the key goes in through stdin, so it never shows up in the process list
    c.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
    let Ok(mut ch) = c.spawn() else { return Err((0, Value::Null)) };
    { use std::io::Write; let mut i = ch.stdin.take().unwrap(); let _ = writeln!(i, "Authorization: Bearer {key}"); }
    let out = match ch.wait_with_output() { Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(), Err(_) => return Err((0, Value::Null)) };
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

/// Telegram allows 4096 characters per message; keep the end, which is where an answer concludes.
const MESSAGE_MAX: usize = 4000;
const KEEP: usize = 3900;

pub fn clip(s: &str) -> String {
    let n = s.chars().count();
    if n <= MESSAGE_MAX { return s.to_string(); }
    let tail: String = s.chars().skip(n - KEEP).collect();
    format!("{}\n{}", crate::text::clipped(n - KEEP), tail)
}
