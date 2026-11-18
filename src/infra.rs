//! knowledge: agents edit its files; after each turn exterior commits what changed there and pushes it to GitHub.
use crate::host::{self, quote};
use crate::store;
use serde_json::{json, Value};
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());

/// Commit and push whatever a session changed in knowledge on its device.
pub fn commit(dev: &Value, topic: &str) {
    let _g = LOCK.lock().unwrap();
    let p = crate::prompt::infra(dev);
    let msg = format!("{topic}（exterior）");
    let script = format!(
        "cd {p} || exit 0; chmod -R u+w . 2>/dev/null; git add -A; git diff --cached --quiet && {{ echo 没有变化; exit 0; }}; \
         git diff --cached --stat | tail -1; git commit -qm {m} && git push -q 2>&1 && echo 已推送",
        p = quote(&p), m = quote(&msg));
    let r = host::run(dev, &script, 90);
    let changed = !matches!(&r, Ok(s) if s.trim() == "没有变化");
    if let Err(e) = &r { store::log(&format!("knowledge {}: {e}", dev["name"])); }
    if changed {
        let mut m = store::state("infra");
        if !m.is_object() { m = json!({}); }
        m[dev["name"].as_str().unwrap_or("")] = json!({"time": store::now(), "ok": r.as_ref().map(|s| s.contains("已推送")).unwrap_or(false), "topic": topic, "msg": match &r { Ok(s) => s.clone(), Err(e) => e.clone() }});
        store::set_state("infra", m);
    }
}

/// Recent changes, for the 记忆 page.
pub fn log(dev: &Value, n: usize) -> String {
    let script = format!("cd {} 2>/dev/null || {{ echo 没有这个目录; exit 0; }}; git log -{n} --date=format:'%m-%d %H:%M' --pretty='%h  %ad  %s' --stat=60 --stat-count=5 | grep -v '^ *$'", quote(&crate::prompt::infra(dev)));
    host::run(dev, &script, 30).unwrap_or_else(|e| format!("读不到：{e}"))
}

/// Undo one commit (the page's 撤回 button).
pub fn revert(dev: &Value, hash: &str) -> Result<String, String> {
    if hash.is_empty() || !hash.chars().all(|c| c.is_ascii_hexdigit()) { return Err("编号不对".into()); }
    let _g = LOCK.lock().unwrap();
    host::run(dev, &format!("cd {} && chmod -R u+w . && git revert --no-edit {hash} >/dev/null && git push -q 2>&1 && echo 已撤回并推送", quote(&crate::prompt::infra(dev))), 90)
}

/// Check that exterior can push knowledge from this device (the page's 测试推送 button).
pub fn test(dev: &Value) -> Result<String, String> {
    let r = host::run(dev, &format!("cd {} && git push --dry-run 2>&1 && echo 可以推送", quote(&crate::prompt::infra(dev))), 60)?;
    if r.trim_end().ends_with("可以推送") { Ok("可以推送".into()) } else { Err(r) }
}
