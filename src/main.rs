//! exterior: Yao's Telegram topics ↔ agents on his devices. Agents read what they need to know about Yao from knowledge.
mod agents;
mod brief;
mod crypt;
mod daemon;
mod host;
mod infra;
mod prompt;
mod store;
mod web;

fn main() {
    // `exterior probe <claude|pi|codex> <command> <dir> [model]`: start one agent on this machine, list its models,
    // optionally switch, ask one question, print what comes back. For checking the agent drivers without Telegram.
    let a: Vec<String> = std::env::args().collect();
    if a.get(1).map(|s| s.as_str()) == Some("probe") { return probe(&a[2..]); }
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(3000);
    let _ = std::fs::create_dir_all(store::var());
    daemon::start();
    web::serve(port);
}

fn probe(a: &[String]) {
    let kind = agents::Kind::parse(&a[0]).expect("kind");
    let dev = serde_json::json!({"name": "local", "host": "local", "root": a[2]});
    let (tx, rx) = std::sync::mpsc::channel();
    let spec = agents::Spec { tid: "tgbot:0".into(), prompt: "你在 exterior 的测试话题「探针」里。".into(), topic: "探针".into(), dev,
                              dir: a[2].clone(), command: a[1].clone(), kind, gen: 1, resume: None };
    let mut ag = agents::Agent::start(spec, tx).expect("start");
    let c = ag.ctl();
    match c.models() { Ok(l) => println!("models: {}", l.iter().map(|m| format!("{}={}", m.label, m.value)).collect::<Vec<_>>().join(", ")), Err(e) => println!("models error: {e}") }
    if let Some(m) = a.get(3) { println!("set_model {m}: {:?}", c.set_model(m)); }
    if kind != agents::Kind::Claude { println!("builtin compact: {:?}", c.builtin("compact", "")); }
    ag.say("你在哪个话题？只回答话题名。").expect("say");
    for ev in rx.iter() {
        match ev {
            agents::Ev::Done { text, error, .. } => { println!("done: {text:?} error: {error:?}"); break; }
            agents::Ev::Exited { why, .. } => { println!("exited: {why}"); break; }
            agents::Ev::Session { id, .. } => println!("session {id}"),
            agents::Ev::Note { text, .. } => println!("note: {text}"),
            _ => {}
        }
    }
    ag.kill();
}
