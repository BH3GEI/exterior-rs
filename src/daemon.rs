//! Every Telegram topic Yao creates is a session. Messages in → the topic's agent; the agent's answer → the topic.
use crate::agents::{self, Agent, Ev, Kind, Spec};
use crate::{brief, store};
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

const IDLE: Duration = Duration::from_secs(2 * 3600);

type Sessions = Arc<Mutex<HashMap<String, Session>>>;
type Own = Arc<Mutex<HashSet<u64>>>;

/// The last model list shown in each topic, so a button can say "/model 3".
static CHOICES: Mutex<Option<HashMap<String, Vec<String>>>> = Mutex::new(None);

/// What the 会话 page shows about the running side (the rest is in the notes).
pub static LIVE: Mutex<Option<Sessions>> = Mutex::new(None);

pub fn running(tid: &str) -> Option<(bool, bool)> {
    let g = LIVE.lock().unwrap();
    let ss = g.as_ref()?.lock().unwrap();
    ss.get(tid).map(|s| (s.agent.is_some(), s.pending))
}

fn set_status(tid: &str, st: &str) { store::note_set(tid, "status", json!(st)); store::note_set(tid, "active", json!(store::now())); }

fn send(own: &Own, sessions: &Sessions, chat: &str, text: &str) -> Option<u64> { send_with(own, sessions, chat, text, None) }

/// Buttons: rows of (label, what pressing it says).
fn buttons(rows: Vec<Vec<(String, String)>>) -> Value {
    Value::Array(rows.into_iter().map(|r| Value::Array(r.into_iter().map(|(t, d)| json!({"text": t, "data": d})).collect())).collect())
}

fn in_rows(items: Vec<(String, String)>, per: usize) -> Vec<Vec<(String, String)>> { items.chunks(per).map(|c| c.to_vec()).collect() }

fn send_with(own: &Own, sessions: &Sessions, chat: &str, text: &str, btns: Option<Value>) -> Option<u64> {
    for _ in 0..3 {
        let mut body = json!({"chat": chat, "text": brief::clip(text)});
        if let Some(b) = &btns { body["buttons"] = b.clone(); }
        match brief::post("/api/send", body) {
            Ok(v) => { let id = v["id"].as_u64(); if let Some(i) = id { own.lock().unwrap().insert(i); } return id; }
            Err((410, _)) => { gone(sessions, chat); return None; }
            Err((c, v)) if c == 0 || c >= 500 => { store::log(&format!("send {chat}: {c} {v}; retry")); std::thread::sleep(Duration::from_secs(3)); }
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
        store::note_set(&s.tid, "status", json!("已删除"));
    }
}

fn safe(name: &str) -> String {
    let s: String = name.trim().chars().map(|c| if matches!(c, '/' | ':' | '\\' | '\0') { '_' } else { c }).collect();
    let s = s.trim_matches('.').to_string();
    if s.is_empty() { "话题".into() } else { s }
}

fn root_of(dev: &str) -> String {
    store::device(dev).and_then(|d| d["root"].as_str().map(|s| s.trim_end_matches('/').to_string())).unwrap_or_default()
}

/// The folder of a topic on its device: what its note says; else <root>/<title>; 「全局」 is <root>/overview.
fn dir_for(dev: &str, tid: &str, name: &str) -> String {
    if let Some(d) = store::note(tid)["dir"].as_str() { return d.to_string(); }
    let root = root_of(dev);
    if name.trim() == "全局" { return format!("{root}/overview"); }
    let taken: Vec<String> = store::notes().iter().filter(|n| n["topic"] != tid).filter_map(|n| n["dir"].as_str().map(String::from)).collect();
    let mut d = format!("{root}/{}", safe(name));
    if taken.contains(&d) || name.trim() == "overview" || name.trim() == "knowledge" { d = format!("{d}-{}", tid.trim_start_matches("tgbot:")); }
    d
}

/// Bring the session list in line with brief's topics (new, renamed, deleted).
fn refresh(sessions: &Sessions) {
    let Some(list) = brief::get("/api/bots") else { return };
    let Some(defdev) = store::default_device() else { return };
    let mut ss = sessions.lock().unwrap();
    for t in list.as_array().into_iter().flatten() {
        let chat = t["chat"].as_str().unwrap_or("").to_string();
        let extra = std::env::var("EXTERIOR_EXTRA").unwrap_or_default();
        if !(t["mine"] == true && (t["user_created"] == true || extra.split(',').any(|e| e == chat))) { continue; }
        let name = t["name"].as_str().unwrap_or("").trim_start_matches("✓ ").to_string();
        if t["deleted"] == true {
            if let Some(s) = ss.get_mut(&chat) { if let Some(a) = s.agent.as_mut() { a.kill(); } s.agent = None; s.deleted = true; }
            if store::note(&chat)["topic"].is_string() { store::note_set(&chat, "deleted", json!(true)); store::note_set(&chat, "status", json!("已删除")); }
            continue;
        }
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
                    store::note_set(&chat, "status", json!("未启动"));
                    store::log(&format!("new session {name} ({chat}) -> {dir}"));
                }
                store::note_set(&chat, "name", json!(name));
                store::note_set(&chat, "device", json!(dev));
                store::note_set(&chat, "dir", json!(dir));
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
    let dev = store::device(&s.dev).ok_or(format!("设备「{}」不存在了", s.dev))?;
    let n = store::note(&s.tid);
    let name = n["agent"].as_str().unwrap_or("").to_string();
    let row = store::agent(&dev, &name).ok_or(format!("设备「{}」上没有 agent「{name}」", s.dev))?;
    let kind = Kind::parse(row["kind"].as_str().unwrap_or("")).ok_or("agent 类型不对")?;
    let resume = n["session"].as_str().map(String::from);
    // cc0 picked cc6 last time: the session lives in cc6, so resume through cc6
    let run_row = if resume.is_some() { n["resolved"].as_str().and_then(|r| store::agent(&dev, r)).unwrap_or(row.clone()) } else { row.clone() };
    s.gen += 1;
    let root = root_of(&s.dev);
    let prompt = crate::prompt::session_text(&dev, &s.name, &s.tid, &s.dir, s.dir == format!("{root}/overview"));
    let spec = Spec { tid: s.tid.clone(), prompt, topic: s.name.clone(), dev: dev.clone(), dir: s.dir.clone(),
                      command: run_row["command"].as_str().unwrap_or("").to_string(), kind, gen: s.gen, resume: resume.clone() };
    store::log(&format!("start {} ({}) for {} in {} resume {:?}", run_row["name"], spec.command, s.name, s.dir, resume));
    s.agent = Some(Agent::start(spec, tx.clone())?);
    set_status(&s.tid, "运行中");
    Ok(())
}

/// The "/" menu: exterior's commands, the built-ins, then each agent's own commands with its prefix (cc_ claude, pi_ pi).
pub fn build_menu() -> Result<usize, String> {
    let dev = store::default_device().ok_or("还没有设备")?;
    let mut cmds = vec![
        json!({"command": "stop", "description": "停下当前这一轮（Esc）"}),
        json!({"command": "exit", "description": "结束这个会话，下一条消息开新的"}),
        json!({"command": "agent", "description": "切换 agent"}),
        json!({"command": "model", "description": "换模型"}),
        json!({"command": "pi_thinking", "description": "Pi · 思考强度"}),
        json!({"command": "pi_compact", "description": "Pi · 压缩上下文"}),
        json!({"command": "pi_new", "description": "Pi · 开新会话"}),
        json!({"command": "cx_compact", "description": "Codex · 压缩上下文"}),
        json!({"command": "cx_new", "description": "Codex · 开新会话"}),
    ];
    let mut map = serde_json::Map::new();
    let mut used: HashSet<String> = cmds.iter().filter_map(|c| c["command"].as_str().map(String::from)).collect();
    let root = dev["root"].as_str().unwrap_or("/").to_string();
    for (kind, prefix) in [(Kind::Claude, "cc"), (Kind::Pi, "pi")] {
        let Some(row) = dev["agents"].as_array().into_iter().flatten().find(|a| a["kind"] == kind.name()) else { continue };
        for (orig, desc) in agents::commands(&dev, &root, row["command"].as_str().unwrap_or(""), kind) {
            if cmds.len() >= 100 { break; }
            let mut tg: String = format!("{prefix}_{}", orig.to_lowercase()).chars().map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' { c } else { '_' }).collect();
            tg.truncate(32);
            if !used.insert(tg.clone()) { continue; }
            let d = if desc.trim().is_empty() { format!("/{orig}") } else { desc.trim().replace('\n', " ") };
            let full = format!("{} · {d}", kind.label());
            let d: String = if full.chars().count() > 60 { format!("{}…", full.chars().take(59).collect::<String>()) } else { full };
            cmds.push(json!({"command": tg, "description": d}));
            map.insert(tg, json!([kind.name(), orig]));
        }
    }
    if map.is_empty() { return Err("没取到 agent 的命令列表（agent 没有回应）".into()); }
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
        for (tid, o) in outs.iter_mut() {
            if !o.buf.trim().is_empty() && o.shown.elapsed() > Duration::from_millis(1500) {
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
            Ev::Done { tid, gen, text, stopped, error } => {
                let o = outs.remove(&tid).filter(|o| o.gen == gen).unwrap_or(Out { gen, buf: String::new(), id: None, shown: Instant::now() });
                let mut body = if stopped { o.buf.clone() } else { text.filter(|t| !t.trim().is_empty()).unwrap_or(o.buf.clone()) };
                if stopped { body.push_str("\n\n（已停）"); }
                if let Some(e) = &error { body = format!("{body}\n\n⚠️ {e}").trim().to_string(); }
                if body.trim().is_empty() { body = "✓ 完成（没有文字输出）".into(); }
                match o.id { None => { send(&own, &sessions, &tid, &body); } Some(i) => edit(&sessions, &tid, i, &body) }
                let who = {
                    let mut ss = sessions.lock().unwrap();
                    ss.get_mut(&tid).map(|s| { if s.gen == gen { s.pending = false; } s.active = Instant::now(); (s.dev.clone(), s.name.clone()) })
                };
                if let Some((d, name)) = who {
                    if let Some(dev) = store::device(&d) { std::thread::spawn(move || crate::infra::commit(&dev, &name)); }
                }
                if current(&tid, gen) {
                    set_status(&tid, if error.is_some() { "出错" } else { "空闲" });
                    store::note_set(&tid, "last_error", error.map(Value::String).unwrap_or(Value::Null));
                    store::note_set(&tid, "from", Value::Null);
                }
            }
            Ev::Session { tid, gen, id } => {
                if current(&tid, gen) && store::note(&tid)["session"].as_str() != Some(&id) { store::note_set(&tid, "session", json!(id)); }
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
                    if !o.buf.trim().is_empty() { let b = format!("{}\n\n（中断）", o.buf); match o.id { None => { send(&own, &sessions, &tid, &b); } Some(i) => edit(&sessions, &tid, i, &b) } }
                }
                if current(&tid, gen) || warn {
                    let why = why.trim().lines().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
                    if warn {
                        store::log(&format!("agent exited mid-turn in {tid}: {why}"));
                        set_status(&tid, "出错");
                        store::note_set(&tid, "last_error", json!(why));
                        let tail = if why.is_empty() { String::new() } else { format!("\n\n{why}") };
                        send(&own, &sessions, &tid, &format!("⚠️ agent 意外退出了。下一条消息会接着原来的会话继续。{tail}"));
                    } else if store::note(&tid)["status"] != "已删除" {
                        set_status(&tid, "未运行");
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
                Ok(_) => { store::set_state("telegram", json!({"ok": true, "time": store::now(), "msg": "已接管群里新建的话题"})); break; }
                Err((c, v)) => store::set_state("telegram", json!({"ok": false, "time": store::now(), "msg": format!("接管失败：{c} {v}")})),
            }
        } else {
            store::set_state("telegram", json!({"ok": false, "time": store::now(), "msg": "还没有设置 brief 地址和密钥，或还没有设备"}));
        }
        std::thread::sleep(Duration::from_secs(15));
    }
    store::log("exterior started");
    let own: Own = Arc::new(Mutex::new(HashSet::new()));
    let (tx, rx) = channel();
    { let (s, o) = (sessions.clone(), own.clone()); std::thread::spawn(move || output(rx, s, o)); }
    std::thread::spawn(|| for _ in 0..5 {
        match build_menu() { Ok(n) => { store::log(&format!("menu: {n} commands")); break; } Err(e) => { store::log(&format!("menu failed: {e}; retry in 5 min")); std::thread::sleep(Duration::from_secs(300)); } }
    });
    std::thread::spawn(|| for d in store::devices() { if let Err(e) = crate::prompt::install(&d) { store::log(&format!("install prompt on {}: {e}", d["name"])); } });
    refresh(&sessions);
    let mut last = store::state("last_id").as_u64().unwrap_or(0);
    if last == 0 {
        last = brief::get("/api/messages?source=telegram&limit=1").and_then(|v| v.as_array().and_then(|a| a.iter().filter_map(|m| m["id"].as_u64()).max())).unwrap_or(0);
        store::set_state("last_id", json!(last));
    }
    let mut refreshed = Instant::now();
    loop {
        if refreshed.elapsed() > Duration::from_secs(60) {
            refresh(&sessions);
            refreshed = Instant::now();
            for s in sessions.lock().unwrap().values_mut() {
                if s.agent.is_some() && !s.pending && s.active.elapsed() > IDLE {
                    store::log(&format!("idle, closing process of {}", s.name));
                    if let Some(a) = s.agent.as_mut() { a.kill(); }
                    s.agent = None; s.gen += 1;
                    set_status(&s.tid, "未运行");
                }
            }
        }
        let msgs = match brief::call("GET", &format!("/api/wait?source=telegram&all=1&after_id={last}&timeout=20&limit=200"), None) {
            Ok(v) => { store::set_state("telegram", json!({"ok": true, "time": store::now(), "msg": "已连接"})); v }
            Err((c, v)) => {
                store::set_state("telegram", json!({"ok": false, "time": store::now(), "msg": format!("连不上 brief：{c} {v}")}));
                std::thread::sleep(Duration::from_secs(5)); continue;
            }
        };
        for m in msgs.as_array().into_iter().flatten() {
            let id = m["id"].as_u64().unwrap_or(0);
            last = last.max(id);
            let chat = m["chat"].as_str().unwrap_or("").to_string();
            if !chat.starts_with("tgbot:") { continue; }
            if !sessions.lock().unwrap().contains_key(&chat) { refresh(&sessions); refreshed = Instant::now(); }
            let mut text = m["text"].as_str().unwrap_or("").replace("@exterior_bot", "");
            let mut from_user = m["sender"] == "我";
            // test mode only: a message starting with 「测试员 → 」 in an EXTERIOR_EXTRA topic counts as Yao's
            if !from_user && std::env::var("EXTERIOR_EXTRA").map(|e| e.split(',').any(|c| c == chat)).unwrap_or(false) && !own.lock().unwrap().contains(&id) {
                if let Some(b) = text.strip_prefix("测试员 → ") { text = b.to_string(); from_user = true; }
            }
            let to_name = {
                let ss = sessions.lock().unwrap();
                let Some(s) = ss.get(&chat) else { continue };
                if s.deleted { continue; }
                if !from_user {
                    if own.lock().unwrap().contains(&id) { continue; }
                    let Some((who, _)) = text.split_once(" → ") else { continue };
                    if who.trim().is_empty() || who.trim() == s.name || who.contains('\n') { continue; }
                }
                s.name.clone()
            };
            if from_user {
                let c = chat.clone();
                std::thread::spawn(move || { let _ = brief::post("/api/react", json!({"chat": c, "id": id, "emoji": "👀"})); });
            } else if let Some((who, body)) = text.split_once(" → ") {
                store::append("messages.jsonl", &json!({"time": store::now(), "from": who.trim(), "to": to_name, "text": body.chars().take(500).collect::<String>()}));
            }
            handle(&sessions, &own, &tx, &chat, &text);
        }
        store::set_state("last_id", json!(last));
    }
}

fn agent_names(dev: &str) -> String {
    store::device(dev).map(|d| d["agents"].as_array().into_iter().flatten().filter_map(|a| a["name"].as_str()).collect::<Vec<_>>().join("、")).unwrap_or_default()
}

fn handle(sessions: &Sessions, own: &Own, tx: &Sender<Ev>, chat: &str, text: &str) {
    let mut reply: Option<String> = None;
    let mut reply_buttons: Option<Value> = None;
    let mut transfer: Option<String> = None;
    let mut ctl: Option<(agents::Ctl, String, String)> = None;
    {
        let mut ss = sessions.lock().unwrap();
        let Some(s) = ss.get_mut(chat) else { return };
        s.active = Instant::now();
        store::note_set(chat, "active", json!(store::now()));
        store::log(&format!("<- {}: {}", s.name, text.chars().take(80).collect::<String>()));
        let t = text.trim();
        let n = store::note(chat);
        let cur = n["agent"].as_str().unwrap_or("").to_string();
        let dev = store::device(&s.dev);
        let cur_kind = dev.as_ref().and_then(|d| store::agent(d, &cur)).and_then(|r| Kind::parse(r["kind"].as_str().unwrap_or("")));
        let mut say: Option<String> = None;
        let head = t.split_whitespace().next().unwrap_or("");
        let arg = t[head.len()..].trim().to_string();
        if head == "/exit" {
            if let Some(a) = s.agent.as_mut() { a.stop(); a.kill(); }
            s.agent = None; s.gen += 1; s.pending = false;
            store::note_set(chat, "session", Value::Null);
            store::note_set(chat, "resolved", Value::Null);
            store::note_set(chat, "from", Value::Null);
            set_status(chat, "未启动");
            reply = Some(format!("已退出。下一条消息会开一个新的 {cur} 会话。"));
        } else if matches!(head, "/model" | "/pi_thinking" | "/pi_compact" | "/pi_new" | "/cx_compact" | "/cx_new") {
            let need = match head { "/model" => None, h if h.starts_with("/pi_") => Some(Kind::Pi), _ => Some(Kind::Codex) };
            if need.is_some() && need != cur_kind {
                reply = Some(format!("这是 {} 的命令，这个话题现在是 {cur}。", need.map(|k| k.name()).unwrap_or("")));
            } else {
                if s.agent.is_none() {
                    if let Err(e) = start_agent(s, tx) { reply = Some(format!("⚠️ 启动 {cur} 失败：{e}")); }
                }
                if let Some(a) = s.agent.as_ref() { ctl = Some((a.ctl(), head.to_string(), arg.clone())); }
            }
        } else if t == "/stop" || t.starts_with("/stop ") {
            match s.agent.as_mut() { Some(a) if s.pending => a.stop(), _ => reply = Some("现在没有在运行的任务。".into()) }
        } else if t == "/agent" {
            reply = Some(format!("现在是 {cur}。换成："));
            let names: Vec<(String, String)> = agent_names(&s.dev).split('、').filter(|n| !n.is_empty() && *n != cur).map(|n| (n.to_string(), format!("/agent {n}"))).collect();
            reply_buttons = Some(buttons(in_rows(names, 4)));
        } else if let Some(want) = t.strip_prefix("/agent ") {
            let want = want.trim().to_string();
            match dev.as_ref().and_then(|d| store::agent(d, &want)) {
                None => reply = Some(format!("没有「{want}」。可用：{}", agent_names(&s.dev))),
                Some(_) if want == cur => reply = Some(format!("现在就是 {cur}。")),
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
                        None => { set_status(chat, "未启动"); reply = Some(format!("已切换到 {want}。")); }
                        Some(p) => { store::note_set(chat, "from", json!(p)); transfer = Some(p); }
                    }
                }
            }
        } else if let Some(cmd) = t.strip_prefix('/').filter(|c| c.split_whitespace().next().map(|h| h.starts_with("cc_") || h.starts_with("pi_")).unwrap_or(false)) {
            let (head, args) = cmd.split_once(char::is_whitespace).unwrap_or((cmd, ""));
            match store::state("menu").get(head).cloned() {
                None => reply = Some(format!("没有 /{head} 这个命令。")),
                Some(e) => {
                    let (k, orig) = (e[0].as_str().unwrap_or(""), e[1].as_str().unwrap_or(""));
                    if Some(k) != cur_kind.map(|k| k.name()) { reply = Some(format!("这是 {k} 的命令，这个话题现在是 {cur}。")); }
                    else { say = Some(format!("/{orig}{}{args}", if args.is_empty() { "" } else { " " })); }
                }
            }
        } else {
            say = Some(text.to_string());
        }
        if let Some(p) = &transfer { say = Some(p.clone()); }
        if let Some(t) = say {
            if s.agent.is_none() {
                if let Err(e) = start_agent(s, tx) { reply = Some(format!("⚠️ 启动 {} 失败：{e}", store::note(chat)["agent"].as_str().unwrap_or(""))); set_status(chat, "出错"); store::note_set(chat, "last_error", json!(e)); transfer = None; }
            }
            if let Some(a) = s.agent.as_mut() {
                s.pending = true;
                if let Err(e) = a.say(&t) { s.pending = false; reply = Some(format!("⚠️ 交给 agent 失败：{e}")); }
                else if transfer.is_some() { set_status(chat, "转移中"); }
            }
        }
    }
    if transfer.is_some() { send(own, sessions, chat, "正在转移上下文"); }
    if let Some(r) = reply { send_with(own, sessions, chat, &r, reply_buttons); }
    if let Some((c, head, arg)) = ctl {
        let (own, sessions, chat) = (own.clone(), sessions.clone(), chat.to_string());
        std::thread::spawn(move || {
            let (text, btns) = control(&c, &chat, &head, &arg);
            send_with(&own, &sessions, &chat, &text, btns);
        });
    }
}

/// /model and the built-in commands, answered without holding the sessions lock.
fn control(c: &agents::Ctl, chat: &str, head: &str, arg: &str) -> (String, Option<Value>) {
    match head {
        "/model" if arg.is_empty() => match c.models() {
            Err(e) => (format!("⚠️ 拿不到模型列表：{e}"), None),
            Ok(list) if list.is_empty() => ("这个 agent 没有可选的模型。".into(), None),
            Ok(list) => {
                let items: Vec<(String, String)> = list.iter().enumerate().map(|(i, m)| (m.label.clone(), format!("/model {}", i + 1))).collect();
                CHOICES.lock().unwrap().get_or_insert_with(HashMap::new).insert(chat.to_string(), list.into_iter().map(|m| m.value).collect());
                ("选一个模型：".into(), Some(buttons(in_rows(items, 2))))
            }
        },
        "/model" => {
            let value = arg.parse::<usize>().ok().and_then(|i| CHOICES.lock().unwrap().as_ref().and_then(|m| m.get(chat)).and_then(|l| l.get(i.wrapping_sub(1)).cloned())).unwrap_or_else(|| arg.to_string());
            match c.set_model(&value) { Ok(()) => (format!("已换成 {value}。"), None), Err(e) => (format!("⚠️ 没换成：{e}"), None) }
        }
        "/pi_thinking" if arg.is_empty() => {
            let items = ["off", "minimal", "low", "medium", "high", "xhigh"].iter().map(|l| (l.to_string(), format!("/pi_thinking {l}"))).collect();
            ("思考强度：".into(), Some(buttons(in_rows(items, 3))))
        }
        _ => {
            let name = head.trim_start_matches("/pi_").trim_start_matches("/cx_");
            match c.builtin(name, arg) {
                Ok(_) => (match name { "thinking" => format!("思考强度已设为 {arg}。"), "compact" => "已压缩上下文。".into(), _ => "已开新会话。".into() }, None),
                Err(e) => (format!("⚠️ {e}"), None),
            }
        }
    }
}
