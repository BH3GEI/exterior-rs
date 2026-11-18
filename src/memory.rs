//! The memory about Yao. The master copy is a git repository inside exterior (var/memory).
//! Every device has a working copy at <root>/memory that the agents read and write as plain files.
//! After each turn exterior commits the device's changes, merges them into the master, pushes the master back
//! to every device, and backs it up to a private GitHub repository.
use crate::host::{self, quote};
use crate::store;
use serde_json::{json, Value};
use std::process::Command;
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());

pub fn dir() -> String { store::path("memory") }

fn git(args: &[&str], dev: Option<&Value>) -> Result<String, String> {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir()).args(args);
    if let Some(d) = dev { c.env("GIT_SSH_COMMAND", host::git_ssh(d)).env("SSHPASS", host::password(d)); }
    let o = c.output().map_err(|e| e.to_string())?;
    let out = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)).trim().to_string();
    if o.status.success() { Ok(out) } else { Err(out) }
}

fn init() {
    if std::path::Path::new(&format!("{}/.git", dir())).exists() { return; }
    let _ = std::fs::create_dir_all(dir());
    let _ = Command::new("git").args(["init", "-q", "-b", "main"]).arg(dir()).status();
    let _ = git(&["config", "user.name", "exterior"], None);
    let _ = git(&["config", "user.email", "exterior@localhost"], None);
}

fn url(dev: &Value) -> String {
    let root = dev["root"].as_str().unwrap_or("").trim_end_matches('/');
    if matches!(dev["host"].as_str().unwrap_or(""), "" | "local" | "localhost") { return format!("{root}/memory"); }
    format!("ssh://{}@{}:{}{}/memory", dev["user"].as_str().unwrap_or(""), dev["host"].as_str().unwrap_or(""), dev["port"].as_u64().unwrap_or(22), root)
}

fn record(key: &str, name: &str, r: &Result<String, String>) {
    let mut m = store::state(key);
    if !m.is_object() { m = json!({}); }
    m[name] = json!({"time": store::now(), "ok": r.is_ok(), "msg": match r { Ok(s) => s.clone(), Err(e) => e.clone() }});
    store::set_state(key, m);
}

/// Bring one device and the master together: commit there, merge here, push back.
fn sync_one(dev: &Value, reason: &str) -> Result<String, String> {
    let root = dev["root"].as_str().unwrap_or("").trim_end_matches('/').to_string();
    if root.is_empty() { return Err("没有设置根目录".into()); }
    let m = format!("{root}/memory");
    let script = format!(
        "mkdir -p {m} && cd {m} && {{ [ -d .git ] || git init -q -b main; }} && git config receive.denyCurrentBranch updateInstead && \
         git add -A && {{ git -c user.name=exterior -c user.email=exterior@localhost commit -qm {r} >/dev/null || true; }} && \
         git rev-parse --verify -q HEAD >/dev/null && echo has-commits || echo empty",
        m = quote(&m), r = quote(reason));
    let state = host::run(dev, &script, 60)?;
    if state.ends_with("has-commits") {
        git(&["pull", "-q", "--no-rebase", "--no-edit", "--allow-unrelated-histories", &url(dev), "HEAD"], Some(dev)).map_err(|e| {
            let _ = git(&["merge", "--abort"], None);
            format!("合并失败：{e}")
        })?;
    }
    if git(&["rev-parse", "--verify", "-q", "HEAD"], None).is_err() { return Ok("还没有记忆".into()); }
    git(&["push", "-q", &url(dev), "HEAD:refs/heads/main"], Some(dev)).map_err(|e| format!("推回设备失败：{e}"))?;
    // the device may have been on another branch name before
    let _ = host::run(dev, &format!("cd {} && git checkout -q main 2>/dev/null; true", quote(&m)), 30);
    let n = git(&["rev-list", "--count", "HEAD"], None).unwrap_or_default();
    Ok(format!("已同步，共 {n} 次提交"))
}

/// Sync every device (the one that just finished a turn first), then back up to GitHub.
pub fn sync(first: Option<&str>, reason: &str) {
    let _g = LOCK.lock().unwrap();
    init();
    let mut devs = store::devices();
    if let Some(f) = first { devs.sort_by_key(|d| d["name"] != f); }
    for d in &devs {
        let r = sync_one(d, reason);
        if let Err(e) = &r { store::log(&format!("memory sync {}: {e}", d["name"])); }
        record("memory_sync", d["name"].as_str().unwrap_or(""), &r);
    }
    // a second pass so that changes merged from a later device reach the earlier ones too
    if devs.len() > 1 { for d in &devs { let _ = sync_one(d, reason); } }
    backup_locked();
}

// ---- files, for the 记忆 page ----

pub fn files() -> Vec<(String, u64, String)> {
    init();
    let mut v = vec![];
    if let Ok(rd) = std::fs::read_dir(dir()) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with('.') || !e.path().is_file() { continue; }
            let len = e.metadata().map(|m| m.len()).unwrap_or(0);
            let body = std::fs::read_to_string(e.path()).unwrap_or_default();
            let desc = body.lines().find_map(|l| l.strip_prefix("description:")).unwrap_or("").trim().to_string();
            v.push((n, len, desc));
        }
    }
    v.sort_by(|a, b| (a.0 != "MEMORY.md").cmp(&(b.0 != "MEMORY.md")).then(a.0.cmp(&b.0)));
    v
}

pub fn safe_name(n: &str) -> Option<String> {
    let n = n.trim();
    if n.is_empty() || n.contains('/') || n.contains('\\') || n.starts_with('.') { return None; }
    Some(n.to_string())
}

pub fn read(name: &str) -> String { safe_name(name).and_then(|n| std::fs::read_to_string(format!("{}/{n}", dir())).ok()).unwrap_or_default() }

/// An edit made on the page: commit it to the master, then push it out.
pub fn write(name: &str, body: Option<&str>) -> Result<(), String> {
    let n = safe_name(name).ok_or("文件名不合法")?;
    {
        let _g = LOCK.lock().unwrap();
        init();
        let p = format!("{}/{n}", dir());
        match body { Some(b) => std::fs::write(&p, b.replace("\r\n", "\n")).map_err(|e| e.to_string())?, None => { let _ = std::fs::remove_file(&p); } }
        let _ = git(&["add", "-A"], None);
        let _ = git(&["commit", "-qm", &format!("界面：{} {n}", if body.is_some() { "编辑" } else { "删除" })], None);
    }
    std::thread::spawn(|| sync(None, "界面编辑"));
    Ok(())
}

pub fn log_lines(n: usize) -> Vec<String> {
    init();
    git(&["log", &format!("-{n}"), "--date=format:%m-%d %H:%M", "--pretty=%ad  %s"], None).map(|s| s.lines().map(String::from).collect()).unwrap_or_default()
}

// ---- GitHub backup ----

pub fn github_key() -> String { store::path("github_key") }

pub fn github_public_key() -> String {
    let k = github_key();
    if !std::path::Path::new(&k).exists() {
        let _ = Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-C", "exterior-memory-backup", "-f"]).arg(&k).status();
    }
    std::fs::read_to_string(format!("{k}.pub")).unwrap_or_default().trim().to_string()
}

fn backup_locked() {
    let remote = store::config()["github"]["remote"].as_str().unwrap_or("").trim().to_string();
    if remote.is_empty() { return; }
    if git(&["rev-parse", "--verify", "-q", "HEAD"], None).is_err() { return; }
    let _ = github_public_key();
    let ssh = format!("ssh -i {} -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile={}", github_key(), store::path("known_hosts"));
    let o = Command::new("git").arg("-C").arg(dir()).args(["push", "-q", &remote, "HEAD:refs/heads/main"]).env("GIT_SSH_COMMAND", ssh).output();
    let r = match o {
        Ok(o) if o.status.success() => Ok("已备份".to_string()),
        Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => Err(e.to_string()),
    };
    if let Err(e) = &r { store::log(&format!("GitHub backup: {e}")); }
    record("backup", "github", &r);
}

pub fn backup() { let _g = LOCK.lock().unwrap(); init(); backup_locked(); }
