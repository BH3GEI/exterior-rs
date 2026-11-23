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
pub enum Kind { Claude, Pi, Codex, Agy }

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        match s.trim().to_lowercase().as_str() { "claude" => Some(Kind::Claude), "pi" => Some(Kind::Pi), "codex" => Some(Kind::Codex), "agy" | "antigravity" => Some(Kind::Agy), _ => None }
    }
    pub fn name(&self) -> &'static str { match self { Kind::Claude => "claude", Kind::Pi => "pi", Kind::Codex => "codex", Kind::Agy => "agy" } }
    pub fn label(&self) -> &'static str { match self { Kind::Claude => "Claude", Kind::Pi => "Pi", Kind::Codex => "Codex", Kind::Agy => "Antigravity" } }
    pub const ALL: [Kind; 4] = [Kind::Claude, Kind::Pi, Kind::Codex, Kind::Agy];
}

/// The agents' own commands that their headless protocols take as requests rather than as typed text. Each keeps
/// the agent's own name (pi's /thinking is pi_thinking, codex's /model is cx_model); exterior only turns it into the
/// protocol call. The menu, the dispatch and the "which agent is this for" check are all built from this table.
pub const BUILTINS: &[(Kind, &str)] = &[
    (Kind::Pi, "model"), (Kind::Pi, "thinking"), (Kind::Pi, "compact"), (Kind::Pi, "new"), (Kind::Pi, "name"), (Kind::Pi, "session"),
    (Kind::Pi, "fork"), (Kind::Pi, "clone"), (Kind::Pi, "tree"), (Kind::Pi, "export"), (Kind::Pi, "copy"), (Kind::Pi, "resume"),
    (Kind::Pi, "import"), (Kind::Pi, "quit"),
    (Kind::Codex, "model"), (Kind::Codex, "compact"), (Kind::Codex, "new"), (Kind::Codex, "clear"), (Kind::Codex, "status"),
    (Kind::Codex, "usage"), (Kind::Codex, "review"), (Kind::Codex, "diff"), (Kind::Codex, "rename"), (Kind::Codex, "resume"),
    (Kind::Codex, "fork"), (Kind::Codex, "archive"), (Kind::Codex, "goal"), (Kind::Codex, "mcp"), (Kind::Codex, "apps"),
    (Kind::Codex, "plugins"), (Kind::Codex, "skills"), (Kind::Codex, "hooks"), (Kind::Codex, "copy"), (Kind::Codex, "logout"),
    (Kind::Codex, "quit"), (Kind::Codex, "exit"),
    (Kind::Agy, "model"), (Kind::Agy, "resume"),
];

/// Built-ins that end the agent's process (the session is kept, so the next message resumes it).
pub fn is_quit(name: &str) -> bool { matches!(name, "exit" | "quit") }

/// The table entry for one of pi's or codex's commands (claude's own commands go to claude as they are).
pub fn builtin(kind: Kind, name: &str) -> Option<&'static str> {
    BUILTINS.iter().find(|(k, n)| *k == kind && *n == name).map(|(_, n)| *n)
}

/// The line that tells a new agent where the previous session is, in the previous agent's own words.
pub fn resume_line(kind: Kind, name: &str, id: &str) -> String {
    match kind { Kind::Claude => format!("{name} --resume {id}"), Kind::Pi => format!("{name} --session {id}"), Kind::Codex => format!("{name} resume {id}"), Kind::Agy => format!("{name} --conversation {id}") }
}

pub enum Ev {
    Text { tid: String, gen: u64, delta: String },
    Done { tid: String, gen: u64, text: Option<String>, stopped: bool, error: Option<String> },
    Session { tid: String, gen: u64, id: String },
    /// side notice worth showing the user: rate limit, model fallback, API retry
    Note { tid: String, gen: u64, text: String },
    /// the command picked a concrete account (cc0 → cc6): later restarts use that one
    Resolved { tid: String, gen: u64, name: String },
    /// a step of the agent's work that is not its answer: thinking, a tool call with its output, a protocol event.
    /// Shown folded; nothing is left out.
    Step { tid: String, gen: u64, title: String, body: String },
    /// claude refused for lack of quota on this account (rate limit "rejected")
    Exhausted { tid: String, gen: u64, why: String },
    /// how long the conversation is now, in tokens (for compacting idle topics); 0 right after a compaction
    Context { tid: String, gen: u64, tokens: u64 },
    /// what a turn used, per model (see usage.rs); kept whatever the generation: the use happened anyway
    Usage { tid: String, uses: Vec<crate::usage::Use> },
    /// the account's quota windows as the agent last heard them
    Quota { tid: String, windows: Vec<crate::usage::Window> },
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
    /// claude's effort level picked with /effort; "/effort" only lasts for one process, so it is passed again at start
    pub effort: Option<String>,
}

/// claude's /effort levels, offered as buttons.
pub const CLAUDE_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max", "auto"];

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
    // EXTERIOR_TOPIC: who is speaking when the agent runs `exterior send`; PATH: where that command is
    vec![("EXTERIOR_SESSION", spec.topic.clone()), ("EXTERIOR_TOPIC", spec.tid.clone()), ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1".into()),
         ("PATH", crate::prompt::bin_dir(&spec.dev))]
}

/// stderr: remember the last lines (for error messages) and notice "→ cc6" style picks.
fn watch_stderr(err: std::process::ChildStderr, tx: Sender<Ev>, tid: String, gen: u64, last: Arc<Mutex<String>>) {
    // everything the process writes to stderr is shown too, gathered into one folded step every couple of seconds
    let pend = Arc::new(Mutex::new(String::new()));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let (pend, done, tx, tid) = (pend.clone(), done.clone(), tx.clone(), tid.clone());
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let t = std::mem::take(&mut *pend.lock().unwrap());
            if !t.trim().is_empty() { let _ = tx.send(Ev::Step { tid: tid.clone(), gen, title: text::STDERR.into(), body: t }); }
            if done.load(std::sync::atomic::Ordering::SeqCst) { break; }
        });
    }
    std::thread::spawn(move || {
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            { let mut p = pend.lock().unwrap(); p.push_str(&line); p.push('\n'); }
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
        done.store(true, std::sync::atomic::Ordering::SeqCst);
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
    tid: String,
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
            Kind::Agy => a.start_agy()?,
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
              thread: self.thread.clone(), model: self.model.clone(), dir: self.spec.dir.clone(), prompt: self.spec.prompt.clone(), tid: self.spec.tid.clone() }
    }


    fn start_claude(&mut self) -> Result<(), String> {
        let mut args: Vec<String> = ["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
                "--include-partial-messages"].map(String::from).to_vec();
        args.extend(params::args("claude_args"));
        if let Some(e) = self.spec.effort.as_ref().filter(|e| *e != "auto") { args.push("--effort".into()); args.push(e.clone()); }
        // claude leaves its thinking out unless asked; the summaries Anthropic's API gives are shown as steps
        args.push("--forward-subagent-text".into());
        let td = params::get("claude_thinking");
        if !td.is_empty() { args.push("--thinking-display".into()); args.push(td); }
        if let Some(id) = &self.spec.resume { args.push("--resume".into()); args.push(id.clone()); }
        if !self.spec.prompt.is_empty() { args.push("--append-system-prompt".into()); args.push(self.spec.prompt.clone()); }
        let (out, last) = self.spawn(args)?;
        let (tx, tid, gen, child, busy, pending, info) = (self.tx.clone(), self.spec.tid.clone(), self.spec.gen, self.child.clone(), self.busy.clone(), self.pending.clone(), self.info.clone());
        let interrupted = self.interrupted.clone();
        // effort auto (the default) has no command-line form: it is set with "/effort auto" right after start.
        // That exchange is shown like everything else (folded), but its end is not the end of the user's turn
        let auto = self.spec.effort.as_deref().map(|e| e == "auto").unwrap_or(true);
        let silent = Arc::new(std::sync::atomic::AtomicU64::new(auto as u64));
        let quiet = silent.clone();
        std::thread::spawn(move || {
            read_claude(out, tx.clone(), tid.clone(), gen, busy, pending, info, interrupted, quiet);
            if let Some(mut c) = child.lock().unwrap().take() { let _ = c.wait(); }
            let why = last.lock().unwrap().clone();
            let _ = tx.send(Ev::Exited { tid, gen, why });
        });
        // ask for the commands and models once, right away
        put(&self.stdin, &json!({"type": "control_request", "request_id": "init", "request": {"subtype": "initialize"}}))?;
        if silent.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            let _ = self.tx.send(Ev::Step { tid: self.spec.tid.clone(), gen: self.spec.gen, title: text::event_title(text::EFFORT_AUTO_SENT), body: "/effort auto".into() });
            put(&self.stdin, &json!({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": "/effort auto"}]}}))?;
        }
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
            read_pi(out, tx.clone(), tid.clone(), gen, busy, pending, stdin);
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
            // this turn's use so far, per model, and the thread's model when not picked with /model
            let (mut turn_uses, mut cx_model): (Vec<crate::usage::Use>, String) = (vec![], String::new());
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
                        if let Some(m) = p["thread"]["model"].as_str() { cx_model = m.to_string(); }
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
                        // the result of codex's /review
                        if let Some(r) = p["item"]["review"].as_str() { reply.push_str(r); let _ = tx.send(Ev::Text { tid: tid.clone(), gen, delta: r.to_string() }); }
                    }
                    "item/completed" if p["item"]["type"] == "agentMessage" => {
                        if !reply.is_empty() && !reply.ends_with("\n\n") { reply.push_str("\n\n"); let _ = tx.send(Ev::Text { tid: tid.clone(), gen, delta: "\n\n".into() }); }
                    }
                    "item/completed" if p["item"]["type"] != "userMessage" => {
                        let (title, body) = codex_step(&p["item"]);
                        let _ = tx.send(Ev::Step { tid: tid.clone(), gen, title, body });
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
                    "thread/tokenUsage/updated" => {
                        let m = model.lock().unwrap().clone().unwrap_or_else(|| cx_model.clone());
                        if let Some(u) = crate::usage::from_codex(&p["tokenUsage"]["last"], &m) { crate::usage::merge(&mut turn_uses, u); }
                        let _ = tx.send(Ev::Step { tid: tid.clone(), gen, title: text::event_title(method), body: pretty(p) });
                    }
                    "account/rateLimits/updated" => {
                        let w = crate::usage::codex_windows(&p["rateLimits"]);
                        if !w.is_empty() { let _ = tx.send(Ev::Quota { tid: tid.clone(), windows: w }); }
                        let _ = tx.send(Ev::Step { tid: tid.clone(), gen, title: text::event_title("rateLimits"), body: pretty(&p["rateLimits"]) });
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
                        if !turn_uses.is_empty() { let _ = tx.send(Ev::Usage { tid: tid.clone(), uses: std::mem::take(&mut turn_uses) }); }
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
                    // pieces (…Delta, item/started) that item/completed repeats in full
                    m if m.ends_with("Delta") || m.ends_with("/delta") || m == "item/started" => {}
                    m => { let _ = tx.send(Ev::Step { tid: tid.clone(), gen, title: text::event_title(m), body: pretty(&p) }); }
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

    /// agy (Antigravity CLI): one `agy acp` (Agent Client Protocol) per session, kept running; each message is a
    /// session/prompt on the same session. Like codex: the handshake runs in the background, messages wait in the queue.
    fn start_agy(&mut self) -> Result<(), String> {
        let mut args = vec!["acp".to_string()];
        args.extend(params::args("agy_args"));
        let (out, last) = self.spawn(args)?;
        let (tx, tid, gen, child, busy, pending) = (self.tx.clone(), self.spec.tid.clone(), self.spec.gen, self.child.clone(), self.busy.clone(), self.pending.clone());
        let (stdin, queue, thread, model, info, spec) = (self.stdin.clone(), self.queue.clone(), self.thread.clone(), self.model.clone(), self.info.clone(), self.spec.clone());
        std::thread::spawn(move || {
            read_agy(out, &spec, &tx, gen, &busy, &pending, &stdin, &queue, &thread, &model, &info);
            if let Some(mut c) = child.lock().unwrap().take() { let _ = c.wait(); }
            let why = last.lock().unwrap().clone();
            let _ = tx.send(Ev::Exited { tid, gen, why });
        });
        let (c, tx, spec, thread, queue, busy, model, stdin, info) = (self.ctl(), self.tx.clone(), self.spec.clone(), self.thread.clone(), self.queue.clone(), self.busy.clone(), self.model.clone(), self.stdin.clone(), self.info.clone());
        std::thread::spawn(move || {
            let r = (|| -> Result<String, String> {
                c.ask(|id| rpc(id, "initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})))?;
                let mut r = Value::Null;
                if let Some(id) = &spec.resume {
                    r = c.ask(|i| rpc(i, "session/resume", agy_session(&spec.dir, &spec.prompt, Some(id))))?;
                    if let Some(e) = r["error"]["message"].as_str() { let _ = tx.send(Ev::Note { tid: spec.tid.clone(), gen: spec.gen, text: text::agy_resume_failed(id, e) }); }
                }
                if r.is_null() || r["error"].is_object() { r = c.ask(|i| rpc(i, "session/new", agy_session(&spec.dir, &spec.prompt, None)))?; }
                if let Some(e) = r["error"]["message"].as_str() { return Err(e.to_string()); }
                let res = &r["result"];
                let sid = res["sessionId"].as_str().map(String::from).or(spec.resume.clone()).ok_or(text::AGY_NO_SESSION)?;
                agy_keep(&info, &model, res);
                // the model picked with /model in this topic outlives the process
                let want = crate::store::note(&spec.tid)["model"].as_str().map(String::from);
                if let Some(m) = want.filter(|m| model.lock().unwrap().as_deref() != Some(m.as_str())) {
                    let r = c.ask(|i| rpc(i, "session/set_model", json!({"sessionId": sid, "modelId": m})))?;
                    if r["error"].is_null() { *model.lock().unwrap() = Some(m); }
                }
                Ok(sid)
            })();
            match r {
                Ok(t) => {
                    let mut q = queue.lock().unwrap();
                    *thread.lock().unwrap() = Some(t.clone());
                    let _ = tx.send(Ev::Session { tid: spec.tid.clone(), gen: spec.gen, id: t.clone() });
                    let next = if q.is_empty() { None } else { Some(q.remove(0)) };
                    drop(q);
                    match next { Some(m) => { let _ = put(&stdin, &agy_turn(&t, &m)); } None => *busy.lock().unwrap() = false }
                }
                Err(e) => {
                    queue.lock().unwrap().clear();
                    *busy.lock().unwrap() = false;
                    let _ = tx.send(Ev::Done { tid: spec.tid.clone(), gen: spec.gen, text: None, stopped: false, error: Some(text::agy_start_failed(&e)) });
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
            Kind::Agy => {
                // one prompt at a time, and only once the session is there: otherwise it waits in the queue
                let th = {
                    let mut q = self.queue.lock().unwrap();
                    let th = self.thread.lock().unwrap().clone();
                    match th.filter(|_| !was_busy) { Some(th) => th, None => { q.push(text.to_string()); return Ok(()); } }
                };
                put(&self.stdin, &agy_turn(&th, text))
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
            Kind::Agy => {
                self.queue.lock().unwrap().clear();
                let th = self.thread.lock().unwrap().clone();
                if let Some(th) = th { let _ = put(&self.stdin, &json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": th}})); }
            }
        }
    }

    pub fn kill(&mut self) {
        *self.stdin.lock().unwrap() = None;
        if let Some(c) = self.child.lock().unwrap().as_mut() { crate::host::kill(c); }
    }

}

impl Ctl {
    /// codex, agy: wait until the background handshake is done.
    fn ready(&self) -> Result<(), String> {
        if !matches!(self.kind, Kind::Codex | Kind::Agy) { return Ok(()); }
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
            Kind::Agy => {
                let info = self.info.lock().unwrap().clone();
                Ok(info["models"]["availableModels"].as_array().into_iter().flatten().filter_map(|m| {
                    let v = m["modelId"].as_str()?;
                    Some(Choice { value: v.into(), label: m["name"].as_str().unwrap_or(v).into(), provider: None, id: v.into() })
                }).collect())
            }
        }
    }

    /// Switch model. Returns what the agent answered (pi: the model it now uses). codex has no such request: the
    /// model is passed with each turn, so nothing comes back (Null).
    pub fn set_model(&self, value: &str) -> Result<Value, String> {
        let r = match self.kind {
            Kind::Claude => return Err(text::NOT_SUPPORTED.into()),
            Kind::Pi => {
                // pi wants the provider and the id separately: find the model in pi's own list
                let list = self.models()?;
                let c = list.iter().find(|c| c.value == value).or_else(|| list.iter().find(|c| c.id == value || c.label == value))
                    .ok_or_else(|| text::model_unknown(self.kind.label(), value))?;
                let (p, m) = (c.provider.clone().unwrap_or_default(), c.id.clone());
                self.ask(|id| json!({"type": "set_model", "id": id, "provider": p, "modelId": m}))?
            }
            Kind::Codex => { *self.model.lock().unwrap() = Some(value.to_string()); return Ok(Value::Null); }
            Kind::Agy => {
                let th = self.thread.lock().unwrap().clone().ok_or(text::AGY_NOT_READY)?;
                let r = self.ask(|id| rpc(id, "session/set_model", json!({"sessionId": th, "modelId": value})))?;
                if let Some(e) = r["error"]["message"].as_str() { return Err(e.to_string()); }
                *self.model.lock().unwrap() = Some(value.to_string());
                crate::store::note_set(&self.tid, "model", json!(value));
                return Ok(json!(text::agy_model_set(value)));
            }
        };
        if r["success"] == false { return Err(r["error"].as_str().unwrap_or(text::SET_MODEL_FAILED).to_string()); }
        Ok(r["data"].clone())
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
        let codex = |method: &str, p: Value| self.ask(|id| json!({"jsonrpc": "2.0", "id": id, "method": method, "params": p}));
        let r = match (self.kind, name) {
            (Kind::Pi, "clone") => self.ask(|id| json!({"type": "clone", "id": id}))?,
            (Kind::Pi, "tree") => self.ask(|id| json!({"type": "get_tree", "id": id}))?,
            (Kind::Pi, "copy") => self.ask(|id| json!({"type": "get_last_assistant_text", "id": id}))?,
            (Kind::Pi, "resume") if arg.is_empty() => {
                // pi has no "list sessions" request: list the session files next to the current one, through pi's own shell
                let st = self.ask(|id| json!({"type": "get_state", "id": id}))?;
                let dir = st["data"]["sessionFile"].as_str().and_then(|f| f.rsplit_once(['/', '\\'])).map(|x| x.0.to_string()).ok_or(text::NO_SESSIONS)?;
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
            (Kind::Agy, "resume") if arg.is_empty() => {
                let r = self.ask(|id| rpc(id, "session/list", json!({"cwd": self.dir})))?;
                // in the shape the resume buttons read (codex's thread/list)
                let items: Vec<Value> = r["result"]["sessions"].as_array().into_iter().flatten().take(10)
                    .map(|t| json!({"id": t["sessionId"], "name": t["title"]})).collect();
                json!({"result": {"data": items}})
            }
            (Kind::Agy, "resume") => {
                let r = self.ask(|id| rpc(id, "session/resume", agy_session(&self.dir, &self.prompt, Some(arg))))?;
                if r["error"].is_null() {
                    *self.thread.lock().unwrap() = Some(arg.to_string());
                    agy_keep(&self.info, &self.model, &r["result"]);
                    if let Some(m) = crate::store::note(&self.tid)["model"].as_str() {
                        let m = m.to_string();
                        if self.ask(|id| rpc(id, "session/set_model", json!({"sessionId": arg, "modelId": m})))?["error"].is_null() { *self.model.lock().unwrap() = Some(m); }
                    }
                    json!({"result": {"_session": arg, "resumed": arg}})
                } else { r }
            }
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
        let mut data = match self.kind { Kind::Codex => r["result"].clone(), Kind::Claude => r["response"].clone(), Kind::Pi => r["data"].clone(), Kind::Agy => r["result"].clone() };
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

/// codex's /review argument: nothing for the uncommitted changes, a commit hash, or a base branch.
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

/// claude's output (stream-json), one JSON line after another, as events.
#[allow(clippy::too_many_arguments)]
fn read_claude(out: impl std::io::Read, tx: Sender<Ev>, tid: String, gen: u64, busy: Arc<Mutex<bool>>, pending: Pending, info: Arc<Mutex<Value>>,
               interrupted: Arc<std::sync::atomic::AtomicBool>, quiet: Arc<std::sync::atomic::AtomicU64>) {
    let mut known = String::new();
    let mut tools: HashMap<String, (String, Value)> = HashMap::new();
    // text that arrived piece by piece since the last whole message; a command's own output (/goal…) comes
    // only as a whole message, and must not be lost
    let mut streamed = false;
    // after the user's /compact (not one claude does by itself mid-turn) and until that turn's result, claude says
    // again what its last local command said: shown folded, it is not an answer
    let mut compacting = false;
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
        // everything the agent does besides answering: thinking, tool calls with their output, system events
        let step = |title: String, body: String| { let _ = tx.send(Ev::Step { tid: tid.clone(), gen, title, body }); };
        // the context the model saw for this answer: what it was given plus what it wrote
        if v["type"] == "assistant" && v["parent_tool_use_id"].is_null() {
            let u = &v["message"]["usage"];
            let n: u64 = ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens", "output_tokens"].iter().filter_map(|k| u[*k].as_u64()).sum();
            if n > 0 { let _ = tx.send(Ev::Context { tid: tid.clone(), gen, tokens: n }); }
        }
        match v["type"].as_str() {
            Some("assistant") => for b in v["message"]["content"].as_array().into_iter().flatten() {
                match b["type"].as_str() {
                    Some("thinking") => if let Some(t) = b["thinking"].as_str().filter(|t| !t.trim().is_empty()) { step(text::THINKING.into(), t.trim().to_string()); },
                    Some("redacted_thinking") => step(text::THINKING.into(), text::THINKING_HIDDEN.into()),
                    // a subagent's words (with --forward-subagent-text) are part of its work, not the answer
                    Some("text") if !v["parent_tool_use_id"].is_null() => step(text::SUBAGENT.into(), b["text"].as_str().unwrap_or("").to_string()),
                Some("text") if compacting => step(text::event_title(text::COMPACT_REPEAT), b["text"].as_str().unwrap_or("").to_string()),
                    // claude's answer to exterior's own "/effort auto": not part of the user's answer
                    Some("text") if quiet.load(std::sync::atomic::Ordering::SeqCst) > 0 => step(text::event_title(text::EFFORT_AUTO_REPLY), b["text"].as_str().unwrap_or("").to_string()),
                    Some("text") if !streamed => if let Some(t) = b["text"].as_str().filter(|t| !t.trim().is_empty()) {
                        let _ = tx.send(Ev::Text { tid: tid.clone(), gen, delta: format!("\n\n{t}") });
                    },
                    Some("tool_use") => { tools.insert(b["id"].as_str().unwrap_or("").to_string(), (b["name"].as_str().unwrap_or("?").to_string(), b["input"].clone())); }
                    Some("text") => {}
                    _ => step(text::event_title(&format!("assistant · {}", b["type"].as_str().unwrap_or("?"))), pretty(b)),
                }
            },
            Some("user") => match &v["message"]["content"] {
                Value::Array(a) => for b in a {
                    if b["type"] == "tool_result" {
                        let (name, input) = tools.remove(b["tool_use_id"].as_str().unwrap_or("")).unwrap_or(("?".into(), Value::Null));
                        let failed = b["is_error"] == true;
                        step(tool_title(&name, &input, failed), tool_body(&input, Some(&result_text(&b["content"]))));
                    } else {
                        // what claude put in front of the model itself: command output, hook text, reminders
                        step(text::event_title(&format!("user · {}", b["type"].as_str().unwrap_or("?"))), result_text(&Value::Array(vec![b.clone()])));
                    }
                },
                c => step(text::event_title("user"), pretty(c)),
            },
            // thinking_tokens is a running counter of the thinking that arrives in full anyway
            Some("system") if v["subtype"] != "thinking_tokens" => {
                if v["subtype"] == "compact_boundary" {
                    let _ = tx.send(Ev::Context { tid: tid.clone(), gen, tokens: 0 });
                    let m = if v["compact_metadata"].is_object() { &v["compact_metadata"] } else { &v["compactMetadata"] };
                    compacting = m["trigger"] != "auto";
                }
                step(text::event_title(&format!("system · {}", v["subtype"].as_str().unwrap_or(""))), pretty(&v));
            }
            Some("rate_limit_event") => {
                let w = crate::usage::claude_windows(&v["rate_limit_info"]);
                if !w.is_empty() { let _ = tx.send(Ev::Quota { tid: tid.clone(), windows: w }); }
                step(text::event_title("rate_limit_event"), pretty(&v))
            }
            Some("result") => {
                let uses = crate::usage::from_claude_result(&v);
                if !uses.is_empty() { let _ = tx.send(Ev::Usage { tid: tid.clone(), uses }); }
                // calls still without a result (a background subagent's, say): shown now with what they were
                // asked, and kept, so the result that comes later is shown under the same call
                for (name, input) in tools.values() { step(text::tool_waiting(&tool_title(name, input, false)), tool_body(input, None)); }
                step(text::event_title("result"), pretty(&v));
            }
            // stream_event pieces come again whole in the assistant message; control_response answers exterior
            Some("stream_event") | Some("control_response") | Some("system") => {}
            Some(t) => step(text::event_title(t), pretty(&v)),
            None => step(text::STDOUT.into(), line.clone()),
        }
        if let Some(t) = claude_note(&v) {
            if v["type"] == "rate_limit_event" && v["rate_limit_info"]["status"] == "rejected" {
                let _ = tx.send(Ev::Exhausted { tid: tid.clone(), gen, why: t.clone() });
            }
            let _ = tx.send(Ev::Note { tid: tid.clone(), gen, text: t });
        }
        // a new text block (after a tool call, say) is a new paragraph
        if !compacting && v["event"]["type"] == "content_block_start" && v["event"]["content_block"]["type"] == "text" { let _ = tx.send(Ev::Text { tid: tid.clone(), gen, delta: "\n\n".into() }); }
        if let Some(d) = v["event"]["delta"]["text"].as_str().filter(|_| !compacting) { streamed = true; let _ = tx.send(Ev::Text { tid: tid.clone(), gen, delta: d.to_string() }); }
        if v["type"] == "assistant" { streamed = false; }
        let repeat = compacting;
        if v["type"] == "result" { compacting = false; }
        if v["type"] == "result" && quiet.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            quiet.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            continue;
        }
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
            let _ = tx.send(Ev::Done { tid: tid.clone(), gen, text: if failed || repeat { None } else { v["result"].as_str().map(String::from) }, stopped, error });
        }
    }
}

/// agy reports no quota while it works: after a turn, ask `agy -p /usage` in the background, at most every agy_quota_min
/// minutes per command (one command is one Google account).
fn agy_quota(spec: &Spec, tx: &Sender<Ev>) {
    static LAST: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);
    let every = params::num("agy_quota_min") * 60;
    if every == 0 || spec.command.is_empty() { return; }
    let key = format!("{}|{}", spec.dev["name"].as_str().unwrap_or(""), spec.command);
    {
        let mut l = LAST.lock().unwrap();
        let m = l.get_or_insert_with(HashMap::new);
        let now = crate::store::now();
        if m.get(&key).is_some_and(|t| now < t + every) { return; }
        m.insert(key, now);
    }
    let (spec, tx) = (spec.clone(), tx.clone());
    std::thread::spawn(move || {
        let c = crate::host::agent(&spec.dev, &spec.dir, &env(&spec), &spec.command, &["-p".into(), "/usage".into()]);
        if let Ok(out) = crate::host::exec(c, params::num("test_timeout_s")) {
            let w = crate::usage::agy_windows(&out);
            if !w.is_empty() { let _ = tx.send(Ev::Quota { tid: spec.tid.clone(), windows: w }); }
        }
    });
}

/// A JSON-RPC request.
fn rpc(id: &str, method: &str, params: Value) -> Value { json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}) }

/// The parameters of agy's session/new and session/resume: where it works, and exterior's instructions appended to
/// its system prompt (agy's own extension, advertised as _meta.antigravity.systemPrompt.append).
fn agy_session(dir: &str, prompt: &str, id: Option<&str>) -> Value {
    let mut p = json!({"cwd": dir, "mcpServers": []});
    if !prompt.is_empty() { p["_meta"] = json!({"antigravity": {"systemPrompt": {"append": prompt}}}); }
    if let Some(id) = id { p["sessionId"] = json!(id); }
    p
}

/// One message as an ACP prompt. Its answer (the turn's end) comes back under the id "turn".
fn agy_turn(session: &str, text: &str) -> Value { rpc("turn", "session/prompt", json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]})) }

/// Keep what session/new or session/resume said: the models offered and the one in use.
fn agy_keep(info: &Arc<Mutex<Value>>, model: &Arc<Mutex<Option<String>>>, res: &Value) {
    let mut i = info.lock().unwrap();
    if !i.is_object() { *i = json!({}); }
    if res["models"].is_object() { i["models"] = res["models"].clone(); }
    if let Some(m) = res["models"]["currentModelId"].as_str() { *model.lock().unwrap() = Some(m.to_string()); }
}

/// agy's output (ACP: answers to our requests, its own requests, session/update notifications) as events.
#[allow(clippy::too_many_arguments)]
fn read_agy(out: impl std::io::Read, spec: &Spec, tx: &Sender<Ev>, gen: u64, busy: &Arc<Mutex<bool>>, pending: &Pending, stdin: &Arc<Mutex<Option<ChildStdin>>>,
            queue: &Arc<Mutex<Vec<String>>>, thread: &Arc<Mutex<Option<String>>>, model: &Arc<Mutex<Option<String>>>, info: &Arc<Mutex<Value>>) {
    let tid = spec.tid.clone();
    let (mut reply, mut thought) = (String::new(), String::new());
    // a tool call or a new message since the last words: the next words start a new paragraph
    let mut gap = false;
    let mut last_msg: Option<String> = None;
    let mut tools: HashMap<String, Value> = HashMap::new();
    let step = |title: String, body: String| { let _ = tx.send(Ev::Step { tid: tid.clone(), gen, title, body }); };
    let flush = |thought: &mut String| { let t = std::mem::take(thought); if !t.trim().is_empty() { step(text::THINKING.into(), t.trim().to_string()); } };
    for line in BufReader::new(out).lines().map_while(Result::ok) {
        let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        if v.is_null() { if !line.trim().is_empty() { step(text::STDOUT.into(), line.clone()); } continue; }
        let method = v["method"].as_str().unwrap_or("");
        if method.is_empty() && v["id"] == "turn" {
            // the turn is over
            flush(&mut thought);
            let r = &v["result"];
            let m = model.lock().unwrap().clone().unwrap_or_default();
            if let Some(u) = crate::usage::from_agy_acp(&r["usage"], &m) { let _ = tx.send(Ev::Usage { tid: tid.clone(), uses: vec![u] }); }
            agy_quota(spec, tx);
            let why = r["stopReason"].as_str().unwrap_or("");
            let error = v["error"]["message"].as_str().map(String::from);
            if !matches!(why, "" | "end_turn" | "cancelled") { let _ = tx.send(Ev::Note { tid: tid.clone(), gen, text: text::agy_stopped_by(why) }); }
            step(text::event_title("session/prompt"), pretty(if r.is_null() { &v } else { r }));
            let t = reply.trim().to_string();
            let _ = tx.send(Ev::Done { tid: tid.clone(), gen, text: (!t.is_empty()).then_some(t), stopped: why == "cancelled", error });
            reply.clear(); gap = false; last_msg = None;
            let next = { let mut q = queue.lock().unwrap(); if q.is_empty() { None } else { Some(q.remove(0)) } };
            match (next, thread.lock().unwrap().clone()) {
                (Some(t), Some(th)) => { let _ = put(stdin, &agy_turn(&th, &t)); }
                _ => { *busy.lock().unwrap() = false; }
            }
            continue;
        }
        if method.is_empty() { route(pending, v["id"].as_str().map(String::from).or(v["id"].as_u64().map(|n| n.to_string())), &v); continue; }
        if !v["id"].is_null() {
            // agy asks: permissions are said yes to (as with codex; agy_args normally skips them anyway), the rest declined
            let res = if method == "session/request_permission" {
                let opts = v["params"]["options"].as_array().cloned().unwrap_or_default();
                let pick = opts.iter().find(|o| o["kind"] == "allow_always").or(opts.iter().find(|o| o["kind"] == "allow_once")).or(opts.first());
                match pick { Some(o) => json!({"jsonrpc": "2.0", "id": v["id"], "result": {"outcome": {"outcome": "selected", "optionId": o["optionId"]}}}),
                             None => json!({"jsonrpc": "2.0", "id": v["id"], "result": {"outcome": {"outcome": "cancelled"}}}) }
            } else {
                let _ = tx.send(Ev::Note { tid: tid.clone(), gen, text: text::codex_request_declined(method) });
                json!({"jsonrpc": "2.0", "id": v["id"], "error": {"code": -32601, "message": text::CODEX_UNHANDLED}})
            };
            let _ = put(stdin, &res);
            continue;
        }
        if method != "session/update" { step(text::event_title(method), pretty(&v["params"])); continue; }
        let u = &v["params"]["update"];
        let kind = u["sessionUpdate"].as_str().unwrap_or("");
        if kind != "agent_thought_chunk" { flush(&mut thought); }
        match kind {
            "agent_message_chunk" => {
                let d = u["content"]["text"].as_str().unwrap_or("");
                let mid = u["messageId"].as_str().map(String::from);
                if mid.is_some() && mid != last_msg { if last_msg.is_some() { gap = true; } last_msg = mid; }
                let d = if gap && !reply.is_empty() && !reply.ends_with("\n\n") { format!("\n\n{d}") } else { d.to_string() };
                gap = false;
                reply.push_str(&d);
                let _ = tx.send(Ev::Text { tid: tid.clone(), gen, delta: d });
            }
            "agent_thought_chunk" => thought.push_str(u["content"]["text"].as_str().unwrap_or("")),
            "tool_call" | "tool_call_update" => {
                gap = true;
                let id = u["toolCallId"].as_str().unwrap_or("").to_string();
                let t = tools.entry(id.clone()).or_insert_with(|| json!({}));
                if let Some(o) = u.as_object() { for (k, x) in o { if !x.is_null() { t[k] = x.clone(); } } }
                let st = t["status"].as_str().unwrap_or("").to_string();
                if st == "completed" || st == "failed" {
                    let t = tools.remove(&id).unwrap_or_default();
                    let name = t["name"].as_str().or(t["title"].as_str()).unwrap_or("?").to_string();
                    let input = if t["rawInput"].is_null() { t["args"].clone() } else { t["rawInput"].clone() };
                    let out = t["rawOutput"]["output"].as_str().map(String::from).unwrap_or_else(|| {
                        let parts: Vec<&str> = t["content"].as_array().into_iter().flatten().filter_map(|c| c["content"]["text"].as_str()).collect();
                        if parts.is_empty() && !t["rawOutput"].is_null() { pretty(&t["rawOutput"]) } else { parts.join("\n") }
                    });
                    step(tool_title(&name, &input, st == "failed"), tool_body(&input, Some(out.trim())));
                }
            }
            "usage_update" => if let Some(n) = u["used"].as_u64() { let _ = tx.send(Ev::Context { tid: tid.clone(), gen, tokens: n }); },
            "available_commands_update" => {
                let mut i = info.lock().unwrap();
                if !i.is_object() { *i = json!({}); }
                i["commands"] = u["availableCommands"].clone();
            }
            // replayed history, the title agy gave the session, settings it repeats back: nothing new
            "user_message_chunk" | "session_info_update" | "current_mode_update" | "config_option_update" => {}
            k => step(text::event_title(&format!("update · {k}")), pretty(u)),
        }
    }
}

/// pi's output (rpc), one JSON line after another, as events.
fn read_pi(out: impl std::io::Read, tx: Sender<Ev>, tid: String, gen: u64, busy: Arc<Mutex<bool>>, pending: Pending, stdin: Arc<Mutex<Option<ChildStdin>>>) {
    let mut last_text: Option<String> = None;
    let mut last_err: Option<String> = None;
    // what this run used so far, per model; reported when the run ends
    let mut uses: Vec<crate::usage::Use> = vec![];
    let used = |uses: &mut Vec<crate::usage::Use>| if !uses.is_empty() { let _ = tx.send(Ev::Usage { tid: tid.clone(), uses: std::mem::take(uses) }); };
    for line in BufReader::new(out).lines().map_while(Result::ok) {
        let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        if v["type"] == "response" && route(&pending, v["id"].as_str().map(String::from), &v) { continue; }
        match v["type"].as_str() {
            Some("message_update") => {
                let e = &v["assistantMessageEvent"];
                if e["type"] == "text_delta" { let _ = tx.send(Ev::Text { tid: tid.clone(), gen, delta: e["delta"].as_str().unwrap_or("").to_string() }); }
                if e["type"] == "thinking_end" {
                    if let Some(t) = e["content"].as_str().filter(|t| !t.trim().is_empty()) { let _ = tx.send(Ev::Step { tid: tid.clone(), gen, title: text::THINKING.into(), body: t.trim().to_string() }); }
                }
            }
            Some("tool_execution_end") => {
                let failed = v["isError"] == true;
                let out = v["result"]["content"].as_array().map(|a| a.iter().map(|c| c["text"].as_str().map(String::from).unwrap_or_else(|| pretty(c))).collect::<Vec<_>>().join("\n")).unwrap_or_else(|| pretty(&v["result"]));
                let name = v["toolName"].as_str().unwrap_or("?");
                let _ = tx.send(Ev::Step { tid: tid.clone(), gen, title: tool_title(name, &v["args"], failed), body: tool_body(&v["args"], Some(&out)) });
            }
            Some("turn_end") => {
                if let Some(u) = crate::usage::from_pi(&v["message"]) { crate::usage::merge(&mut uses, u); }
                let u = &v["message"]["usage"];
                let n = u["totalTokens"].as_u64().unwrap_or_else(|| ["input", "output", "cacheRead", "cacheWrite"].iter().filter_map(|k| u[*k].as_u64()).sum());
                if n > 0 { let _ = tx.send(Ev::Context { tid: tid.clone(), gen, tokens: n }); }
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
                if v["errorMessage"].is_null() { let _ = tx.send(Ev::Context { tid: tid.clone(), gen, tokens: 0 }); }
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
                    m => { let _ = tx.send(Ev::Step { tid: tid.clone(), gen, title: text::event_title(&format!("extension_ui · {m}")), body: pretty(&v) }); }
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
                used(&mut uses);
                *busy.lock().unwrap() = false;
                let _ = tx.send(Ev::Done { tid: tid.clone(), gen, text: last_text.take(), stopped: v["aborted"] == true, error: last_err.take() });
            }
            Some("response") if v["success"] == false => {
                used(&mut uses);
                *busy.lock().unwrap() = false;
                let err = v["error"].as_str().unwrap_or(text::PI_REJECTED).to_string();
                let _ = tx.send(Ev::Done { tid: tid.clone(), gen, text: None, stopped: false, error: Some(err) });
            }
            // these repeat what tool_execution_end and the streamed text already carry in full
            Some("tool_execution_start") | Some("tool_execution_update") | Some("message_start") | Some("message_end") => {}
            // it carries the whole conversation, which went out piece by piece already
            Some("agent_end") => { let _ = tx.send(Ev::Step { tid: tid.clone(), gen, title: text::event_title("agent_end"), body: agent_end_summary(&v) }); }
            Some(t) => { let _ = tx.send(Ev::Step { tid: tid.clone(), gen, title: text::event_title(t), body: pretty(&v) }); }
            None => { let _ = tx.send(Ev::Step { tid: tid.clone(), gen, title: text::STDOUT.into(), body: line.clone() }); }
        }
    }
}


/// One question, one answer: used by the 测试 button on the 设备 page.
pub fn test(dev: &Value, dir: &str, command: &str, kind: Kind) -> Result<String, String> {
    let q = crate::prompt::read(dev, "test_prompt")?;
    let q = q.as_str();
    let args: Vec<String> = match kind {
        Kind::Claude => [vec!["-p".into(), q.into()], params::args("claude_args")].concat(),
        Kind::Pi => vec!["-p".into(), "--no-session".into(), q.into()],
        Kind::Codex => vec!["exec".into(), "--skip-git-repo-check".into(), q.into()],
        Kind::Agy => [vec!["-p".into(), q.into()], params::args("agy_args")].concat(),
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
        // agy lists its commands once a session is open (available_commands_update); nothing is sent to the model
        Kind::Agy => ([vec!["acp".to_string()], params::args("agy_args")].concat(),
                      json!([rpc("1", "initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})), rpc("2", "session/new", agy_session(dir, "", None))])),
        Kind::Codex => return vec![],
    };
    let Ok(mut child) = crate::host::agent(dev, dir, &env(&Spec { tid: String::new(), prompt: String::new(), topic: String::new(), dev: dev.clone(), dir: dir.into(), command: command.into(), kind, gen: 0, resume: None, effort: None }), command, &args)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() else { return vec![] };
    let mut stdin = child.stdin.take().unwrap();
    match ask.as_array() { Some(l) => for a in l { let _ = writeln!(stdin, "{a}"); }, None => { let _ = writeln!(stdin, "{ask}"); } }
    let _ = stdin.flush();
    // never wait forever: if the agent does not answer in time, close it
    let stdout = child.stdout.take().unwrap();
    let child = Arc::new(Mutex::new(child));
    { let c = child.clone(); std::thread::spawn(move || { std::thread::sleep(params::secs("commands_timeout_s")); crate::host::kill(&mut c.lock().unwrap()); }); }
    let mut out = vec![];
    let lines = BufReader::new(stdout).lines();
    for line in lines.map_while(Result::ok).take(500) {
        let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        let list = if v["type"] == "control_response" { v["response"]["response"]["commands"].clone() }
                   else if v["command"] == "get_commands" { if v["data"]["commands"].is_array() { v["data"]["commands"].clone() } else { v["data"].clone() } }
                   else if v["params"]["update"]["sessionUpdate"] == "available_commands_update" { v["params"]["update"]["availableCommands"].clone() }
                   else { continue };
        for c in list.as_array().into_iter().flatten() {
            if let Some(n) = c["name"].as_str() { out.push((n.to_string(), c["description"].as_str().unwrap_or("").to_string())); }
        }
        break;
    }
    drop(stdin);
    crate::host::kill(&mut child.lock().unwrap());
    out
}

/// The commands agy's command line answers itself (/usage, /help…): `agy -p /help` lists them, name (alias) and
/// description separated by a tab. Over ACP they are not commands; exterior runs each on its own (cli below).
pub fn agy_cli_commands(dev: &Value, dir: &str, command: &str) -> Vec<(String, String)> {
    let c = crate::host::agent(dev, dir, &[], command, &["-p".into(), "/help".into()]);
    crate::host::exec(c, params::num("test_timeout_s")).map(|o| parse_agy_help(&o)).unwrap_or_default()
}

fn parse_agy_help(out: &str) -> Vec<(String, String)> {
    out.lines().filter_map(|l| {
        let (name, desc) = l.split_once('\t')?;
        let name = name.trim().strip_prefix('/')?.split_whitespace().next()?;
        Some((name.to_string(), desc.trim().to_string()))
    }).collect()
}

/// One of agy's command-line commands, run on its own (`agy -p "/usage"`): what it printed, or why it failed.
pub fn agy_cli(dev: &Value, dir: &str, command: &str, name: &str, arg: &str) -> Result<String, String> {
    let line = if arg.is_empty() { format!("/{name}") } else { format!("/{name} {arg}") };
    let c = crate::host::agent(dev, dir, &[], command, &["-p".into(), line]);
    crate::host::exec(c, params::num("test_timeout_s"))
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

/// Whether a retry is worth a message: every one is (nothing is held back).
fn worth_noting(_attempt: i64) -> bool { true }

/// The main argument of a tool call, for its one-line title (the full call is in the folded body).
fn short(s: &str) -> String {
    let l = s.trim().lines().next().unwrap_or("").trim();
    if l.chars().count() > 80 { format!("{}…", l.chars().take(80).collect::<String>()) } else { l.to_string() }
}

pub fn pretty(v: &Value) -> String {
    match v { Value::String(s) => s.clone(), Value::Null => String::new(), _ => serde_json::to_string_pretty(v).unwrap_or_default() }
}

/// "🔧 Bash · ls -la": icon, tool, its main argument.
pub fn tool_title(name: &str, input: &Value, failed: bool) -> String {
    let n = name.to_lowercase();
    let icon = if n.contains("bash") || n.contains("command") || n.contains("exec") { "🔧" }
        else if n.contains("read") || n == "view" { "📄" }
        else if n.contains("edit") || n.contains("write") || n.contains("patch") || n.contains("filechange") { "✏️" }
        else if n.contains("grep") || n.contains("glob") || n.contains("find") || n == "ls" { "🔎" }
        else if n.contains("web") || n.contains("fetch") || n.contains("search") { "🌐" }
        else if n.contains("task") || n.contains("agent") { "🤖" } else { "🔧" };
    let arg = ["command", "cmd", "file_path", "path", "pattern", "url", "query", "description", "prompt", "skill"].iter()
        .find_map(|k| input[*k].as_str().map(short)).unwrap_or_default();
    let mark = if failed { " ✗" } else { "" };
    if arg.is_empty() { format!("{icon} {name}{mark}") } else { format!("{icon} {name} · {arg}{mark}") }
}

/// A tool call and what came back, as one text.
pub fn tool_body(input: &Value, output: Option<&str>) -> String {
    let call = match input["command"].as_str() {
        Some(c) => match input["description"].as_str() { Some(d) => format!("{c}\n# {d}"), None => c.to_string() },
        None => pretty(input),
    };
    match output { Some(o) => format!("{call}\n\n── 输出 ──\n{o}"), None => call }
}

/// A finished codex item other than its answer: (title, full content).
fn codex_step(it: &Value) -> (String, String) {
    let kind = it["type"].as_str().unwrap_or("?");
    match kind {
        "commandExecution" => {
            let cmd = it["command"].as_str().unwrap_or("");
            let failed = it["exitCode"].as_i64().map(|c| c != 0).unwrap_or(false);
            let out = format!("{}\n(exit {})", it["aggregatedOutput"].as_str().unwrap_or(""), it["exitCode"]);
            (tool_title("command", &serde_json::json!({"command": cmd}), failed), tool_body(&serde_json::json!({"command": cmd}), Some(out.trim())))
        }
        "reasoning" => {
            let parts: Vec<String> = ["summary", "content"].iter().flat_map(|k| it[*k].as_array().cloned().unwrap_or_default())
                .map(|x| x.as_str().map(String::from).unwrap_or_else(|| pretty(&x))).collect();
            (text::THINKING.into(), parts.join("\n\n"))
        }
        "fileChange" => {
            let ch = it["changes"].as_array().cloned().unwrap_or_default();
            let body = ch.iter().map(|c| format!("{} ({})\n{}", c["path"].as_str().unwrap_or("?"), c["kind"]["type"].as_str().or(c["kind"].as_str()).unwrap_or(""), c["diff"].as_str().unwrap_or(""))).collect::<Vec<_>>().join("\n\n");
            let first = ch.first().and_then(|c| c["path"].as_str()).unwrap_or("");
            (tool_title("fileChange", &serde_json::json!({"path": first}), false), body)
        }
        "mcpToolCall" => (tool_title(&format!("{}.{}", it["server"].as_str().unwrap_or("?"), it["tool"].as_str().unwrap_or("?")), &it["arguments"], !it["error"].is_null()),
                          tool_body(&it["arguments"], Some(&pretty(if it["error"].is_null() { &it["result"] } else { &it["error"] })))),
        "webSearch" => (tool_title("webSearch", &serde_json::json!({"query": it["query"]}), false), pretty(it)),
        _ => (text::event_title(kind), pretty(it)),
    }
}

/// claude's tool result content: a string, or blocks of text and images.
fn result_text(c: &Value) -> String {
    match c {
        Value::Array(a) => a.iter().map(|b| match b["type"].as_str() {
            Some("text") => b["text"].as_str().unwrap_or("").to_string(),
            Some("image") => "[图片]".to_string(),
            _ => pretty(b),
        }).collect::<Vec<_>>().join("\n"),
        _ => pretty(c),
    }
}

/// Keep an error short enough for a message.
/// Error text as the agent gave it. (It used to be shortened; nothing is cut any more, long text is folded when shown.)
pub fn clip(s: &str) -> String { s.trim().to_string() }

/// The last few lines of an error output.
pub fn last_lines(s: &str) -> String { s.trim().to_string() }

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
        assert!(claude_note(&json!({"type":"system","subtype":"api_retry","attempt":2})).is_some());   // every retry is shown
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

#[cfg(test)]
mod read_tests {
    use super::*;

    fn feed_claude(lines: &[Value]) -> Vec<Ev> {
        let (tx, rx) = std::sync::mpsc::channel();
        let input = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        read_claude(std::io::Cursor::new(input.into_bytes()), tx, "tgbot:1".into(), 1, Arc::default(), Arc::default(), Arc::default(), Arc::default(), Arc::default());
        rx.try_iter().collect()
    }

    fn feed_pi(lines: &[Value]) -> Vec<Ev> {
        let (tx, rx) = std::sync::mpsc::channel();
        let input = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        read_pi(std::io::Cursor::new(input.into_bytes()), tx, "tgbot:1".into(), 1, Arc::default(), Arc::default(), Arc::default());
        rx.try_iter().collect()
    }

    /// What the user would read as the answer: the text pieces and the final texts.
    fn feed_agy(lines: &[Value]) -> Vec<Ev> {
        let (tx, rx) = std::sync::mpsc::channel();
        let input = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        let spec = Spec { tid: "tgbot:1".into(), prompt: String::new(), topic: String::new(), dev: Value::Null, dir: String::new(), command: String::new(),
                          kind: Kind::Agy, gen: 1, resume: None, effort: None };
        let model = Arc::new(Mutex::new(Some("gemini-4-neon-high".to_string())));
        read_agy(std::io::Cursor::new(input.into_bytes()), &spec, &tx, 1, &Arc::default(), &Arc::default(), &Arc::default(), &Arc::default(), &Arc::default(), &model, &Arc::default());
        rx.try_iter().collect()
    }
    #[test]
    fn agy_turn() {
        // as agy 1.5.15 (ACP) sent it, shortened
        let up = |u: Value| json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s", "update": u}});
        let evs = feed_agy(&[
            up(json!({"sessionUpdate": "usage_update", "used": 8780, "size": 1048576})),
            up(json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "I should run "}})),
            up(json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "it."}})),
            up(json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "name": "run_command", "status": "in_progress", "kind": "execute", "title": "echo hi", "rawInput": {"CommandLine": "echo hi"}})),
            up(json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "completed", "rawOutput": {"output": "Command output:\nhi_out\n", "exit_code": 0}})),
            up(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "The output "}})),
            up(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "is hi."}})),
            json!({"jsonrpc": "2.0", "id": "turn", "result": {"stopReason": "end_turn", "usage": {"inputTokens": 10925, "outputTokens": 177, "cachedReadTokens": 6617, "cachedWriteTokens": 0, "thoughtTokens": 90}}}),
        ]);
        let text: String = evs.iter().filter_map(|e| match e { Ev::Text { delta, .. } => Some(delta.as_str()), _ => None }).collect();
        assert_eq!(text, "The output is hi.");
        assert!(matches!(evs.iter().find(|e| matches!(e, Ev::Done { .. })), Some(Ev::Done { text: Some(t), error: None, stopped: false, .. }) if t == "The output is hi."));
        let st = steps(&evs);
        assert!(st.iter().any(|(t, b)| t == text::THINKING && b == "I should run it."));
        assert!(st.iter().any(|(_, b)| b.contains("echo hi") && b.contains("hi_out")));
        assert!(evs.iter().any(|e| matches!(e, Ev::Context { tokens: 8780, .. })));
        let u = evs.iter().find_map(|e| match e { Ev::Usage { uses, .. } => Some(uses.clone()), _ => None }).unwrap();
        assert_eq!((u[0].model.as_str(), u[0].input, u[0].output, u[0].cache_read), ("gemini-4-neon-high", 10925, 267, 6617));
        // stopped with session/cancel
        let evs = feed_agy(&[json!({"jsonrpc": "2.0", "id": "turn", "result": {"stopReason": "cancelled"}})]);
        assert!(matches!(evs.iter().find(|e| matches!(e, Ev::Done { .. })), Some(Ev::Done { stopped: true, error: None, .. })));
        // a failed prompt says why
        let evs = feed_agy(&[json!({"jsonrpc": "2.0", "id": "turn", "error": {"code": -32603, "message": "quota"}})]);
        assert!(matches!(evs.iter().find(|e| matches!(e, Ev::Done { .. })), Some(Ev::Done { error: Some(e), .. }) if e == "quota"));
        // the command line's own commands, from `agy -p /help`
        assert_eq!(parse_agy_help("/config (settings)\tOpen settings panel\n/usage (quota)\tView model quota usage\nnoise\n"),
                   vec![("config".to_string(), "Open settings panel".to_string()), ("usage".to_string(), "View model quota usage".to_string())]);
    }
    fn answers(evs: &[Ev]) -> Vec<String> {
        evs.iter().filter_map(|e| match e { Ev::Text { delta, .. } => Some(delta.trim().to_string()), Ev::Done { text, .. } => text.clone(), _ => None })
            .filter(|t| !t.is_empty()).collect()
    }

    fn steps(evs: &[Ev]) -> Vec<(String, String)> {
        evs.iter().filter_map(|e| match e { Ev::Step { title, body, .. } => Some((title.clone(), body.clone())), _ => None }).collect()
    }

    #[test]
    fn compact_repeat_is_not_the_answer() {
        let said = |t: &str| json!({"type": "assistant", "parent_tool_use_id": null, "message": {"content": [{"type": "text", "text": t}]}});
        let result = |t: &str| json!({"type": "result", "subtype": "success", "is_error": false, "result": t});
        let evs = feed_claude(&[
            json!({"type": "system", "subtype": "compact_boundary", "compact_metadata": {"trigger": "manual", "pre_tokens": 14495}}),
            json!({"type": "user", "message": {"content": "<local-command-stdout>Compacted </local-command-stdout>"}}),
            // claude says again what the last local command (/model) said
            said("Set model to opus"),
            result("Set model to opus"),
            // the next turn is answered as usual
            said("真正的回答"),
            result("真正的回答"),
        ]);
        assert_eq!(answers(&evs), vec!["真正的回答", "真正的回答"]);
        // shown folded, nothing dropped
        assert!(steps(&evs).iter().any(|(t, b)| t.contains("压缩") && b == "Set model to opus"), "{:?}", steps(&evs));
        // a turn that ends after a compaction still ends
        assert_eq!(evs.iter().filter(|e| matches!(e, Ev::Done { .. })).count(), 2);
        // compacting by itself in the middle of a turn changes nothing: what follows is the answer
        let evs = feed_claude(&[
            json!({"type": "system", "subtype": "compact_boundary", "compact_metadata": {"trigger": "auto"}}),
            said("继续回答"),
            result("继续回答"),
        ]);
        assert_eq!(answers(&evs), vec!["继续回答", "继续回答"]);
    }

    #[test]
    fn pi_agent_end_is_summed_up() {
        let msg = |role: &str, t: String| json!({"role": role, "content": [{"type": "text", "text": t}]});
        let evs = feed_pi(&[json!({"type": "agent_end", "messages": [
            msg("user", "问".repeat(30_000)), msg("assistant", "答".repeat(30_000)), msg("user", "再问".into()), msg("assistant", format!("最后{}", "尾".repeat(5_000))),
        ]})]);
        let s = steps(&evs);
        assert_eq!(s.len(), 1, "{s:?}");
        let (title, body) = &s[0];
        assert!(title.contains("agent_end"), "{title}");
        // what was forwarded one by one already is not repeated: the count and the start of the last message
        assert!(body.chars().count() < 1_000, "{} 字", body.chars().count());
        assert!(body.contains('4') && body.contains("最后尾尾"), "{body}");
    }
}


/// How much of the last message pi's agent_end shows.
const AGENT_END_HEAD: usize = 300;

/// pi's agent_end in short: how many messages the conversation has, and the start of the last one.
fn agent_end_summary(v: &Value) -> String {
    let msgs = v["messages"].as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    let last = msgs.last().map(|m| {
        let t = match &m["content"] {
            Value::String(t) => t.clone(),
            Value::Array(a) => a.iter().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join(""),
            _ => String::new(),
        };
        (m["role"].as_str().unwrap_or("?").to_string(), t)
    });
    let rest: Vec<String> = v.as_object().into_iter().flatten().filter(|(k, _)| *k != "type" && *k != "messages").map(|(k, x)| format!("{k}: {x}")).collect();
    text::agent_end(msgs.len(), last.as_ref().map(|(r, t)| (r.as_str(), t.chars().take(AGENT_END_HEAD).collect::<String>(), t.chars().count() > AGENT_END_HEAD)), &rest.join("\n"))
}