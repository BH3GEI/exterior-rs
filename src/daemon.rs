//! `exterior run`: every Telegram topic Yao creates is a session. Messages in → agent; agent's reply → the topic.
use crate::agents::{self, Agent, Ev, Kind, Spec};
use crate::note::state;
use crate::{brief, host, memory, note};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct Session {
    pub tid: String, // "tgbot:N"
    pub name: String,
    pub dir: String,
    pub kind: Kind,
    pub agent: Option<Agent>,
    pub gen: u64,
    pub pending: bool, // a message was handed over and no answer came back yet
    pub deleted: bool,
    pub active: Instant, // last message in or answer out
}

const IDLE: Duration = Duration::from_secs(2 * 3600);

type Sessions = Arc<Mutex<HashMap<String, Session>>>;
type Own = Arc<Mutex<HashSet<u64>>>;

pub fn log(s: &str) {
    let t = std::process::Command::new("date").arg("+%m-%d %H:%M:%S").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    eprintln!("{t} {s}");
}

/// Post into a topic; remember the id so the message is not taken as input later.
fn send(own: &Own, sessions: &Sessions, chat: &str, text: &str) -> Option<u64> {
    match brief::post("/api/send", json!({"chat": chat, "text": brief::clip(text)})) {
        Ok(v) => { let id = v["id"].as_u64(); if let Some(i) = id { own.lock().unwrap().insert(i); } id }
        Err((410, _)) => { gone(sessions, chat); None }
        Err((c, v)) => { log(&format!("send {chat} failed {c} {v}")); None }
    }
}

fn edit(sessions: &Sessions, chat: &str, id: u64, text: &str) {
    if let Err((c, v)) = brief::post("/api/edit", json!({"chat": chat, "id": id, "text": brief::clip(text)})) {
        if c == 410 { gone(sessions, chat); } else if c != 0 { log(&format!("edit {chat} failed {c} {v}")); }
    }
}

/// The topic was deleted in Telegram: close the session, delete nothing.
fn gone(sessions: &Sessions, chat: &str) {
    if let Some(s) = sessions.lock().unwrap().get_mut(chat) {
        if !s.deleted { log(&format!("topic deleted: {} ({chat}); session closed", s.name)); }
        s.deleted = true;
        if let Some(a) = s.agent.as_mut() { a.kill(); }
        s.agent = None;
        note::set(&s.tid, "deleted", json!(true));
    }
}

fn safe(name: &str) -> String {
    let s: String = name.trim().chars().map(|c| if matches!(c, '/' | ':' | '\\' | '\0') { '_' } else { c }).collect();
    let s = s.trim_matches('.').to_string();
    if s.is_empty() { "话题".into() } else { s }
}

/// Where a topic's session works on the agents' machine: what its note says; else exterior/<title>/ ; 「全局」 is exterior/ itself.
fn dir_for(root: &str, tid: &str, name: &str) -> String {
    if let Some(d) = note::read(tid)["dir"].as_str() { return d.to_string(); }
    if name.trim() == "全局" { return root.to_string(); }
    let taken: Vec<String> = note::all().iter().filter(|n| n["topic"] != tid).filter_map(|n| n["dir"].as_str().map(String::from)).collect();
    let mut d = format!("{root}/{}", safe(name));
    if taken.contains(&d) { d = format!("{d}-{}", tid.trim_start_matches("tgbot:")); }
    d
}

fn ensure_dir(root: &str, s: &Session) {
    note::set(&s.tid, "topic", json!(s.tid));
    note::set(&s.tid, "name", json!(s.name));
    note::set(&s.tid, "dir", json!(s.dir));
    host::mkdir(&s.dir);
    if s.dir != root {
        let body = format!("# {}\n\n这个目录是 Telegram 话题「{}」（{}）对应的会话。和这件事有关的文件、记忆（memory/）都放在这里。\n", s.name, s.name, s.tid);
        for f in ["CLAUDE.md", "AGENTS.md"] { host::write_new(&format!("{}/{f}", s.dir), &body); }
    }
}

/// Bring the session list in line with brief's topics (new ones, renamed ones, deleted ones).
fn refresh(root: &str, sessions: &Sessions, extra: &[String]) {
    let Some(list) = brief::get("/api/bots") else { return };
    let mut ss = sessions.lock().unwrap();
    for t in list.as_array().into_iter().flatten() {
        let chat = t["chat"].as_str().unwrap_or("").to_string();
        let mine = t["mine"] == true && (t["user_created"] == true || extra.contains(&chat));
        if !mine { continue; }
        let name = t["name"].as_str().unwrap_or("").trim_start_matches("✓ ").to_string();
        if t["deleted"] == true {
            if let Some(s) = ss.get_mut(&chat) { if let Some(a) = s.agent.as_mut() { a.kill(); } s.agent = None; s.deleted = true; }
            continue;
        }
        match ss.get_mut(&chat) {
            Some(s) => {
                if s.name != name { log(&format!("renamed {} -> {name}", s.name)); s.name = name; note::set(&s.tid, "name", json!(s.name)); }
            }
            None => {
                let dir = dir_for(root, &chat, &name);
                let n = note::read(&chat);
                let kind = n["agent"].as_str().and_then(Kind::parse).unwrap_or(Kind::Claude);
                let s = Session { tid: chat.clone(), name: name.clone(), dir, kind, agent: None, gen: 0, pending: false, deleted: false, active: Instant::now() };
                ensure_dir(root, &s);
                log(&format!("session {name} ({chat}) -> {} [{}]", s.dir, kind.name()));
                ss.insert(chat, s);
            }
        }
    }
}

fn start_agent(root: &str, s: &mut Session, tx: &Sender<Ev>) -> Result<(), String> {
    s.gen += 1;
    let n = note::read(&s.tid);
    let cfg_key = match s.kind { Kind::Claude => "claude_config", Kind::Codex => "codex_home", Kind::Pi => "" };
    let spec = Spec {
        tid: s.tid.clone(), name: s.name.clone(), dir: s.dir.clone(), root: root.to_string(), gen: s.gen,
        resume: n[s.kind.note_key()].as_str().map(String::from),
        config_dir: if cfg_key.is_empty() { None } else { n[cfg_key].as_str().map(String::from) },
    };
    log(&format!("start {} for {} (resume {:?})", s.kind.name(), s.name, spec.resume));
    s.agent = Some(Agent::start(s.kind, spec, tx.clone())?);
    note::set(&s.tid, "agent", json!(s.kind.name()));
    Ok(())
}

/// The "/" menu: /agent plus claude's and pi's own commands, prefixed cc_ / pi_.
fn build_menu(root: &str) {
    let mut cmds = vec![
        json!({"command": "stop", "description": "停下这一轮（Esc）"}),
        json!({"command": "exit", "description": "关掉这个话题的 agent 进程；下次说话自动接上"}),
        json!({"command": "new", "description": "这个话题从全新的会话开始"}),
        json!({"command": "agent", "description": "切换 agent：/agent claude | pi | codex"}),
    ];
    let mut map = serde_json::Map::new();
    let mut used = HashSet::new();
    for c in ["stop", "exit", "new", "agent"] { used.insert(c.to_string()); }
    for (kind, list) in [(Kind::Claude, agents::claude_commands(root)), (Kind::Pi, agents::pi_commands(root))] {
        for (orig, desc) in list {
            if cmds.len() >= 100 { break; }
            let mut tg: String = format!("{}_{}", kind.prefix(), orig.to_lowercase())
                .chars().map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' { c } else { '_' }).collect();
            tg.truncate(32);
            if !used.insert(tg.clone()) { continue; }
            let d = if desc.trim().is_empty() { format!("/{orig}") } else { desc.trim().replace('\n', " ") };
            let full = format!("{} · {d}", kind.name());
            let d: String = if full.chars().count() > 60 { format!("{}…", full.chars().take(59).collect::<String>()) } else { full };
            cmds.push(json!({"command": tg, "description": d}));
            map.insert(tg, json!([kind.name(), orig]));
        }
    }
    match brief::post("/api/bots/commands", json!({"commands": cmds})) {
        Ok(v) => log(&format!("menu set: {} commands", v["count"])),
        Err((c, v)) => log(&format!("menu failed {c} {v}")),
    }
    state::set("menu", Value::Object(map));
}

/// What to hand to the agent for one incoming text, or a direct answer.
enum Route { Say(String), Stop, Exit, New, Switch(Option<Kind>), Reply(String) }

fn route(text: &str, kind: Kind) -> Route {
    let t = text.trim();
    let head = t.split_whitespace().next().unwrap_or("");
    match head { "/stop" => return Route::Stop, "/exit" => return Route::Exit, "/new" => return Route::New, _ => {} }
    if t == "/agent" { return Route::Switch(None); }
    if let Some(rest) = t.strip_prefix("/agent ") {
        return match Kind::parse(rest) { Some(k) => Route::Switch(Some(k)), None => Route::Reply("只有 claude、pi、codex 三种：/agent claude".into()) };
    }
    if let Some(cmd) = t.strip_prefix('/') {
        let (head, args) = cmd.split_once(char::is_whitespace).unwrap_or((cmd, ""));
        let menu = state::get("menu");
        if let Some(e) = menu.get(head) {
            let (k, orig) = (e[0].as_str().and_then(Kind::parse), e[1].as_str().unwrap_or(""));
            if k != Some(kind) {
                return Route::Reply(format!("这是 {} 的命令，这个话题现在是 {}。先 /agent {}", e[0].as_str().unwrap_or(""), kind.name(), e[0].as_str().unwrap_or("")));
            }
            return Route::Say(format!("/{orig}{}{args}", if args.is_empty() { "" } else { " " }));
        }
    }
    Route::Say(text.to_string())
}

/// Agent output → Telegram: one message per answer, edited as it grows.
fn output(rx: Receiver<Ev>, sessions: Sessions, own: Own, root: String) {
    struct Out { gen: u64, buf: String, id: Option<u64>, shown: Instant }
    let mut outs: HashMap<String, Out> = HashMap::new();
    loop {
        let ev = match rx.recv_timeout(Duration::from_millis(500)) { Ok(e) => Some(e), Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None, Err(_) => return };
        // flush growing answers every ~1.5s
        for (tid, o) in outs.iter_mut() {
            if !o.buf.trim().is_empty() && o.shown.elapsed() > Duration::from_millis(1500) {
                match o.id { None => o.id = send(&own, &sessions, tid, &o.buf), Some(i) => edit(&sessions, tid, i, &o.buf) }
                o.shown = Instant::now();
            }
        }
        let Some(ev) = ev else { continue };
        match ev {
            Ev::Text { tid, gen, delta } => {
                let o = outs.entry(tid).or_insert(Out { gen, buf: String::new(), id: None, shown: Instant::now() });
                if gen > o.gen { *o = Out { gen, buf: String::new(), id: None, shown: Instant::now() }; }
                if gen == o.gen { o.buf.push_str(&delta); }
            }
            Ev::Done { tid, gen, text, stopped, error } => {
                let o = outs.remove(&tid).filter(|o| o.gen == gen).unwrap_or(Out { gen, buf: String::new(), id: None, shown: Instant::now() });
                let mut body = if stopped { o.buf.clone() } else { text.filter(|t| !t.trim().is_empty()).unwrap_or(o.buf.clone()) };
                if stopped { body.push_str("\n\n（已停）"); }
                if let Some(e) = error { body = format!("{body}\n\n⚠️ {e}").trim().to_string(); }
                if body.trim().is_empty() { body = "✓ 完成（没有文字输出）".into(); }
                match o.id { None => { send(&own, &sessions, &tid, &body); } Some(i) => edit(&sessions, &tid, i, &body) }
                let name = {
                    let mut ss = sessions.lock().unwrap();
                    ss.get_mut(&tid).map(|s| { if s.gen == gen { s.pending = false; } s.active = Instant::now(); s.name.clone() }).unwrap_or_default()
                };
                memory::push(&root, &name);
            }
            Ev::Session { tid, kind, id, config_dir } => {
                if note::read(&tid)[kind.note_key()].as_str() != Some(&id) { note::set(&tid, kind.note_key(), json!(id)); }
                if let Some(c) = config_dir {
                    let k = if kind == Kind::Claude { "claude_config" } else { "codex_home" };
                    note::set(&tid, k, json!(c));
                }
            }
            Ev::Exited { tid, gen } => {
                let warn = {
                    let mut ss = sessions.lock().unwrap();
                    match ss.get_mut(&tid) {
                        Some(s) if s.gen == gen => { s.agent = None; let w = s.pending && !s.deleted; s.pending = false; w }
                        _ => false,
                    }
                };
                if let Some(o) = outs.remove(&tid) {
                    if !o.buf.trim().is_empty() { let b = format!("{}\n\n（中断）", o.buf); match o.id { None => { send(&own, &sessions, &tid, &b); } Some(i) => edit(&sessions, &tid, i, &b) } }
                }
                if warn {
                    log(&format!("agent exited mid-turn in {tid}"));
                    send(&own, &sessions, &tid, "⚠️ 会话意外退出了。下一条消息会接着原来的会话继续；刚才那条如果没做完，请再发一次。");
                }
            }
        }
    }
}

pub fn run(root: &str) {
    let extra: Vec<String> = std::env::var("EXTERIOR_EXTRA").unwrap_or_default().split(',').filter(|s| !s.is_empty()).map(String::from).collect();
    log(&format!("exterior starting, root {root}, extra {extra:?}"));
    loop {
        match brief::post("/api/bots/adopt", json!({})) {
            Ok(_) => break,
            Err((c, v)) => { log(&format!("adopt failed {c} {v}; retry in 30s")); std::thread::sleep(Duration::from_secs(30)); }
        }
    }
    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    let own: Own = Arc::new(Mutex::new(HashSet::new()));
    let (tx, rx) = channel();
    { let (s, o, r) = (sessions.clone(), own.clone(), root.to_string()); std::thread::spawn(move || output(rx, s, o, r)); }
    { let r = root.to_string(); std::thread::spawn(move || build_menu(&r)); }
    refresh(root, &sessions, &extra);
    let mut last = state::get("last_id").as_u64().unwrap_or(0);
    if last == 0 {
        last = brief::get("/api/messages?source=telegram&limit=1").and_then(|v| v.as_array().and_then(|a| a.iter().filter_map(|m| m["id"].as_u64()).max())).unwrap_or(0);
        state::set("last_id", json!(last));
    }
    let mut refreshed = Instant::now();
    loop {
        if refreshed.elapsed() > Duration::from_secs(60) {
            refresh(root, &sessions, &extra);
            refreshed = Instant::now();
            // idle sessions give their process back; the next message resumes them
            for s in sessions.lock().unwrap().values_mut() {
                if s.agent.is_some() && !s.pending && s.active.elapsed() > IDLE {
                    log(&format!("idle, closing process of {}", s.name));
                    if let Some(a) = s.agent.as_mut() { a.kill(); }
                    s.agent = None; s.gen += 1;
                }
            }
        }
        let Some(msgs) = brief::get(&format!("/api/wait?source=telegram&all=1&after_id={last}&timeout=20&limit=200")) else {
            std::thread::sleep(Duration::from_secs(5)); continue;
        };
        for m in msgs.as_array().into_iter().flatten() {
            let id = m["id"].as_u64().unwrap_or(0);
            last = last.max(id);
            let chat = m["chat"].as_str().unwrap_or("").to_string();
            if !chat.starts_with("tgbot:") { continue; }
            if !sessions.lock().unwrap().contains_key(&chat) { refresh(root, &sessions, &extra); refreshed = Instant::now(); }
            let text = m["text"].as_str().unwrap_or("").replace("@exterior_bot", "");
            let from_user = m["sender"] == "我";
            {
                let ss = sessions.lock().unwrap();
                let Some(s) = ss.get(&chat) else { continue };
                if s.deleted { continue; }
                if !from_user {
                    // only messages another session sent here (「名字 → …」) count; our own replies never do
                    if own.lock().unwrap().contains(&id) { continue; }
                    let Some((who, _)) = text.split_once(" → ") else { continue };
                    if who.trim().is_empty() || who.trim() == s.name || who.contains('\n') { continue; }
                }
            }
            if from_user {
                let c = chat.clone();
                std::thread::spawn(move || { let _ = brief::post("/api/react", json!({"chat": c, "id": id, "emoji": "👀"})); });
            }
            handle(root, &sessions, &own, &tx, &chat, &text);
        }
        state::set("last_id", json!(last));
    }
}

fn handle(root: &str, sessions: &Sessions, own: &Own, tx: &Sender<Ev>, chat: &str, text: &str) {
    let mut reply: Option<String> = None;
    {
        let mut ss = sessions.lock().unwrap();
        let Some(s) = ss.get_mut(chat) else { return };
        s.active = Instant::now();
        log(&format!("<- {}: {}", s.name, text.chars().take(80).collect::<String>()));
        match route(text, s.kind) {
            Route::Reply(r) => reply = Some(r),
            Route::Switch(None) => reply = Some(format!("这个话题现在是 {}。换：/agent claude | pi | codex", s.kind.name())),
            Route::Switch(Some(k)) => {
                if let Some(a) = s.agent.as_mut() { a.kill(); }
                s.agent = None;
                s.gen += 1;
                s.kind = k;
                s.pending = false;
                note::set(&s.tid, "agent", json!(k.name()));
                reply = Some(format!("已切换到 {}。它有自己的记录，和之前的 {} 不共用上下文；记忆是共用的。", k.name(), k.name()));
            }
            Route::Exit => {
                if let Some(a) = s.agent.as_mut() { a.kill(); }
                s.agent = None; s.gen += 1; s.pending = false;
                reply = Some("已关掉进程。下次在这里说话，会接着原来的会话。".into());
            }
            Route::New => {
                if let Some(a) = s.agent.as_mut() { a.kill(); }
                s.agent = None; s.gen += 1; s.pending = false;
                note::set(&s.tid, s.kind.note_key(), Value::Null);
                reply = Some("好，下一条消息从全新的会话开始（旧记录还在，没删）。".into());
            }
            Route::Stop => {
                if let Some(a) = s.agent.as_mut() { a.stop(); } else { reply = Some("现在没有在运行的任务。".into()); }
            }
            Route::Say(t) => {
                if s.agent.is_none() {
                    if let Err(e) = start_agent(root, s, tx) { reply = Some(format!("⚠️ 启动 {} 失败：{e}", s.kind.name())); }
                }
                if let Some(a) = s.agent.as_mut() {
                    memory::pull(root);
                    s.pending = true;
                    if let Err(e) = a.say(&t) {
                        log(&format!("say failed ({e}); restarting {}", s.name));
                        a.kill();
                        s.agent = None;
                        match start_agent(root, s, tx) {
                            Ok(()) => { if let Some(a) = s.agent.as_mut() { let _ = a.say(&t); } }
                            Err(e) => { s.pending = false; reply = Some(format!("⚠️ 启动 {} 失败：{e}", s.kind.name())); }
                        }
                    }
                }
            }
        }
    }
    if let Some(r) = reply { send(own, sessions, chat, &r); }
}
