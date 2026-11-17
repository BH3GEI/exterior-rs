//! exterior's own records, kept where exterior runs (not on the agents' machine):
//! one small note per session (which topic, which dir, which agent, each agent's session id) and a little state.
use serde_json::{json, Value};

pub fn state_dir() -> String {
    std::env::var("EXTERIOR_STATE").unwrap_or_else(|_| format!("{}/.exterior", std::env::var("HOME").unwrap_or("/tmp".into())))
}

fn path(tid: &str) -> String { format!("{}/sessions/{}.json", state_dir(), tid.trim_start_matches("tgbot:")) }

fn load(p: &str) -> Value { std::fs::read_to_string(p).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({})) }

fn save(p: &str, v: &Value) {
    if let Some((d, _)) = p.rsplit_once('/') { let _ = std::fs::create_dir_all(d); }
    let tmp = format!("{p}.tmp");
    if std::fs::write(&tmp, serde_json::to_string_pretty(v).unwrap()).is_ok() { let _ = std::fs::rename(tmp, p); }
}

pub fn read(tid: &str) -> Value { load(&path(tid)) }

pub fn set(tid: &str, k: &str, v: Value) {
    let p = path(tid);
    let mut n = load(&p);
    n[k] = v;
    save(&p, &n);
}

pub fn all() -> Vec<Value> {
    let Ok(rd) = std::fs::read_dir(format!("{}/sessions", state_dir())) else { return vec![] };
    rd.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "json")).map(|e| load(&e.path().display().to_string())).collect()
}

pub mod state {
    use super::*;
    fn p() -> String { format!("{}/state.json", state_dir()) }
    pub fn get(k: &str) -> Value { load(&p())[k].clone() }
    pub fn set(k: &str, v: Value) { let mut s = load(&p()); s[k] = v; save(&p(), &s); }
}
