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

/// One API call. Ok(json) on 2xx, Err((status, json)) otherwise (status 0 = no answer at all).
pub fn call(method: &str, path: &str, body: Option<&Value>) -> Result<Value, (u16, Value)> {
    let (base, key) = conf();
    if base.is_empty() || key.is_empty() { return Err((0, Value::String("未设置".into()))); }
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

pub fn get(path: &str) -> Option<Value> { call("GET", path, None).ok() }
pub fn post(path: &str, body: Value) -> Result<Value, (u16, Value)> { call("POST", path, Some(&body)) }

/// Telegram allows 4096 characters per message.
pub fn clip(s: &str) -> String {
    let n = s.chars().count();
    if n <= 4000 { return s.to_string(); }
    let tail: String = s.chars().skip(n - 3900).collect();
    format!("…（前面 {} 字略）\n{}", n - 3900, tail)
}
