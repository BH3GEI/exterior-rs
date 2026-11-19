//! Every Telegram topic the user creates is a session. Messages in → the topic's agent; the agent's answer → the topic.
use crate::agents::{self, Agent, Ev, Kind, Spec};
use crate::text::{self, status as st};
use crate::{brief, params, store};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct Session {
    pub tid: String,
    pub name: String,
    pub dev: String,
    pub dir: String,
    pub agent: Option<Agent>,
    pub gen: u64,
    pub pending: bool,
    pub deleted: bool,
    pub active: Instant,
}

/// Telegram's limit on a button's data.
const BUTTON_DATA_MAX: usize = 64;
/// Telegram's limits on the command menu.
const MENU_MAX: usize = 100;
const COMMAND_NAME_MAX: usize = 32;
const COMMAND_DESC_MAX: usize = 60;

type Sessions = Arc<Mutex<HashMap<String, Session>>>;
type Own = Arc<Mutex<HashSet<u64>>>;

/// The last model list shown in each topic, for buttons whose model name is too long to fit (they say "/model 3").
static CHOICES: Mutex<Option<HashMap<String, Vec<String>>>> = Mutex::new(None);

/// What the 会话 page shows about the running side (the rest is in the notes).
pub static LIVE: Mutex<Option<Sessions>> = Mutex::new(None);

pub fn running(tid: &str) -> Option<(bool, bool)> {
    let g = LIVE.lock().unwrap();
    let ss = g.as_ref()?.lock().unwrap();
    ss.get(tid).map(|s| (s.agent.is_some(), s.pending))
}

fn set_status(tid: &str, status: &str) { store::note_set(tid, "status", json!(status)); store::note_set(tid, "active", json!(store::now())); }

/// "名称 → 正文": a message from another session.
fn relay_split(t: &str) -> Option<(&str, &str)> {
    let mark = params::get("relay_mark");
    if mark.is_empty() { return None; }
    t.split_once(&format!(" {mark} "))
}

fn send(own: &Own, sessions: &Sessions, chat: &str, text: &str) -> Option<u64> { send_with(own, sessions, chat, text, None) }

/// Buttons: rows of (label, what pressing it says).
fn buttons(rows: Vec<Vec<(String, String)>>) -> Value {
    Value::Array(rows.into_iter().map(|r| Value::Array(r.into_iter().map(|(t, d)| json!({"text": t, "data": d})).collect())).collect())
}

fn in_rows(items: Vec<(String, String)>, per: usize) -> Vec<Vec<(String, String)>> { items.chunks(per).map(|c| c.to_vec()).collect() }

fn send_with(own: &Own, sessions: &Sessions, chat: &str, text: &str, btns: Option<Value>) -> Option<u64> {
    for _ in 0..params::num("send_retries").max(1) {
        let mut body = json!({"chat": chat, "text": brief::clip(text)});
        if let Some(b) = &btns { body["buttons"] = b.clone(); }
        match brief::post("/api/send", body) {
            Ok(v) => { let id = v["id"].as_u64(); if let Some(i) = id { own.lock().unwrap().insert(i); } return id; }
            Err((410, _)) => { gone(sessions, chat); return None; }
            Err((c, v)) if c == 0 || c >= 500 => { store::log(&format!("send {chat}: {c} {v}; retry")); std::thread::sleep(params::secs("retry_wait_s")); }
            Err((c, v)) => { store::log(&format!("send {chat} failed {c} {v}")); return None; }
        }
    }
    None
}

fn edit(sessions: &Sessions, chat: &str, id: u64, text: &str) {
    if let Err((c, v)) = brief::post("/api/edit", json!({"chat": chat, "id": id, "text": brief::clip(text)})) {
        if c == 410 { gone(sessions, chat); } else if c != 0 && !v.to_string().contains("not modified") { store::log(&format!("edit {chat} failed {c} {v}")); }
    }
}

/// The topic was deleted in Telegram: close the session, delete nothing.
fn gone(sessions: &Sessions, chat: &str) {
    if let Some(s) = sessions.lock().unwrap().get_mut(chat) {
        if !s.deleted { store::log(&format!("topic deleted: {} ({chat}); session closed", s.name)); }
        s.deleted = true;
        if let Some(a) = s.agent.as_mut() { a.kill(); }
        s.agent = None;
        store::note_set(&s.tid, "deleted", json!(true));
        store::note_set(&s.tid, "status", json!(st::DELETED));
    }
}

fn safe(name: &str) -> String {
    let s: String = name.trim().chars().map(|c| if matches!(c, '/' | ':' | '\\' | '\0') { '_' } else { c }).collect();
    let s = s.trim_matches('.').to_string();
    if s.is_empty() { text::UNNAMED_TOPIC.into() } else { s }
}

/// The folder of a topic on its device: what its note says; else <root>/<title>; the global topic has its own folder.
fn dir_for(dev: &str, tid: &str, name: &str) -> String {
    if let Some(d) = store::note(tid)["dir"].as_str() { return d.to_string(); }
    let Some(device) = store::device(dev) else { return String::new() };
    if name.trim() == params::get("global_topic") { return crate::prompt::global_dir(&device); }
    let root = crate::prompt::root(&device);
    let taken: Vec<String> = store::notes().iter().filter(|n| n["topic"] != tid).filter_map(|n| n["dir"].as_str().map(String::from)).collect();
    let mut d = format!("{root}/{}", safe(name));
    let reserved = [params::get("global_dir"), params::get("knowledge_dir")];
    if taken.contains(&d) || reserved.iter().any(|r| r == name.trim()) { d = format!("{d}-{}", brief::topic_id(tid)); }
    d
}

/// Bring the session list in line with brief's topics (new, renamed, deleted).
fn refresh(sessions: &Sessions) {
    let Some(list) = brief::get("/api/bots") else { return };
    let Some(defdev) = store::default_device() else { return };
    let mut ss = sessions.lock().unwrap();
    for t in list.as_array().into_iter().flatten() {
        let chat = t["chat"].as_str().unwrap_or("").to_string();
        if !(t["mine"] == true && t["user_created"] == true) { continue; }
        let name = t["name"].as_str().unwrap_or("").trim_start_matches(brief::DONE_MARK).to_string();
        if t["deleted"] == true {
            if let Some(s) = ss.get_mut(&chat) { if let Some(a) = s.agent.as_mut() { a.kill(); } s.agent = None; s.deleted = true; }
            if store::note(&chat)["topic"].is_string() { store::note_set(&chat, "deleted", json!(true)); store::note_set(&chat, "status", json!(st::DELETED)); }
            continue;
        }
        if let Some(l) = t["link"].as_str() { if store::note(&chat)["link"].as_str() != Some(l) && store::note(&chat)["topic"].is_string() { store::note_set(&chat, "link", json!(l)); } }
        match ss.get_mut(&chat) {
            Some(s) => if s.name != name { store::log(&format!("renamed {} -> {name}", s.name)); s.name = name.clone(); store::note_set(&chat, "name", json!(name)); },
            None => {
                let n = store::note(&chat);
                let dev = n["device"].as_str().map(String::from).filter(|d| store::device(d).is_some())
                    .unwrap_or_else(|| defdev["name"].as_str().unwrap_or("").to_string());
                let dir = dir_for(&dev, &chat, &name);
                if n["topic"].is_null() {
                    store::note_set(&chat, "topic", json!(chat));
                    store::note_set(&chat, "created", json!(store::now()));
                    store::note_set(&chat, "status", json!(st::NEW));
                    store::log(&format!("new session {name} ({chat}) -> {dir}"));
                }
                store::note_set(&chat, "name", json!(name));
                store::note_set(&chat, "device", json!(dev));
                store::note_set(&chat, "dir", json!(dir));
                if let Some(l) = t["link"].as_str() { store::note_set(&chat, "link", json!(l)); }
                if store::note(&chat)["agent"].is_null() {
                    if let Some(first) = store::device(&dev).and_then(|d| d["agents"][0]["name"].as_str().map(String::from)) { store::note_set(&chat, "agent", json!(first)); }
                }
                ss.insert(chat.clone(), Session { tid: chat, name, dev, dir, agent: None, gen: 0, pending: false, deleted: false, active: Instant::now() });
            }
        }
    }
}

/// Start the topic's current agent: resume its session if it has one.
fn start_agent(s: &mut Session, tx: &Sender<Ev>) -> Result<(), String> {
    let dev = store::device(&s.dev).ok_or_else(|| text::device_missing(&s.dev))?;
    let n = store::note(&s.tid);
    let name = n["agent"].as_str().unwrap_or("").to_string();
    let row = store::agent(&dev, &name).ok_or_else(|| text::agent_missing(&s.dev, &name))?;
    let kind = Kind::parse(row["kind"].as_str().unwrap_or("")).ok_or(text::BAD_KIND)?;
    let resume = n["session"].as_str().map(String::from);
    // cc0 picked cc6 last time: the session lives in cc6, so resume through cc6
    let run_row = if resume.is_some() { n["resolved"].as_str().and_then(|r| store::agent(&dev, r)).unwrap_or(row.clone()) } else { row.clone() };
    s.gen += 1;
    let global = s.dir == crate::prompt::global_dir(&dev);
    let prompt = crate::prompt::session_text(&dev, &s.name, &s.tid, &s.dir, global)?;
    let spec = Spec { tid: s.tid.clone(), prompt, topic: s.name.clone(), dev: dev.clone(), dir: s.dir.clone(),
                      command: run_row["command"].as_str().unwrap_or("").to_string(), kind, gen: s.gen, resume: resume.clone() };
    store::log(&format!("start {} ({}) for {} in {} resume {:?}", run_row["name"], spec.command, s.name, s.dir, resume));
    s.agent = Some(Agent::start(spec, tx.clone())?);
    set_status(&s.tid, st::RUNNING);
    Ok(())
}

/// The "/" menu: exterior's commands, the built-ins, then each agent's own commands with its kind's prefix.
pub fn build_menu() -> Result<usize, String> {
    let dev = store::default_device().ok_or(text::NO_DEVICE)?;
    let mut cmds = vec![
        json!({"command": "stop", "description": text::MENU_STOP}),
        json!({"command": "exit", "description": text::MENU_EXIT}),
        json!({"command": "agent", "description": text::MENU_AGENT}),
        json!({"command": "model", "description": text::MENU_MODEL}),
    ];
    for (k, n) in agents::BUILTINS {
        if dev["agents"].as_array().into_iter().flatten().any(|a| a["kind"] == k.name()) {
            cmds.push(json!({"command": agents::builtin_command(*k, n), "description": text::builtin_desc(k.label(), n)}));
        }
    }
    let mut map = serde_json::Map::new();
    let mut used: HashSet<String> = cmds.iter().filter_map(|c| c["command"].as_str().map(String::from)).collect();
    let root = crate::prompt::root(&dev);
    for kind in Kind::ALL {
        let Some(row) = dev["agents"].as_array().into_iter().flatten().find(|a| a["kind"] == kind.name()) else { continue };
        for (orig, desc) in agents::commands(&dev, &root, row["command"].as_str().unwrap_or(""), kind) {
            if cmds.len() >= MENU_MAX { break; }
            let mut tg: String = format!("{}_{}", kind.prefix(), orig.to_lowercase()).chars().map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' { c } else { '_' }).collect();
            tg.truncate(COMMAND_NAME_MAX);
            if !used.insert(tg.clone()) { continue; }
            let d = if desc.trim().is_empty() { format!("/{orig}") } else { desc.trim().replace('\n', " ") };
            let full = format!("{} · {d}", kind.label());
            let d: String = if full.chars().count() > COMMAND_DESC_MAX { format!("{}…", full.chars().take(COMMAND_DESC_MAX - 1).collect::<String>()) } else { full };
            cmds.push(json!({"command": tg, "description": d}));
            map.insert(tg, json!([kind.name(), orig]));
        }
    }
    if map.is_empty() { return Err(text::NO_COMMANDS.into()); }
    let n = cmds.len();
    brief::post("/api/bots/commands", json!({"commands": cmds})).map_err(|(c, v)| format!("{c} {v}"))?;
    store::set_state("menu", Value::Object(map));
    store::set_state("menu_time", json!(store::now()));
    Ok(n)
}

/// Agent output → Telegram: one message per answer, edited as it grows.
fn output(rx: Receiver<Ev>, sessions: Sessions, own: Own) {
    struct Out { gen: u64, buf: String, id: Option<u64>, shown: Instant }
    let mut outs: HashMap<String, Out> = HashMap::new();
    loop {
        let ev = match rx.recv_timeout(Duration::from_millis(500)) { Ok(e) => Some(e), Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None, Err(_) => return };
        let flush = Duration::from_millis(params::num("flush_ms"));
        for (tid, o) in outs.iter_mut() {
            if !o.buf.trim().is_empty() && o.shown.elapsed() > flush {
                match o.id { None => o.id = send(&own, &sessions, tid, &o.buf), Some(i) => edit(&sessions, tid, i, &o.buf) }
                o.shown = Instant::now();
            }
        }
        let Some(ev) = ev else { continue };
        let current = |tid: &str, gen: u64| sessions.lock().unwrap().get(tid).map(|s| s.gen == gen).unwrap_or(false);
        match ev {
            Ev::Text { tid, gen, delta } => {
                let o = outs.entry(tid).or_insert(Out { gen, buf: String::new(), id: None, shown: Instant::now() });
                if gen > o.gen { *o = Out { gen, buf: String::new(), id: None, shown: Instant::now() }; }
                if gen == o.gen { o.buf.push_str(&delta); }
            }
            Ev::Done { tid, gen, text: answer, stopped, error } => {
                let o = outs.remove(&tid).filter(|o| o.gen == gen).unwrap_or(Out { gen, buf: String::new(), id: None, shown: Instant::now() });
                let mut body = if stopped { o.buf.clone() } else { answer.filter(|t| !t.trim().is_empty()).unwrap_or(o.buf.clone()) };
                if stopped { body = format!("{body}\n\n{}", text::STOPPED_SUFFIX).trim().to_string(); }
                if let Some(e) = &error { body = format!("{body}\n\n{}", text::failed(e)).trim().to_string(); }
                if body.trim().is_empty() { body = text::DONE_EMPTY.into(); }
                match o.id { None => { send(&own, &sessions, &tid, &body); } Some(i) => edit(&sessions, &tid, i, &body) }
                let who = {
                    let mut ss = sessions.lock().unwrap();
                    ss.get_mut(&tid).map(|s| { if s.gen == gen { s.pending = false; } s.active = Instant::now(); (s.dev.clone(), s.name.clone()) })
                };
                if let Some((d, name)) = who {
                    if let Some(dev) = store::device(&d) { std::thread::spawn(move || crate::infra::commit(&dev, &name)); }
                }
                if current(&tid, gen) {
                    set_status(&tid, if error.is_some() { st::ERROR } else { st::IDLE });
                    store::note_set(&tid, "last_error", error.map(Value::String).unwrap_or(Value::Null));
                    store::note_set(&tid, "from", Value::Null);
                }
            }
            Ev::Session { tid, gen, id } => {
                if current(&tid, gen) && store::note(&tid)["session"].as_str() != Some(&id) { store::note_set(&tid, "session", json!(id)); }
            }
            Ev::Note { tid, gen, text: t } => {
                if current(&tid, gen) { send(&own, &sessions, &tid, &text::note(&t)); }
            }
            Ev::Resolved { tid, gen, name } => {
                if current(&tid, gen) { store::note_set(&tid, "resolved", json!(name)); }
            }
            Ev::Exited { tid, gen, why } => {
                let warn = {
                    let mut ss = sessions.lock().unwrap();
                    match ss.get_mut(&tid) {
                        Some(s) if s.gen == gen => { s.agent = None; let w = s.pending && !s.deleted; s.pending = false; w }
                        _ => false,
                    }
                };
                if let Some(o) = outs.remove(&tid) {
                    if !o.buf.trim().is_empty() { let b = format!("{}\n\n{}", o.buf, text::CUT_SUFFIX); match o.id { None => { send(&own, &sessions, &tid, &b); } Some(i) => edit(&sessions, &tid, i, &b) } }
                }
                if current(&tid, gen) || warn {
                    let why = agents::last_lines(&why);
                    if warn {
                        store::log(&format!("agent exited mid-turn in {tid}: {why}"));
                        set_status(&tid, st::ERROR);
                        store::note_set(&tid, "last_error", json!(why));
                        send(&own, &sessions, &tid, &text::exited(&why));
                    } else if store::note(&tid)["status"] != st::DELETED {
                        set_status(&tid, st::STOPPED);
                    }
                }
            }
        }
    }
}

pub fn start() {
    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    *LIVE.lock().unwrap() = Some(sessions.clone());
    std::thread::spawn(move || run(sessions));
}

fn run(sessions: Sessions) {
    loop {
        if brief::configured() && store::default_device().is_some() {
            match brief::post("/api/bots/adopt", json!({})) {
                Ok(_) => { store::set_state("telegram", json!({"ok": true, "time": store::now(), "msg": text::TG_ADOPTED})); break; }
                Err((c, v)) => store::set_state("telegram", json!({"ok": false, "time": store::now(), "msg": text::tg_adopt_failed(&format!("{c} {v}"))})),
            }
        } else {
            store::set_state("telegram", json!({"ok": false, "time": store::now(), "msg": text::TG_NOT_SET}));
        }
        std::thread::sleep(params::secs("reconnect_s"));
    }
    store::log("exterior started");
    for n in store::notes() {
        let tid = n["topic"].as_str().unwrap_or("").to_string();
        if tid.is_empty() { continue; }
        // notes written before 0.7 stored the status label itself
        if let Some(k) = n["status"].as_str().and_then(st::from_legacy) { store::note_set(&tid, "status", json!(k)); }
        // nothing runs right after a restart: a topic still marked running had its turn cut off
        if matches!(store::note(&tid)["status"].as_str(), Some(st::RUNNING) | Some(st::MOVING)) {
            store::note_set(&tid, "status", json!(st::ERROR));
            store::note_set(&tid, "last_error", json!(text::RESTART_CUT));
            store::log(&format!("{}: turn cut off by restart", n["name"]));
        }
    }
    let own: Own = Arc::new(Mutex::new(HashSet::new()));
    let (tx, rx) = channel();
    { let (s, o) = (sessions.clone(), own.clone()); std::thread::spawn(move || output(rx, s, o)); }
    std::thread::spawn(|| for _ in 0..5 {
        match build_menu() {
            Ok(n) => { store::log(&format!("menu: {n} commands")); break; }
            Err(e) => { store::log(&format!("menu failed: {e}; retrying")); std::thread::sleep(Duration::from_secs(params::num("menu_retry_min") * 60)); }
        }
    });
    std::thread::spawn(|| for d in store::devices() { if let Err(e) = crate::prompt::install(&d) { store::log(&format!("install brief key on {}: {e}", d["name"])); } });
    refresh(&sessions);
    let mut last = store::state("last_id").as_u64().unwrap_or(0);
    if last == 0 {
        last = brief::get("/api/messages?source=telegram&limit=1").and_then(|v| v.as_array().and_then(|a| a.iter().filter_map(|m| m["id"].as_u64()).max())).unwrap_or(0);
        store::set_state("last_id", json!(last));
    }
    let mut refreshed = Instant::now();
    loop {
        if refreshed.elapsed() > params::secs("refresh_s") {
            refresh(&sessions);
            refreshed = Instant::now();
            let idle = Duration::from_secs(params::num("idle_hours") * 3600);
            for s in sessions.lock().unwrap().values_mut() {
                if s.agent.is_some() && !s.pending && s.active.elapsed() > idle {
                    store::log(&format!("idle, closing process of {}", s.name));
                    if let Some(a) = s.agent.as_mut() { a.kill(); }
                    s.agent = None; s.gen += 1;
                    set_status(&s.tid, st::STOPPED);
                }
            }
        }
        let wait = format!("/api/wait?source=telegram&all=1&after_id={last}&timeout={}&limit=200", params::num("wait_s"));
        let msgs = match brief::call("GET", &wait, None) {
            Ok(v) => { store::set_state("telegram", json!({"ok": true, "time": store::now(), "msg": text::TG_CONNECTED})); v }
            Err((c, v)) => {
                store::set_state("telegram", json!({"ok": false, "time": store::now(), "msg": text::tg_unreachable(&format!("{c} {v}"))}));
                std::thread::sleep(params::secs("reconnect_s")); continue;
            }
        };
        for m in msgs.as_array().into_iter().flatten() {
            let id = m["id"].as_u64().unwrap_or(0);
            last = last.max(id);
            let chat = m["chat"].as_str().unwrap_or("").to_string();
            if !chat.starts_with(brief::TOPIC_PREFIX) { continue; }
            if !sessions.lock().unwrap().contains_key(&chat) { refresh(&sessions); refreshed = Instant::now(); }
            let mut text = strip_bot_mention(m["text"].as_str().unwrap_or(""));
            let from_user = brief::from_user(m);
            let to_name = {
                let ss = sessions.lock().unwrap();
                let Some(s) = ss.get(&chat) else { continue };
                if s.deleted { continue; }
                if !from_user {
                    if own.lock().unwrap().contains(&id) { continue; }
                    let Some((who, _)) = relay_split(&text) else { continue };
                    if who.trim().is_empty() || who.trim() == s.name || who.contains('\n') { continue; }
                }
                s.name.clone()
            };
            if from_user {
                let c = chat.clone();
                std::thread::spawn(move || { let _ = brief::post("/api/react", json!({"chat": c, "id": id, "emoji": params::get("ack_emoji")})); });
            } else if let Some((who, body)) = relay_split(&text) {
                store::append("messages.jsonl", &json!({"time": store::now(), "from": who.trim(), "to": to_name, "text": body.chars().take(500).collect::<String>()}));
            }
            if from_user && m["media"]["url"].is_string() {
                text = with_file(&sessions, &chat, id, &m["media"], &text);
            }
            handle(&sessions, &own, &tx, &chat, &text);
        }
        store::set_state("last_id", json!(last));
    }
}

/// "/model@some_bot 3" → "/model 3": in groups Telegram adds the bot's name to commands.
fn strip_bot_mention(t: &str) -> String {
    if !t.starts_with('/') { return t.to_string(); }
    let head = t.split_whitespace().next().unwrap_or("");
    match head.split_once('@') { Some((cmd, _)) => format!("{cmd}{}", &t[head.len()..]), None => t.to_string() }
}

/// The user sent a picture or a file: put it in the topic's folder and tell the agent where it is.
fn with_file(sessions: &Sessions, chat: &str, id: u64, media: &Value, msg: &str) -> String {
    let (dev, dir) = { let ss = sessions.lock().unwrap(); match ss.get(chat) { Some(s) => (s.dev.clone(), s.dir.clone()), None => return msg.to_string() } };
    let kind = text::file_kind(media["kind"] == "image");
    let name: String = media["name"].as_str().unwrap_or("file").chars().map(|c| if c == '/' || c == '\\' || c.is_control() { '_' } else { c }).collect();
    let path = format!("{dir}/files/{id}_{name}");
    let r = store::device(&dev).ok_or_else(|| text::device_missing(&dev))
        .and_then(|d| brief::download(media["url"].as_str().unwrap_or("")).and_then(|b| crate::host::write_bytes(&d, &path, &b)));
    let (base, user) = (msg.trim(), params::get("user_name"));
    let line = match r {
        Ok(()) => { store::log(&format!("file for {chat}: {path}")); text::file_saved(&user, kind, &path) }
        Err(e) => { store::log(&format!("file for {chat} failed: {e}")); text::file_failed(&user, kind, &e) }
    };
    format!("{base}\n\n{line}").trim().to_string()
}

fn agent_names(dev: &str) -> Vec<String> {
    store::device(dev).map(|d| d["agents"].as_array().into_iter().flatten().filter_map(|a| a["name"].as_str().map(String::from)).collect()).unwrap_or_default()
}

/// A command for exterior itself: /model, or a built-in of some kind of agent.
enum Control { Model, Builtin(Kind, &'static str) }

fn handle(sessions: &Sessions, own: &Own, tx: &Sender<Ev>, chat: &str, msg: &str) {
    let mut reply: Option<String> = None;
    let mut reply_buttons: Option<Value> = None;
    let mut transfer: Option<(String, String)> = None;
    let mut ctl: Option<(agents::Ctl, Control, String)> = None;
    {
        let mut ss = sessions.lock().unwrap();
        let Some(s) = ss.get_mut(chat) else { return };
        s.active = Instant::now();
        store::note_set(chat, "active", json!(store::now()));
        store::log(&format!("<- {}: {}", s.name, msg.chars().take(80).collect::<String>()));
        let t = msg.trim();
        let n = store::note(chat);
        let cur = n["agent"].as_str().unwrap_or("").to_string();
        let dev = store::device(&s.dev);
        let cur_kind = dev.as_ref().and_then(|d| store::agent(d, &cur)).and_then(|r| Kind::parse(r["kind"].as_str().unwrap_or("")));
        let mut say: Option<String> = None;
        let head = t.split_whitespace().next().unwrap_or("");
        let arg = t[head.len()..].trim().to_string();
        let control = if head == "/model" { Some(Control::Model) } else { agents::find_builtin(head).map(|(k, n)| Control::Builtin(k, n)) };
        let own_prefix = Kind::ALL.iter().find(|k| head.starts_with(&format!("/{}_", k.prefix()))).copied();
        if head == "/exit" {
            if let Some(a) = s.agent.as_mut() { a.stop(); a.kill(); }
            s.agent = None; s.gen += 1; s.pending = false;
            store::note_set(chat, "session", Value::Null);
            store::note_set(chat, "resolved", Value::Null);
            store::note_set(chat, "from", Value::Null);
            set_status(chat, st::NEW);
            reply = Some(text::session_ended(&cur));
        } else if let Some(c) = control {
            if let Control::Builtin(k, _) = &c {
                if Some(*k) != cur_kind { reply = Some(text::wrong_agent(k.label(), &cur)); }
            }
            if reply.is_none() {
                if s.agent.is_none() {
                    if let Err(e) = start_agent(s, tx) { reply = Some(text::start_failed(&cur, &e)); }
                }
                if let Some(a) = s.agent.as_ref() { ctl = Some((a.ctl(), c, arg.clone())); }
            }
        } else if head == "/stop" {
            match s.agent.as_mut() { Some(a) if s.pending => a.stop(), _ => reply = Some(text::NOTHING_RUNNING.into()) }
        } else if head == "/agent" && arg.is_empty() {
            reply = Some(text::current_agent(&cur));
            let names: Vec<(String, String)> = agent_names(&s.dev).into_iter().filter(|n| *n != cur).map(|n| (n.clone(), format!("/agent {n}"))).collect();
            reply_buttons = Some(buttons(in_rows(names, 4)));
        } else if head == "/agent" {
            let want = arg.clone();
            match dev.as_ref().and_then(|d| store::agent(d, &want)) {
                None => reply = Some(text::no_such_agent(&want, &agent_names(&s.dev).join("、"))),
                Some(_) if want == cur => reply = Some(text::already_agent(&cur)),
                Some(_) => {
                    let old_session = n["session"].as_str().map(String::from);
                    let prev = old_session.as_ref().and_then(|id| cur_kind.map(|k| agents::resume_line(k, n["resolved"].as_str().unwrap_or(&cur), id)));
                    if let Some(a) = s.agent.as_mut() { a.stop(); a.kill(); }
                    s.agent = None; s.gen += 1; s.pending = false;
                    store::note_set(chat, "agent", json!(want));
                    store::note_set(chat, "session", Value::Null);
                    store::note_set(chat, "resolved", Value::Null);
                    store::note_set(chat, "last_error", Value::Null);
                    store::log(&format!("{}: {cur} -> {want} (prev {prev:?})", s.name));
                    match prev {
                        None => { set_status(chat, st::NEW); reply = Some(text::switched_agent(&want)); }
                        Some(p) => { store::note_set(chat, "from", json!(p)); transfer = Some((p, want.clone())); }
                    }
                }
            }
        } else if let Some(kind) = own_prefix {
            // one of an agent's own commands, as listed in the menu
            let cmd = &head[1..];
            match store::state("menu").get(cmd).cloned() {
                None => reply = Some(text::no_such_command(cmd)),
                Some(e) => {
                    let orig = e[1].as_str().unwrap_or("");
                    if Some(kind) != cur_kind { reply = Some(text::wrong_agent(kind.label(), &cur)); }
                    else { say = Some(format!("/{orig}{}{arg}", if arg.is_empty() { "" } else { " " })); }
                }
            }
        } else {
            say = Some(msg.to_string());
        }
        if let Some((p, _)) = &transfer { say = Some(p.clone()); }
        if let Some(line) = say {
            if s.agent.is_none() {
                if let Err(e) = start_agent(s, tx) {
                    reply = Some(text::start_failed(store::note(chat)["agent"].as_str().unwrap_or(""), &e));
                    set_status(chat, st::ERROR); store::note_set(chat, "last_error", json!(e)); transfer = None;
                }
            }
            if let Some(a) = s.agent.as_mut() {
                s.pending = true;
                if let Err(e) = a.say(&line) { s.pending = false; reply = Some(text::say_failed(&e)); }
                else if transfer.is_some() { set_status(chat, st::MOVING); }
            }
        }
    }
    if let Some((_, want)) = &transfer { send(own, sessions, chat, &text::transferring(want)); }
    if let Some(r) = reply { send_with(own, sessions, chat, &r, reply_buttons); }
    if let Some((c, what, arg)) = ctl {
        let (own, sessions, chat) = (own.clone(), sessions.clone(), chat.to_string());
        std::thread::spawn(move || {
            let (t, btns) = control(&c, &chat, what, &arg);
            send_with(&own, &sessions, &chat, &t, btns);
        });
    }
}

/// /model and the built-in commands, answered without holding the sessions lock.
fn control(c: &agents::Ctl, chat: &str, what: Control, arg: &str) -> (String, Option<Value>) {
    match what {
        Control::Model if arg.is_empty() => match c.models() {
            Err(e) => (text::models_failed(&e), None),
            Ok(list) if list.is_empty() => (text::NO_MODELS.into(), None),
            Ok(list) => {
                let items: Vec<(String, String)> = list.iter().enumerate().map(|(i, m)| {
                    let full = format!("/model {}", m.value);
                    (m.label.clone(), if full.len() <= BUTTON_DATA_MAX { full } else { format!("/model {}", i + 1) })
                }).collect();
                CHOICES.lock().unwrap().get_or_insert_with(HashMap::new).insert(chat.to_string(), list.into_iter().map(|m| m.value).collect());
                (text::CHOOSE_MODEL.into(), Some(buttons(in_rows(items, 2))))
            }
        },
        Control::Model => {
            let value = match arg.parse::<usize>() {
                Ok(i) => {
                    let cached = CHOICES.lock().unwrap().as_ref().and_then(|m| m.get(chat)).and_then(|l| l.get(i.wrapping_sub(1)).cloned());
                    match cached.or_else(|| c.models().ok().and_then(|l| l.get(i.wrapping_sub(1)).map(|m| m.value.clone()))) {
                        Some(v) => v,
                        None => return (text::no_model_at(i), None),
                    }
                }
                Err(_) => arg.to_string(),
            };
            match c.set_model(&value) { Ok(()) => (text::model_set(&value), None), Err(e) => (text::model_failed(&e), None) }
        }
        Control::Builtin(k, "thinking") if arg.is_empty() => match c.thinking_levels() {
            Ok(levels) => {
                let cmd = agents::builtin_command(k, "thinking");
                let items = levels.into_iter().map(|l| (l.clone(), format!("/{cmd} {l}"))).collect();
                (text::CHOOSE_THINKING.into(), Some(buttons(in_rows(items, 3))))
            }
            Err(e) => (text::thinking_failed(&e), None),
        },
        Control::Builtin(_, name) => match c.builtin(name, arg) {
            Ok(()) => (match name { "thinking" => text::thinking_set(arg), "compact" => text::COMPACTED.into(), _ => text::NEW_SESSION.into() }, None),
            Err(e) => (text::failed(&e), None),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parsing() {
        assert_eq!(strip_bot_mention("/model@exterior_bot 3"), "/model 3");
        assert_eq!(strip_bot_mention("/stop@x"), "/stop");
        assert_eq!(strip_bot_mention("邮件 a@b.c"), "邮件 a@b.c");
        assert_eq!(relay_split("测试 → 你好"), Some(("测试", "你好")));
        assert_eq!(relay_split("没有分隔符"), None);
        assert!(matches!(agents::find_builtin("/pi_thinking"), Some((Kind::Pi, "thinking"))));
        assert!(matches!(agents::find_builtin("/cx_new"), Some((Kind::Codex, "new"))));
        assert!(agents::find_builtin("/cc_compact").is_none());
        assert_eq!(text::status::from_legacy("运行中"), Some(text::status::RUNNING));
        assert_eq!(text::status::label("stopped"), "进程已关闭");
    }
}
