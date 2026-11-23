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
mod usage;
mod web;

fn main() {
    // `exterior probe <claude|pi|codex> <command> <dir> <question> [model]`: start one agent on this machine with no
    // start text, list its models, optionally switch, ask the question, print what comes back. For checking the drivers.
    // Windows: the agents exterior starts must not outlive it
    #[cfg(windows)]
    if let Err(e) = host::job::enter() { eprintln!("job object: {e}"); }
    let a: Vec<String> = std::env::args().collect();
    if a.get(1).map(|s| s.as_str()) == Some("probe") { return probe(&a[2..]); }
    let addr = web::listen_addr(store::on_lazycat(), std::env::var("EXTERIOR_ADDR").ok().as_deref(), std::env::var("PORT").ok().as_deref());
    let _ = std::fs::create_dir_all(store::var());
    daemon::start();
    #[cfg(unix)]
    term::watch();
    web::serve(&addr);
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
            agents::Ev::Step { title, body, .. } => println!("step: {title} ({} 字)", body.chars().count()),
            agents::Ev::Text { delta, .. } => println!("text: {delta:?}"),
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
        println!("/{name} {arg} ({}): {:?}", k.label(), r.as_ref().map(|v| v.to_string().chars().take(3000).collect::<String>()));
        if *name == "fork" {
            if let Some(e) = r.ok().and_then(|v| v["messages"][0]["entryId"].as_str().map(String::from)) {
                println!("/pi_fork {e}: {:?}", c.builtin("fork", &e).map(|v| v.to_string()));
            }
        }
    }
    if kind == agents::Kind::Agy {
        let dev = serde_json::json!({"name": "local", "host": "local", "root": a[2]});
        let acp = agents::commands(&dev, &a[2], &a[1], kind);
        let cli = agents::agy_cli_commands(&dev, &a[2], &a[1]);
        println!("acp commands: {}", acp.iter().map(|c| c.0.as_str()).collect::<Vec<_>>().join(" "));
        println!("cli commands: {}", cli.iter().map(|c| c.0.as_str()).collect::<Vec<_>>().join(" "));
        // a long turn, stopped as soon as it starts answering
        ag.say("Write a 600-word essay about rivers.").expect("say");
        for ev in rx.iter() {
            match ev {
                agents::Ev::Text { .. } => { println!("text came: stopping"); ag.stop(); }
                agents::Ev::Done { stopped, error, .. } => { println!("done: stopped={stopped} error={error:?}"); break; }
                agents::Ev::Exited { why, .. } => { println!("exited: {why}"); break; }
                _ => {}
            }
        }
    }
    ag.kill();
}

/// SIGTERM/SIGINT: send what is still on its way (as long as the stop allows), then exit.
#[cfg(unix)]
mod term {
    use std::sync::atomic::{AtomicBool, Ordering};
    static TERM: AtomicBool = AtomicBool::new(false);
    extern "C" fn on_term(_: i32) { TERM.store(true, Ordering::SeqCst); }
    extern "C" { fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize; }
    pub fn watch() {
        unsafe { signal(15, on_term); signal(2, on_term); }
        std::thread::spawn(|| {
            while !TERM.load(Ordering::SeqCst) { std::thread::sleep(std::time::Duration::from_millis(200)); }
            crate::store::log("stop signal: sending what is left");
            let left = crate::daemon::drain(std::time::Duration::from_secs(8));
            crate::store::log(&format!("stopping ({left} topics still had output)"));
            std::process::exit(0);
        });
    }
}
