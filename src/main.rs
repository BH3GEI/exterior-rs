//! exterior: the user's Telegram topics ↔ agents on his devices. Agents read what they need to know from knowledge.
mod agents;
mod brief;
mod crypt;
mod daemon;
mod host;
mod infra;
mod params;
mod prompt;
mod store;
mod text;
mod web;

fn main() {
    // `exterior probe <claude|pi|codex> <command> <dir> <question> [model]`: start one agent on this machine with no
    // start text, list its models, optionally switch, ask the question, print what comes back. For checking the drivers.
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
    let spec = agents::Spec { tid: format!("{}0", brief::TOPIC_PREFIX), prompt: String::new(), topic: "probe".into(), dev,
                              dir: a[2].clone(), command: a[1].clone(), kind, gen: 1, resume: None, effort: None };
    let mut ag = agents::Agent::start(spec, tx).expect("start");
    let c = ag.ctl();
    match c.models() { Ok(l) => println!("models: {}", l.iter().map(|m| format!("{}={}", m.label, m.value)).collect::<Vec<_>>().join(", ")), Err(e) => println!("models error: {e}") }
    if let Some(m) = a.get(4) { println!("set_model {m}: {:?}", c.set_model(m)); }
    if kind == agents::Kind::Pi { println!("thinking levels: {:?}", c.thinking_levels()); }
    ag.say(&a[3]).expect("say");
    for ev in rx.iter() {
        match ev {
            agents::Ev::Done { text, error, .. } => { println!("done: {text:?} error: {error:?}"); break; }
            agents::Ev::Exited { why, .. } => { println!("exited: {why}"); break; }
            agents::Ev::Session { id, .. } => println!("session {id}"),
            agents::Ev::Note { text, .. } => println!("note: {text}"),
            _ => {}
        }
    }
    // the agent's own commands that go through the protocol (read-only ones, plus naming the session)
    for (k, name) in agents::BUILTINS.iter().filter(|(k, _)| *k == kind) {
        let arg = match *name {
            "name" | "rename" => "probe",
            "thinking" | "model" | "new" | "clear" | "compact" | "review" | "archive" | "logout" | "import" | "exit" | "quit" => continue,
            "copy" if kind == agents::Kind::Codex => continue,
            _ => "",
        };
        let r = c.builtin(name, arg);
        println!("/{} {arg}: {:?}", agents::builtin_command(*k, name), r.as_ref().map(|v| v.to_string().chars().take(3000).collect::<String>()));
        if *name == "fork" {
            if let Some(e) = r.ok().and_then(|v| v["messages"][0]["entryId"].as_str().map(String::from)) {
                println!("/pi_fork {e}: {:?}", c.builtin("fork", &e).map(|v| v.to_string()));
            }
        }
    }
    ag.kill();
}
