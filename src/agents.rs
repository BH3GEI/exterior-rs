//! Driving one agent process per session: claude (stream-json), pi (rpc), codex (exec --json, one process per turn).
//! Every driver turns the agent's output into the same few events.
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind { Claude, Pi, Codex }

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        match s.trim().to_lowercase().as_str() {
            "claude" | "cc" => Some(Kind::Claude),
            "pi" => Some(Kind::Pi),
            "codex" | "cx" => Some(Kind::Codex),
            _ => None,
        }
    }
    pub fn name(&self) -> &'static str { match self { Kind::Claude => "claude", Kind::Pi => "pi", Kind::Codex => "codex" } }
    pub fn prefix(&self) -> &'static str { match self { Kind::Claude => "cc", Kind::Pi => "pi", Kind::Codex => "cx" } }
    /// the key in the session note that holds this agent's own session id
    pub fn note_key(&self) -> &'static str { match self { Kind::Claude => "claude_session", Kind::Pi => "pi_session", Kind::Codex => "codex_thread" } }
}

/// What a session's agent says, as seen by exterior.
pub enum Ev {
    Text { tid: String, gen: u64, delta: String },
    Done { tid: String, gen: u64, text: Option<String>, stopped: bool, error: Option<String> },
    Session { tid: String, kind: Kind, id: String, config_dir: Option<String> },
    Exited { tid: String, gen: u64 },
}

pub struct Spec {
    pub tid: String,
    pub name: String,
    pub dir: String,
    pub root: String,
    pub gen: u64,
    pub resume: Option<String>,
    pub config_dir: Option<String>,
}

/// A running agent. For codex there is no long-lived process: each turn starts one.
pub struct Agent {
    pub kind: Kind,
    stdin: Option<ChildStdin>,
    child: Arc<Mutex<Option<Child>>>,
    spec: Spec,
    tx: Sender<Ev>,
    busy: Arc<Mutex<bool>>,
    queue: Arc<Mutex<Vec<String>>>, // codex: messages that arrived while a turn was running
    codex_thread: Arc<Mutex<Option<String>>>,
}

fn launch(spec: &Spec, env: &[(&str, String)], prog: &str, args: &[String]) -> Command {
    let mut e: Vec<(&str, String)> = vec![("EXTERIOR_SESSION", spec.name.clone()), ("EXTERIOR_ROOT", spec.root.clone())];
    e.extend(env.iter().cloned());
    crate::host::command(&spec.dir, &e, prog, args)
}

fn out(cmd: &str) -> String {
    crate::host::sh(cmd).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

/// Pick the claude config dir (account) with the most headroom, like `cc0` does.
pub fn pick_claude_config() -> String {
    let n = out("zsh ~/.agents/cc-pick.zsh $HOME 2>/dev/null");
    format!("/Users/you/.claude-cc{}", if n.is_empty() { "4".into() } else { n })
}

pub fn pick_codex_home() -> String {
    let n = out("zsh ~/.agents/cx-pick.zsh $HOME 2>/dev/null");
    format!("/Users/you/.codex-cx{}", if n.is_empty() { "1".into() } else { n })
}

impl Agent {
    pub fn start(kind: Kind, spec: Spec, tx: Sender<Ev>) -> Result<Agent, String> {
        let mut a = Agent { kind, stdin: None, child: Arc::new(Mutex::new(None)), spec, tx, busy: Arc::new(Mutex::new(false)),
                            queue: Arc::new(Mutex::new(vec![])), codex_thread: Arc::new(Mutex::new(None)) };
        match kind {
            Kind::Claude => a.start_claude()?,
            Kind::Pi => a.start_pi()?,
            Kind::Codex => { *a.codex_thread.lock().unwrap() = a.spec.resume.clone(); }
        }
        Ok(a)
    }

    fn start_claude(&mut self) -> Result<(), String> {
        let cfg = self.spec.config_dir.clone().unwrap_or_else(pick_claude_config);
        let mut args: Vec<String> = ["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
                "--include-partial-messages", "--dangerously-skip-permissions"].map(String::from).to_vec();
        if let Some(id) = &self.spec.resume { args.push("--resume".into()); args.push(id.clone()); }
        let mut c = launch(&self.spec, &[("CLAUDE_CONFIG_DIR", cfg.clone()), ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1".into())], "claude", &args);
        let mut child = c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(|e| e.to_string())?;
        self.stdin = child.stdin.take();
        let out = child.stdout.take().unwrap();
        *self.child.lock().unwrap() = Some(child);
        let (tx, tid, gen, child, busy) = (self.tx.clone(), self.spec.tid.clone(), self.spec.gen, self.child.clone(), self.busy.clone());
        std::thread::spawn(move || {
            let mut known = String::new();
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                if let Some(s) = v["session_id"].as_str() {
                    if s != known {
                        known = s.to_string();
                        let _ = tx.send(Ev::Session { tid: tid.clone(), kind: Kind::Claude, id: known.clone(), config_dir: Some(cfg.clone()) });
                    }
                }
                if let Some(d) = v["event"]["delta"]["text"].as_str() {
                    let _ = tx.send(Ev::Text { tid: tid.clone(), gen, delta: d.to_string() });
                }
                if v["type"] == "result" {
                    *busy.lock().unwrap() = false;
                    let stopped = v["subtype"] == "error_during_execution";
                    let error = if v["is_error"] == true && !stopped { Some(v["result"].as_str().unwrap_or("出错了").to_string()) } else { None };
                    let _ = tx.send(Ev::Done { tid: tid.clone(), gen, text: v["result"].as_str().map(String::from), stopped, error });
                }
            }
            if let Some(mut c) = child.lock().unwrap().take() { let _ = c.wait(); }
            let _ = tx.send(Ev::Exited { tid, gen });
        });
        Ok(())
    }

    fn start_pi(&mut self) -> Result<(), String> {
        let sid = self.spec.resume.clone().unwrap_or_else(|| format!("exterior-{}", self.spec.tid));
        let mut c = launch(&self.spec, &[], "pi", &["--mode".into(), "rpc".into(), "--session-id".into(), sid.clone()]);
        let mut child = c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(|e| e.to_string())?;
        self.stdin = child.stdin.take();
        let out = child.stdout.take().unwrap();
        *self.child.lock().unwrap() = Some(child);
        let _ = self.tx.send(Ev::Session { tid: self.spec.tid.clone(), kind: Kind::Pi, id: sid, config_dir: None });
        let (tx, tid, gen, child, busy) = (self.tx.clone(), self.spec.tid.clone(), self.spec.gen, self.child.clone(), self.busy.clone());
        std::thread::spawn(move || {
            let mut last_text: Option<String> = None;
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                match v["type"].as_str() {
                    Some("message_update") => {
                        let e = &v["assistantMessageEvent"];
                        if e["type"] == "text_delta" {
                            let _ = tx.send(Ev::Text { tid: tid.clone(), gen, delta: e["delta"].as_str().unwrap_or("").to_string() });
                        }
                    }
                    Some("turn_end") => {
                        let t: String = v["message"]["content"].as_array().into_iter().flatten()
                            .filter(|c| c["type"] == "text").filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("");
                        if !t.is_empty() { last_text = Some(t); }
                    }
                    Some("agent_settled") => {
                        *busy.lock().unwrap() = false;
                        let stopped = v["aborted"] == true;
                        let _ = tx.send(Ev::Done { tid: tid.clone(), gen, text: last_text.take(), stopped, error: None });
                    }
                    Some("response") if v["success"] == false => {
                        let err = v["error"].as_str().unwrap_or("pi 拒绝了这条命令").to_string();
                        *busy.lock().unwrap() = false;
                        let _ = tx.send(Ev::Done { tid: tid.clone(), gen, text: None, stopped: false, error: Some(err) });
                    }
                    _ => {}
                }
            }
            if let Some(mut c) = child.lock().unwrap().take() { let _ = c.wait(); }
            let _ = tx.send(Ev::Exited { tid, gen });
        });
        Ok(())
    }

    /// Hand one message (or a slash command) to the agent.
    pub fn say(&mut self, text: &str) -> Result<(), String> {
        let was_busy = { let mut b = self.busy.lock().unwrap(); let w = *b; *b = true; w };
        match self.kind {
            Kind::Claude => self.write(json!({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": text}]}})),
            Kind::Pi => {
                let mut m = json!({"type": "prompt", "message": text});
                if was_busy { m["streamingBehavior"] = json!("followUp"); }
                self.write(m)
            }
            Kind::Codex => {
                if was_busy { self.queue.lock().unwrap().push(text.to_string()); return Ok(()); }
                self.codex_turn(text.to_string());
                Ok(())
            }
        }
    }

    pub fn stop(&mut self) {
        match self.kind {
            Kind::Claude => { let _ = self.write(json!({"type": "control_request", "request_id": "stop", "request": {"subtype": "interrupt"}})); }
            Kind::Pi => { let _ = self.write(json!({"type": "abort"})); }
            Kind::Codex => {
                self.queue.lock().unwrap().clear();
                if let Some(c) = self.child.lock().unwrap().as_mut() { let _ = c.kill(); }
            }
        }
    }

    pub fn kill(&mut self) {
        self.stdin = None;
        if let Some(c) = self.child.lock().unwrap().as_mut() { let _ = c.kill(); }
    }

    fn write(&mut self, v: Value) -> Result<(), String> {
        let s = self.stdin.as_mut().ok_or("agent 没在运行")?;
        writeln!(s, "{v}").and_then(|_| s.flush()).map_err(|e| e.to_string())
    }

    /// codex: one `codex exec --json [resume <thread>] <text>` per turn; queued messages follow in order.
    fn codex_turn(&self, text: String) {
        let (tx, tid, gen) = (self.tx.clone(), self.spec.tid.clone(), self.spec.gen);
        let (child_slot, busy, queue, thread) = (self.child.clone(), self.busy.clone(), self.queue.clone(), self.codex_thread.clone());
        let home = self.spec.config_dir.clone().unwrap_or_else(pick_codex_home);
        let spec = Spec { tid: self.spec.tid.clone(), name: self.spec.name.clone(), dir: self.spec.dir.clone(), root: self.spec.root.clone(),
                          gen: self.spec.gen, resume: None, config_dir: None };
        std::thread::spawn(move || {
            let mut text = text;
            loop {
                let mut args: Vec<String> = ["exec", "--json", "--skip-git-repo-check", "--dangerously-bypass-approvals-and-sandbox"].map(String::from).to_vec();
                if let Some(t) = thread.lock().unwrap().clone() { args.push("resume".into()); args.push(t); }
                args.push(text.clone());
                let mut cmd = launch(&spec, &[("CODEX_HOME", home.clone())], "codex", &args);
                let mut child = match cmd.stdout(Stdio::piped()).stderr(Stdio::null()).stdin(Stdio::null()).spawn() {
                    Ok(ch) => ch,
                    Err(e) => { let _ = tx.send(Ev::Done { tid: tid.clone(), gen, text: None, stopped: false, error: Some(e.to_string()) }); break; }
                };
                let out = child.stdout.take().unwrap();
                *child_slot.lock().unwrap() = Some(child);
                let (mut reply, mut error) = (String::new(), None);
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                    match v["type"].as_str() {
                        Some("thread.started") => {
                            if let Some(t) = v["thread_id"].as_str() {
                                *thread.lock().unwrap() = Some(t.to_string());
                                let _ = tx.send(Ev::Session { tid: tid.clone(), kind: Kind::Codex, id: t.to_string(), config_dir: Some(home.clone()) });
                            }
                        }
                        Some("item.completed") if v["item"]["type"] == "agent_message" => {
                            let t = v["item"]["text"].as_str().unwrap_or("");
                            if !reply.is_empty() { reply.push_str("\n\n"); let _ = tx.send(Ev::Text { tid: tid.clone(), gen, delta: "\n\n".into() }); }
                            reply.push_str(t);
                            let _ = tx.send(Ev::Text { tid: tid.clone(), gen, delta: t.to_string() });
                        }
                        Some("turn.failed") => { error = Some(v["error"]["message"].as_str().unwrap_or("codex 出错了").to_string()); }
                        _ => {}
                    }
                }
                let killed = { let mut s = child_slot.lock().unwrap(); let st = s.as_mut().and_then(|c| c.wait().ok()); s.take(); st.map(|s| !s.success()).unwrap_or(false) };
                let stopped = killed && error.is_none() && reply.is_empty();
                let _ = tx.send(Ev::Done { tid: tid.clone(), gen, text: if reply.is_empty() { None } else { Some(reply) }, stopped, error });
                let next = { let mut q = queue.lock().unwrap(); if q.is_empty() { None } else { Some(q.remove(0)) } };
                match next { Some(t) => text = t, None => { *busy.lock().unwrap() = false; break; } }
            }
        });
    }
}

/// claude's slash commands, with descriptions (no model call: just the initialize handshake).
pub fn claude_commands(dir: &str) -> Vec<(String, String)> {
    let args: Vec<String> = ["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--dangerously-skip-permissions"].map(String::from).to_vec();
    let mut c = crate::host::command(dir, &[("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1".into()), ("CLAUDE_CONFIG_DIR", pick_claude_config())], "claude", &args);
    let Ok(mut child) = c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() else { return vec![] };
    let mut stdin = child.stdin.take().unwrap();
    let _ = writeln!(stdin, "{}", json!({"type": "control_request", "request_id": "init", "request": {"subtype": "initialize"}}));
    let mut out = vec![];
    for line in BufReader::new(child.stdout.take().unwrap()).lines().map_while(Result::ok) {
        let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        if v["type"] == "control_response" {
            for c in v["response"]["response"]["commands"].as_array().into_iter().flatten() {
                if let Some(n) = c["name"].as_str() { out.push((n.to_string(), c["description"].as_str().unwrap_or("").to_string())); }
            }
            break;
        }
    }
    drop(stdin);
    let _ = child.kill();
    out
}

pub fn pi_commands(dir: &str) -> Vec<(String, String)> {
    let Ok(mut child) = crate::host::command(dir, &[], "pi", &["--mode".into(), "rpc".into(), "--no-session".into()])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() else { return vec![] };
    let mut stdin = child.stdin.take().unwrap();
    let _ = writeln!(stdin, "{}", json!({"type": "get_commands"}));
    let mut out = vec![];
    for line in BufReader::new(child.stdout.take().unwrap()).lines().map_while(Result::ok) {
        let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        if v["command"] == "get_commands" {
            let list = if v["data"]["commands"].is_array() { &v["data"]["commands"] } else { &v["data"] };
            for c in list.as_array().into_iter().flatten() {
                if let Some(n) = c["name"].as_str() { out.push((n.to_string(), c["description"].as_str().unwrap_or("").to_string())); }
            }
            break;
        }
    }
    drop(stdin);
    let _ = child.kill();
    out
}
