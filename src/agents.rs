//! Driving one agent per topic. An agent is a row of the device's table: a name, the command that starts it,
//! and which protocol it speaks (claude: stream-json; pi: rpc; codex: app-server). Each stays running for the session.
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use crate::{params, text};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind { Claude, Pi, Codex }

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        match s.trim().to_lowercase().as_str() { "claude" => Some(Kind::Claude), "pi" => Some(Kind::Pi), "codex" => Some(Kind::Codex), _ => None }
    }
    pub fn name(&self) -> &'static str { match self { Kind::Claude => "claude", Kind::Pi => "pi", Kind::Codex => "codex" } }
    pub fn label(&self) -> &'static str { match self { Kind::Claude => "Claude", Kind::Pi => "Pi", Kind::Codex => "Codex" } }
    /// The prefix of this kind's commands in the Telegram menu (cc_model, pi_compact, cx_new…).
    pub fn prefix(&self) -> &'static str { match self { Kind::Claude => "cc", Kind::Pi => "pi", Kind::Codex => "cx" } }
    pub const ALL: [Kind; 3] = [Kind::Claude, Kind::Pi, Kind::Codex];
}

/// The agents' own commands that their headless protocols take as requests rather than as typed text. Each keeps
/// the agent's own name (pi's /thinking is pi_thinking, codex's /model is cx_model); exterior only turns it into the
/// protocol call. The menu, the dispatch and the "which agent is this for" check are all built from this table.
pub const BUILTINS: &[(Kind, &str)] = &[
    (Kind::Claude, "model"), (Kind::Claude, "status"), (Kind::Claude, "cost"), (Kind::Claude, "export"), (Kind::Claude, "hooks"),
    (Kind::Claude, "permissions"), (Kind::Claude, "memory"), (Kind::Claude, "exit"),
    (Kind::Pi, "model"), (Kind::Pi, "thinking"), (Kind::Pi, "compact"), (Kind::Pi, "new"), (Kind::Pi, "name"), (Kind::Pi, "session"),
    (Kind::Pi, "fork"), (Kind::Pi, "clone"), (Kind::Pi, "tree"), (Kind::Pi, "export"), (Kind::Pi, "copy"), (Kind::Pi, "resume"),
    (Kind::Pi, "import"), (Kind::Pi, "quit"),
    (Kind::Codex, "model"), (Kind::Codex, "compact"), (Kind::Codex, "new"), (Kind::Codex, "clear"), (Kind::Codex, "status"),
    (Kind::Codex, "usage"), (Kind::Codex, "review"), (Kind::Codex, "diff"), (Kind::Codex, "rename"), (Kind::Codex, "resume"),
    (Kind::Codex, "fork"), (Kind::Codex, "archive"), (Kind::Codex, "goal"), (Kind::Codex, "mcp"), (Kind::Codex, "apps"),
    (Kind::Codex, "plugins"), (Kind::Codex, "skills"), (Kind::Codex, "hooks"), (Kind::Codex, "copy"), (Kind::Codex, "logout"),
    (Kind::Codex, "quit"), (Kind::Codex, "exit"),
];

/// Built-ins that end the agent's process (the session is kept, so the next message resumes it).
pub fn is_quit(name: &str) -> bool { matches!(name, "exit" | "quit") }

/// The menu name of a built-in command, e.g. pi_thinking.
pub fn builtin_command(kind: Kind, name: &str) -> String { format!("{}_{name}", kind.prefix()) }

/// "/pi_thinking" → (Pi, "thinking").
pub fn find_builtin(head: &str) -> Option<(Kind, &'static str)> {
    let h = head.trim_start_matches('/');
    BUILTINS.iter().find(|(k, n)| builtin_command(*k, n) == h).copied()
}

/// The line that tells a new agent where the previous session is, in the previous agent's own words.
pub fn resume_line(kind: Kind, name: &str, id: &str) -> String {
    match kind { Kind::Claude => format!("{name} --resume {id}"), Kind::Pi => format!("{name} --session {id}"), Kind::Codex => format!("{name} resume {id}") }
}

pub enum Ev {
    Text { tid: String, gen: u64, delta: String },
    Done { tid: String, gen: u64, text: Option<String>, stopped: bool, error: Option<String> },
    Session { tid: String, gen: u64, id: String },
    /// side notice worth showing the user: rate limit, model fallback, API retry
    Note { tid: String, gen: u64, text: String },
    /// the command picked a concrete account (cc0 → cc6): later restarts use that one
    Resolved { tid: String, gen: u64, name: String },
    Exited { tid: String, gen: u64, why: String },
}

#[derive(Clone)]
pub struct Spec {
    pub tid: String,
    /// what exterior tells the agent at start (see prompt.rs)
    pub prompt: String,
    pub topic: String,
    pub dev: Value,
    pub dir: String,
    pub command: String,
    pub kind: Kind,
    pub gen: u64,
    pub resume: Option<String>,
}

type Pending = Arc<Mutex<HashMap<String, Sender<Value>>>>;

pub struct Agent {
    pub kind: Kind,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    child: Arc<Mutex<Option<Child>>>,
    spec: Spec,
    tx: Sender<Ev>,
    busy: Arc<Mutex<bool>>,
    /// claude: set when we asked it to stop, so the result that follows is shown as stopped, not as an error
    interrupted: Arc<std::sync::atomic::AtomicBool>,
    queue: Arc<Mutex<Vec<String>>>,
    /// requests waiting for their answer, by id
    pending: Pending,
    next: Arc<std::sync::atomic::AtomicU64>,
    /// claude: the initialize answer (commands, models)
    info: Arc<Mutex<Value>>,
    /// codex: thread, current turn, chosen model
    thread: Arc<Mutex<Option<String>>>,
    turn: Arc<Mutex<Option<String>>>,
    model: Arc<Mutex<Option<String>>>,
}

fn env(spec: &Spec) -> Vec<(&'static str, String)> {
    vec![("EXTERIOR_SESSION", spec.topic.clone()), ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1".into())]
}

/// stderr: remember the last lines (for error messages) and notice "→ cc6" style picks.
fn watch_stderr(err: std::process::ChildStderr, tx: Sender<Ev>, tid: String, gen: u64, last: Arc<Mutex<String>>) {
    std::thread::spawn(move || {
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            let l = line.trim();
            let mark = params::get("pick_mark");
            if let Some(n) = l.strip_prefix(mark.as_str()).filter(|_| !mark.is_empty()) {
                let n = n.trim();
                if !n.is_empty() && !n.contains(' ') { let _ = tx.send(Ev::Resolved { tid: tid.clone(), gen, name: n.to_string() }); }
            }
            if !l.is_empty() {
                let mut s = last.lock().unwrap();
                s.push_str(l); s.push('\n');
                if s.len() > 2000 { let cut = s.len() - 1500; *s = s[s.char_indices().find(|(i, _)| *i >= cut).map(|x| x.0).unwrap_or(0)..].to_string(); }
            }
        }
    });
}

fn put(stdin: &Arc<Mutex<Option<ChildStdin>>>, v: &Value) -> Result<(), String> {
    let mut g = stdin.lock().unwrap();
    let s = g.as_mut().ok_or(text::NOT_RUNNING)?;
    writeln!(s, "{v}").and_then(|_| s.flush()).map_err(|e| e.to_string())
}

/// An answer to one of our requests: hand it to whoever waits for it. Returns true if it was one.
fn route(pending: &Pending, id: Option<String>, v: &Value) -> bool {
    let Some(id) = id else { return false };
    if let Some(w) = pending.lock().unwrap().remove(&id) { let _ = w.send(v.clone()); return true; }
    false
}

/// A model to choose: what to send back, what to show, and (pi) the provider and id it is made of.
pub struct Choice { pub value: String, pub label: String, pub provider: Option<String>, pub id: String }

/// The part of a running agent that answers questions (models, built-in commands). Cheap to clone, so it can be
/// used without holding the sessions lock.
#[derive(Clone)]
pub struct Ctl {
    pub kind: Kind,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    pending: Pending,
    next: Arc<std::sync::atomic::AtomicU64>,
    info: Arc<Mutex<Value>>,
    thread: Arc<Mutex<Option<String>>>,
    model: Arc<Mutex<Option<String>>>,
    dir: String,
    prompt: String,
}

impl Agent {
    pub fn start(spec: Spec, tx: Sender<Ev>) -> Result<Agent, String> {
        let mut a = Agent { kind: spec.kind, stdin: Arc::new(Mutex::new(None)), child: Arc::new(Mutex::new(None)), spec, tx,
                            busy: Arc::new(Mutex::new(false)), interrupted: Arc::new(std::sync::atomic::AtomicBool::new(false)), queue: Arc::new(Mutex::new(vec![])), pending: Arc::new(Mutex::new(HashMap::new())),
                            next: Arc::new(std::sync::atomic::AtomicU64::new(0)), info: Arc::new(Mutex::new(Value::Null)), thread: Arc::new(Mutex::new(None)),
                            turn: Arc::new(Mutex::new(None)), model: Arc::new(Mutex::new(None)) };
        match a.kind {
            Kind::Claude => a.start_claude()?,
            Kind::Pi => a.start_pi()?,
            Kind::Codex => a.start_codex()?,
        }
        Ok(a)
    }

    fn spawn(&mut self, args: Vec<String>) -> Result<(std::process::ChildStdout, Arc<Mutex<String>>), String> {
        let mut c = crate::host::agent(&self.spec.dev, &self.spec.dir, &env(&self.spec), &self.spec.command, &args);
        let mut child = c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
        *self.stdin.lock().unwrap() = child.stdin.take();
        let out = child.stdout.take().unwrap();
        let last = Arc::new(Mutex::new(String::new()));
        watch_stderr(child.stderr.take().unwrap(), self.tx.clone(), self.spec.tid.clone(), self.spec.gen, last.clone());
        *self.child.lock().unwrap() = Some(child);
        Ok((out, last))
    }

    pub fn ctl(&self) -> Ctl {
        Ctl { kind: self.kind, stdin: self.stdin.clone(), pending: self.pending.clone(), next: self.next.clone(), info: self.info.clone(),
              thread: self.thread.clone(), model: self.model.clone(), dir: self.spec.dir.clone(), prompt: self.spec.prompt.clone() }
    }


    fn start_claude(&mut self) -> Result<(), String> {
        let mut args: Vec<String> = ["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
                "--include-partial-messages"].map(String::from).to_vec();
        args.extend(params::args("claude_args"));
        if let Some(id) = &self.spec.resume { args.push("--resume".into()); args.push(id.clone()); }
        if !self.spec.prompt.is_empty() { args.push("--append-system-prompt".into()); args.push(self.spec.prompt.clone()); }
        let (out, last) = self.spawn(args)?;
        let (tx, tid, gen, child, busy, pending, info) = (self.tx.clone(), self.spec.tid.clone(), self.spec.gen, self.child.clone(), self.busy.clone(), self.pending.clone(), self.info.clone());
        let interrupted = self.interrupted.clone();
        std::thread::spawn(move || {
            let mut known = String::new();
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                if v["type"] == "control_response" {
                    let id = v["response"]["request_id"].as_str().map(String::from);
                    if id.as_deref() == Some("init") { *info.lock().unwrap() = v["response"]["response"].clone(); continue; }
                    route(&pending, id, &v["response"]);
                    continue;
                }
                if let Some(s) = v["session_id"].as_str() {
                    if s != known { known = s.to_string(); let _ = tx.send(Ev::Session { tid: tid.clone(), gen, id: known.clone() }); }
                }
                if let Some(t) = claude_note(&v) { let _ = tx.send(Ev::Note { tid: tid.clone(), gen, text: t }); }
                if let Some(d) = v["event"]["delta"]["text"].as_str() { let _ = tx.send(Ev::Text { tid: tid.clone(), gen, delta: d.to_string() }); }
                if v["type"] == "result" {
                    *busy.lock().unwrap() = false;
                    let sub = v["subtype"].as_str().unwrap_or("");
                    let stopped = interrupted.swap(false, std::sync::atomic::Ordering::SeqCst) && sub == "error_during_execution";
                    let failed = !stopped && (v["is_error"] == true || sub.starts_with("error"));
                    let error = failed.then(|| {
                        let listed: Vec<&str> = v["errors"].as_array().into_iter().flatten().filter_map(|e| e.as_str()).collect();
                        let said = v["result"].as_str().filter(|s| !s.trim().is_empty()).map(String::from);
                        said.or_else(|| (!listed.is_empty()).then(|| clip(&listed.join("；")))).unwrap_or_else(|| text::result_error(sub))
                    });
                    let _ = tx.send(Ev::Done { tid: tid.clone(), gen, text: if failed { None } else { v["result"].as_str().map(String::from) }, stopped, error });
                }
            }
            if let Some(mut c) = child.lock().unwrap().take() { let _ = c.wait(); }
            let why = last.lock().unwrap().clone();
            let _ = tx.send(Ev::Exited { tid, gen, why });
        });
        // ask for the commands and models once, right away
        put(&self.stdin, &json!({"type": "control_request", "request_id": "init", "request": {"subtype": "initialize"}}))?;
        Ok(())
    }

    fn start_pi(&mut self) -> Result<(), String> {
        let sid = self.spec.resume.clone().unwrap_or_else(|| format!("exterior-{}-{}", crate::brief::topic_id(&self.spec.tid), crate::store::now()));
        let mut args: Vec<String> = vec!["--mode".into(), "rpc".into(), "--session-id".into(), sid.clone()];
        if !self.spec.prompt.is_empty() { args.push("--append-system-prompt".into()); args.push(self.spec.prompt.clone()); }
        let (out, last) = self.spawn(args)?;
        let _ = self.tx.send(Ev::Session { tid: self.spec.tid.clone(), gen: self.spec.gen, id: sid });
        let (tx, tid, gen, child, busy, pending) = (self.tx.clone(), self.spec.tid.clone(), self.spec.gen, self.child.clone(), self.busy.clone(), self.pending.clone());
        let stdin = self.stdin.clone();
        std::thread::spawn(move || {
            let mut last_text: Option<String> = None;
            let mut last_err: Option<String> = None;
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                if v["type"] == "response" && route(&pending, v["id"].as_str().map(String::from), &v) { continue; }
                match v["type"].as_str() {
                    Some("message_update") => {
                        let e = &v["assistantMessageEvent"];
                        if e["type"] == "text_delta" { let _ = tx.send(Ev::Text { tid: tid.clone(), gen, delta: e["delta"].as_str().unwrap_or("").to_string() }); }
                    }
                    Some("turn_end") => {
                        let t: String = v["message"]["content"].as_array().into_iter().flatten()
                            .filter(|c| c["type"] == "text").filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("");
                        if !t.is_empty() { last_text = Some(t); }
                        if v["message"]["stopReason"] == "error" {
                            let m = v["message"]["errorMessage"].as_str().unwrap_or(text::MODEL_ERROR);
                            // pi writes "<status> <json body>": show the status and the body's message
                            let inner = serde_json::from_str::<Value>(m.split_once(' ').map(|x| x.1).unwrap_or("")).ok()
                                .and_then(|j| j["error"]["message"].as_str().map(String::from));
                            last_err = Some(match inner { Some(i) => format!("{} {i}", m.split(' ').next().unwrap_or("")), None => clip(m) });
                        } else { last_err = None; }
                    }
                    Some("auto_retry_start") => {
                        let a = v["attempt"].as_i64().unwrap_or(0);
                        if worth_noting(a) {
                            let _ = tx.send(Ev::Note { tid: tid.clone(), gen, text: text::retrying(text::Source::Model, a, v["maxAttempts"].as_i64().unwrap_or(0), &clip(v["errorMessage"].as_str().unwrap_or(""))) });
                        }
                    }
                    Some("auto_retry_end") if v["success"] == false => {
                        if let Some(e) = v["finalError"].as_str() { if last_err.is_none() { last_err = Some(clip(e)); } }
                    }
                    Some("summarization_retry_scheduled") => {
                        let a = v["attempt"].as_i64().unwrap_or(0);
                        if worth_noting(a) {
                            let _ = tx.send(Ev::Note { tid: tid.clone(), gen, text: text::retrying(text::Source::Compaction, a, v["maxAttempts"].as_i64().unwrap_or(0), &clip(v["errorMessage"].as_str().unwrap_or(""))) });
                        }
                    }
                    Some("compaction_end") => {
                        if let Some(e) = v["errorMessage"].as_str() { let _ = tx.send(Ev::Note { tid: tid.clone(), gen, text: text::compaction_failed(&clip(e)) }); }
                    }
                    Some("extension_ui_request") => {
                        let what = v["title"].as_str().or(v["message"].as_str()).unwrap_or("").to_string();
                        match v["method"].as_str().unwrap_or("") {
                            "notify" if matches!(v["notifyType"].as_str(), Some("warning") | Some("error")) => {
                                let _ = tx.send(Ev::Note { tid: tid.clone(), gen, text: text::extension_notice(&clip(&what)) });
                            }
                            // dialogs wait for an answer; exterior has no way to ask the user yet, so it cancels them and says so
                            m @ ("select" | "confirm" | "input" | "editor") => {
                                let _ = put(&stdin, &json!({"type": "extension_ui_response", "id": v["id"], "cancelled": true}));
                                let _ = tx.send(Ev::Note { tid: tid.clone(), gen, text: text::dialog_cancelled(m, &clip(&what)) });
                            }
                            _ => {}
                        }
                    }
                    Some("response") if v["command"] == "prompt" && v["data"]["disposition"] == "handled" => {
                        // an extension consumed the prompt: no run follows
                        *busy.lock().unwrap() = false;
                        let _ = tx.send(Ev::Done { tid: tid.clone(), gen, text: None, stopped: false, error: None });
                    }
                    Some("extension_error") => {
                        let e = v["error"].as_str().map(String::from).unwrap_or_else(|| v.to_string());
                        let _ = tx.send(Ev::Note { tid: tid.clone(), gen, text: text::extension_error(&clip(&e)) });
                    }
                    Some("agent_settled") => {
                        *busy.lock().unwrap() = false;
                        let _ = tx.send(Ev::Done { tid: tid.clone(), gen, text: last_text.take(), stopped: v["aborted"] == true, error: last_err.take() });
                    }
                    Some("response") if v["success"] == false => {
                        *busy.lock().unwrap() = false;
                        let err = v["error"].as_str().unwrap_or(text::PI_REJECTED).to_string();
                        let _ = tx.send(Ev::Done { tid: tid.clone(), gen, text: None, stopped: false, error: Some(err) });
                    }
                    _ => {}
                }
            }
            if let Some(mut c) = child.lock().unwrap().take() { let _ = c.wait(); }
            let why = last.lock().unwrap().clone();
            let _ = tx.send(Ev::Exited { tid, gen, why });
        });
        Ok(())
    }

    /// codex: one `codex app-server` per session, kept running; each message is a turn on the same thread.
    fn start_codex(&mut self) -> Result<(), String> {
        let (out, last) = self.spawn(vec!["app-server".into()])?;
        let (tx, tid, gen, child, busy, pending) = (self.tx.clone(), self.spec.tid.clone(), self.spec.gen, self.child.clone(), self.busy.clone(), self.pending.clone());
        let (stdin, queue, thread, turn, model) = (self.stdin.clone(), self.queue.clone(), self.thread.clone(), self.turn.clone(), self.model.clone());
        std::thread::spawn(move || {
            let (mut reply, mut error, mut retries): (String, Option<String>, i64) = (String::new(), None, 0);
            let mut limits = CodexLimits::default();
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                let method = v["method"].as_str().unwrap_or("");
                if method.is_empty() {
                    route(&pending, v["id"].as_str().map(String::from).or(v["id"].as_u64().map(|n| n.to_string())), &v);
                    continue;
                }
                if !v["id"].is_null() {
                    // a request from codex (approvals): we run without approvals, so say yes; anything else, decline and say so
                    let res = if method.ends_with("requestApproval") { json!({"id": v["id"], "result": {"decision": "accept"}}) }
                              else if method == "execCommandApproval" || method == "applyPatchApproval" { json!({"id": v["id"], "result": {"decision": "approved"}}) }
                              else {
                                  let _ = tx.send(Ev::Note { tid: tid.clone(), gen, text: text::codex_request_declined(method) });
                                  json!({"id": v["id"], "error": {"code": -32601, "message": text::CODEX_UNHANDLED}})
                              };
                    let _ = put(&stdin, &res);
                    continue;
                }
                let p = &v["params"];
                match method {
                    "thread/started" => {
                        if let Some(t) = p["thread"]["id"].as_str() {
                            *thread.lock().unwrap() = Some(t.to_string());
                            let _ = tx.send(Ev::Session { tid: tid.clone(), gen, id: t.to_string() });
                        }
                    }
                    "turn/started" => { *turn.lock().unwrap() = p["turn"]["id"].as_str().map(String::from); retries = 0; }
                    "item/agentMessage/delta" => {
                        let d = p["delta"].as_str().unwrap_or("");
                        reply.push_str(d);
                        let _ = tx.send(Ev::Text { tid: tid.clone(), gen, delta: d.to_string() });
                    }
                    "item/completed" if p["item"]["type"] == "exitedReviewMode" => {
                        // the result of /cx_review
                        if let Some(r) = p["item"]["review"].as_str() { reply.push_str(r); let _ = tx.send(Ev::Text { tid: tid.clone(), gen, delta: r.to_string() }); }
                    }
                    "item/completed" if p["item"]["type"] == "agentMessage" => {
                        if !reply.is_empty() && !reply.ends_with("\n\n") { reply.push_str("\n\n"); let _ = tx.send(Ev::Text { tid: tid.clone(), gen, delta: "\n\n".into() }); }
                    }
                    "error" if p["willRetry"] == true => {
                        retries += 1;
                        if worth_noting(retries) {
                            let _ = tx.send(Ev::Note { tid: tid.clone(), gen, text: text::retrying(text::Source::Codex, retries, 0, &clip(p["error"]["message"].as_str().unwrap_or(""))) });
                        }
                    }
                    "error" => { error = Some(codex_error(&p["error"])); }
                    "warning" | "guardianWarning" => {
                        if let Some(m) = p["message"].as_str() { let _ = tx.send(Ev::Note { tid: tid.clone(), gen, text: clip(m) }); }
                    }
                    "deprecationNotice" | "configWarning" => {
                        let t = [p["summary"].as_str(), p["details"].as_str(), p["path"].as_str()].into_iter().flatten().collect::<Vec<_>>().join("；");
                        if !t.is_empty() { let _ = tx.send(Ev::Note { tid: tid.clone(), gen, text: clip(&t) }); }
                    }
                    "model/rerouted" => {
                        let _ = tx.send(Ev::Note { tid: tid.clone(), gen, text: text::fallback(p["fromModel"].as_str().unwrap_or("?"), p["toModel"].as_str().unwrap_or("?")) });
                    }
                    "account/rateLimits/updated" => {
                        for t in limits.update(&p["rateLimits"]) { let _ = tx.send(Ev::Note { tid: tid.clone(), gen, text: t }); }
                    }
                    "mcpServer/startupStatus/updated" if p["status"] == "failed" => {
                        let _ = tx.send(Ev::Note { tid: tid.clone(), gen, text: text::mcp_failed(p["name"].as_str().unwrap_or("?"), &clip(p["error"].as_str().unwrap_or(""))) });
                    }
                    "modelProvider/authRecoveryStarted" => {
                        let _ = tx.send(Ev::Note { tid: tid.clone(), gen, text: text::auth_recovery(p["provider"].as_str().unwrap_or(""), &clip(p["message"].as_str().unwrap_or(""))) });
                    }
                    "thread/status/changed" if p["status"]["type"] == "systemError" => {
                        let _ = tx.send(Ev::Note { tid: tid.clone(), gen, text: text::CODEX_SYSTEM_ERROR.into() });
                    }
                    "hook/completed" if matches!(p["run"]["status"].as_str(), Some("failed") | Some("blocked")) => {
                        let r = &p["run"];
                        let detail = r["statusMessage"].as_str().map(String::from).unwrap_or_else(|| r["entries"].as_array().into_iter().flatten().filter_map(|e| e["text"].as_str()).collect::<Vec<_>>().join("；"));
                        let _ = tx.send(Ev::Note { tid: tid.clone(), gen, text: text::hook_failed(r["eventName"].as_str().unwrap_or(""), &clip(&detail)) });
                    }
                    "turn/completed" => {
                        *turn.lock().unwrap() = None;
                        let st = p["turn"]["status"].as_str().unwrap_or("");
                        if st == "failed" && error.is_none() { error = Some(codex_error(&p["turn"]["error"])); }
                        let text = reply.trim().to_string();
                        let _ = tx.send(Ev::Done { tid: tid.clone(), gen, text: if text.is_empty() { None } else { Some(text) }, stopped: st == "interrupted", error: error.take() });
                        reply.clear();
                        // the next waiting message, if any
                        let next = { let mut q = queue.lock().unwrap(); if q.is_empty() { None } else { Some(q.remove(0)) } };
                        match (next, thread.lock().unwrap().clone()) {
                            (Some(t), Some(th)) => { let _ = put(&stdin, &codex_turn(&th, &t, model.lock().unwrap().clone())); }
                            _ => { *busy.lock().unwrap() = false; }
                        }
                    }
                    _ => {}
                }
            }
            if let Some(mut c) = child.lock().unwrap().take() { let _ = c.wait(); }
            let why = last.lock().unwrap().clone();
            let _ = tx.send(Ev::Exited { tid, gen, why });
        });
        // the handshake runs in the background, so starting codex never holds anything up; messages wait in the queue
        let (c, tx, spec, thread, queue, busy, model, stdin) = (self.ctl(), self.tx.clone(), self.spec.clone(), self.thread.clone(), self.queue.clone(), self.busy.clone(), self.model.clone(), self.stdin.clone());
        std::thread::spawn(move || {
            let r = (|| -> Result<String, String> {
                c.ask(|id| json!({"jsonrpc": "2.0", "id": id, "method": "initialize", "params": {"clientInfo": {"name": "exterior", "version": env!("CARGO_PKG_VERSION")}}}))?;
                put(&stdin, &json!({"jsonrpc": "2.0", "method": "initialized"}))?;
                let mut params = match &spec.resume { Some(t) => json!({"threadId": t}), None => json!({}) };
                for (k, v) in [("cwd", json!(spec.dir)), ("developerInstructions", json!(spec.prompt)), ("approvalPolicy", json!(params::get("codex_approval"))), ("sandbox", json!(params::get("codex_sandbox")))] { params[k] = v; }
                let method = if spec.resume.is_some() { "thread/resume" } else { "thread/start" };
                let r = c.ask(|id| json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
                if let Some(e) = r["error"]["message"].as_str() { return Err(e.to_string()); }
                r["result"]["thread"]["id"].as_str().map(String::from).ok_or(text::CODEX_NO_THREAD.into())
            })();
            match r {
                Ok(t) => {
                    let mut q = queue.lock().unwrap();
                    let new = thread.lock().unwrap().as_deref() != Some(t.as_str());
                    *thread.lock().unwrap() = Some(t.clone());
                    if new { let _ = tx.send(Ev::Session { tid: spec.tid.clone(), gen: spec.gen, id: t.clone() }); }
                    let next = if q.is_empty() { None } else { Some(q.remove(0)) };
                    drop(q);
                    match next { Some(m) => { let _ = put(&stdin, &codex_turn(&t, &m, model.lock().unwrap().clone())); } None => *busy.lock().unwrap() = false }
                }
                Err(e) => {
                    queue.lock().unwrap().clear();
                    *busy.lock().unwrap() = false;
                    let _ = tx.send(Ev::Done { tid: spec.tid.clone(), gen: spec.gen, text: None, stopped: false, error: Some(text::codex_start_failed(&e)) });
                }
            }
        });
        Ok(())
    }

    /// Hand one message (or one of the agent's own slash commands) to the agent.
    pub fn say(&mut self, text: &str) -> Result<(), String> {
        let was_busy = { let mut b = self.busy.lock().unwrap(); let w = *b; *b = true; w };
        match self.kind {
            Kind::Claude => put(&self.stdin, &json!({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": text}]}})),
            Kind::Pi => {
                let mut m = json!({"type": "prompt", "message": text});
                if was_busy { m["streamingBehavior"] = json!("followUp"); }
                put(&self.stdin, &m)
            }
            Kind::Codex => {
                // the queue lock also guards "is the thread ready yet", so a message can't slip between the two
                let th = {
                    let mut q = self.queue.lock().unwrap();
                    let th = self.thread.lock().unwrap().clone();
                    match th.filter(|_| !was_busy) { Some(th) => th, None => { q.push(text.to_string()); return Ok(()); } }
                };
                put(&self.stdin, &codex_turn(&th, text, self.model.lock().unwrap().clone()))
            }
        }
    }

    /// Esc.
    pub fn stop(&mut self) {
        match self.kind {
            Kind::Claude => {
                self.interrupted.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = put(&self.stdin, &json!({"type": "control_request", "request_id": "stop", "request": {"subtype": "interrupt"}}));
            }
            Kind::Pi => { let _ = put(&self.stdin, &json!({"type": "abort"})); }
            Kind::Codex => {
                self.queue.lock().unwrap().clear();
                let (th, tu) = (self.thread.lock().unwrap().clone(), self.turn.lock().unwrap().clone());
                if let (Some(th), Some(tu)) = (th, tu) { let _ = put(&self.stdin, &json!({"jsonrpc": "2.0", "id": "stop", "method": "turn/interrupt", "params": {"threadId": th, "turnId": tu}})); }
            }
        }
    }

    pub fn kill(&mut self) {
        *self.stdin.lock().unwrap() = None;
        if let Some(c) = self.child.lock().unwrap().as_mut() { let _ = c.kill(); }
    }

}

impl Ctl {
    /// codex: wait until the background handshake is done.
    fn ready(&self) -> Result<(), String> {
        if self.kind != Kind::Codex { return Ok(()); }
        let until = std::time::Instant::now() + params::secs("codex_ready_s");
        while std::time::Instant::now() < until {
            if self.thread.lock().unwrap().is_some() { return Ok(()); }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        Err(text::CODEX_NOT_READY.into())
    }

    /// Send a request and wait for its answer. `make` puts the id into the request.
    pub fn ask(&self, make: impl FnOnce(&str) -> Value) -> Result<Value, String> {
        let id = format!("x{}", self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1);
        let (w, r) = std::sync::mpsc::channel();
        self.pending.lock().unwrap().insert(id.clone(), w);
        let v = make(&id);
        if let Err(e) = put(&self.stdin, &v) { self.pending.lock().unwrap().remove(&id); return Err(e); }
        let secs = params::num("ask_timeout_s");
        r.recv_timeout(std::time::Duration::from_secs(secs)).map_err(|_| { self.pending.lock().unwrap().remove(&id); text::no_response(secs) })
    }

    /// The models this agent can switch to.
    pub fn models(&self) -> Result<Vec<Choice>, String> {
        self.ready()?;
        match self.kind {
            Kind::Claude => {
                let until = std::time::Instant::now() + params::secs("ask_timeout_s");
                while self.info.lock().unwrap().is_null() && std::time::Instant::now() < until { std::thread::sleep(std::time::Duration::from_millis(200)); }
                let info = self.info.lock().unwrap().clone();
                if info.is_null() { return Err(text::no_response(params::num("ask_timeout_s"))); }
                Ok(info["models"].as_array().into_iter().flatten().filter_map(|m| { let v = m["value"].as_str()?; Some(Choice { value: v.into(), label: m["displayName"].as_str().unwrap_or(v).into(), provider: None, id: v.into() }) }).collect())
            }
            Kind::Pi => {
                let r = self.ask(|id| json!({"type": "get_available_models", "id": id}))?;
                Ok(r["data"]["models"].as_array().into_iter().flatten().filter_map(|m| {
                    let (p, id) = (m["provider"].as_str()?, m["id"].as_str()?);
                    Some(Choice { value: format!("{p}/{id}"), label: m["name"].as_str().unwrap_or(id).into(), provider: Some(p.into()), id: id.into() })
                }).collect())
            }
            Kind::Codex => {
                let r = self.ask(|id| json!({"jsonrpc": "2.0", "id": id, "method": "model/list", "params": {}}))?;
                Ok(r["result"]["data"].as_array().into_iter().flatten().filter(|m| m["hidden"] != true).filter_map(|m| {
                    let v = m["model"].as_str().or(m["id"].as_str())?;
                    Some(Choice { value: v.into(), label: m["displayName"].as_str().unwrap_or(v).into(), provider: None, id: v.into() })
                }).collect())
            }
        }
    }

    pub fn set_model(&self, value: &str) -> Result<(), String> {
        let r = match self.kind {
            Kind::Claude => self.ask(|id| json!({"type": "control_request", "request_id": id, "request": {"subtype": "set_model", "model": value}}))?,
            Kind::Pi => {
                // pi wants the provider and the id separately: find the model in pi's own list
                let list = self.models()?;
                let c = list.iter().find(|c| c.value == value).or_else(|| list.iter().find(|c| c.id == value || c.label == value))
                    .ok_or_else(|| text::model_unknown(self.kind.label(), value))?;
                let (p, m) = (c.provider.clone().unwrap_or_default(), c.id.clone());
                self.ask(|id| json!({"type": "set_model", "id": id, "provider": p, "modelId": m}))?
            }
            Kind::Codex => { *self.model.lock().unwrap() = Some(value.to_string()); return Ok(()); }
        };
        if r["subtype"] == "error" || r["success"] == false { return Err(r["error"].as_str().unwrap_or(text::SET_MODEL_FAILED).to_string()); }
        Ok(())
    }

    /// The thinking levels the agent offers for its current model (pi).
    pub fn thinking_levels(&self) -> Result<Vec<String>, String> {
        match self.kind {
            Kind::Pi => {
                let r = self.ask(|id| json!({"type": "get_available_thinking_levels", "id": id}))?;
                if r["success"] == false { return Err(r["error"].as_str().unwrap_or(text::BUILTIN_FAILED).to_string()); }
                Ok(r["data"]["levels"].as_array().into_iter().flatten().filter_map(|l| l.as_str().map(String::from)).collect())
            }
            _ => Err(text::NOT_SUPPORTED.into()),
        }
    }

    /// Built-in commands that are not prompts (see BUILTINS).
    /// Returns the answer's data; for pi's new and fork also the new session id under "_session".
    pub fn builtin(&self, name: &str, arg: &str) -> Result<Value, String> {
        self.ready()?;
        let codex_thread = || self.thread.lock().unwrap().clone().ok_or_else(|| text::CODEX_NOT_READY.to_string());
        let start_params = || json!({"cwd": self.dir, "developerInstructions": self.prompt, "approvalPolicy": params::get("codex_approval"), "sandbox": params::get("codex_sandbox")});
        let claude = |sub: &str| self.ask(|id| json!({"type": "control_request", "request_id": id, "request": {"subtype": sub}}));
        let codex = |method: &str, p: Value| self.ask(|id| json!({"jsonrpc": "2.0", "id": id, "method": method, "params": p}));
        let r = match (self.kind, name) {
            (Kind::Claude, "status") => claude("get_status")?,
            (Kind::Claude, "cost") => claude("get_session_cost")?,
            (Kind::Claude, "export") => claude("export_conversation")?,
            (Kind::Claude, "hooks") => claude("get_hooks_listing")?,
            (Kind::Claude, "permissions") => claude("list_permission_rules")?,
            (Kind::Claude, "memory") => claude("get_memory_dialog")?,
            (Kind::Pi, "clone") => self.ask(|id| json!({"type": "clone", "id": id}))?,
            (Kind::Pi, "tree") => self.ask(|id| json!({"type": "get_tree", "id": id}))?,
            (Kind::Pi, "copy") => self.ask(|id| json!({"type": "get_last_assistant_text", "id": id}))?,
            (Kind::Pi, "resume") if arg.is_empty() => {
                // pi has no "list sessions" request: list the session files next to the current one, through pi's own shell
                let st = self.ask(|id| json!({"type": "get_state", "id": id}))?;
                let dir = st["data"]["sessionFile"].as_str().and_then(|f| f.rsplit_once('/')).map(|x| x.0.to_string()).ok_or(text::NO_SESSIONS)?;
                self.ask(|id| json!({"type": "bash", "id": id, "command": format!("ls -t {} | head -n 10", crate::host::quote(&dir))}))?
                    .pipe(|mut r| { r["data"]["_dir"] = json!(dir); r })
            }
            (Kind::Pi, "resume") | (Kind::Pi, "import") => self.ask(|id| json!({"type": "switch_session", "id": id, "sessionPath": arg}))?,
            (Kind::Codex, "clear") => codex("thread/start", start_params())?,
            (Kind::Codex, "usage") => codex("account/usage/read", json!({}))?,
            (Kind::Codex, "diff") => codex("command/exec", json!({"command": ["sh", "-c", "git status --short && git diff"], "cwd": self.dir}))?,
            (Kind::Codex, "rename") => codex("thread/name/set", json!({"threadId": codex_thread()?, "name": arg}))?,
            (Kind::Codex, "resume") if arg.is_empty() => codex("thread/list", json!({"cwd": self.dir, "limit": 10}))?,
            (Kind::Codex, "resume") => {
                let mut p = start_params();
                p["threadId"] = json!(arg);
                let r = codex("thread/resume", p)?;
                if let Some(t) = r["result"]["thread"]["id"].as_str() { *self.thread.lock().unwrap() = Some(t.to_string()); }
                r
            }
            (Kind::Codex, "fork") => {
                let r = codex("thread/fork", json!({"threadId": codex_thread()?}))?;
                if let Some(t) = r["result"]["thread"]["id"].as_str() { *self.thread.lock().unwrap() = Some(t.to_string()); }
                r
            }
            (Kind::Codex, "archive") => {
                codex("thread/archive", json!({"threadId": codex_thread()?}))?;
                // the archived thread is no longer used: continue in a new one
                codex("thread/start", start_params())?
            }
            (Kind::Codex, "goal") if arg.is_empty() => codex("thread/goal/get", json!({"threadId": codex_thread()?}))?,
            (Kind::Codex, "goal") if arg == "clear" => codex("thread/goal/clear", json!({"threadId": codex_thread()?}))?,
            (Kind::Codex, "goal") => codex("thread/goal/set", json!({"threadId": codex_thread()?, "objective": arg}))?,
            (Kind::Codex, "mcp") => codex("mcpServerStatus/list", json!({"threadId": codex_thread()?}))?,
            (Kind::Codex, "apps") => codex("app/list", json!({"threadId": codex_thread()?}))?,
            (Kind::Codex, "plugins") => codex("plugin/list", json!({"cwds": [self.dir]}))?,
            (Kind::Codex, "skills") => codex("skills/list", json!({"cwds": [self.dir]}))?,
            (Kind::Codex, "hooks") => codex("hooks/list", json!({"cwds": [self.dir]}))?,
            (Kind::Codex, "logout") => codex("account/logout", json!({}))?,
            (Kind::Pi, "name") => self.ask(|id| json!({"type": "set_session_name", "id": id, "name": arg}))?,
            (Kind::Pi, "session") => self.ask(|id| json!({"type": "get_session_stats", "id": id}))?,
            (Kind::Pi, "export") if arg.is_empty() => self.ask(|id| json!({"type": "export_html", "id": id}))?,
            (Kind::Pi, "export") => self.ask(|id| json!({"type": "export_html", "id": id, "outputPath": arg}))?,
            (Kind::Pi, "fork") if arg.is_empty() => self.ask(|id| json!({"type": "get_fork_messages", "id": id}))?,
            (Kind::Pi, "fork") => self.ask(|id| json!({"type": "fork", "id": id, "entryId": arg}))?,
            (Kind::Codex, "status") => {
                let mut r = self.ask(|id| json!({"jsonrpc": "2.0", "id": id, "method": "account/rateLimits/read", "params": {}}))?;
                r["result"]["_model"] = json!(self.model.lock().unwrap().clone());
                r
            }
            (Kind::Codex, "review") => {
                let target = review_target(arg);
                let th = codex_thread()?;
                self.ask(|id| json!({"jsonrpc": "2.0", "id": id, "method": "review/start", "params": {"threadId": th, "target": target}}))?
            }
            (Kind::Pi, "thinking") => self.ask(|id| json!({"type": "set_thinking_level", "id": id, "level": arg}))?,
            (Kind::Pi, "compact") => self.ask(|id| json!({"type": "compact", "id": id}))?,
            (Kind::Pi, "new") => self.ask(|id| json!({"type": "new_session", "id": id}))?,
            (Kind::Codex, "compact") => {
                let th = codex_thread()?;
                self.ask(|id| json!({"jsonrpc": "2.0", "id": id, "method": "thread/compact/start", "params": {"threadId": th}}))?
            }
            (Kind::Codex, "new") => codex("thread/start", start_params())?,
            _ => return Err(text::NOT_SUPPORTED.into()),
        };
        if r["success"] == false || !r["error"].is_null() || r["subtype"] == "error" {
            return Err(r["error"]["message"].as_str().or(r["error"].as_str()).unwrap_or(text::BUILTIN_FAILED).to_string());
        }
        let mut data = match self.kind { Kind::Codex => r["result"].clone(), Kind::Claude => r["response"].clone(), Kind::Pi => r["data"].clone() };
        if self.kind == Kind::Codex && matches!(name, "new" | "clear" | "resume" | "fork" | "archive") {
            if let Some(t) = data["thread"]["id"].as_str() { *self.thread.lock().unwrap() = Some(t.to_string()); data["_session"] = json!(t); }
        }
        // pi writes the export next to its working directory: report the full path
        if let Some(p) = data["path"].as_str().filter(|p| !p.starts_with('/')) { data["path"] = json!(format!("{}/{p}", self.dir.trim_end_matches('/'))); }
        if self.kind == Kind::Pi && matches!(name, "new" | "fork" | "clone" | "resume" | "import") && !(matches!(name, "fork" | "resume") && arg.is_empty()) {
            // pi now writes to another session: remember it, so a restart resumes that one
            if let Ok(s) = self.ask(|id| json!({"type": "get_state", "id": id})) {
                if !data.is_object() { data = json!({}); }
                data["_session"] = s["data"]["sessionId"].clone();
            }
        }
        Ok(data)
    }
}

/// `x.pipe(f)` = `f(x)`, to adjust a value inside an expression.
trait Pipe: Sized { fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T { f(self) } }
impl<S> Pipe for S {}

/// /cx_review's argument: nothing for the uncommitted changes, a commit hash, or a base branch.
pub fn review_target(arg: &str) -> Value {
    let a = arg.trim();
    if a.is_empty() { json!({"type": "uncommittedChanges"}) }
    else if a.len() >= 7 && a.len() <= 40 && a.chars().all(|c| c.is_ascii_hexdigit()) { json!({"type": "commit", "sha": a}) }
    else { json!({"type": "baseBranch", "branch": a}) }
}

fn codex_turn(thread: &str, text: &str, model: Option<String>) -> Value {
    let mut p = json!({"threadId": thread, "input": [{"type": "text", "text": text}]});
    if let Some(m) = model { p["model"] = json!(m); }
    json!({"jsonrpc": "2.0", "id": format!("turn-{}", crate::store::now()), "method": "turn/start", "params": p})
}

/// One question, one answer: used by the 测试 button on the 设备 page.
pub fn test(dev: &Value, dir: &str, command: &str, kind: Kind) -> Result<String, String> {
    let q = crate::prompt::read(dev, "test_prompt")?;
    let q = q.as_str();
    let args: Vec<String> = match kind {
        Kind::Claude => [vec!["-p".into(), q.into()], params::args("claude_args")].concat(),
        Kind::Pi => vec!["-p".into(), "--no-session".into(), q.into()],
        Kind::Codex => vec!["exec".into(), "--skip-git-repo-check".into(), q.into()],
    };
    let mut c = crate::host::agent(dev, dir, &[("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1".into())], command, &args);
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = c.spawn().map_err(|e| e.to_string())?;
    let out = crate::host::wait(child, params::num("test_timeout_s"))?;
    let so = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let se = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let mark = params::get("pick_mark");
    let picked = se.lines().filter_map(|l| l.trim().strip_prefix(mark.as_str()).filter(|_| !mark.is_empty())).last().map(|n| text::picked(command, n.trim())).unwrap_or_default();
    if out.status.success() && !so.is_empty() {
        Ok(format!("{}{picked}", clip(so.lines().last().unwrap_or(""))))
    } else {
        let msg = if se.is_empty() { so } else { se };
        Err(text::exit_code(out.status.code(), &last_lines(&msg).replace('\n', " / ")))
    }
}

/// The agent's own slash commands, with descriptions (no model call).
pub fn commands(dev: &Value, dir: &str, command: &str, kind: Kind) -> Vec<(String, String)> {
    let (args, ask): (Vec<String>, Value) = match kind {
        Kind::Claude => ([["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose"].map(String::from).to_vec(), params::args("claude_args")].concat(),
                         json!({"type": "control_request", "request_id": "init", "request": {"subtype": "initialize"}})),
        Kind::Pi => (vec!["--mode".into(), "rpc".into(), "--no-session".into()], json!({"type": "get_commands"})),
        Kind::Codex => return vec![],
    };
    let Ok(mut child) = crate::host::agent(dev, dir, &env(&Spec { tid: String::new(), prompt: String::new(), topic: String::new(), dev: dev.clone(), dir: dir.into(), command: command.into(), kind, gen: 0, resume: None }), command, &args)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() else { return vec![] };
    let mut stdin = child.stdin.take().unwrap();
    let _ = writeln!(stdin, "{ask}");
    let _ = stdin.flush();
    // never wait forever: if the agent does not answer in time, close it
    let stdout = child.stdout.take().unwrap();
    let child = Arc::new(Mutex::new(child));
    { let c = child.clone(); std::thread::spawn(move || { std::thread::sleep(params::secs("commands_timeout_s")); let _ = c.lock().unwrap().kill(); }); }
    let mut out = vec![];
    let lines = BufReader::new(stdout).lines();
    for line in lines.map_while(Result::ok).take(500) {
        let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        let list = if v["type"] == "control_response" { v["response"]["response"]["commands"].clone() }
                   else if v["command"] == "get_commands" { if v["data"]["commands"].is_array() { v["data"]["commands"].clone() } else { v["data"].clone() } }
                   else { continue };
        for c in list.as_array().into_iter().flatten() {
            if let Some(n) = c["name"].as_str() { out.push((n.to_string(), c["description"].as_str().unwrap_or("").to_string())); }
        }
        break;
    }
    drop(stdin);
    let _ = child.lock().unwrap().kill();
    out
}

/// codex's error object: its message, with the extra details and the error kind when given.
fn codex_error(e: &Value) -> String {
    let mut parts: Vec<String> = vec![e["message"].as_str().filter(|s| !s.is_empty()).unwrap_or(text::CODEX_ERROR).to_string()];
    if let Some(d) = e["additionalDetails"].as_str().filter(|s| !s.is_empty()) { parts.push(d.to_string()); }
    match &e["codexErrorInfo"] { Value::String(k) => parts.push(format!("（{k}）")), Value::Object(o) => if let Some(k) = o.keys().next() { parts.push(format!("（{k}）")) }, _ => {} }
    clip(&parts.join(" "))
}

/// codex's rate-limit updates arrive often; say something only when a limit is reached, or when usage first
/// passes the `limit_warn_pct` setting in a window.
#[derive(Default)]
struct CodexLimits { reached: Option<String>, warned: std::collections::HashSet<String> }

impl CodexLimits {
    fn update(&mut self, r: &Value) -> Vec<String> {
        let mut out = vec![];
        let reached = r["rateLimitReachedType"].as_str().map(String::from);
        if reached.is_some() && reached != self.reached { out.push(text::limit_reached(reached.as_deref().unwrap_or(""), &resets(&r["primary"]))); }
        self.reached = reached;
        let warn = params::num("limit_warn_pct") as i64;
        for w in ["primary", "secondary"] {
            let Some(used) = r[w]["usedPercent"].as_i64() else { continue };
            let key = format!("{w}:{}", r[w]["resetsAt"]);
            if used >= warn && self.warned.insert(key) { out.push(text::limit_near(&text::window(r[w]["windowDurationMins"].as_i64()), &text::used_pct(used as f64), &resets(&r[w]))); }
        }
        out
    }
}

fn resets(w: &Value) -> String {
    w["resetsAt"].as_i64().map(|t| t - crate::store::now() as i64).filter(|l| *l > 0).map(|l| text::resets_in(l / 3600, l % 3600 / 60)).unwrap_or_default()
}

/// Whether a retry is worth a message: the first one, then every `notice_every`-th.
fn worth_noting(attempt: i64) -> bool {
    let every = params::num("notice_every").max(1) as i64;
    attempt == 1 || attempt % every == 0
}

/// Keep an error short enough for a message.
pub fn clip(s: &str) -> String {
    let n = params::num("error_chars") as usize;
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

/// The last few lines of an error output.
pub fn last_lines(s: &str) -> String {
    let n = params::num("error_lines") as usize;
    let v: Vec<&str> = s.trim().lines().collect();
    v[v.len().saturating_sub(n)..].join("\n")
}

/// Claude's side notices that the user should see. Claude marks its own warnings with `level`; limits and
/// retries come as their own events.
pub fn claude_note(v: &Value) -> Option<String> {
    match (v["type"].as_str()?, v["subtype"].as_str().unwrap_or("")) {
        ("rate_limit_event", _) => {
            let i = &v["rate_limit_info"];
            let st = i["status"].as_str().unwrap_or("");
            if st == "allowed" || st.is_empty() { return None; }
            let kind = i["rateLimitType"].as_str().unwrap_or("");
            let when = i["resetsAt"].as_i64().map(|t| {
                let t = if t > 1_000_000_000_000 { t / 1000 } else { t };
                let left = t - crate::store::now() as i64;
                if left > 0 { text::resets_in(left / 3600, left % 3600 / 60) } else { String::new() }
            }).unwrap_or_default();
            let pct = i["utilization"].as_f64().map(|u| text::used_pct(if u <= 1.0 { u * 100.0 } else { u })).unwrap_or_default();
            Some(if st == "rejected" { text::limit_reached(kind, &when) } else { text::limit_near(kind, &pct, &when) })
        }
        ("system", "api_retry") => {
            let a = v["attempt"].as_i64().unwrap_or(0);
            if !worth_noting(a) { return None; }
            let e = v["error"].as_str().map(String::from).unwrap_or_else(|| v["error_status"].to_string());
            Some(text::retrying(text::Source::Api, a, v["max_retries"].as_i64().unwrap_or(0), &clip(&e)))
        }
        ("system", "hook_response") if v["outcome"] == "error" => {
            let e = v["stderr"].as_str().filter(|s| !s.trim().is_empty()).or(v["error"].as_str()).or(v["output"].as_str()).unwrap_or("");
            Some(text::hook_failed(v["hook_name"].as_str().unwrap_or(""), &clip(e)))
        }
        ("system", "stop_hook_summary") if v["hook_errors"].as_array().map(|a| !a.is_empty()).unwrap_or(false) => {
            let e: Vec<String> = v["hook_errors"].as_array().into_iter().flatten().map(|e| e.as_str().map(String::from).unwrap_or_else(|| e.to_string())).collect();
            Some(text::hook_failed("Stop", &clip(&e.join("；"))))
        }
        ("auth_status", _) => v["error"].as_str().filter(|e| !e.is_empty()).map(|e| text::auth_error(&clip(e))),
        ("system", "instruction_size_warning") => Some(text::instructions_too_long(v["total_chars"].as_u64().unwrap_or(0), v["total_limit_chars"].as_u64().unwrap_or(0))),
        ("system", "permission_denied") => Some(text::tool_denied(v["tool_name"].as_str().unwrap_or("?"), v["decision_reason_type"].as_str().unwrap_or(""))),
        ("system", "session_writes_lost") => Some(text::writes_lost(v["error_code"].as_str().unwrap_or(""))),
        ("system", "mirror_error") => Some(text::mirror_failed(&clip(v["error"].as_str().unwrap_or("")))),
        ("system", "model_refusal_no_fallback") => Some(v["content"].as_str().filter(|c| !c.trim().is_empty()).map(String::from).unwrap_or_else(|| text::REFUSED_NO_FALLBACK.into())),
        ("system", "init") => {
            let failed: Vec<String> = v["mcp_servers"].as_array().into_iter().flatten().filter(|s| s["status"] == "failed").filter_map(|s| s["name"].as_str().map(String::from)).collect();
            (!failed.is_empty()).then(|| text::mcp_failed(&failed.join("、"), ""))
        }
        ("system", "notification") => {
            let t = v["text"].as_str()?.trim();
            if t.is_empty() || v["priority"] == "low" { None } else { Some(t.to_string()) }
        }
        ("system", _) if matches!(v["level"].as_str(), Some("warning") | Some("error")) => {
            let c = v["content"].as_str().unwrap_or("").trim();
            if !c.is_empty() { return Some(c.to_string()); }
            let (from, to) = (v["original_model"].as_str(), v["fallback_model"].as_str());
            match (from, to) { (Some(f), Some(t)) => Some(text::fallback(f, t)), _ => None }
        }
        ("system", "model_fallback") => {
            let c = v["content"].as_str().unwrap_or("").trim();
            Some(if c.is_empty() { text::fallback(v["original_model"].as_str().unwrap_or("?"), v["fallback_model"].as_str().unwrap_or("?")) } else { c.to_string() })
        }
        _ => None,
    }
}

#[cfg(test)]
mod note_tests {
    use super::claude_note;
    use serde_json::json;
    #[test]
    fn notes() {
        assert_eq!(claude_note(&json!({"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}})), None);
        assert!(claude_note(&json!({"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour","resetsAt":4102444800i64}})).unwrap().starts_with("用量已达上限（five_hour），预计"));
        assert_eq!(claude_note(&json!({"type":"system","subtype":"model_fallback","original_model":"claude-opus-5-5","fallback_model":"claude-opus-4-8","content":""})).unwrap(), "模型已从 claude-opus-5-5 切换为 claude-opus-4-8");
        assert_eq!(claude_note(&json!({"type":"system","subtype":"model_refusal_fallback","level":"warning","content":"已切换模型"})).unwrap(), "已切换模型");
        assert_eq!(claude_note(&json!({"type":"system","subtype":"init"})), None);
        assert!(claude_note(&json!({"type":"system","subtype":"api_retry","attempt":1,"max_retries":10,"error":"overloaded"})).unwrap().contains("1/10"));
        assert_eq!(claude_note(&json!({"type":"system","subtype":"api_retry","attempt":2})), None);
        assert_eq!(claude_note(&json!({"type":"assistant"})), None);
    }
}

#[cfg(test)]
mod error_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn errors() {
        assert_eq!(codex_error(&json!({"message": "boom", "codexErrorInfo": "usageLimitExceeded"})), "boom （usageLimitExceeded）");
        assert_eq!(codex_error(&json!({"message": "x", "codexErrorInfo": {"httpConnectionFailed": {"httpStatusCode": 502}}})), "x （httpConnectionFailed）");
        let mut l = CodexLimits::default();
        assert_eq!(l.update(&json!({"primary": {"usedPercent": 50}})).len(), 0);
        assert_eq!(l.update(&json!({"primary": {"usedPercent": 95, "resetsAt": 1}})).len(), 1);
        assert_eq!(l.update(&json!({"primary": {"usedPercent": 96, "resetsAt": 1}})).len(), 0);
        assert_eq!(l.update(&json!({"rateLimitReachedType": "workspace", "primary": {"usedPercent": 100, "resetsAt": 1}})).len(), 1);
        assert!(claude_note(&json!({"type":"system","subtype":"hook_response","outcome":"error","hook_name":"PreToolUse","stderr":"bad"})).unwrap().contains("bad"));
        assert!(claude_note(&json!({"type":"system","subtype":"init","mcp_servers":[{"name":"a","status":"failed"},{"name":"b","status":"connected"}]})).unwrap().contains("a"));
        assert_eq!(claude_note(&json!({"type":"system","subtype":"init","mcp_servers":[{"name":"b","status":"connected"}]})), None);
        assert!(claude_note(&json!({"type":"auth_status","error":"expired"})).unwrap().contains("expired"));
    }
}
