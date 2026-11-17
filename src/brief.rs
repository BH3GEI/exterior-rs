//! brief: the only way exterior talks to Telegram (and the only way agents reach WeChat/Telegram messages).
use serde_json::Value;
use std::process::Command;

pub const BASE: &str = "https://brief.example.com";
pub const TOKEN_FILE: &str = "/path/to/brief-token";

/// One API call. Ok(json) on 2xx, Err((status, json)) otherwise (status 0 = no answer at all).
pub fn call(method: &str, path: &str, body: Option<&Value>) -> Result<Value, (u16, Value)> {
    let key = std::fs::read_to_string(TOKEN_FILE).unwrap_or_default();
    let mut c = Command::new("curl");
    c.args(["-s", "-m", "70", "-w", "\n%{http_code}", "-X", method])
        .arg("-H").arg(format!("Authorization: Bearer {}", key.trim()))
        .arg(format!("{BASE}{path}"));
    if let Some(b) = body {
        c.args(["-H", "Content-Type: application/json", "-d"]).arg(b.to_string());
    }
    let out = match c.output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(_) => return Err((0, Value::Null)),
    };
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
