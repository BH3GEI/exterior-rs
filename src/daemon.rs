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

/// The last list of choices shown for each topic and command, for buttons whose value is too long to fit in a
/// button (they say "/pi_resume 3" and the number is looked up here).
static CHOICES: Mutex<Option<HashMap<String, Vec<String>>>> = Mutex::new(None);

/// Buttons "/<cmd> <value>", or "/<cmd> <n>" where the value is too long; remembers the values for the numbers.
fn choice_buttons(chat: &str, cmd: &str, items: Vec<(String, String)>, per: usize) -> Value {
    let rows: Vec<(String, String)> = items.iter().enumerate().map(|(i, (label, value))| {
        let full = format!("/{cmd} {value}");
        (label.clone(), if full.len() <= BUTTON_DATA_MAX { full } else { format!("/{cmd} {}", i + 1) })
    }).collect();
    CHOICES.lock().unwrap().get_or_insert_with(HashMap::new).insert(format!("{chat}:{cmd}"), items.into_iter().map(|(_, v)| v).collect());
    buttons(in_rows(rows, per))
}

/// The value behind "/<cmd> <n>" from the last list shown.
fn chosen(chat: &str, cmd: &str, arg: &str) -> Option<String> {
    let i = arg.parse::<usize>().ok()?;
    CHOICES.lock().unwrap().as_ref()?.get(&format!("{chat}:{cmd}"))?.get(i.wrapping_sub(1)).cloned()
}

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
        match brief::tg_post("/api/send", body, true) {
            Ok(v) => { let id = v["id"].as_u64(); if let Some(i) = id { own.lock().unwrap().insert(i); } return id; }
            Err((410, _)) => { gone(sessions, chat); return None; }
            Err((c, v)) if c == 0 || c >= 500 => { store::log(&format!("send {chat}: {c} {v}; retry")); std::thread::sleep(params::secs("retry_wait_s")); }
            Err((c, v)) => { store::log(&format!("send {chat} failed {c} {v}")); return None; }
        }
    }
    None
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
                      command: run_row["command"].as_str().unwrap_or("").to_string(), kind, gen: s.gen, resume: resume.clone(),
                      effort: n["effort"].as_str().map(String::from) };
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
            let mut tg: String = format!("{}_{}", kind.prefix(), orig.to_lowercase()).chars().map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' { c } else { '_' }).collect();
            tg.truncate(COMMAND_NAME_MAX);
            if !used.insert(tg.clone()) { continue; }
            let d = if desc.trim().is_empty() { format!("/{orig}") } else { desc.trim().replace('\n', " ") };
            let full = format!("{} · {d}", kind.label());
            let d: String = if full.chars().count() > COMMAND_DESC_MAX { format!("{}…", full.chars().take(COMMAND_DESC_MAX - 1).collect::<String>()) } else { full };
            // Telegram shows at most MENU_MAX commands; the rest still work when typed
            if cmds.len() < MENU_MAX { cmds.push(json!({"command": tg.clone(), "description": d})); }
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

/// Agent output → Telegram. No live editing: what the agent says is sent as it is said. A paragraph is done when the
/// agent pauses (pause_ms); paragraphs of one topic within merge_s go out together as one message, and whatever is
/// left goes out at once when the turn ends. Every send waits its turn in brief's queue.
fn output(rx: Receiver<Ev>, sessions: Sessions, own: Own) {
    /// buf: text still coming in; last: when the last piece came; said: paragraphs waiting to go out, since when
    struct Out { gen: u64, buf: String, last: Instant, said: Vec<String>, since: Option<Instant>, spoke: bool }
    let fresh = |gen| Out { gen, buf: String::new(), last: Instant::now(), said: Vec::new(), since: None, spoke: false };
    fn settle(o: &mut Out) {
        let t = std::mem::take(&mut o.buf);
        if !t.trim().is_empty() { o.said.push(t.trim().to_string()); o.since.get_or_insert(Instant::now()); }
    }
    fn take(o: &mut Out) -> String { o.since = None; if !o.said.is_empty() { o.spoke = true; } std::mem::take(&mut o.said).join("\n\n") }
    let mut outs: HashMap<String, Out> = HashMap::new();
    loop {
        let ev = match rx.recv_timeout(Duration::from_millis(500)) { Ok(e) => Some(e), Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None, Err(_) => return };
        let (pause, merge) = (Duration::from_millis(params::num("pause_ms")), params::secs("merge_s"));
        let mut due = Vec::new();
        for (tid, o) in outs.iter_mut() {
            if !o.buf.is_empty() && o.last.elapsed() > pause { settle(o); }
            if o.since.map(|s| s.elapsed() >= merge).unwrap_or(false) { due.push((tid.clone(), take(o))); }
        }
        for (tid, t) in due { send(&own, &sessions, &tid, &t); }
        let Some(ev) = ev else { continue };
        let current = |tid: &str, gen: u64| sessions.lock().unwrap().get(tid).map(|s| s.gen == gen).unwrap_or(false);
        match ev {
            Ev::Text { tid, gen, delta } => {
                let o = outs.entry(tid).or_insert_with(|| fresh(gen));
                if gen > o.gen { *o = fresh(gen); }
                if gen == o.gen { o.buf.push_str(&delta); o.last = Instant::now(); }
            }
            Ev::Done { tid, gen, text: answer, stopped, error } => {
                let mut o = outs.remove(&tid).filter(|o| o.gen == gen).unwrap_or_else(|| fresh(gen));
                let streamed = !o.buf.trim().is_empty() || !o.said.is_empty();
                settle(&mut o);
                // the final text came only as a whole (no deltas): send it
                if !streamed { if let Some(a) = answer.filter(|t| !t.trim().is_empty()) { o.said.push(a.trim().to_string()); } }
                if stopped { o.said.push(text::sys(text::STOPPED_SUFFIX)); }
                if let Some(e) = &error { o.said.push(text::failed(e)); }
                if o.said.is_empty() && !o.spoke { o.said.push(text::sys(text::DONE_EMPTY)); }
                let body = take(&mut o);
                if !body.is_empty() && send(&own, &sessions, &tid, &body).is_none() { store::log(&format!("answer for {tid} could not be delivered")); }
                let who = {
                    let mut ss = sessions.lock().unwrap();
                    ss.get_mut(&tid).map(|s| { if s.gen == gen { s.pending = false; } s.active = Instant::now(); (s.dev.clone(), s.name.clone()) })
                };
                if let Some((d, name)) = who {
                    if let Some(dev) = store::device(&d) { std::thread::spawn(move || crate::infra::commit(&dev, &name)); }
                }
                if !body.trim().is_empty() && error.is_none() { store::note_set(&tid, "last_answer", json!(body)); }
                if current(&tid, gen) {
                    set_status(&tid, if error.is_some() { st::ERROR } else { st::IDLE });
                    store::note_set(&tid, "last_error", error.map(Value::String).unwrap_or(Value::Null));
                    store::note_set(&tid, "from", Value::Null);
                    store::note_set(&tid, "turn_msg", Value::Null);
                    store::note_set(&tid, "turn_resent", Value::Null);
                }
            }
            Ev::Session { tid, gen, id } => {
                if current(&tid, gen) && store::note(&tid)["session"].as_str() != Some(&id) { store::note_set(&tid, "session", json!(id)); }
            }
            Ev::Note { tid, gen, text: t } => {
                // notes join the topic's waiting paragraphs, in order
                if current(&tid, gen) {
                    let o = outs.entry(tid).or_insert_with(|| fresh(gen));
                    if gen > o.gen { *o = fresh(gen); }
                    settle(o); o.said.push(text::note(&t)); o.since.get_or_insert(Instant::now());
                }
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
                if let Some(mut o) = outs.remove(&tid) {
                    settle(&mut o);
                    if !o.said.is_empty() { o.said.push(text::sys_warn(text::CUT_SUFFIX)); let b = take(&mut o); send(&own, &sessions, &tid, &b); }
                }
                if current(&tid, gen) || warn {
                    let why = agents::last_lines(&why);
                    if warn {
                        store::log(&format!("agent exited mid-turn in {tid}: {why}"));
                        set_status(&tid, st::ERROR);
                        store::note_set(&tid, "last_error", json!(why));
                        // the device may have rebooted or dropped off the network: keep the message and carry on
                        // once it answers again (see retry_waiting); only once per turn, like after a restart
                        let n = store::note(&tid);
                        match n["turn_msg"].as_str().filter(|_| n["turn_resent"] != json!(true)) {
                            Some(m) => {
                                store::note_set(&tid, "wait_msg", json!(m));
                                store::note_set(&tid, "wait_since", json!(store::now()));
                                send(&own, &sessions, &tid, &text::exited_waiting(&why));
                            }
                            None => { send(&own, &sessions, &tid, &text::exited(&why)); }
                        }
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
    ensure_global();
    let mut resend: Vec<(String, String)> = Vec::new();
    for n in store::notes() {
        let tid = n["topic"].as_str().unwrap_or("").to_string();
        if tid.is_empty() { continue; }
        // notes written before 0.7 stored the status label itself
        if let Some(k) = n["status"].as_str().and_then(st::from_legacy) { store::note_set(&tid, "status", json!(k)); }
        // nothing runs right after a restart: a topic still marked running had its turn cut off
        let n = store::note(&tid);
        let last_msg = n["turn_msg"].as_str().map(String::from);
        store::note_set(&tid, "turn_msg", Value::Null);
        store::note_set(&tid, "turn_resent", Value::Null);
        // only once: a turn that itself restarts exterior (say, deploying it) must not loop
        if n["status"].as_str() == Some(st::RUNNING) && last_msg.is_some() && n["turn_resent"] != json!(true) {
            // the user should not have to repeat himself: send the cut-off message again once the loop is up
            resend.push((tid.clone(), last_msg.unwrap_or_default()));
            store::log(&format!("{}: turn cut off by restart, resending", n["name"]));
        } else if matches!(n["status"].as_str(), Some(st::RUNNING) | Some(st::MOVING)) {
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
    for (tid, msg) in resend {
        send(&own, &sessions, &tid, &text::sys(text::RESTART_RESEND));
        let line = if msg.starts_with('/') { msg } else { text::resent(&msg) };
        handle(&sessions, &own, &tx, &tid, &line);
        store::note_set(&tid, "turn_resent", json!(true));
    }
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
            retry_waiting(&sessions, &own, &tx);
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
                std::thread::spawn(move || { let _ = brief::tg_post("/api/react", json!({"chat": c, "id": id, "emoji": params::get("ack_emoji")}), true); });
            } else if let Some((who, body)) = relay_split(&text) {
                store::append("messages.jsonl", &json!({"time": store::now(), "from": who.trim(), "to": to_name, "text": body.chars().take(500).collect::<String>()}));
            }
            if from_user && m["media"]["url"].is_string() {
                text = with_file(&sessions, &chat, id, &m["media"], &text);
            }
            store::note_set(&chat, "turn_resent", Value::Null);
            store::note_set(&chat, "wait_msg", Value::Null);
            handle(&sessions, &own, &tx, &chat, &text);
        }
        store::set_state("last_id", json!(last));
    }
}

/// Topics whose turn was cut off when the agent's process went away (device rebooted, network dropped): once the
/// device answers again, send the message again. One check at a time, in the background.
fn retry_waiting(sessions: &Sessions, own: &Own, tx: &Sender<Ev>) {
    static BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let waiting: Vec<Value> = store::notes().into_iter().filter(|n| n["wait_msg"].is_string() && n["deleted"] != true).collect();
    if waiting.is_empty() || BUSY.swap(true, std::sync::atomic::Ordering::SeqCst) { return; }
    let (sessions, own, tx) = (sessions.clone(), own.clone(), tx.clone());
    std::thread::spawn(move || {
        let max = params::num("wait_hours") * 3600;
        let mut up: HashMap<String, bool> = HashMap::new();
        for n in waiting {
            let tid = n["topic"].as_str().unwrap_or("").to_string();
            let msg = n["wait_msg"].as_str().unwrap_or("").to_string();
            if store::now().saturating_sub(n["wait_since"].as_u64().unwrap_or(0)) > max {
                store::log(&format!("{}: device did not come back in time, giving up", n["name"]));
                store::note_set(&tid, "wait_msg", Value::Null);
                continue;
            }
            let dev = n["device"].as_str().unwrap_or("").to_string();
            let ok = *up.entry(dev.clone()).or_insert_with(|| store::device(&dev).map(|d| crate::host::run(&d, "true", 30).is_ok()).unwrap_or(false));
            if !ok { continue; }
            // the user may have moved on in the meantime: then his newer message stands, nothing to repeat
            if store::note(&tid)["wait_msg"].as_str() != Some(msg.as_str()) { continue; }
            store::note_set(&tid, "wait_msg", Value::Null);
            store::log(&format!("{}: device back, resending", n["name"]));
            send(&own, &sessions, &tid, &text::sys(text::DEVICE_BACK));
            let line = if msg.starts_with('/') { msg } else { text::resent(&msg) };
            handle(&sessions, &own, &tx, &tid, &line);
            store::note_set(&tid, "turn_resent", json!(true));
        }
        BUSY.store(false, std::sync::atomic::Ordering::SeqCst);
    });
}

/// The global topic: created as the user (brief does it with his account, since only his topics are adopted
/// and only he can pin) if he has none with that title, and pinned. Safe to repeat.
fn ensure_global() {
    let name = params::get("global_topic");
    match brief::post("/api/bots/user_topic", json!({"name": name, "pin": true})) {
        Ok(v) => { store::log(&format!("global topic {name}: {}", v["chat"])); store::set_state("global", json!({"ok": true, "time": store::now(), "msg": text::global_ready(&name)})); }
        Err((c, v)) => { store::log(&format!("global topic {name} failed: {c} {v}")); store::set_state("global", json!({"ok": false, "time": store::now(), "msg": text::global_failed(&name, &format!("{c} {v}"))})); }
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

fn handle(sessions: &Sessions, own: &Own, tx: &Sender<Ev>, chat: &str, msg: &str) {
    let mut reply: Option<String> = None;
    let mut reply_buttons: Option<Value> = None;
    let mut transfer: Option<(String, String)> = None;
    let mut ctl: Option<(agents::Ctl, Kind, &'static str, String)> = None;
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
        let builtin = agents::find_builtin(head);
        let own_prefix = Kind::ALL.iter().find(|k| head.starts_with(&format!("/{}_", k.prefix()))).copied();
        if head == "/exit" {
            if let Some(a) = s.agent.as_mut() { a.stop(); a.kill(); }
            s.agent = None; s.gen += 1; s.pending = false;
            store::note_set(chat, "session", Value::Null);
            store::note_set(chat, "resolved", Value::Null);
            store::note_set(chat, "from", Value::Null);
            set_status(chat, st::NEW);
            reply = Some(text::session_ended(&cur));
        } else if let Some((k, name)) = builtin {
            if Some(k) != cur_kind { reply = Some(text::wrong_agent(k.label(), &cur)); }
            else if agents::is_quit(name) {
                if let Some(a) = s.agent.as_mut() { a.stop(); a.kill(); }
                s.agent = None; s.gen += 1; s.pending = false;
                set_status(chat, st::STOPPED);
                reply = Some(text::QUIT.into());
            } else if name == "copy" && k == Kind::Codex {
                reply = Some(n["last_answer"].as_str().filter(|a| !a.trim().is_empty()).unwrap_or(text::NOTHING_TO_COPY).to_string());
            } else {
                if s.agent.is_none() {
                    if let Err(e) = start_agent(s, tx) { reply = Some(text::start_failed(&cur, &e)); }
                }
                if let Some(a) = s.agent.as_ref() { ctl = Some((a.ctl(), k, name, arg.clone())); }
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
                    else if kind == Kind::Claude && orig == "effort" && arg.is_empty() {
                        reply = Some(text::choose_effort(n["effort"].as_str()));
                        reply_buttons = Some(buttons(in_rows(agents::CLAUDE_EFFORTS.iter().map(|l| (l.to_string(), format!("/{cmd} {l}"))).collect(), 3)));
                    } else {
                        // remembered, so a restarted process starts with the same level
                        if kind == Kind::Claude && orig == "effort" && agents::CLAUDE_EFFORTS.contains(&arg.as_str()) { store::note_set(chat, "effort", json!(arg)); }
                        say = Some(format!("/{orig}{}{arg}", if arg.is_empty() { "" } else { " " })); }
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
                else {
                    // the agent may already have been running: the turn starts now either way
                    set_status(chat, st::RUNNING);
                    store::note_set(chat, "turn_msg", json!(msg));
                }
            }
        }
    }
    if let Some((_, want)) = &transfer { send(own, sessions, chat, &text::sys(&text::transferring(want))); }
    if let Some(r) = reply { send_with(own, sessions, chat, &r, reply_buttons); }
    if let Some((c, kind, name, arg)) = ctl {
        let (own, sessions, chat) = (own.clone(), sessions.clone(), chat.to_string());
        std::thread::spawn(move || {
            let a = control(&c, &chat, kind, name, &arg);
            if let Some(id) = &a.session { store::note_set(&chat, "session", json!(id)); }
            if a.started {
                // /cx_review runs as a turn: its result arrives like an answer
                if let Some(s) = sessions.lock().unwrap().get_mut(&chat) { s.pending = true; }
                set_status(&chat, st::RUNNING);
            }
            match a.file {
                Some((name, data)) => {
                    let r = brief::tg_post("/api/send_file", json!({"chat": chat, "name": name, "data": brief::base64(&data), "text": a.text}), true);
                    if let Err((c, v)) = r { send_with(&own, &sessions, &chat, &text::failed(&format!("{c} {v}")), None); }
                }
                None => { send_with(&own, &sessions, &chat, &a.text, a.buttons); }
            }
        });
    }
}

/// What a built-in command answers in the topic, and what it changed.
struct Answer { text: String, buttons: Option<Value>, session: Option<String>, started: bool, file: Option<(String, Vec<u8>)> }

impl Answer {
    fn say(t: impl Into<String>) -> Answer { Answer { text: t.into(), buttons: None, session: None, started: false, file: None } }
    fn choose(t: impl Into<String>, b: Value) -> Answer { Answer { text: t.into(), buttons: Some(b), session: None, started: false, file: None } }
}

/// The agents' own commands that go through the protocol (agents::BUILTINS), answered without holding the sessions lock.
fn control(c: &agents::Ctl, chat: &str, kind: Kind, name: &str, arg: &str) -> Answer {
    let cmd = agents::builtin_command(kind, name);
    match name {
        "model" if arg.is_empty() => match c.models() {
            Err(e) => Answer::say(text::models_failed(&e)),
            Ok(list) if list.is_empty() => Answer::say(text::NO_MODELS),
            Ok(list) => Answer::choose(text::CHOOSE_MODEL, choice_buttons(chat, &cmd, list.into_iter().map(|m| (m.label, m.value)).collect(), 2)),
        },
        "model" => {
            let value = match arg.parse::<usize>() {
                Ok(i) => match chosen(chat, &cmd, arg).or_else(|| c.models().ok().and_then(|l| l.get(i.wrapping_sub(1)).map(|m| m.value.clone()))) {
                    Some(v) => v,
                    None => return Answer::say(text::no_model_at(i, &cmd)),
                },
                Err(_) => arg.to_string(),
            };
            match c.set_model(&value) { Ok(()) => Answer::say(text::model_set(&value)), Err(e) => Answer::say(text::model_failed(&e)) }
        }
        "thinking" if arg.is_empty() => match c.thinking_levels() {
            Ok(levels) => Answer::choose(text::CHOOSE_THINKING, buttons(in_rows(levels.into_iter().map(|l| (l.clone(), format!("/{cmd} {l}"))).collect(), 3))),
            Err(e) => Answer::say(text::thinking_failed(&e)),
        },
        "name" | "rename" if arg.is_empty() => Answer::say(text::name_needed(&cmd)),
        _ => match c.builtin(name, &chosen(chat, &cmd, arg).unwrap_or_else(|| arg.to_string())) {
            Err(e) => Answer::say(text::failed(&e)),
            Ok(d) => {
                let mut a = match name {
                    "thinking" => Answer::say(text::thinking_set(arg)),
                    "compact" => Answer::say(text::COMPACTED),
                    "new" | "clear" => Answer::say(text::NEW_SESSION),
                    "name" => Answer::say(text::name_set(arg)),
                    "rename" => Answer::say(text::renamed(arg)),
                    "clone" => Answer::say(text::cloned(d["_session"].as_str().unwrap_or("—"))),
                    "archive" => Answer::say(text::archived(d["_session"].as_str().unwrap_or("—"))),
                    "copy" => Answer::say(d["text"].as_str().filter(|t| !t.trim().is_empty()).unwrap_or(text::NOTHING_TO_COPY)),
                    "logout" => Answer::say(text::LOGGED_OUT),
                    "goal" if arg == "clear" => Answer::say(text::GOAL_CLEARED),
                    "goal" if !arg.is_empty() => Answer::say(text::goal_set(arg)),
                    "diff" => {
                        let out = d["stdout"].as_str().unwrap_or("").trim().to_string();
                        Answer::say(if out.is_empty() { text::NO_DIFF.to_string() } else { out })
                    }
                    "resume" if arg.is_empty() && kind == Kind::Pi => {
                        let dir = d["_dir"].as_str().unwrap_or("");
                        let items: Vec<(String, String)> = d["output"].as_str().unwrap_or("").lines().map(str::trim).filter(|f| f.ends_with(".jsonl"))
                            .map(|f| (f.chars().take(40).collect(), format!("{dir}/{f}"))).collect();
                        if items.is_empty() { Answer::say(text::NO_SESSIONS) } else { Answer::choose(text::CHOOSE_SESSION, choice_buttons(chat, &cmd, items, 1)) }
                    }
                    "resume" if arg.is_empty() => {
                        let items: Vec<(String, String)> = d["data"].as_array().into_iter().flatten().filter_map(|t| {
                            let id = t["id"].as_str()?;
                            let label = t["name"].as_str().or(t["preview"].as_str()).unwrap_or(id).chars().take(40).collect();
                            Some((label, id.to_string()))
                        }).collect();
                        if items.is_empty() { Answer::say(text::NO_SESSIONS) } else { Answer::choose(text::CHOOSE_SESSION, choice_buttons(chat, &cmd, items, 1)) }
                    }
                    "resume" | "import" => Answer::say(text::resumed(d["_session"].as_str().unwrap_or(arg))),
                    "fork" if kind == Kind::Codex => Answer::say(text::resumed(d["_session"].as_str().unwrap_or("—"))),
                    "export" if kind == Kind::Claude => {
                        let name = d["default_filename"].as_str().or(d["filename"].as_str()).unwrap_or("conversation.txt").to_string();
                        let body = ["content", "text", "conversation"].iter().find_map(|k| d[*k].as_str()).unwrap_or("").to_string();
                        let mut a = Answer::say(text::exported_file(&name));
                        a.file = Some((name, body.into_bytes()));
                        a
                    }
                    "session" => Answer::say(text::session_stats(&d)),
                    "export" => Answer::say(text::exported(d["path"].as_str().unwrap_or("—"))),
                    "fork" if arg.is_empty() && kind == Kind::Pi => {
                        let items: Vec<(String, String)> = d["messages"].as_array().into_iter().flatten().filter_map(|m| {
                            Some((m["text"].as_str()?.chars().take(30).collect(), m["entryId"].as_str()?.to_string()))
                        }).collect();
                        if items.is_empty() { Answer::say(text::NO_FORK_POINTS) } else { Answer::choose(text::CHOOSE_FORK, choice_buttons(chat, &cmd, items, 1)) }
                    }
                    "fork" if d["cancelled"] == true => Answer::say(text::FORK_CANCELLED),
                    "fork" => Answer::say(text::forked(d["text"].as_str().unwrap_or(""))),
                    "status" if kind == Kind::Codex => Answer::say(text::codex_status(d["_model"].as_str().unwrap_or(text::DEFAULT_MODEL), &d["rateLimits"])),
                    "review" => {
                        let t = agents::review_target(arg);
                        let v = t["sha"].as_str().or(t["branch"].as_str()).unwrap_or("");
                        let mut a = Answer::say(text::review_started(&text::review_target(t["type"].as_str().unwrap_or(""), v)));
                        a.started = true;
                        a
                    }
                    // the commands that only show information
                    _ => Answer::say(text::show(&d)),
                };
                a.session = d["_session"].as_str().map(String::from);
                a
            }
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
        assert!(matches!(agents::find_builtin("/cc_model"), Some((Kind::Claude, "model"))));
        assert!(matches!(agents::find_builtin("/pi_fork"), Some((Kind::Pi, "fork"))));
        assert!(matches!(agents::find_builtin("/cx_review"), Some((Kind::Codex, "review"))));
        assert!(agents::find_builtin("/model").is_none());
        assert_eq!(agents::review_target("")["type"], "uncommittedChanges");
        assert_eq!(agents::review_target("abc1234")["type"], "commit");
        assert_eq!(agents::review_target("main")["branch"], "main");
        assert_eq!(text::status::from_legacy("运行中"), Some(text::status::RUNNING));
        assert_eq!(text::status::label("stopped"), "进程已关闭");
    }
}

#[cfg(test)]
mod text_tests {
    use serde_json::json;
    #[test]
    fn status() {
        let s = crate::text::codex_status("gpt", &json!({"credits": {"hasCredits": false}, "primary": {"usedPercent": 100, "windowDurationMins": 10080}, "secondary": null, "rateLimitReachedType": "x"}));
        assert!(s.contains("7 天用量：100%") && s.contains("额度余额：无") && s.contains("已达上限：x"), "{s}");
        assert_eq!(crate::text::window(Some(300)), "5 小时");
    }
}

#[cfg(test)]
mod more_tests {
    #[test]
    fn helpers() {
        assert_eq!(crate::brief::base64(b"hello"), "aGVsbG8=");
        assert_eq!(crate::brief::base64(b"ab"), "YWI=");
        assert_eq!(crate::brief::base64(b"abc"), "YWJj");
        let b = super::choice_buttons("tgbot:1", "pi_resume", vec![("a".into(), "x".repeat(80)), ("b".into(), "short".into())], 1);
        assert_eq!(b[0][0]["data"], "/pi_resume 1");
        assert_eq!(b[1][0]["data"], "/pi_resume short");
        assert_eq!(super::chosen("tgbot:1", "pi_resume", "1").as_deref(), Some("x".repeat(80).as_str()));
        assert!(crate::text::show(&serde_json::json!({"a": 1, "b": {"c": "d"}, "_x": 2})).contains("  c：d"));
    }
}
