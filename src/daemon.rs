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
/// Telegram's limit on a command's name.
const COMMAND_NAME_MAX: usize = 32;

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

/// Who sent a message in topic `me` that the user did not write himself, when it is another topic's message: only
/// "<the name of another topic> → …" counts. exterior's own messages can look like that too (the folded
/// "ℹ️ exterior → claude（…）" a new session starts with, an answer quoting another topic).
fn relayed_from<'a>(text: &'a str, me: &str, names: &[&str]) -> Option<&'a str> {
    let (who, _) = relay_split(text)?;
    let who = who.trim();
    (!who.is_empty() && who != me && !who.contains('\n') && names.contains(&who)).then_some(who)
}

/// Sends exterior has under way, per chat: a message of its own can come back from brief before the send that put it
/// there has returned its id.
static SENDING: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);

/// Counts as a send under way while it lives.
struct Sending(String);
impl Sending {
    fn new(chat: &str) -> Sending { *SENDING.lock().unwrap().get_or_insert_with(HashMap::new).entry(chat.to_string()).or_default() += 1; Sending(chat.to_string()) }
}
impl Drop for Sending {
    fn drop(&mut self) {
        let mut g = SENDING.lock().unwrap();
        if let Some(m) = g.as_mut() { if let Some(n) = m.get_mut(&self.0) { *n -= 1; if *n == 0 { m.remove(&self.0); } } }
    }
}

/// How long a message waits for exterior's sends to `chat` under way to finish before it is judged.
const OWN_WAIT: Duration = Duration::from_secs(5);

/// Message `id` in `chat` is one exterior sent itself. While sends to that chat are still under way, wait for them
/// (at most OWN_WAIT): the message may be one of theirs.
fn mine(own: &Own, chat: &str, id: u64) -> bool {
    let t = Instant::now();
    loop {
        if own.lock().unwrap().contains(&id) { return true; }
        let busy = SENDING.lock().unwrap().as_ref().is_some_and(|m| m.contains_key(chat));
        if !busy || t.elapsed() >= OWN_WAIT { return own.lock().unwrap().contains(&id); }
        std::thread::sleep(Duration::from_millis(20));
    }
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
    let _s = Sending::new(chat);
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
            Err((c, v)) if maybe_sent(path, c, &v) => {
                store::log(&format!("{path} {chat}: {c} {v}; may have been sent, not retried"));
                return Err(text::FILE_MAYBE_SENT.into());
            }
            Err((c, v)) if c == 0 || c >= 500 => {
                tries += 1;
                if tries == 1 || tries % 20 == 0 { store::log(&format!("{path} {chat}: {c} {v}; retrying (attempt {tries})")); }
                std::thread::sleep(params::secs("retry_wait_s"));
            }
            Err((c, v)) => { store::log(&format!("{path} {chat} failed {c} {v}")); return Err(format!("{c} {v}")); }
        }
    }
}

/// A file brief may have sent although the post failed: brief received it (no answer after the upload began, or the
/// gateway in front of brief gave up waiting, 504) and goes on sending it to Telegram. It is not posted again, which
/// would show it twice, and is reported as possibly not sent. Texts are posted again as before: an answer that is
/// lost is worse than one that shows twice, and a text goes out within seconds.
fn maybe_sent(path: &str, c: u16, v: &Value) -> bool {
    path == "/api/send_file" && ((c == 0 && v["maybe_sent"] == true) || c == 504)
}

/// Telegram takes files up to 50 MB from a bot.
const FILE_MAX: usize = 50_000_000;

/// A file on the topic's device, sent to `chat` with the agent's caption. A relative path is in the topic's folder.
fn send_file(own: &Own, sessions: &Sessions, chat: &str, dev: &str, dir: &str, alt: &str, path: &str) -> Result<(), String> {
    let device = store::device(dev).ok_or_else(|| text::device_missing(dev))?;
    let data = crate::host::read_file(&device, path, dir, FILE_MAX)?;
    if data.len() > FILE_MAX { return Err(text::file_too_big(FILE_MAX / 1_000_000)); }
    let name = path.rsplit(|c| c == '/' || (cfg!(windows) && c == '\\')).next().unwrap_or(path).to_string();
    let _s = Sending::new(chat);
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
                send(own, sessions, chat, &if e == text::FILE_MAYBE_SENT { text::file_unsure(&path) } else { text::file_not_sent(&path, &e) });
            },
        }
    }
    id
}

/// `exterior send <话题> <内容>` from an agent: signed with the sending topic's name and sent to the other topic
/// (or to any chat brief knows, by its id), files included. Not marked as exterior's own, so the receiving topic's
/// agent gets it.
/// `exterior watch <会话ID>` / `exterior unwatch <会话ID>` / `exterior watch` (list): which chats brief knows (a WeChat
/// group, say) the topic's agent wants to follow. exterior itself waits for their new messages and hands them to the
/// agent in small batches, so the agent needs no polling script of its own.
pub fn watch(from: &str, chat: &str, on: bool) -> Result<String, String> {
    let sessions = LIVE.lock().unwrap().clone().ok_or(text::NOT_READY)?;
    if !sessions.lock().unwrap().get(from).map(|s| !s.deleted).unwrap_or(false) { return Err(text::relay_from_unknown(from)); }
    let mut list: Vec<String> = store::note(from)["watch"].as_array().into_iter().flatten().filter_map(|c| c.as_str().map(String::from)).collect();
    // "--batch 30m": how long this topic's batches gather (off: the default); may come with a chat or alone
    let (chat, batch) = split_batch(chat)?;
    let mut said = String::new();
    if let Some(b) = batch {
        store::note_set(from, "watch_batch", if b == 0 { Value::Null } else { json!(b) });
        store::log(&format!("{from}: watch batch {b}s"));
        said = text::watch_batch_set(b);
    }
    let batch_now = store::note(from)["watch_batch"].as_u64().unwrap_or(0);
    // "--flush [topic]": what that topic's batch has gathered goes to it now, without waiting for --batch
    if let Some(rest) = chat.trim().strip_prefix("--flush") {
        let want = alias(rest.trim());
        let tid = if want.is_empty() { from.to_string() } else {
            sessions.lock().unwrap().values().find(|t| !t.deleted && (t.name == want || t.tid == want)).map(|t| t.tid.clone()).ok_or_else(|| text::relay_to_unknown(&want, ""))?
        };
        FLUSH.lock().unwrap().get_or_insert_with(HashSet::new).insert(tid.clone());
        store::log(&format!("{from}: watch flush {tid}"));
        return Ok(text::watch_flushed(if want.is_empty() { None } else { Some(want.as_str()) }));
    }
    let chat = watch_entry(&chat);
    // a name instead of an id: the one chat with that name, or the candidates to choose from
    let chat = if chat.is_empty() || chat.starts_with('-') || chat.starts_with("all") || chat.starts_with(brief::TOPIC_PREFIX) { chat } else { resolve_chat(&chat)? };
    let chat = chat.as_str();
    if chat.is_empty() || chat == "--list" {
        return Ok(if batch.is_some() && chat.is_empty() { said } else { format!("{}{}", text::watching(&list), text::watch_batch_note(batch_now)) });
    }
    if chat.starts_with(brief::TOPIC_PREFIX) { return Err(text::WATCH_TOPIC.into()); }
    // with "all" on, unwatching one chat keeps it out ("-<chat>"); watching it again takes it back in
    let excluded = format!("-{chat}");
    let covered_by_all = list.iter().any(|e| e.starts_with("all"));
    list.retain(|c| c != chat && *c != excluded);
    if on { list.push(chat.to_string()); } else if covered_by_all && !chat.starts_with("all") { list.push(excluded); }
    store::note_set(from, "watch", json!(list));
    store::log(&format!("{from}: {} {chat}", if on { "watch" } else { "unwatch" }));
    let done = if on { text::watch_on(chat) } else { text::watch_off(chat) };
    Ok(if said.is_empty() { done } else { format!("{done}；{said}") })
}

/// "<chat or --all …> --batch 30m" → the rest and the batch time in seconds (0 for off/default).
fn split_batch(arg: &str) -> Result<(String, Option<u64>), String> {
    let words: Vec<&str> = arg.split_whitespace().collect();
    let Some(i) = words.iter().position(|w| *w == "--batch") else { return Ok((arg.trim().to_string(), None)) };
    let v = words.get(i + 1).copied().unwrap_or("");
    let secs = match v {
        "off" | "0" | "default" => 0,
        _ => {
            let (n, unit) = v.split_at(v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len()));
            let n: u64 = n.parse().ok().filter(|n| *n > 0).ok_or_else(|| text::WATCH_BATCH_USAGE.to_string())?;
            n * match unit { "s" => 1, "m" | "" => 60, "h" => 3600, _ => return Err(text::WATCH_BATCH_USAGE.into()) }
        }
    };
    let rest: Vec<&str> = words.iter().enumerate().filter(|(j, _)| *j != i && *j != i + 1).map(|(_, w)| *w).collect();
    Ok((rest.join(" "), Some(secs)))
}

/// What `exterior watch` was given, as kept in the list: a chat id; or "--all [source]" → "all" (every chat, of every
/// source) or "all:<source>" (every chat of that source, e.g. all:wechat), new chats included.
fn watch_entry(arg: &str) -> String {
    let a = arg.trim();
    match a.strip_prefix("--all") {
        Some(rest) => { let src = rest.trim().trim_start_matches("source=").trim(); if src.is_empty() { "all".into() } else { format!("all:{src}") } }
        None => a.to_string(),
    }
}

/// Whether a watch entry covers a message (its chat and source). The Telegram topics exterior serves are never
/// watched: they talk to each other with exterior send.
fn watch_covers(entry: &str, chat: &str, source: &str) -> bool {
    if chat.starts_with(brief::TOPIC_PREFIX) { return false; }
    entry == chat || entry == "all" || entry.strip_prefix("all:").is_some_and(|s| s == source)
}

/// Topics whose gathered batch should go out now (`exterior watch --flush`).
static FLUSH: Mutex<Option<HashSet<String>>> = Mutex::new(None);
fn flush_take() -> HashSet<String> { FLUSH.lock().unwrap().take().unwrap_or_default() }

/// A watched chat's new messages wait this long for more before going to the agent, and at most WATCH_MAX.
const WATCH_QUIET: Duration = Duration::from_secs(20);
const WATCH_MAX: Duration = Duration::from_secs(120);

/// One message from a watched chat, as the agent reads it: "[时间] 发送者：内容".
fn watch_line(m: &Value) -> String {
    let t = m["ts"].as_u64().map(|t| crate::web::fmt_time(t)).unwrap_or_default();
    let kind = m["type"].as_str().filter(|k| !k.is_empty() && *k != "text").map(|k| format!("[{k}] ")).unwrap_or_default();
    format!("[{t}] {}：{kind}{}", m["sender"].as_str().unwrap_or("?"), m["text"].as_str().unwrap_or("").trim())
}

/// One batch as the agent reads it: "群名 → [新消息]" and its lines; several chats one after another, each with its
/// own heading, in the order they first spoke.
fn watch_batch(lines: &[(String, String)]) -> String {
    let mut order: Vec<&str> = vec![];
    for (n, _) in lines { if !order.contains(&n.as_str()) { order.push(n); } }
    order.iter().map(|n| format!("{n} {} {}\n{}", params::get("relay_mark"), text::WATCH_HEADER,
        lines.iter().filter(|(c, _)| c == n).map(|(_, l)| l.as_str()).collect::<Vec<_>>().join("\n"))).collect::<Vec<_>>().join("\n\n")
}

fn watcher(sessions: Sessions, own: Own, tx: Sender<Ev>) {
    // topic → ((chat name, line) in arrival order, first, last arrival)
    let mut pend: HashMap<String, (Vec<(String, String)>, Instant, Instant)> = HashMap::new();
    let latest = || brief::get("/api/messages?limit=1").and_then(|v| v.as_array().and_then(|a| a.iter().filter_map(|m| m["id"].as_u64()).max()));
    let mut last = store::state("watch_after").as_u64().filter(|l| *l > 0).or_else(latest).unwrap_or(0);
    loop {
        // topic → its watch entries (chat ids, "all", "all:<source>"); and how long its batches gather, if set
        let mut watched: Vec<(String, Vec<String>)> = vec![];
        let mut batch: HashMap<String, Duration> = HashMap::new();
        for n in store::notes().iter().filter(|n| n["deleted"] != true) {
            let l: Vec<String> = n["watch"].as_array().into_iter().flatten().filter_map(|c| c.as_str().map(String::from)).collect();
            let tid = n["topic"].as_str().unwrap_or("").to_string();
            if let Some(b) = n["watch_batch"].as_u64().filter(|b| *b > 0) { batch.insert(tid.clone(), Duration::from_secs(b)); }
            if !l.is_empty() { watched.push((tid, l)); }
        }
        if watched.is_empty() && pend.is_empty() {
            // nothing followed: keep the mark current, so a new watch starts from now, not from old messages
            std::thread::sleep(Duration::from_secs(20));
            if let Some(l) = latest() { last = last.max(l); store::set_state("watch_after", json!(last)); }
            continue;
        }
        match brief::call("GET", &format!("/api/wait?after_id={last}&timeout=15&limit=500"), None) {
            Ok(v) => for m in v.as_array().into_iter().flatten() {
                last = last.max(m["id"].as_u64().unwrap_or(0));
                let (chat, source) = (m["chat"].as_str().unwrap_or(""), m["source"].as_str().unwrap_or(""));
                // each topic once, however many of its entries cover the chat
                let out = format!("-{chat}");
                for (tid, _) in watched.iter().filter(|(_, l)| !l.contains(&out) && l.iter().any(|e| watch_covers(e, chat, source))) {
                    let name = m["chat_name"].as_str().filter(|n| !n.is_empty()).unwrap_or(chat).to_string();
                    let e = pend.entry(tid.clone()).or_insert_with(|| (Vec::new(), Instant::now(), Instant::now()));
                    e.0.push((name, watch_line(m)));
                    e.2 = Instant::now();
                }
            },
            Err(_) => std::thread::sleep(params::secs("reconnect_s")),
        }
        store::set_state("watch_after", json!(last));
        // default: once the chat is quiet for a moment (at most WATCH_MAX); with --batch: everything that came within it;
        // and at once for a topic that asked (--flush)
        let flush = flush_take();
        let ready: Vec<String> = pend.iter().filter(|(t, (_, first, at))| flush.contains(*t) || match batch.get(*t) {
            Some(b) => first.elapsed() >= *b,
            None => at.elapsed() >= WATCH_QUIET || first.elapsed() >= WATCH_MAX,
        }).map(|(t, _)| t.clone()).collect();
        for tid in ready {
            let Some((lines, _, _)) = pend.remove(&tid) else { continue };
            store::log(&format!("watch -> {tid}: {} messages", lines.len()));
            handle(&sessions, &own, &tx, &tid, &watch_batch(&lines));
        }
    }
}

/// `exterior topic open <name>` / `exterior topic close <name or chat>`: a topic made as the user (brief user_topic)
/// for another agent to work in, and closing it when the work is done (brief release).
pub fn topic_cmd(what: &str, name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() { return Err(text::TOPIC_USAGE.into()); }
    let err = |(c, v): (u16, Value)| format!("{c} {}", v["error"].as_str().or(v["message"].as_str()).map(String::from).unwrap_or_else(|| v.to_string()));
    match what {
        "open" => {
            let v = brief::post("/api/bots/user_topic", json!({"name": name})).map_err(err)?;
            if let Some(s) = LIVE.lock().unwrap().clone() { refresh(&s); }
            Ok(text::topic_opened(name, v["chat"].as_str().unwrap_or("?"), v["link"].as_str().unwrap_or("")))
        }
        "close" => {
            let to = alias(name);
            let chat = if to.starts_with(brief::TOPIC_PREFIX) { to.clone() } else {
                let sessions = LIVE.lock().unwrap().clone().ok_or(text::NOT_READY)?;
                let ss = sessions.lock().unwrap();
                ss.values().find(|t| !t.deleted && t.name == to).map(|t| t.tid.clone()).ok_or_else(|| text::relay_to_unknown(&to, ""))?
            };
            brief::post("/api/bots/release", json!({"chat": chat})).map_err(err)?;
            Ok(text::topic_closed(&to, &chat))
        }
        _ => Err(text::TOPIC_USAGE.into()),
    }
}

/// `exterior draft <chat id or name> <text>`, `exterior draft list`, `exterior draft rm <id>`: brief's drafts, the
/// replies the user reads and sends himself.
pub fn draft_cmd(to: &str, body: &str) -> Result<String, String> {
    let err = |(c, v): (u16, Value)| format!("{c} {}", v["error"].as_str().or(v["message"].as_str()).map(String::from).unwrap_or_else(|| v.to_string()));
    let to = to.trim();
    match to {
        "" => Err(text::DRAFT_USAGE.into()),
        "list" => {
            let v = brief::call("GET", "/api/drafts", None).map_err(err)?;
            let l = v.as_array().cloned().or_else(|| v["drafts"].as_array().cloned()).unwrap_or_default();
            if l.is_empty() { return Ok(text::NO_DRAFTS.into()); }
            Ok(l.iter().map(|d| format!("#{} {} · {}", d["id"], d["chat_name"].as_str().or(d["chat"].as_str()).unwrap_or("?"),
                d["body"].as_str().unwrap_or("").replace('\n', " ").chars().take(60).collect::<String>())).collect::<Vec<_>>().join("\n"))
        }
        "rm" => {
            let id = body.trim().trim_start_matches('#');
            if id.is_empty() { return Err(text::DRAFT_USAGE.into()); }
            brief::call("DELETE", &format!("/api/drafts/{id}"), None).map_err(err)?;
            Ok(text::draft_removed(id))
        }
        chat => {
            if body.trim().is_empty() { return Err(text::DRAFT_USAGE.into()); }
            let id = draft_chat(chat)?;
            let v = brief::post("/api/drafts", json!({"chat": id, "body": body})).map_err(err)?;
            Ok(text::draft_saved(&id, &v["id"].to_string()))
        }
    }
}

/// A chat by its id, or by its name (a group's name, a contact's remark) looked up in brief. A name must match exactly
/// one chat; otherwise the candidates are listed for the user to pick, nothing is guessed.
fn draft_chat(c: &str) -> Result<String, String> { resolve_chat(c) }

fn resolve_chat(c: &str) -> Result<String, String> {
    let c = c.trim();
    let v = brief::get(&format!("/api/chats?q={}&limit=50", urlenc(c))).unwrap_or(Value::Null);
    let l = v.as_array().cloned().or_else(|| v["chats"].as_array().cloned()).unwrap_or_default();
    let found: Vec<(String, String)> = l.iter().filter_map(|x| Some((x["chat"].as_str().or(x["id"].as_str())?.to_string(), x["name"].as_str().unwrap_or("").trim().to_string()))).collect();
    pick_chat(c, &found)
}

/// The choice among brief's search results (id, name) for what the user wrote.
fn pick_chat(c: &str, found: &[(String, String)]) -> Result<String, String> {
    if found.iter().any(|(id, _)| id == c) { return Ok(c.to_string()); }
    let exact: Vec<&(String, String)> = found.iter().filter(|(_, n)| n == c).collect();
    let list = |l: &[&(String, String)]| l.iter().map(|(id, n)| format!("{n}（{id}）")).collect::<Vec<_>>().join("、");
    match exact.len() {
        1 => Ok(exact[0].0.clone()),
        0 => {
            // something that looks like an id (ASCII, no spaces) is taken as one; a name nobody has is not guessed at
            let id_like = !c.is_empty() && c.is_ascii() && !c.contains(char::is_whitespace);
            if id_like { return Ok(c.to_string()); }
            let near: Vec<&(String, String)> = found.iter().take(10).collect();
            Err(if near.is_empty() { text::draft_no_chat(c) } else { text::chat_candidates(c, &list(&near)) })
        }
        _ => Err(text::draft_many_chats(c, &list(&exact))),
    }
}

fn urlenc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

pub fn relay(from: &str, to: &str, body: &str) -> Result<String, String> {
    let sessions = LIVE.lock().unwrap().clone().ok_or(text::NOT_READY)?;
    let to = alias(to.trim());
    let to = to.as_str();
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
    let prompt = crate::prompt::session_text(&dev, &s.name, &s.tid, &s.dir)?;
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

/// The "/" menu and the agents' command lists. Every agent's commands are read as it lists them: claude's from
/// initialize, pi's from get_commands plus the table (agents::BUILTINS), codex's from the table, agy's from
/// available_commands_update plus `agy -p /help`. The group's menu shows only the few they all have, plus exterior's
/// own; /commands shows the topic's agent's full list. The topic's current agent decides what a command does.
pub fn build_menu() -> Result<usize, String> {
    let dev = store::default_device().ok_or(text::NO_DEVICE)?;
    let has = |k: Kind| dev["agents"].as_array().into_iter().flatten().any(|a| a["kind"] == k.name());
    // menu name → (agent kind → the agent's own name for it): how a typed command reaches the topic's agent
    let mut map = serde_json::Map::new();
    // each agent's full list, in its own words, for /commands: kind → [[menu name, description]]
    let mut lists = serde_json::Map::new();
    let root = crate::prompt::root(&dev);
    // agy's command-line commands (/usage…): not ACP commands, each is run on its own (agents::agy_cli)
    let mut agy_cli: Vec<String> = vec![];
    for kind in Kind::ALL {
        let Some(row) = dev["agents"].as_array().into_iter().flatten().find(|a| a["kind"] == kind.name()) else { continue };
        let cmd = row["command"].as_str().unwrap_or("");
        let table = agents::BUILTINS.iter().filter(|(k, _)| *k == kind).map(|(_, n)| (n.to_string(), agents::builtin_desc(kind, n), false));
        let own = agents::commands(&dev, &root, cmd, kind).into_iter().map(|(n, d)| (n, d, false));
        let cli = if kind == Kind::Agy { agents::agy_cli_commands(&dev, &root, cmd) } else { vec![] }.into_iter().map(|(n, d)| (n, d, true));
        let mut list: Vec<Value> = vec![];
        for (orig, d, is_cli) in table.chain(own).chain(cli) {
            let tg = menu_name(&orig);
            if tg.is_empty() || tg.starts_with(OWN) { continue; }
            let e = map.entry(tg.clone()).or_insert_with(|| json!({}));
            if !e[kind.name()].is_null() { continue; }
            e[kind.name()] = json!(orig);
            if is_cli { agy_cli.push(tg.clone()); }
            list.push(json!([tg, d.trim().replace('\n', " ")]));
        }
        lists.insert(kind.name().into(), Value::Array(list));
    }
    if map.is_empty() { return Err(text::NO_COMMANDS.into()); }
    // Telegram has one menu for the whole group: only what every agent has (and is used often), plus exterior's own.
    // Everything else works when typed, and /commands lists the topic's agent's full set.
    let mut cmds = vec![json!({"command": "commands", "description": text::MENU_COMMANDS})];
    for n in MENU_COMMON { if map.contains_key(*n) { cmds.push(json!({"command": n, "description": text::builtin_desc(n)})); } }
    cmds.push(json!({"command": format!("{OWN}stop"), "description": text::MENU_STOP}));
    cmds.push(json!({"command": format!("{OWN}exit"), "description": text::MENU_EXIT}));
    cmds.push(json!({"command": format!("{OWN}agent"), "description": text::MENU_AGENT}));
    if has(Kind::Claude) { cmds.push(json!({"command": format!("{OWN}account"), "description": text::MENU_ACCOUNT})); }
    let n = cmds.len();
    brief::post("/api/bots/commands", json!({"commands": cmds})).map_err(|(c, v)| format!("{c} {v}"))?;
    store::set_state("menu", Value::Object(map));
    store::set_state("commands", Value::Object(lists));
    store::set_state("agy_cli", json!(agy_cli));
    store::set_state("menu_time", json!(store::now()));
    Ok(n)
}

/// The agents' commands that go into the group's menu: the ones they all have and that are used most.
const MENU_COMMON: &[&str] = &["model", "compact", "new", "clear", "resume", "usage"];
/// /commands shows this many to a page.
const COMMANDS_PAGE: usize = 20;

/// /commands [page]: the topic's agent's full list, in its own words, as buttons that send the command as it is.
fn commands_page(kind: Kind, agent: &str, page: usize) -> (String, Value) {
    let all = store::state("commands")[kind.name()].as_array().cloned().unwrap_or_default();
    let pages = all.len().div_ceil(COMMANDS_PAGE).max(1);
    let page = page.clamp(1, pages);
    let shown: Vec<&Value> = all.iter().skip((page - 1) * COMMANDS_PAGE).take(COMMANDS_PAGE).collect();
    let mut t = text::commands_head(agent, kind.label(), all.len(), page, pages);
    for c in &shown {
        let (n, d) = (c[0].as_str().unwrap_or(""), c[1].as_str().unwrap_or(""));
        t.push_str(&format!("\n/{n}{}", if d.is_empty() { String::new() } else { format!(" — {d}") }));
    }
    let mut rows = in_rows(shown.iter().map(|c| { let n = c[0].as_str().unwrap_or(""); (format!("/{n}"), format!("/{n}")) }).collect(), 3);
    let mut nav = vec![];
    if page > 1 { nav.push((text::PAGE_PREV.to_string(), format!("/commands {}", page - 1))); }
    if page < pages { nav.push((text::PAGE_NEXT.to_string(), format!("/commands {}", page + 1))); }
    if !nav.is_empty() { rows.push(nav); }
    (t, buttons(rows))
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
                // claude's answer to a bare /model ("Usage: /model <name>. Available: …"): buttons to pick one
                let model_choice = answer.as_deref().and_then(text::claude_model_usage);
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
                if let Some(listed) = model_choice {
                    let (own, sessions, t) = (own.clone(), sessions.clone(), tid.clone());
                    lanes.push(&tid, move || {
                        // claude's own list from its initialize answer (with names); else the one in its reply
                        let ctl = sessions.lock().unwrap().get(&t).and_then(|s| s.agent.as_ref().filter(|a| a.kind == Kind::Claude).map(|a| a.ctl()));
                        let mut items: Vec<(String, String)> = ctl.and_then(|c| c.models().ok()).unwrap_or_default().into_iter().map(|m| (m.label, m.value)).collect();
                        if items.is_empty() { items = listed.into_iter().map(|n| (n.clone(), n)).collect(); }
                        if !items.is_empty() { send_with(&own, &sessions, &t, &text::sys(text::CHOOSE_MODEL), Some(choice_buttons(&t, "model", items, 2))); }
                    });
                }
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
                    store::note_set(&tid, "turn_end", json!(store::now()));
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
            Ev::Context { tid, gen, tokens } => {
                if current(&tid, gen) { store::note_set(&tid, "context", json!(tokens)); }
            }
            Ev::Usage { tid, uses } => {
                let (name, dev) = sessions.lock().unwrap().get(&tid).map(|s| (s.name.clone(), s.dev.clone())).unwrap_or_else(|| (tid.clone(), String::new()));
                let n = store::note(&tid);
                let agent = n["agent"].as_str().unwrap_or("");
                crate::usage::record(&name, &dev, agent, n["resolved"].as_str().unwrap_or(agent), &uses);
            }
            Ev::Quota { tid, windows } => {
                let n = store::note(&tid);
                crate::usage::set_windows(n["resolved"].as_str().or(n["agent"].as_str()).unwrap_or(""), &windows);
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
    { let (s, o, t) = (sessions.clone(), own.clone(), tx.clone()); std::thread::spawn(move || watcher(s, o, t)); }
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
            idle_compact(&sessions, &own, &tx);
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
            // asked before taking the sessions: it may wait for a send under way
            if !from_user && mine(&own, &chat, id) { continue; }
            let to_name = {
                let ss = sessions.lock().unwrap();
                let Some(s) = ss.get(&chat) else { continue };
                if s.deleted { continue; }
                if !from_user {
                    let names: Vec<&str> = ss.values().map(|t| t.name.as_str()).collect();
                    if relayed_from(&text, &s.name, &names).is_none() { continue; }
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
            // someone talked: an idle compaction may happen again after this turn
            store::note_set(&chat, "idle_compact_wait", Value::Null);
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

/// A topic's old name, kept as an alias after it was renamed (topic_aliases "old=new；old2=new2"), as its new name.
pub fn alias(name: &str) -> String { alias_in(&params::get("topic_aliases"), name) }

fn alias_in(spec: &str, name: &str) -> String {
    spec.split(['；', ';', '\n']).filter_map(|p| p.split_once('=')).find(|(old, _)| old.trim() == name.trim())
        .map(|(_, new)| new.trim().to_string()).filter(|n| !n.is_empty()).unwrap_or_else(|| name.trim().to_string())
}

/// `exterior schedule list | add <HH:MM> <话题名> <内容> | rm <编号>`: the schedules setting, read and written directly
/// (nothing else in the settings changes). Times are exterior's local time (旧金山 on Lazycat).
pub fn schedule_cmd(what: &str, rest: &str) -> Result<String, String> {
    let mut c = store::config();
    let spec = c["params"]["schedules"].as_str().unwrap_or("").to_string();
    let mut entries: Vec<String> = spec.split("||").map(|e| e.trim().to_string()).filter(|e| !e.is_empty()).collect();
    let save = |c: &mut Value, entries: &[String]| {
        if !c["params"].is_object() { c["params"] = json!({}); }
        if entries.is_empty() { c["params"].as_object_mut().unwrap().remove("schedules"); } else { c["params"]["schedules"] = json!(entries.join(" || ")); }
        store::set_config(c);
    };
    match what.trim() {
        "list" | "" => Ok(text::schedule_list(&entries)),
        "add" => {
            let names: Vec<String> = LIVE.lock().unwrap().clone().map(|s| s.lock().unwrap().values().filter(|t| !t.deleted).map(|t| t.name.clone()).collect()).unwrap_or_default();
            let (at, topic, body) = schedule_parse(rest, &names, &|n| alias(n)).ok_or(text::SCHEDULE_USAGE)?;
            if body.contains("||") { return Err(text::SCHEDULE_BARS.into()); }
            let e = format!("{at}|{topic}|{body}");
            if schedules(&e).is_empty() { return Err(text::SCHEDULE_USAGE.into()); }
            entries.push(e.clone());
            save(&mut c, &entries);
            store::log(&format!("schedule add: {}", e.chars().take(80).collect::<String>()));
            Ok(text::schedule_added(entries.len(), &e))
        }
        "rm" => {
            let n: usize = rest.trim().trim_start_matches('#').parse().ok().filter(|n| *n >= 1 && *n <= entries.len()).ok_or_else(|| text::schedule_no(rest.trim(), entries.len()))?;
            let gone = entries.remove(n - 1);
            save(&mut c, &entries);
            store::log(&format!("schedule rm: {}", gone.chars().take(80).collect::<String>()));
            Ok(text::schedule_removed(n, &gone))
        }
        _ => Err(text::SCHEDULE_USAGE.into()),
    }
}

/// "07:30 exterior 开发 内容…" → ("07:30", topic, content). The topic is the longest known topic name (or old name)
/// the rest starts with, since names may have spaces; else the first word. Old names become the current name.
fn schedule_parse(s: &str, names: &[String], alias: &dyn Fn(&str) -> String) -> Option<(String, String, String)> {
    let s = s.trim();
    let (at, rest) = s.split_once(char::is_whitespace)?;
    let (h, m) = at.split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    if h > 23 || m > 59 { return None; }
    let rest = rest.trim_start();
    let mut known: Vec<String> = names.to_vec();
    known.extend(params::get("topic_aliases").split(['；', ';', '\n']).filter_map(|p| p.split_once('=')).map(|(o, _)| o.trim().to_string()));
    known.sort_by_key(|n| std::cmp::Reverse(n.len()));
    let (topic, body) = known.iter().filter(|n| !n.is_empty())
        .find_map(|n| rest.strip_prefix(n.as_str()).filter(|b| b.starts_with(char::is_whitespace)).map(|b| (n.clone(), b.trim().to_string())))
        .or_else(|| rest.split_once(char::is_whitespace).map(|(t, b)| (t.to_string(), b.trim().to_string())))?;
    if body.is_empty() { return None; }
    Some((format!("{h:02}:{m:02}"), alias(&topic), body))
}

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
        let topic = alias(&topic);
        let tid = sessions.lock().unwrap().values().find(|s| !s.deleted && s.name == topic).map(|s| s.tid.clone());
        let Some(tid) = tid else { store::log(&format!("schedule: no topic named {topic}")); continue };
        store::log(&format!("schedule -> {topic}: {}", content.chars().take(60).collect::<String>()));
        send(own, sessions, &tid, &text::sys(&text::scheduled(&content)));
        // a scheduled turn is a turn like any other: an idle compaction may follow it (dream has no other)
        store::note_set(&tid, "idle_compact_wait", Value::Null);
        handle(sessions, own, tx, &tid, &format!("{} {} {content}", text::SCHEDULE_FROM, params::get("relay_mark")));
    }
}

/// Claude Code compacts a long conversation while it sits idle, before the prompt cache expires, so coming back
/// later does not pay for the whole context again; it does that only in its terminal, not in the mode exterior runs
/// it in. exterior does the same through the agent's own /compact, in the open: idle_compact_min minutes after a turn
/// ended, once per turn, when the agent's process is still there and the context is at least idle_compact_tokens.
fn due_compact(n: &Value, now: u64, running: bool, min: u64, tokens: u64) -> bool {
    let end = n["turn_end"].as_u64().unwrap_or(0);
    // after one idle compaction, nothing more until someone talks in the topic again: compacting a conversation
    // nobody came back to only burns tokens
    min > 0 && running && end > 0 && now >= end + min * 60 && n["context"].as_u64().unwrap_or(0) >= tokens
        && n["idle_compacted"].as_u64() != Some(end) && n["status"] == st::IDLE && n["idle_compact_wait"] != true
}

fn idle_compact(sessions: &Sessions, own: &Own, tx: &Sender<Ev>) {
    let (min, tokens, now) = (params::num("idle_compact_min"), params::num("idle_compact_tokens"), store::now());
    if min == 0 { return; }
    let due: Vec<(String, u64)> = sessions.lock().unwrap().values()
        .filter(|s| !s.deleted && s.agent.is_some() && !s.pending)
        .filter_map(|s| { let n = store::note(&s.tid); due_compact(&n, now, true, min, tokens).then(|| (s.tid.clone(), n["context"].as_u64().unwrap_or(0))) })
        .collect();
    for (tid, ctx) in due {
        let end = store::note(&tid)["turn_end"].clone();
        store::note_set(&tid, "idle_compacted", end);
        store::note_set(&tid, "idle_compact_wait", json!(true));
        store::log(&format!("{tid}: idle {min} min, context {ctx} tokens: /compact"));
        send(own, sessions, &tid, &text::sys(&text::idle_compact(min, ctx)));
        handle(sessions, own, tx, &tid, "/compact");
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
        // one of agy's command-line commands: run on its own, its output goes to the topic as it is
        let agy_cli = cur_kind == Some(Kind::Agy) && agent_cmd.is_some()
            && head.strip_prefix('/').is_some_and(|h| store::state("agy_cli").as_array().is_some_and(|l| l.iter().any(|x| x == h)));
        if head == "/commands" || own_cmd == Some("commands") {
            match cur_kind {
                Some(k) => { let (t, b) = commands_page(k, &cur, arg.parse().unwrap_or(1)); reply = Some(t); reply_buttons = Some(b); }
                None => reply = Some(text::NO_COMMANDS.into()),
            }
        } else if own_cmd == Some("exit") {
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
        } else if agy_cli && builtin.is_none() {
            let row = dev.as_ref().and_then(|d| store::agent(d, n["resolved"].as_str().unwrap_or(&cur)));
            let command = row.map(|r| r["command"].as_str().unwrap_or("").to_string()).unwrap_or_default();
            let (own, sessions, chat, d, dir, orig, arg) = (own.clone(), sessions.clone(), chat.to_string(), dev.clone(), s.dir.clone(), agent_cmd.clone().unwrap_or_default(), arg.clone());
            std::thread::spawn(move || {
                let r = d.ok_or_else(|| text::NO_DEVICE.to_string()).and_then(|d| agents::agy_cli(&d, &dir, &command, &orig, &arg));
                let t = match r {
                    Ok(o) if o.trim().is_empty() => text::sys(&text::no_output(Kind::Agy.label(), &orig)),
                    Ok(o) => format!("```\n{}\n```", o.trim()),
                    Err(e) => text::sys_warn(&e),
                };
                send(&own, &sessions, &chat, &t);
            });
        } else if let Some(orig) = &agent_cmd {
            // the agent's own command (claude's; pi's extensions, templates, skills), under the agent's own name.
            // claude's effort is remembered so a restarted process keeps it
            if cur_kind == Some(Kind::Claude) && orig == "effort" && agents::CLAUDE_EFFORTS.contains(&arg.as_str()) { store::note_set(chat, "effort", json!(arg)); }
            // a button whose value was too long to fit says "/model 3": the value is looked up
            let arg = chosen(chat, orig, &arg).unwrap_or(arg.clone());
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
        // the answer never hides at the end of a message full of folded steps
        let m = text::pack(vec![text::step("💭 思考", "x")[0].clone(), "回答".into(), text::step("🔧 Bash", "y")[0].clone(), text::step("🔧 Bash", "z")[0].clone()]);
        assert_eq!(m.len(), 3);
        assert_eq!(m[1], "回答");
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
    fn idle_compaction_rule() {
        let n = json!({"turn_end": 1000, "context": 150000, "status": "idle"});
        assert!(!due_compact(&n, 1000 + 49 * 60, true, 50, 100000));
        assert!(due_compact(&n, 1000 + 50 * 60, true, 50, 100000));
        assert!(!due_compact(&n, 1000 + 50 * 60, false, 50, 100000), "no process, no cache to keep");
        assert!(!due_compact(&n, 1000 + 50 * 60, true, 0, 100000), "off");
        assert!(!due_compact(&json!({"turn_end": 1000, "context": 90000, "status": "idle"}), 9000, true, 50, 100000), "short context");
        assert!(!due_compact(&json!({"turn_end": 1000, "context": 150000, "status": "idle", "idle_compacted": 1000}), 9000, true, 50, 100000), "once per turn");
        assert!(!due_compact(&json!({"turn_end": 1000, "context": 150000, "status": "error"}), 9000, true, 50, 100000));
        // the compaction's own turn ended later, context still reported high: no second compaction without a message
        assert!(!due_compact(&json!({"turn_end": 5000, "context": 150000, "status": "idle", "idle_compacted": 1000, "idle_compact_wait": true}), 99999, true, 50, 100000));
    }

    #[test]
    fn watch_batches_by_chat() {
        let l = vec![("A".to_string(), "[1] x：a1".to_string()), ("B".into(), "[2] y：b1".into()), ("A".into(), "[3] x：a2".into())];
        let t = watch_batch(&l);
        assert!(t.starts_with("A → ") && t.contains("[1] x：a1\n[3] x：a2\n\nB → ") && t.ends_with("[2] y：b1"), "{t}");
    }
    #[test]
    fn watch_batch_option() {
        assert_eq!(split_batch("--batch 30m").unwrap(), (String::new(), Some(1800)));
        assert_eq!(split_batch("--all wechat --batch 2h").unwrap(), ("--all wechat".into(), Some(7200)));
        assert_eq!(split_batch("1@chatroom --batch 90s").unwrap(), ("1@chatroom".into(), Some(90)));
        assert_eq!(split_batch("--batch off").unwrap(), (String::new(), Some(0)));
        assert_eq!(split_batch("1@chatroom").unwrap(), ("1@chatroom".into(), None));
        assert!(split_batch("--batch x").is_err());
        assert_eq!(crate::text::watch_batch_note(1800), "（攒批时长 30 分钟）");
    }
    #[test]
    fn chat_by_name() {
        let f = vec![("1@chatroom".to_string(), "内存采购".to_string()), ("2@chatroom".into(), "内存采购二群".into()),
                     ("3@chatroom".into(), "同名".into()), ("4@chatroom".into(), "同名".into())];
        assert_eq!(pick_chat("内存采购", &f).unwrap(), "1@chatroom");
        assert_eq!(pick_chat("2@chatroom", &f).unwrap(), "2@chatroom");
        assert!(pick_chat("同名", &f).unwrap_err().contains("3@chatroom"));
        assert!(pick_chat("内存", &f).unwrap_err().contains("内存采购（1@chatroom）"), "a partial name lists candidates");
        assert_eq!(pick_chat("zenghaochen", &[]).unwrap(), "zenghaochen", "an id-like word is an id");
        assert!(pick_chat("不存在的群", &[]).is_err());
    }
    #[test]
    fn schedule_args() {
        let names = vec!["exterior 开发".to_string(), "exterior".to_string(), "Polaris".to_string()];
        let alias = |n: &str| if n == "全局" { "Polaris".to_string() } else { n.to_string() };
        assert_eq!(schedule_parse("7:30 exterior 开发 看一眼 | 进度", &names, &alias), Some(("07:30".into(), "exterior 开发".into(), "看一眼 | 进度".into())));
        assert_eq!(schedule_parse("22:00 新话题 做点事", &names, &alias), Some(("22:00".into(), "新话题".into(), "做点事".into())));
        assert_eq!(schedule_parse("08:00 全局 早报", &names, &alias).map(|x| x.1), Some("Polaris".into()));
        assert_eq!(schedule_parse("25:00 Polaris x", &names, &alias), None);
        assert_eq!(schedule_parse("08:00 Polaris", &names, &alias), None);
        assert!(crate::text::schedule_list(&["07:30|dream|整理".into()]).contains("1. 07:30 | dream | 整理"));
    }
    #[test]
    fn watch_all() {
        assert_eq!(watch_entry("--all"), "all");
        assert_eq!(watch_entry("--all wechat"), "all:wechat");
        assert_eq!(watch_entry("--all source=wechat"), "all:wechat");
        assert_eq!(watch_entry(" 123@chatroom "), "123@chatroom");
        assert!(watch_covers("all:wechat", "999@chatroom", "wechat"));
        assert!(!watch_covers("all:wechat", "tg:u1", "telegram"));
        assert!(watch_covers("all", "tg:u1", "telegram"));
        assert!(!watch_covers("all", "tgbot:5", "telegram"), "exterior's own topics are never watched");
        assert!(watch_covers("1@chatroom", "1@chatroom", "wechat") && !watch_covers("1@chatroom", "2@chatroom", "wechat"));
        assert!(crate::text::watching(&["all:wechat".into(), "tg:u1".into(), "-2@chatroom".into()]).contains("wechat 的全部会话"));
        assert!(crate::text::watching(&["all:wechat".into(), "-2@chatroom".into()]).contains("不盯 1 个：2@chatroom"));
    }
    #[test]
    fn aliases() {
        let a = "全局=Polaris；全局二号=Comes; x = y";
        assert_eq!(alias_in(a, "全局"), "Polaris");
        assert_eq!(alias_in(a, "全局二号"), "Comes");
        assert_eq!(alias_in(a, " x "), "y");
        assert_eq!(alias_in(a, "Polaris"), "Polaris");
        assert_eq!(alias_in("", "全局"), "全局");
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
    fn claude_model_answer() {
        let a = "Current model: `Opus 5.5`\nUsage: /model <name>. Available: sonnet, opus, haiku, fable, best, sonnet[1m], opus[1m], fable[1m], opusplan, default, or a full model ID.";
        assert_eq!(crate::text::claude_model_usage(a).unwrap(), vec!["sonnet", "opus", "haiku", "fable", "best", "sonnet[1m]", "opus[1m]", "fable[1m]", "opusplan", "default"]);
        assert_eq!(crate::text::claude_model_usage("Set model to opus"), None);
    }
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

    #[test]
    fn own_messages_are_not_relays() {
        let _g = store::TEST.lock().unwrap_or_else(|e| e.into_inner());
        test_dir();
        let names = ["exterior 开发", "全局", "T1"];
        // the folded note a new claude session starts with is exterior's own, though it has " → " in it
        assert_eq!(relayed_from("ℹ️ exterior → claude（启动时设置 effort auto…）\n/effort auto", "T1", &names), None);
        assert_eq!(relayed_from("exterior → claude", "T1", &names), None);
        // another topic's message
        assert_eq!(relayed_from("exterior 开发 → 任务：修复", "T1", &names), Some("exterior 开发"));
        assert_eq!(relayed_from("全局 → a → b", "T1", &names), Some("全局"));
        // the topic itself, or no mark at all
        assert_eq!(relayed_from("T1 → 自己", "T1", &names), None);
        assert_eq!(relayed_from("没有分隔符", "T1", &names), None);
    }

    #[test]
    fn file_that_may_have_gone_out_is_not_sent_again() {
        let unsure = json!({"error": "no_answer", "maybe_sent": true});
        assert!(maybe_sent("/api/send_file", 0, &unsure));
        assert!(maybe_sent("/api/send_file", 504, &Value::Null));
        // brief was never reached, or refused it: sending again is safe
        assert!(!maybe_sent("/api/send_file", 0, &json!({"error": "no_answer", "maybe_sent": false})));
        assert!(!maybe_sent("/api/send_file", 502, &Value::Null));
        // a text is sent again as before
        assert!(!maybe_sent("/api/send", 0, &unsure));
    }

    #[test]
    fn own_message_back_before_its_send_returns() {
        let own: Own = Arc::default();
        // brief hands exterior's message back while the send that put it there is still waiting for its answer
        let s = Sending::new("tgbot:own");
        let o = own.clone();
        let h = std::thread::spawn(move || { std::thread::sleep(Duration::from_millis(300)); o.lock().unwrap().insert(42); drop(s); });
        assert!(mine(&own, "tgbot:own", 42));
        h.join().unwrap();
        assert!(SENDING.lock().unwrap().as_ref().is_none_or(|m| !m.contains_key("tgbot:own")));
        // nothing under way: decided at once
        let t = Instant::now();
        assert!(!mine(&own, "tgbot:own", 43));
        assert!(t.elapsed() < Duration::from_secs(1));
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
    fn commands_list() {
        let _g = store::TEST.lock().unwrap_or_else(|e| e.into_inner());
        let _dir = test_dir();
        let list: Vec<Value> = (0..45).map(|i| json!([format!("c{i}"), format!("does {i}")])).collect();
        store::set_state("commands", json!({"agy": list}));
        let (t, b) = commands_page(Kind::Agy, "agy-claude", 1);
        assert!(t.contains("共 45 条") && t.contains("1/3") && t.contains("/c0 — does 0") && !t.contains("/c20 "), "{t}");
        let rows = b["inline_keyboard"].as_array().or(b.as_array()).cloned().unwrap_or_default();
        let flat: Vec<String> = rows.iter().flat_map(|r| r.as_array().cloned().unwrap_or_default()).map(|x| x.to_string()).collect();
        assert!(flat.iter().any(|x| x.contains("/c19")) && flat.iter().any(|x| x.contains("/commands 2")) && !flat.iter().any(|x| x.contains("上一页")), "{flat:?}");
        let (t, b) = commands_page(Kind::Agy, "agy-claude", 9);
        assert!(t.contains("3/3") && t.contains("/c44") && b.to_string().contains("/commands 2"), "{t}");
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
        // its turn ends: everything goes out at once: the steps as one message, the answer as its own
        tx.send(Ev::Text { tid: "tgbot:run".into(), gen: 1, delta: "ans".into() }).unwrap();
        tx.send(done("tgbot:run", "ans")).unwrap();
        wait(&|| of(&got, "tgbot:run").len() >= 2);
        let run = of(&got, "tgbot:run");
        assert_eq!(run.len(), 2, "{run:?}");
        assert!(run[0].contains("s1") && run[0].contains("s2") && run[1] == "ans", "{run:?}");
        // not busy: merge_s again
        brief::BUSY_TEST.store(false, std::sync::atomic::Ordering::SeqCst);
        sessions.lock().unwrap().get_mut("tgbot:run").unwrap().pending = true;
        tx.send(step("tgbot:run", "s3")).unwrap();
        wait(&|| of(&got, "tgbot:run").len() == 3);
        assert!(of(&got, "tgbot:run")[2].contains("s3"));
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
