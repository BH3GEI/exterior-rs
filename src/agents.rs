//! Driving one agent per topic. An agent is a row of the device's table: a name, the command that starts it,
//! and which protocol it speaks (claude: stream-json; pi: rpc; codex: app-server). Each stays running for the session.
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind { Claude, Pi, Codex }

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        match s.trim().to_lowercase().as_str() { "claude" => Some(Kind::Claude), "pi" => Some(Kind::Pi), "codex" => Some(Kind::Codex), _ => None }
    }
    pub fn name(&self) -> &'static str { match self { Kind::Claude => "claude", Kind::Pi => "pi", Kind::Codex => "codex" } }
    pub fn label(&self) -> &'static str { match self { Kind::Claude => "Claude", Kind::Pi => "Pi", Kind::Codex => "Codex" } }
}

/// The line that tells a new agent where the previous session is, in the previous agent's own words.
pub fn resume_line(kind: Kind, name: &str, id: &str) -> String {
    match kind { Kind::Claude => format!("{name} --resume {id}"), Kind::Pi => format!("{name} --session {id}"), Kind::Codex => format!("{name} resume {id}") }
}

pub enum Ev {
    Text { tid: String, gen: u64, delta: String },
    Done { tid: String, gen: u64, text: Option<String>, stopped: bool, error: Option<String> },
    Session { tid: String, gen: u64, id: String },
    /// side notice worth showing Yao: rate limit, model fallback, API retry
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
            if let Some(n) = l.strip_prefix("→ ") {
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
    let s = g.as_mut().ok_or("agent 没在运行")?;
    writeln!(s, "{v}").and_then(|_| s.flush()).map_err(|e| e.to_string())
}

/// An answer to one of our requests: hand it to whoever waits for it. Returns true if it was one.
fn route(pending: &Pending, id: Option<String>, v: &Value) -> bool {
    let Some(id) = id else { return false };
    if let Some(w) = pending.lock().unwrap().remove(&id) { let _ = w.send(v.clone()); return true; }
    false
}

/// A model to choose: what to send back, and what to show.
pub struct Choice { pub value: String, pub label: String }

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
                            busy: Arc::new(Mutex::new(false)), queue: Arc::new(Mutex::new(vec![])), pending: Arc::new(Mutex::new(HashMap::new())),
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
                "--include-partial-messages", "--dangerously-skip-permissions"].map(String::from).to_vec();
        if let Some(id) = &self.spec.resume { args.push("--resume".into()); args.push(id.clone()); }
        if !self.spec.prompt.is_empty() { args.push("--append-system-prompt".into()); args.push(self.spec.prompt.clone()); }
        let (out, last) = self.spawn(args)?;
        let (tx, tid, gen, child, busy, pending, info) = (self.tx.clone(), self.spec.tid.clone(), self.spec.gen, self.child.clone(), self.busy.clone(), self.pending.clone(), self.info.clone());
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
                    let stopped = v["subtype"] == "error_during_execution";
                    let error = if v["is_error"] == true && !stopped { Some(v["result"].as_str().unwrap_or("出错了").to_string()) } else { None };
                    let _ = tx.send(Ev::Done { tid: tid.clone(), gen, text: v["result"].as_str().map(String::from), stopped, error });
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
        let sid = self.spec.resume.clone().unwrap_or_else(|| format!("exterior-{}-{}", self.spec.tid.trim_start_matches("tgbot:"), crate::store::now()));
        let mut args: Vec<String> = vec!["--mode".into(), "rpc".into(), "--session-id".into(), sid.clone()];
        if !self.spec.prompt.is_empty() { args.push("--append-system-prompt".into()); args.push(self.spec.prompt.clone()); }
        let (out, last) = self.spawn(args)?;
        let _ = self.tx.send(Ev::Session { tid: self.spec.tid.clone(), gen: self.spec.gen, id: sid });
        let (tx, tid, gen, child, busy, pending) = (self.tx.clone(), self.spec.tid.clone(), self.spec.gen, self.child.clone(), self.busy.clone(), self.pending.clone());
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
                            let m = v["message"]["errorMessage"].as_str().unwrap_or("模型出错了");
                            let inner = serde_json::from_str::<Value>(m.splitn(2, ' ').nth(1).unwrap_or("")).ok()
                                .and_then(|j| j["error"]["message"].as_str().map(String::from));
                            last_err = Some(match inner { Some(i) => format!("{} {i}", m.split(' ').next().unwrap_or("")), None => m.chars().take(300).collect() });
                        } else { last_err = None; }
                    }
                    Some("auto_retry_start") => {
                        let a = v["attempt"].as_i64().unwrap_or(0);
                        if a == 1 || a % 3 == 0 {
                            let _ = tx.send(Ev::Note { tid: tid.clone(), gen, text: format!("模型出错，正在重试 {a}/{}：{}", v["maxAttempts"].as_i64().unwrap_or(0), v["errorMessage"].as_str().unwrap_or("").chars().take(200).collect::<String>()) });
                        }
                    }
                    Some("extension_error") => {
                        let e = v["error"].as_str().map(String::from).unwrap_or_else(|| v.to_string());
                        let _ = tx.send(Ev::Note { tid: tid.clone(), gen, text: format!("pi 扩展出错：{}", e.chars().take(200).collect::<String>()) });
                    }
                    Some("agent_settled") => {
                        *busy.lock().unwrap() = false;
                        let _ = tx.send(Ev::Done { tid: tid.clone(), gen, text: last_text.take(), stopped: v["aborted"] == true, error: last_err.take() });
                    }
                    Some("response") if v["success"] == false => {
                        *busy.lock().unwrap() = false;
                        let err = v["error"].as_str().unwrap_or("pi 拒绝了这条命令").to_string();
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
            let (mut reply, mut error): (String, Option<String>) = (String::new(), None);
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                let method = v["method"].as_str().unwrap_or("");
                if method.is_empty() {
                    route(&pending, v["id"].as_str().map(String::from).or(v["id"].as_u64().map(|n| n.to_string())), &v);
                    continue;
                }
                if !v["id"].is_null() {
                    // a request from codex (approvals): we run without approvals, so say yes; anything else, decline
                    let res = if method.ends_with("requestApproval") { json!({"id": v["id"], "result": {"decision": "accept"}}) }
                              else if method == "execCommandApproval" || method == "applyPatchApproval" { json!({"id": v["id"], "result": {"decision": "approved"}}) }
                              else { json!({"id": v["id"], "error": {"code": -32601, "message": "exterior 不处理这个请求"}}) };
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
                    "turn/started" => { *turn.lock().unwrap() = p["turn"]["id"].as_str().map(String::from); }
                    "item/agentMessage/delta" => {
                        let d = p["delta"].as_str().unwrap_or("");
                        reply.push_str(d);
                        let _ = tx.send(Ev::Text { tid: tid.clone(), gen, delta: d.to_string() });
                    }
                    "item/completed" if p["item"]["type"] == "agentMessage" => {
                        if !reply.is_empty() && !reply.ends_with("\n\n") { reply.push_str("\n\n"); let _ = tx.send(Ev::Text { tid: tid.clone(), gen, delta: "\n\n".into() }); }
                    }
                    "error" if p["willRetry"] == true => {
                        let _ = tx.send(Ev::Note { tid: tid.clone(), gen, text: format!("codex 出错，正在重试：{}", p["error"]["message"].as_str().unwrap_or("")) });
                    }
                    "error" if p["willRetry"] != true => { error = p["error"]["message"].as_str().map(String::from).or(Some("codex 出错了".into())); }
                    "turn/completed" => {
                        *turn.lock().unwrap() = None;
                        let st = p["turn"]["status"].as_str().unwrap_or("");
                        if st == "failed" && error.is_none() { error = Some(p["turn"]["error"]["message"].as_str().unwrap_or("codex 出错了").to_string()); }
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
                for (k, v) in [("cwd", json!(spec.dir)), ("developerInstructions", json!(spec.prompt)), ("approvalPolicy", json!("never")), ("sandbox", json!("danger-full-access"))] { params[k] = v; }
                let method = if spec.resume.is_some() { "thread/resume" } else { "thread/start" };
                let r = c.ask(|id| json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
                if let Some(e) = r["error"]["message"].as_str() { return Err(format!("codex：{e}")); }
                r["result"]["thread"]["id"].as_str().map(String::from).ok_or("codex 没给会话编号".into())
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
                    let _ = tx.send(Ev::Done { tid: spec.tid.clone(), gen: spec.gen, text: None, stopped: false, error: Some(format!("codex 没启动起来：{e}")) });
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
            Kind::Claude => { let _ = put(&self.stdin, &json!({"type": "control_request", "request_id": "stop", "request": {"subtype": "interrupt"}})); }
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
    /// codex: wait (up to 40 s) until the background handshake is done.
    fn ready(&self) -> Result<(), String> {
        if self.kind != Kind::Codex { return Ok(()); }
        for _ in 0..80 { if self.thread.lock().unwrap().is_some() { return Ok(()); } std::thread::sleep(std::time::Duration::from_millis(500)); }
        Err("codex 还没准备好".into())
    }

    /// Send a request and wait for its answer (up to 30 s). `make` puts the id into the request.
    pub fn ask(&self, make: impl FnOnce(&str) -> Value) -> Result<Value, String> {
        let id = format!("x{}", self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1);
        let (w, r) = std::sync::mpsc::channel();
        self.pending.lock().unwrap().insert(id.clone(), w);
        let v = make(&id);
        if let Err(e) = put(&self.stdin, &v) { self.pending.lock().unwrap().remove(&id); return Err(e); }
        r.recv_timeout(std::time::Duration::from_secs(30)).map_err(|_| { self.pending.lock().unwrap().remove(&id); "agent 没有回应".to_string() })
    }

    /// The models this agent can switch to.
    pub fn models(&self) -> Result<Vec<Choice>, String> {
        self.ready()?;
        match self.kind {
            Kind::Claude => {
                for _ in 0..30 { if !self.info.lock().unwrap().is_null() { break; } std::thread::sleep(std::time::Duration::from_secs(1)); }
                let info = self.info.lock().unwrap().clone();
                Ok(info["models"].as_array().into_iter().flatten().filter_map(|m| Some(Choice { value: m["value"].as_str()?.to_string(), label: m["displayName"].as_str().unwrap_or(m["value"].as_str()?).to_string() })).collect())
            }
            Kind::Pi => {
                let r = self.ask(|id| json!({"type": "get_available_models", "id": id}))?;
                Ok(r["data"]["models"].as_array().into_iter().flatten().filter_map(|m| Some(Choice { value: format!("{}/{}", m["provider"].as_str()?, m["id"].as_str()?), label: m["name"].as_str().unwrap_or(m["id"].as_str()?).to_string() })).collect())
            }
            Kind::Codex => {
                let r = self.ask(|id| json!({"jsonrpc": "2.0", "id": id, "method": "model/list", "params": {}}))?;
                Ok(r["result"]["data"].as_array().into_iter().flatten().filter(|m| m["hidden"] != true).filter_map(|m| Some(Choice { value: m["model"].as_str().or(m["id"].as_str())?.to_string(), label: m["displayName"].as_str().unwrap_or(m["id"].as_str()?).to_string() })).collect())
            }
        }
    }

    pub fn set_model(&self, value: &str) -> Result<(), String> {
        let r = match self.kind {
            Kind::Claude => self.ask(|id| json!({"type": "control_request", "request_id": id, "request": {"subtype": "set_model", "model": value}}))?,
            Kind::Pi => {
                let (p, m) = value.split_once('/').ok_or("模型要写成 provider/id")?;
                self.ask(|id| json!({"type": "set_model", "id": id, "provider": p, "modelId": m}))?
            }
            Kind::Codex => { *self.model.lock().unwrap() = Some(value.to_string()); return Ok(()); }
        };
        if r["subtype"] == "error" || r["success"] == false { return Err(r["error"].as_str().unwrap_or("没换成").to_string()); }
        Ok(())
    }

    /// Built-in commands that are not prompts: pi's thinking/compact/new, codex's compact/new.
    pub fn builtin(&self, name: &str, arg: &str) -> Result<String, String> {
        self.ready()?;
        let r = match (self.kind, name) {
            (Kind::Pi, "thinking") => self.ask(|id| json!({"type": "set_thinking_level", "id": id, "level": arg}))?,
            (Kind::Pi, "compact") => self.ask(|id| json!({"type": "compact", "id": id}))?,
            (Kind::Pi, "new") => self.ask(|id| json!({"type": "new_session", "id": id}))?,
            (Kind::Codex, "compact") => {
                let th = self.thread.lock().unwrap().clone().ok_or("codex 还没准备好")?;
                self.ask(|id| json!({"jsonrpc": "2.0", "id": id, "method": "thread/compact/start", "params": {"threadId": th}}))?
            }
            (Kind::Codex, "new") => {
                let p = json!({"cwd": self.dir, "developerInstructions": self.prompt, "approvalPolicy": "never", "sandbox": "danger-full-access"});
                self.ask(|id| json!({"jsonrpc": "2.0", "id": id, "method": "thread/start", "params": p}))?
            }
            _ => return Err("没有这个命令".into()),
        };
        if r["success"] == false || !r["error"].is_null() {
            return Err(r["error"]["message"].as_str().or(r["error"].as_str()).unwrap_or("失败了").to_string());
        }
        Ok("好了".into())
    }
}

fn codex_turn(thread: &str, text: &str, model: Option<String>) -> Value {
    let mut p = json!({"threadId": thread, "input": [{"type": "text", "text": text}]});
    if let Some(m) = model { p["model"] = json!(m); }
    json!({"jsonrpc": "2.0", "id": format!("turn-{}", crate::store::now()), "method": "turn/start", "params": p})
}

/// One question, one answer: used by the 测试 button on the Agent page.
pub fn test(dev: &Value, dir: &str, command: &str, kind: Kind) -> Result<String, String> {
    let q = "只回复两个字母：OK";
    let args: Vec<String> = match kind {
        Kind::Claude => vec!["-p".into(), q.into(), "--dangerously-skip-permissions".into()],
        Kind::Pi => vec!["-p".into(), "--no-session".into(), q.into()],
        Kind::Codex => vec!["exec".into(), "--skip-git-repo-check".into(), q.into()],
    };
    let mut c = crate::host::agent(dev, dir, &[("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1".into())], command, &args);
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = c.spawn().map_err(|e| e.to_string())?;
    let out = crate::host::wait(child, 180)?;
    let so = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let se = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let picked = se.lines().filter_map(|l| l.trim().strip_prefix("→ ")).last().map(|n| format!("（{command} 选了 {n}）")).unwrap_or_default();
    if out.status.success() && !so.is_empty() {
        let tail: String = so.lines().last().unwrap_or("").chars().take(200).collect();
        Ok(format!("{tail}{picked}"))
    } else {
        let msg = if se.is_empty() { so } else { se };
        let tail: Vec<&str> = msg.lines().rev().take(3).collect();
        Err(format!("退出码 {:?}：{}", out.status.code(), tail.into_iter().rev().collect::<Vec<_>>().join(" / ")))
    }
}

/// The agent's own slash commands, with descriptions (no model call).
pub fn commands(dev: &Value, dir: &str, command: &str, kind: Kind) -> Vec<(String, String)> {
    let (args, ask): (Vec<String>, Value) = match kind {
        Kind::Claude => (["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--dangerously-skip-permissions"].map(String::from).to_vec(),
                         json!({"type": "control_request", "request_id": "init", "request": {"subtype": "initialize"}})),
        Kind::Pi => (vec!["--mode".into(), "rpc".into(), "--no-session".into()], json!({"type": "get_commands"})),
        Kind::Codex => return vec![],
    };
    let Ok(mut child) = crate::host::agent(dev, dir, &env(&Spec { tid: String::new(), prompt: String::new(), topic: String::new(), dev: dev.clone(), dir: dir.into(), command: command.into(), kind, gen: 0, resume: None }), command, &args)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() else { return vec![] };
    let mut stdin = child.stdin.take().unwrap();
    let _ = writeln!(stdin, "{ask}");
    let _ = stdin.flush();
    // never wait forever: if the agent does not answer in 90 s, close it
    let stdout = child.stdout.take().unwrap();
    let child = Arc::new(Mutex::new(child));
    { let c = child.clone(); std::thread::spawn(move || { std::thread::sleep(std::time::Duration::from_secs(90)); let _ = c.lock().unwrap().kill(); }); }
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

/// Claude's side notices that Yao should see (limits, fallback to another model, retries).
pub fn claude_note(v: &Value) -> Option<String> {
    let model = |k: &str| v[k].as_str().unwrap_or("?").to_string();
    match (v["type"].as_str()?, v["subtype"].as_str().unwrap_or("")) {
        ("rate_limit_event", _) => {
            let i = &v["rate_limit_info"];
            let st = i["status"].as_str().unwrap_or("");
            if st == "allowed" || st.is_empty() { return None; }
            let kind = i["rateLimitType"].as_str().unwrap_or("");
            let when = i["resetsAt"].as_i64().map(|t| {
                let t = if t > 1_000_000_000_000 { t / 1000 } else { t };
                let left = t - std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(t);
                if left > 0 { format!("，约 {} 小时 {} 分后恢复", left / 3600, left % 3600 / 60) } else { String::new() }
            }).unwrap_or_default();
            let pct = i["utilization"].as_f64().map(|u| format!("（已用 {:.0}%）", if u <= 1.0 { u * 100.0 } else { u })).unwrap_or_default();
            Some(match st { "rejected" => format!("额度用完了 {kind}{when}"), _ => format!("额度快用完 {kind}{pct}{when}") })
        }
        ("system", "model_fallback") | ("system", "model_refusal_fallback") | ("system", "model_consent_fallback") => {
            let c = v["content"].as_str().unwrap_or("").trim();
            Some(if c.is_empty() { format!("降级：{} → {}", model("original_model"), model("fallback_model")) } else { c.to_string() })
        }
        ("system", "api_retry") => {
            let a = v["attempt"].as_i64().unwrap_or(0);
            if a != 1 && a % 3 != 0 { return None; }
            Some(format!("接口出错，正在重试 {a}/{}：{}", v["max_retries"].as_i64().unwrap_or(0), v["error"].as_str().map(String::from).unwrap_or_else(|| v["error_status"].to_string())))
        }
        ("system", "notification") => {
            let t = v["text"].as_str()?.trim();
            if t.is_empty() || v["priority"] == "low" { None } else { Some(t.to_string()) }
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
        assert!(claude_note(&json!({"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour","resetsAt":4102444800i64}})).unwrap().starts_with("额度用完了 five_hour，约"));
        assert_eq!(claude_note(&json!({"type":"system","subtype":"model_fallback","original_model":"claude-opus-5-5","fallback_model":"claude-opus-4-8","content":""})).unwrap(), "降级：claude-opus-5-5 → claude-opus-4-8");
        assert!(claude_note(&json!({"type":"system","subtype":"api_retry","attempt":1,"max_retries":10,"error":"overloaded"})).unwrap().contains("1/10"));
        assert_eq!(claude_note(&json!({"type":"system","subtype":"api_retry","attempt":2})), None);
        assert_eq!(claude_note(&json!({"type":"assistant"})), None);
    }
}
