//! Everything exterior keeps, in one folder (on Lazycat: /lzcapp/var; elsewhere a folder of the user's, see `default_var`):
//! config.json (devices, agents, brief, settings), secret.key, sessions/<topic>.json, state.json,
//! messages.jsonl (session → session), exterior.log.
use serde_json::{json, Value};
use std::io::Write;
use std::sync::{Mutex, OnceLock};

/// Running as the Lazycat app (its domain is in the environment, or its folders exist).
pub fn on_lazycat() -> bool {
    static L: OnceLock<bool> = OnceLock::new();
    *L.get_or_init(|| std::env::var("LAZYCAT_APP_DOMAIN").is_ok_and(|d| !d.is_empty()) || std::path::Path::new("/lzcapp").is_dir())
}

/// The data folder: EXTERIOR_VAR if set; on Lazycat /lzcapp/var; else the user's own application data folder.
pub fn var() -> String {
    std::env::var("EXTERIOR_VAR").ok().filter(|v| !v.is_empty())
        .unwrap_or_else(|| default_var(on_lazycat(), std::env::consts::OS, &|k| std::env::var(k).ok().filter(|v| !v.is_empty())))
}

/// macOS: ~/Library/Application Support/exterior; Windows: %APPDATA%\exterior; other systems: $XDG_DATA_HOME/exterior
/// (or ~/.local/share/exterior).
pub fn default_var(lazycat: bool, os: &str, env: &dyn Fn(&str) -> Option<String>) -> String {
    if lazycat { return "/lzcapp/var".into(); }
    let home = env("HOME").or_else(|| env("USERPROFILE"));
    let p = match os {
        "macos" => home.map(|h| format!("{h}/Library/Application Support/exterior")),
        "windows" => env("APPDATA").map(|a| format!("{a}\\exterior")).or_else(|| env("USERPROFILE").or(home).map(|h| format!("{h}\\AppData\\Roaming\\exterior"))),
        _ => env("XDG_DATA_HOME").map(|x| format!("{x}/exterior")).or_else(|| home.map(|h| format!("{h}/.local/share/exterior"))),
    };
    p.unwrap_or_else(|| "exterior-data".into())
}
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

#[cfg(test)]
mod tests {
    use super::default_var;

    #[test]
    fn data_folder() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|p| p.0 == k).map(|p| p.1.to_string());
        let mac = env(&[("HOME", "/Users/a")]);
        assert_eq!(default_var(true, "macos", &mac), "/lzcapp/var");
        assert_eq!(default_var(true, "linux", &env(&[])), "/lzcapp/var");
        assert_eq!(default_var(false, "macos", &mac), "/Users/a/Library/Application Support/exterior");
        assert_eq!(default_var(false, "windows", &env(&[("APPDATA", r"C:\Users\a\AppData\Roaming"), ("USERPROFILE", r"C:\Users\a")])), r"C:\Users\a\AppData\Roaming\exterior");
        assert_eq!(default_var(false, "windows", &env(&[("USERPROFILE", r"C:\Users\a")])), r"C:\Users\a\AppData\Roaming\exterior");
        assert_eq!(default_var(false, "linux", &env(&[("HOME", "/home/a")])), "/home/a/.local/share/exterior");
        assert_eq!(default_var(false, "linux", &env(&[("HOME", "/home/a"), ("XDG_DATA_HOME", "/x")])), "/x/exterior");
        assert_eq!(default_var(false, "linux", &env(&[])), "exterior-data");
    }

    /// The Lazycat package stays as it is: data in /lzcapp/var, port 3000, only /api/ open without the login,
    /// San Francisco time, the static x86_64 program, and the same version as the program.
    #[test]
    fn lazycat_package() {
        let start = include_str!("../pkg/content/start.sh").replace('\r', "");
        assert!(start.contains("export EXTERIOR_VAR=/lzcapp/var PORT=3000 HOME=/lzcapp/var\n"), "{start}");
        assert!(start.contains("exec /lzcapp/pkg/content/exterior\n"), "{start}");
        assert!(start.contains("apk add -q --no-network --no-cache /lzcapp/pkg/content/apk/*.apk"), "{start}");
        let manifest = include_str!("../pkg/lzc-manifest.yml").replace('\r', "");
        for want in ["subdomain: exterior\n", "public_path:\n    - /api/\n", "- /=http://worker:3000/\n",
                     "command: /bin/sh /lzcapp/pkg/content/start.sh\n", "- TZ=America/Los_Angeles\n"] {
            assert!(manifest.contains(want), "{want:?} not in lzc-manifest.yml");
        }
        let build = include_str!("../pkg/build.sh").replace('\r', "");
        assert!(build.contains("--target x86_64-unknown-linux-musl"), "{build}");
        assert!(build.contains("cp ../target/x86_64-unknown-linux-musl/release/exterior content/exterior"), "{build}");
        let package = include_str!("../pkg/package.yml").replace('\r', "");
        assert!(package.contains("package: bh3gei.lzcapp.exterior\n"), "{package}");
        assert!(package.contains(&format!("\nversion: {}\n", env!("CARGO_PKG_VERSION"))), "pkg/package.yml version differs from Cargo.toml");
    }
}
