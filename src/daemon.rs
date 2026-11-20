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
/// button (they say "/resume 3" and the number is looked up here).
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

/// (process running, turn running). A topic whose output has not all gone out yet counts as running: the 会话 page
/// shows it so, and a deploy waits for it, so a restart cannot lose what is still on its way.
pub fn running(tid: &str) -> Option<(bool, bool)> {
    let owed = owes(tid);
    let g = LIVE.lock().unwrap();
    let ss = g.as_ref()?.lock().unwrap();
    ss.get(tid).map(|s| (s.agent.is_some(), s.pending || owed))
}

/// What each topic still has to send: messages handed to its sender and not yet sent, and topics whose text is still
/// being gathered (output keeps this up to date). pushed: how many jobs each topic's sender has been given; urgent:
/// up to which of them the user is waiting for (the rest of a turn that has ended).
#[derive(Default)]
struct Owed { handed: HashMap<String, usize>, gathering: HashSet<String>, pushed: HashMap<String, u64>, urgent: HashMap<String, u64> }
static OWED: Mutex<Option<Owed>> = Mutex::new(None);

fn owed<T>(f: impl FnOnce(&mut Owed) -> T) -> T { f(OWED.lock().unwrap().get_or_insert_with(Owed::default)) }
fn handed(tid: &str) -> usize { owed(|o| o.handed.get(tid).copied().unwrap_or(0)) }
fn owes(tid: &str) -> bool { owed(|o| o.handed.get(tid).is_some_and(|n| *n > 0) || o.gathering.contains(tid)) }
/// The topic's turn is over: what it has been given so far, up to the answer, goes before running topics' output.
fn hurry(tid: &str) { owed(|o| if let Some(n) = o.pushed.get(tid).copied() { o.urgent.insert(tid.to_string(), n); }) }

type Job = Box<dyn FnOnce() + Send>;

/// Each topic sends through its own thread, its jobs one after another in the order given: a slow send (a file read
/// from a device that does not answer, an upload, brief being retried) holds up only its own topic. Every send still
/// waits its turn in brief's one queue, so the rate limit stays shared; a job is urgent there once `hurry` covers it.
#[derive(Default)]
struct Lanes(HashMap<String, Sender<(u64, Job)>>);

impl Lanes {
    fn push(&mut self, tid: &str, job: impl FnOnce() + Send + 'static) {
        let n = owed(|o| {
            *o.handed.entry(tid.to_string()).or_insert(0) += 1;
            let n = o.pushed.entry(tid.to_string()).or_insert(0);
            *n += 1;
            *n
        });
        let tx = self.0.entry(tid.to_string()).or_insert_with(|| {
            let (tx, rx) = channel::<(u64, Job)>();
            let t = tid.to_string();
            std::thread::spawn(move || for (n, job) in rx {
                let u = t.clone();
                let urgent = move || owed(|o| o.urgent.get(&u).is_some_and(|m| *m >= n));
                // a job that panics must not take the topic's later messages with it, nor leave it running forever
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| brief::with_urgency(urgent, job)));
                if r.is_err() { store::log(&format!("sending to {t} panicked")); }
                owed(|o| if let Some(n) = o.handed.get_mut(&t) { *n -= 1; });
            });
            tx
        });
        // the thread never ends while its sender is here, so this cannot fail
        let _ = tx.send((n, Box::new(job)));
    }
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

/// Send a text; one longer than a message can hold goes out as several, nothing is cut. Buttons go on the last.
fn send_with(own: &Own, sessions: &Sessions, chat: &str, text: &str, btns: Option<Value>) -> Option<u64> {
    let parts = text::pack(vec![text.to_string()]);
    let n = parts.len();
    let mut id = None;
    for (i, p) in parts.into_iter().enumerate() { id = send_one(own, sessions, chat, &p, if i + 1 == n { btns.clone() } else { None }); }
    id
}

fn send_one(own: &Own, sessions: &Sessions, chat: &str, text: &str, btns: Option<Value>) -> Option<u64> {
    let mut body = json!({"chat": chat, "text": text});
    if let Some(b) = &btns { body["buttons"] = b.clone(); }
    let id = post(sessions, chat, "/api/send", body).ok()?["id"].as_u64();
    if let Some(i) = id { own.lock().unwrap().insert(i); }
    id
}

/// One request to brief that puts something in a chat. A network error or a server error is not a reason to drop
/// it: try again until it is delivered (the only things that end it: delivered, topic deleted, or a refusal that
/// retrying cannot change).
fn post(sessions: &Sessions, chat: &str, path: &str, body: Value) -> Result<Value, String> {
    let mut tries = 0u64;
    loop {
        match brief::tg_post(path, body.clone(), true) {
            Ok(v) => return Ok(v),
            Err((410, v)) => { gone(sessions, chat); return Err(format!("410 {v}")); }
            Err((c, v)) if c == 0 || c >= 500 => {
                tries += 1;
                if tries == 1 || tries % 20 == 0 { store::log(&format!("{path} {chat}: {c} {v}; retrying (attempt {tries})")); }
                std::thread::sleep(params::secs("retry_wait_s"));
            }
            Err((c, v)) => { store::log(&format!("{path} {chat} failed {c} {v}")); return Err(format!("{c} {v}")); }
        }
    }
}

/// Telegram takes files up to 50 MB from a bot.
const FILE_MAX: usize = 50_000_000;

/// A file on the topic's device, sent to `chat` with the agent's caption. A relative path is in the topic's folder.
fn send_file(own: &Own, sessions: &Sessions, chat: &str, dev: &str, dir: &str, alt: &str, path: &str) -> Result<(), String> {
    let device = store::device(dev).ok_or_else(|| text::device_missing(dev))?;
    let data = crate::host::read_file(&device, path, dir, FILE_MAX)?;
    if data.len() > FILE_MAX { return Err(text::file_too_big(FILE_MAX / 1_000_000)); }
    let name = path.rsplit(|c| c == '/' || (cfg!(windows) && c == '\\')).next().unwrap_or(path).to_string();
    let v = post(sessions, chat, "/api/send_file", json!({"chat": chat, "name": name, "data": brief::base64(&data), "text": alt}))?;
    if let Some(i) = v["id"].as_u64() { own.lock().unwrap().insert(i); }
    store::log(&format!("file to {chat}: {path}"));
    Ok(())
}

/// Something the agent said, with the files it pointed to (`![说明](路径)`) sent in their places.
fn deliver(own: &Own, sessions: &Sessions, chat: &str, msg: &str) -> Option<u64> {
    let ps = text::pieces(msg);
    if let [text::Piece::Text(t)] = ps.as_slice() { return send(own, sessions, chat, t); }
    let (dev, dir) = { let ss = sessions.lock().unwrap(); ss.get(chat).map(|s| (s.dev.clone(), s.dir.clone())).unwrap_or_default() };
    let mut id = None;
    for p in ps {
        match p {
            text::Piece::Text(t) => id = send(own, sessions, chat, &t),
            text::Piece::File { alt, path } => if let Err(e) = send_file(own, sessions, chat, &dev, &dir, &alt, &path) {
                store::log(&format!("file to {chat} failed: {path}: {e}"));
                send(own, sessions, chat, &text::file_not_sent(&path, &e));
            },
        }
    }
    id
}

/// `exterior send <话题> <内容>` from an agent: signed with the sending topic's name and sent to the other topic
/// (or to any chat brief knows, by its id), files included. Not marked as exterior's own, so the receiving topic's
/// agent gets it.
pub fn relay(from: &str, to: &str, body: &str) -> Result<String, String> {
    let sessions = LIVE.lock().unwrap().clone().ok_or(text::NOT_READY)?;
    let to = to.trim();
    // a topic opened a moment ago (an agent handing off a task) is not in the list until the next refresh: do it now
    if !sessions.lock().unwrap().values().any(|t| !t.deleted && (t.name == to || t.tid == to)) { refresh(&sessions); }
    let (name, dev, dir, target, names) = {
        let ss = sessions.lock().unwrap();
        let s = ss.get(from).filter(|s| !s.deleted).ok_or_else(|| text::relay_from_unknown(from))?;
        let target = ss.values().find(|t| !t.deleted && (t.name == to || t.tid == to)).map(|t| t.tid.clone());
        let names: Vec<String> = ss.values().filter(|t| !t.deleted).map(|t| t.name.clone()).collect();
        (s.name.clone(), s.dev.clone(), s.dir.clone(), target, names.join("、"))
    };
    let target = target.or_else(|| to.contains(':').then(|| to.to_string())).ok_or_else(|| text::relay_to_unknown(to, &names))?;
    if body.trim().is_empty() { return Err(text::RELAY_EMPTY.into()); }
    let own: Own = Arc::new(Mutex::new(HashSet::new()));
    for p in text::pieces(&format!("{name} {} {}", params::get("relay_mark"), body.trim())) {
        match p {
            // a running agent's message to another topic: nobody is waiting for it, it queues like the agents' steps
            text::Piece::Text(t) => for part in text::pack(vec![t]) { brief::with_urgency(|| false, || post(&sessions, &target, "/api/send", json!({"chat": target, "text": part})))?; },
            text::Piece::File { alt, path } => brief::with_urgency(|| false, || send_file(&own, &sessions, &target, &dev, &dir, &alt, &path)).map_err(|e| format!("{path}：{e}"))?,
        }
    }
    store::log(&format!("relay {name} -> {to}"));
    Ok(text::relayed(to))
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

/// A topic's title as a folder name. `windows`: also without the characters Windows does not allow in names.
fn safe(name: &str, windows: bool) -> String {
    let s: String = name.trim().chars().map(|c| if matches!(c, '/' | ':' | '\\' | '\0') || (windows && (matches!(c, '<' | '>' | '"' | '|' | '?' | '*') || c.is_control())) { '_' } else { c }).collect();
    let s = if windows { s.trim_end_matches([' ', '.']).trim_matches('.').to_string() } else { s.trim_matches('.').to_string() };
    if s.is_empty() { text::UNNAMED_TOPIC.into() } else { s }
}

/// The folder of a topic on its device: what its note says; else <root>/<title>; the global topic has its own folder.
fn dir_for(dev: &str, tid: &str, name: &str) -> String {
    if let Some(d) = store::note(tid)["dir"].as_str() { return d.to_string(); }
    let Some(device) = store::device(dev) else { return String::new() };
    if name.trim() == params::get("global_topic") { return crate::prompt::global_dir(&device); }
    let root = crate::prompt::root(&device);
    let taken: Vec<String> = store::notes().iter().filter(|n| n["topic"] != tid).filter_map(|n| n["dir"].as_str().map(String::from)).collect();
    let mut d = format!("{root}/{}", safe(name, cfg!(windows) && crate::host::is_local(&device)));
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
    let run_row = n["resolved"].as_str().and_then(|r| store::agent(&dev, r)).unwrap_or(row.clone());
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

/// exterior's own commands carry its name, so every other command belongs to the topic's agent.
pub const OWN: &str = "exterior_";

/// "design-sync" → "design_sync": Telegram allows only lowercase letters, digits and _ in a command.
fn menu_name(orig: &str) -> String {
    let mut n: String = orig.to_lowercase().chars().map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' { c } else { '_' }).collect();
    n.truncate(COMMAND_NAME_MAX);
    n
}

/// Telegram refuses a menu whose names and descriptions together are longer than about 5200 characters
/// (BOT_COMMANDS_TOO_MUCH; measured 11-19 with 100 commands). Shorten every description alike until it fits.
const MENU_CHARS: usize = 5000;

fn fit_menu(cmds: &mut [Value]) {
    let len = |v: &Value, k: &str| v[k].as_str().unwrap_or("").chars().count();
    let mut cap = COMMAND_DESC_MAX;
    while cap > 8 && cmds.iter().map(|c| len(c, "command") + len(c, "description").min(cap)).sum::<usize>() > MENU_CHARS { cap -= 1; }
    for c in cmds.iter_mut() {
        let d = c["description"].as_str().unwrap_or("").to_string();
        if d.chars().count() > cap { c["description"] = json!(format!("{}…", d.chars().take(cap - 1).collect::<String>())); }
    }
}

/// The "/" menu: exterior's own commands, then the agents' commands without any prefix: claude's as claude lists
/// them, pi's as pi lists them plus the table (agents::BUILTINS), codex's from the table. One entry per name; its
/// grey description says which agents have it. The topic's current agent decides what a command does.
pub fn build_menu() -> Result<usize, String> {
    let dev = store::default_device().ok_or(text::NO_DEVICE)?;
    let has = |k: Kind| dev["agents"].as_array().into_iter().flatten().any(|a| a["kind"] == k.name());
    let mut cmds = vec![
        json!({"command": format!("{OWN}stop"), "description": text::MENU_STOP}),
        json!({"command": format!("{OWN}exit"), "description": text::MENU_EXIT}),
        json!({"command": format!("{OWN}agent"), "description": text::MENU_AGENT}),
    ];
    if has(Kind::Claude) { cmds.push(json!({"command": format!("{OWN}account"), "description": text::MENU_ACCOUNT})); }
    // menu name → (agent kind → the agent's own name for it), and the first description found
    let mut order: Vec<String> = Vec::new();
    let mut map = serde_json::Map::new();
    let mut desc: HashMap<String, String> = HashMap::new();
    let root = crate::prompt::root(&dev);
    for kind in Kind::ALL {
        let Some(row) = dev["agents"].as_array().into_iter().flatten().find(|a| a["kind"] == kind.name()) else { continue };
        let table = agents::BUILTINS.iter().filter(|(k, _)| *k == kind).map(|(_, n)| (n.to_string(), text::builtin_desc(n)));
        let own = agents::commands(&dev, &root, row["command"].as_str().unwrap_or(""), kind);
        for (orig, d) in table.chain(own) {
            let tg = menu_name(&orig);
            if tg.is_empty() || tg.starts_with(OWN) { continue; }
            let e = map.entry(tg.clone()).or_insert_with(|| { order.push(tg.clone()); json!({}) });
            if e[kind.name()].is_null() { e[kind.name()] = json!(orig); }
            let d = d.trim().replace('\n', " ");
            if !d.is_empty() { desc.entry(tg).or_insert(d); }
        }
    }
    if map.is_empty() { return Err(text::NO_COMMANDS.into()); }
    for tg in &order {
        // Telegram shows at most MENU_MAX commands; the rest still work when typed
        if cmds.len() >= MENU_MAX { break; }
        let who: Vec<&str> = Kind::ALL.iter().filter(|k| !map[tg][k.name()].is_null()).map(|k| k.label()).collect();
        let full = format!("{} · {}", who.join(" / "), desc.get(tg).cloned().unwrap_or_else(|| format!("/{tg}")));
        cmds.push(json!({"command": tg, "description": full}));
    }
    fit_menu(&mut cmds);
    let n = cmds.len();
    brief::post("/api/bots/commands", json!({"commands": cmds})).map_err(|(c, v)| format!("{c} {v}"))?;
    store::set_state("menu", Value::Object(map));
    store::set_state("menu_time", json!(store::now()));
    Ok(n)
}

/// Agent output → Telegram. No live editing: what the agent says is sent as it is said. A paragraph is done when the
/// agent pauses (pause_ms); paragraphs of one topic within merge_s go out together as one message, and whatever is
/// left goes out at once when the turn ends. This thread only gathers; each topic's sender (Lanes) sends, so one
/// topic's slow send does not hold up the others. Every send waits its turn in brief's queue.
fn output(rx: Receiver<Ev>, sessions: Sessions, own: Own) {
    /// buf: text still coming in; last: when the last piece came; said: paragraphs waiting to go out, since when
    struct Out { gen: u64, buf: String, last: Instant, said: Vec<String>, since: Option<Instant>, spoke: bool }
    let fresh = |gen| Out { gen, buf: String::new(), last: Instant::now(), said: Vec::new(), since: None, spoke: false };
    fn settle(o: &mut Out) {
        let t = std::mem::take(&mut o.buf);
        if !t.trim().is_empty() { o.said.push(t.trim().to_string()); o.since.get_or_insert(Instant::now()); }
    }
    fn take(o: &mut Out) -> Vec<String> { o.since = None; if !o.said.is_empty() { o.spoke = true; } text::pack(std::mem::take(&mut o.said)) }
    let mut outs: HashMap<String, Out> = HashMap::new();
    let mut lanes = Lanes::default();
    loop {
        // take in everything that has arrived, so no backlog builds up while a send waits for its slot
        let mut batch = Vec::new();
        match rx.recv_timeout(Duration::from_millis(500)) { Ok(e) => batch.push(e), Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}, Err(_) => return }
        while let Ok(e) = rx.try_recv() { batch.push(e); }
        let current = |tid: &str, gen: u64| sessions.lock().unwrap().get(tid).map(|s| s.gen == gen).unwrap_or(false);
        for ev in batch {
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
                let msgs = take(&mut o);
                let body = msgs.join("\n\n");
                // handed over before the turn is marked over, so the topic counts as running until they are sent
                { let (own, sessions, t) = (own.clone(), sessions.clone(), tid.clone());
                  lanes.push(&tid, move || for m in &msgs { if deliver(&own, &sessions, &t, m).is_none() { store::log(&format!("answer for {t} could not be delivered")); } }); }
                // the bookkeeping is done now, not after the sends: a message the user sends meanwhile starts a new
                // turn, which must not be marked over by this one
                let who = {
                    let mut ss = sessions.lock().unwrap();
                    ss.get_mut(&tid).map(|s| { if s.gen == gen { s.pending = false; } s.active = Instant::now(); (s.dev.clone(), s.name.clone()) })
                };
                if let Some((d, name)) = who {
                    if let Some(dev) = store::device(&d) { std::thread::spawn(move || crate::infra::commit(&dev, &name)); }
                }
                if !body.trim().is_empty() && error.is_none() { store::note_set(&tid, "last_answer", json!(body)); }
                if current(&tid, gen) {
                    let n = store::note(&tid);
                    if let Some(why) = n["exhausted"].as_str() {
                        // out of quota: offer the other accounts; the message is kept and re-sent after the switch
                        store::note_set(&tid, "retry_msg", n["turn_msg"].clone());
                        store::note_set(&tid, "exhausted", Value::Null);
                        let acct = n["resolved"].as_str().or(n["agent"].as_str()).unwrap_or("").to_string();
                        // after the answer: in the topic's own line
                        let (own, sessions, t, why) = (own.clone(), sessions.clone(), tid.clone(), why.to_string());
                        lanes.push(&tid, move || {
                            let dev = store::note(&t)["device"].as_str().and_then(store::device);
                            let b = dev.and_then(|d| account_buttons(&d, &acct).ok()).map(|(_, b)| b);
                            send_with(&own, &sessions, &t, &text::sys_warn(&text::exhausted(&acct, &why)), b);
                        });
                    }
                    set_status(&tid, if error.is_some() { st::ERROR } else { st::IDLE });
                    store::note_set(&tid, "last_error", error.map(Value::String).unwrap_or(Value::Null));
                    store::note_set(&tid, "from", Value::Null);
                    store::note_set(&tid, "turn_msg", Value::Null);
                    store::note_set(&tid, "turn_resent", Value::Null);
                }
                hurry(&tid);
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
            Ev::Step { tid, gen, title, body } => {
                if current(&tid, gen) {
                    let o = outs.entry(tid).or_insert_with(|| fresh(gen));
                    if gen > o.gen { *o = fresh(gen); }
                    settle(o); o.said.extend(text::step(&title, &body)); o.since.get_or_insert(Instant::now());
                }
            }
            Ev::Exhausted { tid, gen, why } => {
                if current(&tid, gen) { store::note_set(&tid, "exhausted", json!(why)); }
            }
            Ev::Resolved { tid, gen, name } => {
                if current(&tid, gen) { store::note_set(&tid, "resolved", json!(name)); }
            }
            Ev::Exited { tid, gen, why } => {
                // what was left goes out first, handed over before the turn is marked over
                if let Some(mut o) = outs.remove(&tid) {
                    settle(&mut o);
                    if !o.said.is_empty() {
                        o.said.push(text::sys_warn(text::CUT_SUFFIX));
                        let (own, sessions, t, msgs) = (own.clone(), sessions.clone(), tid.clone(), take(&mut o));
                        lanes.push(&tid, move || for b in msgs { deliver(&own, &sessions, &t, &b); });
                    }
                }
                let warn = {
                    let mut ss = sessions.lock().unwrap();
                    match ss.get_mut(&tid) {
                        Some(s) if s.gen == gen => { s.agent = None; let w = s.pending && !s.deleted; s.pending = false; w }
                        _ => false,
                    }
                };
                if current(&tid, gen) || warn {
                    let why = agents::last_lines(&why);
                    if warn {
                        store::log(&format!("agent exited mid-turn in {tid}: {why}"));
                        set_status(&tid, st::ERROR);
                        store::note_set(&tid, "last_error", json!(why));
                        // the device may have rebooted or dropped off the network: keep the message and carry on
                        // once it answers again (see retry_waiting); only once per turn, like after a restart
                        let n = store::note(&tid);
                        let says = match n["turn_msg"].as_str().filter(|_| n["turn_resent"] != json!(true)) {
                            Some(m) => {
                                store::note_set(&tid, "wait_msg", json!(m));
                                store::note_set(&tid, "wait_since", json!(store::now()));
                                text::exited_waiting(&why)
                            }
                            None => text::exited(&why),
                        };
                        let (own, sessions, t) = (own.clone(), sessions.clone(), tid.clone());
                        lanes.push(&tid, move || { send(&own, &sessions, &t, &says); });
                    } else if store::note(&tid)["status"] != st::DELETED {
                        set_status(&tid, st::STOPPED);
                    }
                }
                hurry(&tid);
            }
        }
        }
        // then one message for each topic whose words have waited merge_s and whose sender is free, longest waiting
        // first. Under load each topic's text simply grows while its last message waits for its turn, so the total
        // stays within the queue's rate however much the agents say. While the queue is busy, a topic whose turn is
        // still running waits busy_merge_s: fewer, bigger messages, so the slots go to answers the user waits for.
        let pause = Duration::from_millis(params::num("pause_ms"));
        let merge = window(&sessions);
        for o in outs.values_mut() { if !o.buf.is_empty() && o.last.elapsed() > pause { settle(o); } }
        let mut ready: Vec<(Instant, String)> = outs.iter().filter_map(|(t, o)| o.since.filter(|s| s.elapsed() >= merge(t)).map(|s| (s, t.clone()))).collect();
        ready.sort();
        for (_, tid) in ready {
            if handed(&tid) > 0 { continue; }
            let o = outs.get_mut(&tid).unwrap();
            let mut msgs = take(o);
            if msgs.is_empty() { continue; }
            let first = msgs.remove(0);
            // what did not fit stays first in line for this topic
            if !msgs.is_empty() { o.said = msgs; o.since = Instant::now().checked_sub(merge(&tid)).or(o.since); }
            let (own, sessions, t) = (own.clone(), sessions.clone(), tid.clone());
            lanes.push(&tid, move || { deliver(&own, &sessions, &t, &first); });
        }
        // text still being gathered is owed too
        let gathering: HashSet<String> = outs.iter().filter(|(_, o)| !o.said.is_empty() || !o.buf.trim().is_empty()).map(|(t, _)| t.clone()).collect();
        owed(|o| o.gathering = gathering);
    }
}

/// How long each topic's paragraphs are gathered into one message right now (see output).
fn window(sessions: &Sessions) -> impl Fn(&str) -> Duration {
    let (merge, busy) = (params::secs("merge_s"), brief::busy());
    let long = params::secs("busy_merge_s").max(merge);
    let running: HashSet<String> = if busy { sessions.lock().unwrap().values().filter(|s| s.pending).map(|s| s.tid.clone()).collect() } else { HashSet::new() };
    move |tid: &str| if running.contains(tid) { long } else { merge }
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
            scheduled(&sessions, &own, &tx);
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

/// The other claude accounts with their usage, as buttons that switch to them.
fn account_buttons(dev: &Value, cur: &str) -> Result<(String, Value), String> {
    let path = params::get("cc_usage");
    let raw = if crate::host::is_local(dev) { crate::host::read_local(&path) } else { crate::host::run(dev, &format!("cat {path}"), 20) }
        .map_err(|e| text::accounts_failed(&e))?;
    let mut age = 0;
    let mut items = Vec::new();
    for l in raw.lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        if l.starts_with('#') { if let Some(t) = f.last().and_then(|t| t.parse::<u64>().ok()) { age = store::now().saturating_sub(t) / 60; } continue; }
        if f.len() < 4 || f[3] != "ok" || format!("cc{}", f[0]) == cur { continue; }
        if store::agent(dev, &format!("cc{}", f[0])).is_none() { continue; }
        items.push((text::account_label(f[0], f[1], f[2]), format!("/{OWN}account {}", f[0])));
    }
    if items.is_empty() { return Err(text::NO_ACCOUNTS.into()); }
    Ok((text::choose_account(cur, age), buttons(in_rows(items, 1))))
}

/// Move the topic's claude session from one account to another (copy its record, which lives in the account's
/// own config folder), resume it there, and re-send the message that ran out of quota, if any.
fn switch_account(sessions: &Sessions, own: &Own, tx: &Sender<Ev>, chat: &str, dev: Option<Value>, from: &str, to: &str, sid: Option<String>) {
    let Some(dev) = dev else { send(own, sessions, chat, &text::sys_warn(text::NO_DEVICE)); return };
    // the copy below is a sh script
    if crate::host::is_local(&dev) && cfg!(windows) { send(own, sessions, chat, &text::sys_warn(text::ACCOUNT_SWITCH_WINDOWS)); return; }
    if let Some(id) = &sid {
        let dir = |name: &str| params::get("claude_dir").replace("{name}", name);
        let script = format!("f=$(ls {src}/projects/*/{id}.jsonl 2>/dev/null | head -1); [ -n \"$f\" ] || {{ echo 'no record of {id} under {src}'; exit 3; }}; \
             d={dst}/projects/$(basename \"$(dirname \"$f\")\"); mkdir -p \"$d\" && cp \"$f\" \"$d/\" && {{ [ ! -d \"${{f%.jsonl}}\" ] || cp -R \"${{f%.jsonl}}\" \"$d/\"; }}",
             src = dir(from), dst = dir(to));
        if let Err(e) = crate::host::run(&dev, &script, 60) { send(own, sessions, chat, &text::sys_warn(&text::account_copy_failed(&e))); return; }
    }
    store::note_set(chat, "resolved", json!(to));
    store::log(&format!("{chat}: account {from} -> {to}"));
    let retry = store::note(chat)["retry_msg"].as_str().map(String::from);
    store::note_set(chat, "retry_msg", Value::Null);
    match retry {
        Some(m) => {
            send(own, sessions, chat, &text::sys(&text::account_switched_resend(to)));
            let line = if m.starts_with('/') { m } else { text::resent(&m) };
            handle(sessions, own, tx, chat, &line);
        }
        None => { send(own, sessions, chat, &text::sys(&text::account_switched(to))); }
    }
}

/// The 定时消息 setting: "HH:MM|话题名|内容 || …". Each fires once a day, at or up to SCHEDULE_LATE minutes after
/// its time in exterior's time zone (so a restart around that time does not skip it, and setting one up in the
/// evening does not fire it at once). The content goes to the topic's agent as a message from "定时"; exterior only
/// passes it on.
const SCHEDULE_LATE: i64 = 120;

fn schedules(spec: &str) -> Vec<(i64, String, String)> {
    spec.split("||").filter_map(|e| {
        let mut f = e.trim().splitn(3, '|');
        let (t, topic, content) = (f.next()?.trim(), f.next()?.trim(), f.next()?.trim());
        let (h, m) = t.split_once(':')?;
        let (h, m): (i64, i64) = (h.trim().parse().ok()?, m.trim().parse().ok()?);
        (h < 24 && m < 60 && !topic.is_empty() && !content.is_empty()).then(|| (h * 60 + m, topic.to_string(), content.to_string()))
    }).collect()
}

/// Which schedules are due now: (minute of day, day number) in local time → entries to fire, given what fired today.
fn due(list: &[(i64, String, String)], minute: i64, day: i64, fired: &Value) -> Vec<(String, String, String)> {
    list.iter().filter(|(at, _, _)| minute >= *at && minute - at <= SCHEDULE_LATE).filter_map(|(at, topic, content)| {
        let key = format!("{at}|{topic}|{content}");
        (fired[&key].as_i64() != Some(day)).then(|| (key, topic.clone(), content.clone()))
    }).collect()
}

fn scheduled(sessions: &Sessions, own: &Own, tx: &Sender<Ev>) {
    let list = schedules(&params::get("schedules"));
    if list.is_empty() { return; }
    let local = store::now() as i64 + crate::web::offset();
    let mut fired = store::state("schedules");
    if !fired.is_object() { fired = json!({}); }
    for (key, topic, content) in due(&list, local % 86400 / 60, local / 86400, &fired) {
        fired[&key] = json!(local / 86400);
        store::set_state("schedules", fired.clone());
        let tid = sessions.lock().unwrap().values().find(|s| !s.deleted && s.name == topic).map(|s| s.tid.clone());
        let Some(tid) = tid else { store::log(&format!("schedule: no topic named {topic}")); continue };
        store::log(&format!("schedule -> {topic}: {}", content.chars().take(60).collect::<String>()));
        send(own, sessions, &tid, &text::sys(&text::scheduled(&content)));
        handle(sessions, own, tx, &tid, &format!("{} {} {content}", text::SCHEDULE_FROM, params::get("relay_mark")));
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
            let ok = *up.entry(dev.clone()).or_insert_with(|| store::device(&dev).map(|d| crate::host::alive(&d)).unwrap_or(false));
            if !ok { continue; }
            // the user may have moved on in the meantime: then his newer message stands, nothing to repeat
            if store::note(&tid)["wait_msg"].as_str() != Some(msg.as_str()) { continue; }
            // what the topic still has to send (the notice that it is waiting, among others) goes first: next round
            if owes(&tid) { continue; }
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
    let mut reply_raw = false;
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
        // a command: exterior's own (exterior_…), else the current agent's: from the table (pi, codex) or passed on
        // as the agent's own command; anything the agent does not have goes to it as it is
        let own_cmd = head.strip_prefix('/').and_then(|h| h.strip_prefix(OWN));
        let agent_cmd: Option<String> = match (head.strip_prefix('/'), cur_kind) {
            (Some(h), Some(k)) if own_cmd.is_none() => store::state("menu")[h][k.name()].as_str().map(String::from),
            _ => None,
        };
        let builtin = match (&agent_cmd, cur_kind) { (Some(o), Some(k)) => agents::builtin(k, o).map(|n| (k, n)), _ => None };
        if own_cmd == Some("exit") {
            if let Some(a) = s.agent.as_mut() { a.stop(); a.kill(); }
            s.agent = None; s.gen += 1; s.pending = false;
            store::note_set(chat, "session", Value::Null);
            store::note_set(chat, "resolved", Value::Null);
            store::note_set(chat, "from", Value::Null);
            set_status(chat, st::NEW);
            reply = Some(text::session_ended(&cur));
        } else if let Some((k, name)) = builtin {
            if agents::is_quit(name) {
                if let Some(a) = s.agent.as_mut() { a.stop(); a.kill(); }
                s.agent = None; s.gen += 1; s.pending = false;
                set_status(chat, st::STOPPED);
                reply = Some(text::QUIT.into());
            } else if name == "copy" && k == Kind::Codex {
                match n["last_answer"].as_str().filter(|a| !a.trim().is_empty()) { Some(a) => { reply = Some(a.to_string()); reply_raw = true; } None => reply = Some(text::NOTHING_TO_COPY.into()) }
            } else {
                if s.agent.is_none() {
                    if let Err(e) = start_agent(s, tx) { reply = Some(text::start_failed(&cur, &e)); }
                }
                if let Some(a) = s.agent.as_ref() { ctl = Some((a.ctl(), k, name, arg.clone())); }
            }
        } else if own_cmd == Some("stop") {
            match s.agent.as_mut() { Some(a) if s.pending => a.stop(), _ => reply = Some(text::NOTHING_RUNNING.into()) }
        } else if own_cmd == Some("agent") && arg.is_empty() {
            reply = Some(text::current_agent(&cur));
            let names: Vec<(String, String)> = agent_names(&s.dev).into_iter().filter(|n| *n != cur).map(|n| (n.clone(), format!("/{OWN}agent {n}"))).collect();
            reply_buttons = Some(buttons(in_rows(names, 4)));
        } else if own_cmd == Some("agent") {
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
        } else if own_cmd == Some("account") {
            let acct = n["resolved"].as_str().unwrap_or(&cur).to_string();
            if cur_kind != Some(Kind::Claude) { reply = Some(text::wrong_agent(Kind::Claude.label(), &cur)); }
            else if arg.is_empty() {
                let (own, sessions, chat, d) = (own.clone(), sessions.clone(), chat.to_string(), dev.clone());
                std::thread::spawn(move || {
                    let r = d.ok_or_else(|| text::NO_DEVICE.to_string()).and_then(|d| account_buttons(&d, &acct));
                    match r {
                        Ok((t, b)) => { send_with(&own, &sessions, &chat, &text::sys(&t), Some(b)); }
                        Err(e) => { send(&own, &sessions, &chat, &text::sys_warn(&e)); }
                    }
                });
            } else {
                let want = if arg.starts_with("cc") { arg.clone() } else { format!("cc{arg}") };
                if dev.as_ref().and_then(|d| store::agent(d, &want)).is_none() { reply = Some(text::no_such_account(&want)); }
                else if want == acct { reply = Some(text::already_account(&want)); }
                else {
                    if let Some(a) = s.agent.as_mut() { a.stop(); a.kill(); }
                    s.agent = None; s.gen += 1; s.pending = false;
                    set_status(chat, st::STOPPED);
                    let (own, sessions, tx, chat, d) = (own.clone(), sessions.clone(), tx.clone(), chat.to_string(), dev.clone());
                    let sid = n["session"].as_str().map(String::from);
                    std::thread::spawn(move || switch_account(&sessions, &own, &tx, &chat, d, &acct, &want, sid));
                }
            }
        } else if let Some(c) = own_cmd {
            reply = Some(text::no_such_command(&format!("{OWN}{c}")));
        } else if let Some(orig) = &agent_cmd {
            // the agent's own command (claude's; pi's extensions, templates, skills), under the agent's own name.
            // claude's effort is remembered so a restarted process keeps it
            if cur_kind == Some(Kind::Claude) && orig == "effort" && agents::CLAUDE_EFFORTS.contains(&arg.as_str()) { store::note_set(chat, "effort", json!(arg)); }
            say = Some(format!("/{orig}{}{arg}", if arg.is_empty() { "" } else { " " }));
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
    if let Some(r) = reply { let r = if reply_raw { r } else { text::as_sys(&r) }; send_with(own, sessions, chat, &r, reply_buttons); }
    if let Some((c, kind, name, arg)) = ctl {
        let (own, sessions, chat) = (own.clone(), sessions.clone(), chat.to_string());
        std::thread::spawn(move || {
            let a = control(&c, &chat, kind, name, &arg);
            if let Some(id) = &a.session { store::note_set(&chat, "session", json!(id)); }
            if a.started {
                // /review runs as a turn: its result arrives like an answer
                if let Some(s) = sessions.lock().unwrap().get_mut(&chat) { s.pending = true; }
                set_status(&chat, st::RUNNING);
            }
            let t = if a.raw { a.text.clone() } else { text::as_sys(&a.text) };
            send_with(&own, &sessions, &chat, &t, a.buttons);
        });
    }
}

/// What a table command (pi, codex) answers in the topic, and what it changed.
/// raw: what the agent itself answered, shown as it is; otherwise a notice of exterior's own (a choice to make,
/// or a failure).
struct Answer { text: String, buttons: Option<Value>, session: Option<String>, started: bool, raw: bool }

impl Answer {
    fn say(t: impl Into<String>) -> Answer { Answer { text: t.into(), buttons: None, session: None, started: false, raw: false } }
    fn raw(t: impl Into<String>) -> Answer { Answer { raw: true, ..Answer::say(t) } }
    fn choose(t: impl Into<String>, b: Value) -> Answer { Answer { buttons: Some(b), ..Answer::say(t) } }
}

/// The agent's answer to a request, as it gave it; or a notice that it gave none.
fn agent_said(kind: Kind, name: &str, d: &Value) -> Answer {
    let empty = d.is_null() || d.as_object().map(|o| o.keys().all(|k| k.starts_with('_'))).unwrap_or(false) || d.as_str().map(|t| t.trim().is_empty()).unwrap_or(false);
    if empty { return Answer::say(text::no_output(kind.label(), name)); }
    Answer::raw(match d.as_str() { Some(t) => t.to_string(), None => text::shown(&text::show(d)) })
}

/// The table commands (agents::BUILTINS), answered without holding the sessions lock. What the agent answers is
/// shown as it is; exterior adds only the buttons for a choice the agent offers.
fn control(c: &agents::Ctl, chat: &str, kind: Kind, name: &str, arg: &str) -> Answer {
    let cmd = name;
    match name {
        "model" if arg.is_empty() => match c.models() {
            Err(e) => Answer::say(text::models_failed(&e)),
            Ok(list) if list.is_empty() => Answer::say(text::NO_MODELS),
            Ok(list) => Answer::choose(text::CHOOSE_MODEL, choice_buttons(chat, cmd, list.into_iter().map(|m| (m.label, m.value)).collect(), 2)),
        },
        "model" => {
            let value = match arg.parse::<usize>() {
                Ok(i) => match chosen(chat, cmd, arg).or_else(|| c.models().ok().and_then(|l| l.get(i.wrapping_sub(1)).map(|m| m.value.clone()))) {
                    Some(v) => v,
                    None => return Answer::say(text::no_model_at(i, cmd)),
                },
                Err(_) => arg.to_string(),
            };
            match c.set_model(&value) {
                // codex takes the model with each turn: exterior keeps it, codex has nothing to say yet
                Ok(Value::Null) if kind == Kind::Codex => Answer::say(text::codex_model_kept(&value)),
                Ok(d) => agent_said(kind, name, &d),
                Err(e) => Answer::say(text::model_failed(&e)),
            }
        }
        "thinking" if arg.is_empty() => match c.thinking_levels() {
            Ok(levels) => Answer::choose(text::CHOOSE_THINKING, buttons(in_rows(levels.into_iter().map(|l| (l.clone(), format!("/{cmd} {l}"))).collect(), 3))),
            Err(e) => Answer::say(text::thinking_failed(&e)),
        },
        "name" | "rename" if arg.is_empty() => Answer::say(text::name_needed(cmd)),
        _ => match c.builtin(name, &chosen(chat, cmd, arg).unwrap_or_else(|| arg.to_string())) {
            Err(e) => Answer::say(text::failed(&e)),
            Ok(d) => {
                let mut a = match name {
                    // the choices the agent offers, as buttons
                    "resume" if arg.is_empty() && kind == Kind::Pi => {
                        let dir = d["_dir"].as_str().unwrap_or("");
                        let items: Vec<(String, String)> = d["output"].as_str().unwrap_or("").lines().map(str::trim).filter(|f| f.ends_with(".jsonl"))
                            .map(|f| (f.chars().take(40).collect(), format!("{dir}/{f}"))).collect();
                        if items.is_empty() { agent_said(kind, name, &d) } else { Answer::choose(text::CHOOSE_SESSION, choice_buttons(chat, cmd, items, 1)) }
                    }
                    "resume" if arg.is_empty() => {
                        let items: Vec<(String, String)> = d["data"].as_array().into_iter().flatten().filter_map(|t| {
                            let id = t["id"].as_str()?;
                            let label = t["name"].as_str().or(t["preview"].as_str()).unwrap_or(id).chars().take(40).collect();
                            Some((label, id.to_string()))
                        }).collect();
                        if items.is_empty() { agent_said(kind, name, &d) } else { Answer::choose(text::CHOOSE_SESSION, choice_buttons(chat, cmd, items, 1)) }
                    }
                    "fork" if arg.is_empty() && kind == Kind::Pi => {
                        let items: Vec<(String, String)> = d["messages"].as_array().into_iter().flatten().filter_map(|m| {
                            Some((m["text"].as_str()?.chars().take(30).collect(), m["entryId"].as_str()?.to_string()))
                        }).collect();
                        if items.is_empty() { agent_said(kind, name, &d) } else { Answer::choose(text::CHOOSE_FORK, choice_buttons(chat, cmd, items, 1)) }
                    }
                    // what a command printed, as it printed it
                    "diff" | "copy" => {
                        let out = d["stdout"].as_str().or(d["text"].as_str()).unwrap_or("").trim().to_string();
                        if out.is_empty() { agent_said(kind, name, &d) } else { Answer::raw(out) }
                    }
                    _ => agent_said(kind, name, &d),
                };
                // /review runs as a turn: its result arrives like an answer
                if name == "review" { a.started = true; }
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
    fn nothing_is_cut() {
        // a long answer and a huge tool output come out whole, across several messages
        let long: String = (0..5000).map(|i| format!("第 {i} 行\n")).collect();
        let parts = text::step("🔧 Bash · cat big.log", &long);
        assert!(parts.len() > 1 && parts[0].contains("（1/") && parts[0].starts_with("<details><summary><sub><i>"));
        let msgs = text::pack([vec!["开头".to_string()], parts, vec![long.clone()]].concat());
        assert!(msgs.iter().all(|m| m.chars().count() <= text::MESSAGE_MAX));
        let all = msgs.join("\n");
        for i in [0, 2500, 4999] { assert_eq!(all.matches(&format!("第 {i} 行\n")).count(), 2, "line {i}"); }
        // a blank line early on does not make it cut mid-line
        let body = format!("\n\n── 输出 ──\n{}", "一行代码;\n".repeat(20000));
        for part in text::split_text(&body, 28000) { assert!(part.ends_with(';') || part.ends_with(";\n"), "{}", &part[part.len() - 20..]); }
        // code fences inside the output do not break the block
        assert!(text::step("x", "a ``` b")[0].contains("````\na ``` b\n````"));
        // a long notice keeps its whole text, folded
        let n = text::sys_warn(&format!("出错了\n{}", "细节 ".repeat(100)));
        assert!(n.starts_with("<footer>⚠️ exterior · 出错了</footer>") && n.contains(&"细节 ".repeat(100).trim_end().to_string()));
    }

    #[test]
    fn schedule_times() {
        let l = schedules(" 07:30|dream|整理 inbox || bad || 25:00|x|y || 8:05|全局|a|b ");
        assert_eq!(l, vec![(450, "dream".into(), "整理 inbox".into()), (485, "全局".into(), "a|b".into())]);
        let none = json!({});
        assert!(due(&l, 449, 10, &none).is_empty());
        assert_eq!(due(&l, 450, 10, &none).len(), 1);
        assert_eq!(due(&l, 450 + SCHEDULE_LATE, 10, &none).len(), 2);
        assert!(due(&l, 450 + SCHEDULE_LATE + 1, 10, &none).iter().all(|d| d.1 == "全局"));
        // once a day
        let fired = json!({"450|dream|整理 inbox": 10});
        assert!(due(&l, 460, 10, &fired).is_empty());
        assert_eq!(due(&l, 460, 11, &fired).len(), 1);
    }

    #[test]
    fn parsing() {
        assert_eq!(strip_bot_mention("/model@exterior_bot 3"), "/model 3");
        assert_eq!(strip_bot_mention("/stop@x"), "/stop");
        assert_eq!(strip_bot_mention("邮件 a@b.c"), "邮件 a@b.c");
        assert_eq!(relay_split("测试 → 你好"), Some(("测试", "你好")));
        assert_eq!(relay_split("没有分隔符"), None);
        assert_eq!(agents::builtin(Kind::Pi, "thinking"), Some("thinking"));
        assert_eq!(agents::builtin(Kind::Codex, "review"), Some("review"));
        assert_eq!(agents::builtin(Kind::Claude, "model"), None);
        assert_eq!(agents::builtin(Kind::Codex, "thinking"), None);
        assert_eq!(menu_name("design-sync"), "design_sync");
        let mut m: Vec<Value> = (0..100).map(|i| json!({"command": format!("command_{i}"), "description": "说".repeat(100)})).collect();
        fit_menu(&mut m);
        assert!(m.iter().map(|c| c["command"].as_str().unwrap().chars().count() + c["description"].as_str().unwrap().chars().count()).sum::<usize>() <= MENU_CHARS);
        assert!(m[0]["description"].as_str().unwrap().ends_with('…'));
        assert_eq!(menu_name("skill:Foo.Bar"), "skill_foo_bar");
        assert_eq!(agents::review_target("")["type"], "uncommittedChanges");
        assert_eq!(agents::review_target("abc1234")["type"], "commit");
        assert_eq!(agents::review_target("main")["branch"], "main");
        assert_eq!(text::status::from_legacy("运行中"), Some(text::status::RUNNING));
        assert_eq!(text::status::label("stopped"), "进程已关闭");
    }
}

#[cfg(test)]
mod text_tests {
    #[test]
    fn status() {
        assert_eq!(crate::text::window(Some(300)), "5 小时");
        assert_eq!(crate::text::window(Some(10080)), "7 天");
    }
}

#[cfg(test)]
mod more_tests {
    #[test]
    fn helpers() {
        assert_eq!(crate::brief::base64(b"hello"), "aGVsbG8=");
        assert_eq!(crate::brief::base64(b"ab"), "YWI=");
        assert_eq!(crate::brief::base64(b"abc"), "YWJj");
        let b = super::choice_buttons("tgbot:1", "resume", vec![("a".into(), "x".repeat(80)), ("b".into(), "short".into())], 1);
        assert_eq!(b[0][0]["data"], "/resume 1");
        assert_eq!(b[1][0]["data"], "/resume short");
        assert_eq!(super::chosen("tgbot:1", "resume", "1").as_deref(), Some("x".repeat(80).as_str()));
        assert!(crate::text::show(&serde_json::json!({"a": 1, "b": {"c": "d"}, "_x": 2})).contains("  c：d"));
        // what an agent answers is shown as it is; nothing at all is said to be nothing
        let a = super::agent_said(crate::agents::Kind::Pi, "compact", &serde_json::json!({"summary": "s", "tokensBefore": 5}));
        assert!(a.raw && a.text.contains("summary：s") && a.text.starts_with("```"));
        let a = super::agent_said(crate::agents::Kind::Codex, "compact", &serde_json::json!({}));
        assert!(!a.raw && a.text.contains("Codex 执行了 /compact"));
    }

    #[test]
    fn files_in_answers() {
        use crate::text::{pieces, Piece};
        let file = |a: &str, p: &str| Piece::File { alt: a.into(), path: p.into() };
        assert_eq!(pieces("没有图"), vec![Piece::Text("没有图".into())]);
        assert_eq!(pieces("看图：\n![结果](/tmp/a b.png)\n说明"), vec![Piece::Text("看图：".into()), file("结果", "/tmp/a b.png"), Piece::Text("说明".into())]);
        assert_eq!(pieces("![x](<files/a (1).png>)"), vec![file("x", "files/a (1).png")]);
        // web images, code and inline code are left alone
        let keep = ["![x](https://a.com/b.png)", "```\n![x](/a.png)\n```", "`![x](/a.png)`", "````\n```\n![x](/a.png)\n```\n````"];
        for k in keep { assert_eq!(pieces(k), vec![Piece::Text(k.into())], "{k}"); }
        // a step's folded content is in a fence: never sent as a file
        let step = crate::text::step("🔧 Read", "![x](/a.png)").join("");
        assert_eq!(pieces(&step), vec![Piece::Text(step.clone())]);
    }
}

#[cfg(test)]
mod lane_tests {
    use super::*;

    #[test]
    fn a_failing_send_does_not_stop_its_topic() {
        let mut lanes = Lanes::default();
        let seen = Arc::new(Mutex::new(Vec::new()));
        lanes.push("tgbot:panic", || panic!("boom"));
        let s = seen.clone();
        lanes.push("tgbot:panic", move || s.lock().unwrap().push(2));
        let t = Instant::now();
        while handed("tgbot:panic") > 0 && t.elapsed() < Duration::from_secs(5) { std::thread::sleep(Duration::from_millis(20)); }
        assert_eq!(*seen.lock().unwrap(), vec![2]);
        assert!(!owes("tgbot:panic"));
    }

    type Got = Arc<Mutex<Vec<(String, String)>>>;

    /// A fake brief that answers each request on its own thread; uploading the topic "tgbot:slow"'s file takes 3 s.
    /// It records what arrived (chat, text or file), in the order it was answered. Returns its port.
    fn fake_brief(got: &Got) -> u16 {
        let srv = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = srv.server_addr().to_ip().unwrap().port();
        let g = got.clone();
        std::thread::spawn(move || for mut r in srv.incoming_requests() {
            let g = g.clone();
            std::thread::spawn(move || {
                let mut b = String::new();
                let _ = std::io::Read::read_to_string(r.as_reader(), &mut b);
                let v: Value = serde_json::from_str(&b).unwrap_or(Value::Null);
                let what = v["name"].as_str().map(|n| format!("file {n}")).or(v["text"].as_str().map(String::from)).unwrap_or_default();
                if r.url() == "/api/send_file" && v["chat"] == "tgbot:slow" { std::thread::sleep(Duration::from_secs(3)); }
                g.lock().unwrap().push((v["chat"].as_str().unwrap_or("").to_string(), what));
                let _ = r.respond(tiny_http::Response::from_string(r#"{"id":1}"#));
            });
        });
        port
    }

    fn test_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("exterior-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("EXTERIOR_VAR", &dir);
        dir
    }

    fn of(got: &Got, chat: &str) -> Vec<String> { got.lock().unwrap().iter().filter(|(c, _)| c == chat).map(|(_, w)| w.clone()).collect() }
    fn wait(what: &dyn Fn() -> bool) { let t = Instant::now(); while !what() && t.elapsed() < Duration::from_secs(10) { std::thread::sleep(Duration::from_millis(20)); } }

    /// Sessions for these topics (turn running or not) and an output thread for them.
    fn topics(dir: &std::path::Path, ts: &[(&str, bool)]) -> (Sessions, Sender<Ev>) {
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
        for (t, pending) in ts {
            sessions.lock().unwrap().insert(t.to_string(), Session { tid: t.to_string(), name: t.to_string(), dev: "here".into(), dir: dir.display().to_string(),
                agent: None, gen: 1, pending: *pending, deleted: false, active: Instant::now() });
        }
        *LIVE.lock().unwrap() = Some(sessions.clone());
        let (tx, rx) = channel();
        { let s = sessions.clone(); std::thread::spawn(move || output(rx, s, Arc::new(Mutex::new(HashSet::new())))); }
        (sessions, tx)
    }

    fn done(tid: &str, t: &str) -> Ev { Ev::Done { tid: tid.into(), gen: 1, text: Some(t.into()), stopped: false, error: None } }

    #[test]
    fn slow_topic_does_not_hold_up_others() {
        let _g = store::TEST.lock().unwrap_or_else(|e| e.into_inner());
        let dir = test_dir();
        std::fs::write(dir.join("pic.png"), b"png").unwrap();
        let got: Got = Arc::default();
        let port = fake_brief(&got);
        store::set_config(&json!({"brief": {"url": format!("http://127.0.0.1:{port}"), "key": crate::crypt::seal("k")},
            "params": {"tg_gap_ms": "0", "merge_s": "0", "pause_ms": "0"},
            "devices": [{"name": "here", "host": "local", "infra": dir.join("no-knowledge").display().to_string()}]}));
        let (sessions, tx) = topics(&dir, &[("tgbot:slow", true), ("tgbot:fast", true)]);
        tx.send(done("tgbot:slow", "a1\n\n![图](pic.png)\n\na2")).unwrap();
        tx.send(done("tgbot:fast", "b1")).unwrap();
        let of = |chat: &str| of(&got, chat);
        // the fast topic is answered while the slow one's file is still uploading
        wait(&|| !of("tgbot:fast").is_empty());
        assert_eq!(of("tgbot:fast"), vec!["b1"]);
        assert_eq!(of("tgbot:slow"), vec!["a1"]);
        // its turn is over, but what it still has to send keeps it running (a deploy waits for it)
        assert!(!sessions.lock().unwrap()["tgbot:slow"].pending);
        assert_eq!(running("tgbot:slow"), Some((false, true)));
        // within a topic the order stays: the text after the file waits for the file
        wait(&|| of("tgbot:slow").len() == 3);
        assert_eq!(of("tgbot:slow"), vec!["a1", "file pic.png", "a2"]);
        wait(&|| running("tgbot:slow") == Some((false, false)));
        assert_eq!(running("tgbot:slow"), Some((false, false)));
        assert_eq!(running("tgbot:fast"), Some((false, false)));
    }

    #[test]
    fn finished_topic_goes_first() {
        let _g = store::TEST.lock().unwrap_or_else(|e| e.into_inner());
        let dir = test_dir();
        let got: Got = Arc::default();
        let port = fake_brief(&got);
        store::set_config(&json!({"brief": {"url": format!("http://127.0.0.1:{port}"), "key": crate::crypt::seal("k")}, "params": {"tg_gap_ms": "300"}}));
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
        let own: Own = Arc::default();
        let mut lanes = Lanes::default();
        let push = |lanes: &mut Lanes, chat: &'static str, t: String| { let (o, s) = (own.clone(), sessions.clone()); lanes.push(chat, move || { send(&o, &s, chat, &t); }); };
        // a running topic has a lot to say; another one, also running, has two steps queued behind it
        for i in 1..=6 { push(&mut lanes, "tgbot:busy", format!("b{i}")); }
        std::thread::sleep(Duration::from_millis(50));
        push(&mut lanes, "tgbot:ans", "a1".into());
        push(&mut lanes, "tgbot:ans", "a2".into());
        // then its turn ends: its answer is pushed and all it has goes before the busy topic's steps
        std::thread::sleep(Duration::from_millis(50));
        push(&mut lanes, "tgbot:ans", "a3".into());
        hurry("tgbot:ans");
        wait(&|| got.lock().unwrap().len() == 9);
        let order: Vec<String> = got.lock().unwrap().iter().map(|(_, w)| w.clone()).collect();
        let at = |w: &str| order.iter().position(|x| x == w).unwrap();
        // nothing lost, each topic in its own order
        assert_eq!(of(&got, "tgbot:busy"), (1..=6).map(|i| format!("b{i}")).collect::<Vec<_>>(), "{order:?}");
        assert_eq!(of(&got, "tgbot:ans"), vec!["a1", "a2", "a3"], "{order:?}");
        // the finished topic's three overtook most of the queue, but the busy topic still got a slot in between
        assert!(at("a3") < at("b5"), "{order:?}");
        assert!(at("b2") < at("b6"), "{order:?}");
        // what the topic says next (a new turn) is not urgent any more
        push(&mut lanes, "tgbot:ans", "a4".into());
        assert!(!owed(|o| o.urgent["tgbot:ans"] >= o.pushed["tgbot:ans"]));
        wait(&|| handed("tgbot:ans") == 0);
    }

    #[test]
    fn running_topic_merges_longer_while_busy() {
        let _g = store::TEST.lock().unwrap_or_else(|e| e.into_inner());
        let dir = test_dir();
        let got: Got = Arc::default();
        let port = fake_brief(&got);
        store::set_config(&json!({"brief": {"url": format!("http://127.0.0.1:{port}"), "key": crate::crypt::seal("k")},
            "params": {"tg_gap_ms": "0", "merge_s": "0", "pause_ms": "0", "busy_merge_s": "60"},
            "devices": [{"name": "here", "host": "local", "infra": dir.join("no-knowledge").display().to_string()}]}));
        let (sessions, tx) = topics(&dir, &[("tgbot:run", true), ("tgbot:idle", false)]);
        let step = |tid: &str, b: &str| Ev::Step { tid: tid.into(), gen: 1, title: "t".into(), body: b.into() };
        brief::BUSY_TEST.store(true, std::sync::atomic::Ordering::SeqCst);
        tx.send(step("tgbot:run", "s1")).unwrap();
        tx.send(Ev::Note { tid: "tgbot:idle".into(), gen: 1, text: "n1".into() }).unwrap();
        // a topic whose turn is over still goes out after merge_s; the running one waits for busy_merge_s
        wait(&|| !of(&got, "tgbot:idle").is_empty());
        tx.send(step("tgbot:run", "s2")).unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        assert!(of(&got, "tgbot:run").is_empty());
        assert_eq!(running("tgbot:run"), Some((false, true)));
        // its turn ends: everything goes out at once, as one message
        tx.send(Ev::Text { tid: "tgbot:run".into(), gen: 1, delta: "ans".into() }).unwrap();
        tx.send(done("tgbot:run", "ans")).unwrap();
        wait(&|| !of(&got, "tgbot:run").is_empty());
        let run = of(&got, "tgbot:run");
        assert_eq!(run.len(), 1, "{run:?}");
        assert!(run[0].contains("s1") && run[0].contains("s2") && run[0].contains("ans"), "{run:?}");
        // not busy: merge_s again
        brief::BUSY_TEST.store(false, std::sync::atomic::Ordering::SeqCst);
        sessions.lock().unwrap().get_mut("tgbot:run").unwrap().pending = true;
        tx.send(step("tgbot:run", "s3")).unwrap();
        wait(&|| of(&got, "tgbot:run").len() == 2);
        assert!(of(&got, "tgbot:run")[1].contains("s3"));
        wait(&|| running("tgbot:run") == Some((false, false)) || handed("tgbot:run") == 0);
    }
}

#[cfg(test)]
mod folder_tests {
    #[test]
    fn folder_names() {
        // as before for Linux and macOS devices; Windows names lose what Windows does not allow
        assert_eq!(super::safe("a/b:c?*<>|\"d.", false), "a_b_c?*<>|\"d");
        assert_eq!(super::safe("a/b:c?*<>|\"d. ", true), "a_b_c______d");
        assert_eq!(super::safe("..", true), crate::text::UNNAMED_TOPIC);
    }
}
