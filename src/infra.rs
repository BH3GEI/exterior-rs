//! knowledge: agents edit its files; after each turn exterior commits what changed there and pushes it to GitHub.
//! On a remote device this is done with shell lines over ssh; on this machine git is run directly (no shell).
use crate::host::{self, quote};
use crate::{params, store, text};
use serde_json::{json, Value};
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());

/// Markers the shell scripts below print, so the outcome can be told apart from git's own output.
const NO_CHANGE: &str = "EXTERIOR_NO_CHANGE";
const PUSHED: &str = "EXTERIOR_PUSHED";

/// Commit and push whatever a session changed in knowledge on its device.
pub fn commit(dev: &Value, topic: &str) {
    let _g = LOCK.lock().unwrap();
    let p = crate::prompt::infra(dev);
    let msg = format!("{topic}（exterior）");
    let script = format!(
        "cd {p} || exit 0; chmod -R u+w . 2>/dev/null; git add -A; git diff --cached --quiet && {{ echo {NO_CHANGE}; exit 0; }}; \
         git diff --cached --stat | tail -1; git commit -qm {m} && git push -q 2>&1 && echo {PUSHED}",
        p = quote(&p), m = quote(&msg));
    let r = if host::is_local(dev) { commit_local(&p, &msg) } else { host::run(dev, &script, params::num("git_timeout_s")) };
    let changed = !matches!(&r, Ok(s) if s.trim() == NO_CHANGE);
    if let Err(e) = &r { store::log(&format!("knowledge {}: {e}", dev["name"])); }
    if changed {
        let mut m = store::state("infra");
        if !m.is_object() { m = json!({}); }
        let ok = r.as_ref().map(|s| s.contains(PUSHED)).unwrap_or(false);
        let shown = match &r { Ok(s) => s.replace(PUSHED, text::PUSHED).trim().to_string(), Err(e) => e.clone() };
        m[dev["name"].as_str().unwrap_or("")] = json!({"time": store::now(), "ok": ok, "topic": topic, "msg": shown});
        store::set_state("infra", m);
    }
}

/// `git -C <knowledge> …` on this machine.
fn git(dir: &str, args: &[&str]) -> std::process::Command {
    let mut c = std::process::Command::new("git");
    c.arg("-C").arg(dir).args(args);
    c
}

/// Agents may leave files read-only; git needs to write them (`chmod -R u+w .`; nothing to do on Windows).
fn writable(dir: &str) {
    if cfg!(unix) { let _ = std::process::Command::new("chmod").args(["-R", "u+w", "."]).current_dir(dir).output(); }
}

/// The commit script above, on this machine.
fn commit_local(p: &str, msg: &str) -> Result<String, String> {
    if !std::path::Path::new(p).is_dir() { return Ok(String::new()); }
    let t = params::num("git_timeout_s");
    writable(p);
    let _ = host::exec(git(p, &["add", "-A"]), t);
    if host::exec(git(p, &["diff", "--cached", "--quiet"]), t).is_ok() { return Ok(NO_CHANGE.into()); }
    let stat = host::exec(git(p, &["diff", "--cached", "--stat"]), t).unwrap_or_default().lines().last().unwrap_or("").to_string();
    host::exec(git(p, &["commit", "-qm", msg]), t)?;
    let push = host::exec_all(git(p, &["push", "-q"]), t).map_err(|e| format!("{stat}\n{e}"))?;
    Ok(format!("{stat}\n{push}\n{PUSHED}"))
}

/// Recent changes, for the 记忆 page.
pub fn log(dev: &Value, n: usize) -> String {
    if host::is_local(dev) {
        let args = [format!("-{n}"), "--date=format:%m-%d %H:%M".into(), "--pretty=%h  %ad  %s".into(), "--stat=60".into(), "--stat-count=5".into()];
        let mut c = git(&crate::prompt::infra(dev), &["log"]);
        c.args(&args);
        return host::exec(c, 30).map(|s| s.lines().filter(|l| !l.trim().is_empty()).collect::<Vec<_>>().join("\n")).unwrap_or_else(|e| text::read_failed(&e));
    }
    let script = format!("cd {} && git log -{n} --date=format:'%m-%d %H:%M' --pretty='%h  %ad  %s' --stat=60 --stat-count=5 | grep -v '^ *$'", quote(&crate::prompt::infra(dev)));
    host::run(dev, &script, 30).unwrap_or_else(|e| text::read_failed(&e))
}

/// Where knowledge is pushed (the 记忆 page shows it).
pub fn remote(dev: &Value) -> String {
    if host::is_local(dev) { return host::exec(git(&crate::prompt::infra(dev), &["remote", "get-url", "origin"]), 30).unwrap_or_else(|e| text::read_failed(&e)); }
    host::run(dev, &format!("cd {} && git remote get-url origin", quote(&crate::prompt::infra(dev))), 30).unwrap_or_else(|e| text::read_failed(&e))
}

/// Undo one commit (the page's 撤回 button).
pub fn revert(dev: &Value, hash: &str) -> Result<String, String> {
    if hash.is_empty() || !hash.chars().all(|c| c.is_ascii_hexdigit()) { return Err(text::BAD_COMMIT.into()); }
    let _g = LOCK.lock().unwrap();
    if host::is_local(dev) {
        let (p, t) = (crate::prompt::infra(dev), params::num("git_timeout_s"));
        writable(&p);
        host::exec(git(&p, &["revert", "--no-edit", hash]), t)?;
        host::exec_all(git(&p, &["push", "-q"]), t)?;
        return Ok(text::REVERTED.into());
    }
    host::run(dev, &format!("cd {} && chmod -R u+w . && git revert --no-edit {hash} >/dev/null && git push -q 2>&1", quote(&crate::prompt::infra(dev))), params::num("git_timeout_s"))?;
    Ok(text::REVERTED.into())
}

/// Check that exterior can push knowledge from this device (the page's 测试推送 button).
pub fn test(dev: &Value) -> Result<String, String> {
    if host::is_local(dev) {
        host::exec_all(git(&crate::prompt::infra(dev), &["push", "--dry-run"]), params::num("git_timeout_s"))?;
        return Ok(text::PUSH_OK.into());
    }
    host::run(dev, &format!("cd {} && git push --dry-run 2>&1", quote(&crate::prompt::infra(dev))), params::num("git_timeout_s"))?;
    Ok(text::PUSH_OK.into())
}

/// The .md files in knowledge (the 记忆 page), without .git/ and archive/.
pub fn files(dev: &Value) -> String {
    let p = crate::prompt::infra(dev);
    if !host::is_local(dev) {
        return host::run(dev, &format!("cd {} && find . -name '*.md' -not -path './.git/*' -not -path './archive/*' | sed 's#^./##' | sort", quote(&p)), 30).unwrap_or_else(|e| text::read_failed(&e));
    }
    fn walk(base: &std::path::Path, rel: &str, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(base.join(rel)) else { return };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let r = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            if e.path().is_dir() { if !(rel.is_empty() && (name == ".git" || name == "archive")) { walk(base, &r, out); } }
            else if name.ends_with(".md") { out.push(r); }
        }
    }
    if !std::path::Path::new(&p).is_dir() { return text::read_failed(&format!("{p}: {}", text::FILE_MISSING)); }
    let mut v = Vec::new();
    walk(std::path::Path::new(&p), "", &mut v);
    v.sort();
    v.join("\n")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    /// knowledge on this machine: committed, listed, logged and reverted with git run directly.
    #[test]
    fn local_knowledge() {
        let _g = crate::store::TEST.lock().unwrap_or_else(|e| e.into_inner());
        let ok = |c: &mut std::process::Command| assert!(c.output().unwrap().status.success());
        let d = std::env::temp_dir().join(format!("exterior-infra-{}", std::process::id()));
        let (bare, k) = (d.join("remote.git"), d.join("knowledge"));
        std::fs::create_dir_all(&k).unwrap();
        std::env::set_var("EXTERIOR_VAR", d.join("var"));
        crate::store::set_config(&json!({}));
        ok(std::process::Command::new("git").args(["init", "-q", "--bare"]).arg(&bare));
        let g = |args: &[&str]| { let mut c = std::process::Command::new("git"); c.arg("-C").arg(&k).args(args); c };
        ok(&mut g(&["init", "-q", "-b", "main"]));
        ok(&mut g(&["config", "user.email", "t@example.com"]));
        ok(&mut g(&["config", "user.name", "t"]));
        ok(&mut g(&["remote", "add", "origin", bare.to_str().unwrap()]));
        std::fs::write(k.join("a.md"), "a").unwrap();
        ok(&mut g(&["add", "-A"]));
        ok(&mut g(&["commit", "-qm", "first"]));
        ok(&mut g(&["push", "-q", "-u", "origin", "main"]));
        let dev = json!({"name": "here", "host": "local", "infra": k.to_str().unwrap()});
        std::fs::create_dir_all(k.join("sub")).unwrap();
        std::fs::create_dir_all(k.join("archive")).unwrap();
        std::fs::write(k.join("sub").join("b.md"), "b").unwrap();
        std::fs::write(k.join("archive").join("old.md"), "o").unwrap();
        super::commit(&dev, "话题");
        let st = &crate::store::state("infra")["here"];
        assert_eq!(st["ok"], true, "{st}");
        assert_eq!(super::files(&dev), "a.md\nsub/b.md");
        let log = super::log(&dev, 5);
        assert!(log.contains("话题（exterior）"), "{log}");
        assert_eq!(super::remote(&dev), bare.to_str().unwrap());
        assert_eq!(super::test(&dev).unwrap(), crate::text::PUSH_OK);
        let hash = log.split_whitespace().next().unwrap().to_string();
        assert_eq!(super::revert(&dev, &hash).unwrap(), crate::text::REVERTED);
        assert!(!k.join("sub").join("b.md").exists());
        // nothing changed: nothing recorded
        crate::store::set_state("infra", json!({}));
        super::commit(&dev, "话题");
        assert!(crate::store::state("infra")["here"].is_null());
        let _ = std::fs::remove_dir_all(&d);
    }
}
