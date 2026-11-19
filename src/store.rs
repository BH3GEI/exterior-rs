//! Everything exterior keeps, in one folder (on Lazycat: /lzcapp/var):
//! config.json (devices, agents, brief, settings), secret.key, sessions/<topic>.json, state.json,
//! messages.jsonl (session → session), exterior.log.
use serde_json::{json, Value};
use std::io::Write;
use std::sync::Mutex;

pub fn var() -> String { std::env::var("EXTERIOR_VAR").unwrap_or_else(|_| "/lzcapp/var".into()) }
pub fn path(p: &str) -> String { format!("{}/{p}", var()) }

static LOCK: Mutex<()> = Mutex::new(());

/// Tests that point the config at their own fake brief take turns.
#[cfg(test)]
pub static TEST: Mutex<()> = Mutex::new(());

pub fn load(p: &str) -> Value { std::fs::read_to_string(p).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({})) }

pub fn save(p: &str, v: &Value) {
    if let Some((d, _)) = p.rsplit_once('/') { let _ = std::fs::create_dir_all(d); }
    let tmp = format!("{p}.tmp");
    if std::fs::write(&tmp, serde_json::to_string_pretty(v).unwrap()).is_ok() { let _ = std::fs::rename(tmp, p); }
}

pub fn now() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) }

pub fn log(s: &str) {
    let line = format!("{} {s}", crate::web::fmt_time(now()));
    eprintln!("{line}");
    let _g = LOCK.lock().unwrap();
    let p = path("exterior.log");
    if std::fs::metadata(&p).map(|m| m.len() > 2_000_000).unwrap_or(false) { let _ = std::fs::rename(&p, format!("{p}.1")); }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) { let _ = writeln!(f, "{line}"); }
}

pub fn tail(file: &str, n: usize) -> Vec<String> {
    let s = std::fs::read_to_string(path(file)).unwrap_or_default();
    let v: Vec<String> = s.lines().map(String::from).collect();
    v[v.len().saturating_sub(n)..].to_vec()
}

pub fn append(file: &str, v: &Value) {
    let _g = LOCK.lock().unwrap();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path(file)) { let _ = writeln!(f, "{v}"); }
}

// ---- config ----

pub fn config() -> Value {
    let _g = LOCK.lock().unwrap();
    let mut c = load(&path("config.json"));
    if !c["devices"].is_array() { c["devices"] = json!([]); }
    if !c["brief"].is_object() { c["brief"] = json!({"url": ""}); }
    c
}

pub fn set_config(c: &Value) { let _g = LOCK.lock().unwrap(); save(&path("config.json"), c); }

pub fn devices() -> Vec<Value> { config()["devices"].as_array().cloned().unwrap_or_default() }

pub fn device(name: &str) -> Option<Value> { devices().into_iter().find(|d| d["name"] == name) }

pub fn default_device() -> Option<Value> { devices().into_iter().next() }

/// The agent row `name` on device `dev` (or None).
pub fn agent(dev: &Value, name: &str) -> Option<Value> {
    dev["agents"].as_array().into_iter().flatten().find(|a| a["name"] == name).cloned()
}

// ---- per-topic notes ----

fn note_path(tid: &str) -> String { path(&format!("sessions/{}.json", crate::brief::topic_id(tid))) }
pub fn note(tid: &str) -> Value { let _g = LOCK.lock().unwrap(); load(&note_path(tid)) }
pub fn note_set(tid: &str, k: &str, v: Value) {
    let _g = LOCK.lock().unwrap();
    let p = note_path(tid);
    let mut n = load(&p);
    n[k] = v;
    save(&p, &n);
}
pub fn notes() -> Vec<Value> {
    let Ok(rd) = std::fs::read_dir(path("sessions")) else { return vec![] };
    let mut v: Vec<Value> = rd.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "json")).map(|e| load(&e.path().display().to_string())).collect();
    v.sort_by_key(|n| std::cmp::Reverse(n["active"].as_u64().unwrap_or(0)));
    v
}

// ---- small state ----

pub fn state(k: &str) -> Value { let _g = LOCK.lock().unwrap(); load(&path("state.json"))[k].clone() }
pub fn set_state(k: &str, v: Value) {
    let _g = LOCK.lock().unwrap();
    let p = path("state.json");
    let mut s = load(&p);
    s[k] = v;
    save(&p, &s);
}
