//! Driving one agent per topic. An agent is a row of the device's table: a name, the command that starts it,
//! and which protocol it speaks (claude: stream-json; pi: rpc; codex: exec --json, one process per turn).
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Stdio};
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
    /// the command picked a concrete account (cc0 → cc6): later restarts use that one
    Resolved { tid: String, gen: u64, name: String },
    Exited { tid: String, gen: u64, why: String },
}

#[derive(Clone)]
pub struct Spec {
    pub tid: String,
    pub topic: String,
    pub dev: Value,
    pub dir: String,
    pub command: String,
    pub kind: Kind,
    pub gen: u64,
    pub resume: Option<String>,
}

pub struct Agent {
    pub kind: Kind,
    stdin: Option<ChildStdin>,
    child: Arc<Mutex<Option<Child>>>,
    spec: Spec,
    tx: Sender<Ev>,
    busy: Arc<Mutex<bool>>,
    queue: Arc<Mutex<Vec<String>>>,
    codex_thread: Arc<Mutex<Option<String>>>,
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

impl Agent {
    pub fn start(spec: Spec, tx: Sender<Ev>) -> Result<Agent, String> {
        let mut a = Agent { kind: spec.kind, stdin: None, child: Arc::new(Mutex::new(None)), spec, tx, busy: Arc::new(Mutex::new(false)),
                            queue: Arc::new(Mutex::new(vec![])), codex_thread: Arc::new(Mutex::new(None)) };
        match a.kind {
            Kind::Claude => a.start_claude()?,
            Kind::Pi => a.start_pi()?,
            Kind::Codex => { *a.codex_thread.lock().unwrap() = a.spec.resume.clone(); }
        }
        Ok(a)
    }

    fn spawn(&mut self, args: Vec<String>) -> Result<(std::process::ChildStdout, Arc<Mutex<String>>), String> {
        let mut c = crate::host::agent(&self.spec.dev, &self.spec.dir, &env(&self.spec), &self.spec.command, &args);
        let mut child = c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
        self.stdin = child.stdin.take();
        let out = child.stdout.take().unwrap();
        let last = Arc::new(Mutex::new(String::new()));
        watch_stderr(child.stderr.take().unwrap(), self.tx.clone(), self.spec.tid.clone(), self.spec.gen, last.clone());
        *self.child.lock().unwrap() = Some(child);
        Ok((out, last))
    }

    fn start_claude(&mut self) -> Result<(), String> {
        let mut args: Vec<String> = ["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
                "--include-partial-messages", "--dangerously-skip-permissions"].map(String::from).to_vec();
        if let Some(id) = &self.spec.resume { args.push("--resume".into()); args.push(id.clone()); }
        let (out, last) = self.spawn(args)?;
        let (tx, tid, gen, child, busy) = (self.tx.clone(), self.spec.tid.clone(), self.spec.gen, self.child.clone(), self.busy.clone());
        std::thread::spawn(move || {
            let mut known = String::new();
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                if let Some(s) = v["session_id"].as_str() {
                    if s != known { known = s.to_string(); let _ = tx.send(Ev::Session { tid: tid.clone(), gen, id: known.clone() }); }
                }
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
        Ok(())
    }

    fn start_pi(&mut self) -> Result<(), String> {
        let sid = self.spec.resume.clone().unwrap_or_else(|| format!("exterior-{}-{}", self.spec.tid.trim_start_matches("tgbot:"), crate::store::now()));
        let (out, last) = self.spawn(vec!["--mode".into(), "rpc".into(), "--session-id".into(), sid.clone()])?;
        let _ = self.tx.send(Ev::Session { tid: self.spec.tid.clone(), gen: self.spec.gen, id: sid });
        let (tx, tid, gen, child, busy) = (self.tx.clone(), self.spec.tid.clone(), self.spec.gen, self.child.clone(), self.busy.clone());
        std::thread::spawn(move || {
            let mut last_text: Option<String> = None;
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                match v["type"].as_str() {
                    Some("message_update") => {
                        let e = &v["assistantMessageEvent"];
                        if e["type"] == "text_delta" { let _ = tx.send(Ev::Text { tid: tid.clone(), gen, delta: e["delta"].as_str().unwrap_or("").to_string() }); }
                    }
                    Some("turn_end") => {
                        let t: String = v["message"]["content"].as_array().into_iter().flatten()
                            .filter(|c| c["type"] == "text").filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("");
                        if !t.is_empty() { last_text = Some(t); }
                    }
                    Some("agent_settled") => {
                        *busy.lock().unwrap() = false;
                        let _ = tx.send(Ev::Done { tid: tid.clone(), gen, text: last_text.take(), stopped: v["aborted"] == true, error: None });
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

    /// Hand one message (or one of the agent's own slash commands) to the agent.
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

    /// Esc.
    pub fn stop(&mut self) {
        match self.kind {
            Kind::Claude => { let _ = self.write(json!({"type": "control_request", "request_id": "stop", "request": {"subtype": "interrupt"}})); }
            Kind::Pi => { let _ = self.write(json!({"type": "abort"})); }
            Kind::Codex => { self.queue.lock().unwrap().clear(); if let Some(c) = self.child.lock().unwrap().as_mut() { let _ = c.kill(); } }
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

    /// codex: one `<command> exec --json [resume <thread>] <text>` per turn; messages that arrive meanwhile follow in order.
    fn codex_turn(&self, text: String) {
        let (tx, spec) = (self.tx.clone(), self.spec.clone());
        let (child_slot, busy, queue, thread) = (self.child.clone(), self.busy.clone(), self.queue.clone(), self.codex_thread.clone());
        std::thread::spawn(move || {
            let (tid, gen) = (spec.tid.clone(), spec.gen);
            let mut text = text;
            loop {
                let mut args: Vec<String> = ["exec", "--json", "--skip-git-repo-check"].map(String::from).to_vec();
                if let Some(t) = thread.lock().unwrap().clone() { args.push("resume".into()); args.push(t); }
                args.push(text.clone());
                let mut cmd = crate::host::agent(&spec.dev, &spec.dir, &env(&spec), &spec.command, &args);
                let mut child = match cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null()).spawn() {
                    Ok(ch) => ch,
                    Err(e) => { let _ = tx.send(Ev::Done { tid: tid.clone(), gen, text: None, stopped: false, error: Some(e.to_string()) }); break; }
                };
                let out = child.stdout.take().unwrap();
                let last = Arc::new(Mutex::new(String::new()));
                watch_stderr(child.stderr.take().unwrap(), tx.clone(), tid.clone(), gen, last.clone());
                *child_slot.lock().unwrap() = Some(child);
                let (mut reply, mut error) = (String::new(), None);
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                    match v["type"].as_str() {
                        Some("thread.started") => {
                            if let Some(t) = v["thread_id"].as_str() {
                                *thread.lock().unwrap() = Some(t.to_string());
                                let _ = tx.send(Ev::Session { tid: tid.clone(), gen, id: t.to_string() });
                            }
                        }
                        Some("item.completed") if v["item"]["type"] == "agent_message" => {
                            let t = v["item"]["text"].as_str().unwrap_or("");
                            if !reply.is_empty() { reply.push_str("\n\n"); let _ = tx.send(Ev::Text { tid: tid.clone(), gen, delta: "\n\n".into() }); }
                            reply.push_str(t);
                            let _ = tx.send(Ev::Text { tid: tid.clone(), gen, delta: t.to_string() });
                        }
                        Some("turn.failed") | Some("error") => {
                            let m = v["error"]["message"].as_str().or(v["message"].as_str()).unwrap_or("codex 出错了");
                            error = Some(m.to_string());
                        }
                        _ => {}
                    }
                }
                let ok = { let mut s = child_slot.lock().unwrap(); let st = s.as_mut().and_then(|c| c.wait().ok()); s.take(); st.map(|s| s.success()).unwrap_or(false) };
                if !ok && error.is_none() && reply.is_empty() {
                    let e = last.lock().unwrap().trim().to_string();
                    if !e.is_empty() { error = Some(e.lines().last().unwrap_or("").to_string()); }
                }
                let stopped = !ok && error.is_none() && reply.is_empty();
                let _ = tx.send(Ev::Done { tid: tid.clone(), gen, text: if reply.is_empty() { None } else { Some(reply) }, stopped, error });
                let next = { let mut q = queue.lock().unwrap(); if q.is_empty() { None } else { Some(q.remove(0)) } };
                match next { Some(t) => text = t, None => { *busy.lock().unwrap() = false; break; } }
            }
        });
    }
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
    let Ok(mut child) = crate::host::agent(dev, dir, &env(&Spec { tid: String::new(), topic: String::new(), dev: dev.clone(), dir: dir.into(), command: command.into(), kind, gen: 0, resume: None }), command, &args)
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
