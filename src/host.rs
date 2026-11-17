//! Where the agents run. exterior itself may live elsewhere (on Lazycat) and reach the agents' machine over ssh.
//! EXTERIOR_HOST="user@host:port" → every agent command goes through `ssh`; unset → run locally.
use std::process::{Command, Output};

fn quote(s: &str) -> String { format!("'{}'", s.replace('\'', "'\\''")) }

pub fn remote() -> Option<(String, String)> {
    let h = std::env::var("EXTERIOR_HOST").ok().filter(|s| !s.is_empty())?;
    let (who, port) = h.rsplit_once(':').map(|(a, b)| (a.to_string(), b.to_string())).unwrap_or((h, "22".into()));
    Some((who, port))
}

/// PATH the agents see on their machine
pub fn agent_path() -> String {
    std::env::var("EXTERIOR_AGENT_PATH").unwrap_or("/Users/you/.agents/exterior/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".into())
}

/// A command `prog args` run in `dir` with `env`, on the agents' machine.
pub fn command(dir: &str, env: &[(&str, String)], prog: &str, args: &[String]) -> Command {
    match remote() {
        None => {
            let mut c = Command::new(prog);
            c.args(args).current_dir(dir).env("PATH", agent_path());
            for (k, v) in env { c.env(k, v); }
            c
        }
        Some((who, port)) => {
            let mut line = format!("cd {} && exec env PATH={}", quote(dir), quote(&agent_path()));
            for (k, v) in env { line.push_str(&format!(" {k}={}", quote(v))); }
            line.push_str(&format!(" {}", quote(prog)));
            for a in args { line.push(' '); line.push_str(&quote(a)); }
            let mut c = Command::new("ssh");
            c.args(["-p", &port, "-o", "BatchMode=yes", "-o", "ServerAliveInterval=30", "-o", "ConnectTimeout=20", &who, &line]);
            c
        }
    }
}

/// Run a short shell snippet on the agents' machine and wait for it.
pub fn sh(script: &str) -> Option<Output> {
    command("/", &[], "/bin/sh", &["-c".into(), script.into()]).output().ok()
}

/// Write a file on the agents' machine (creating its directory), unless it already exists.
pub fn write_new(path: &str, body: &str) {
    let dir = path.rsplit_once('/').map(|x| x.0).unwrap_or("/");
    let script = format!("mkdir -p {d} && [ -e {p} ] || printf '%s' {b} > {p}", d = quote(dir), p = quote(path), b = quote(body));
    let _ = sh(&script);
}

pub fn mkdir(path: &str) { let _ = sh(&format!("mkdir -p {}", quote(path))); }
