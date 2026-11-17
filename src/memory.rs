//! The "about Yao" memory: a git clone (on the agents' machine) of the master copy on Lazycat mem.
//! exterior pulls before a turn and pushes after it; the agents only read and write files.
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());

fn git(root: &str, script: &str) {
    let _ = crate::host::sh(&format!("cd '{root}/memory' && {script}"));
}

pub fn pull(root: &str) {
    let _g = LOCK.lock().unwrap();
    git(root, "git pull -q --rebase --autostash");
}

pub fn push(root: &str, who: &str) {
    let _g = LOCK.lock().unwrap();
    let who: String = who.chars().filter(|c| !matches!(c, '\'' | '"' | '\\' | '`' | '$')).collect();
    git(root, &format!(
        "git add -A && (git diff --cached --quiet || git -c user.name=exterior -c user.email=exterior@local commit -q -m '{who}: memory') \
         && (git push -q 2>/dev/null || (git pull -q --rebase --autostash && git push -q))"));
}
