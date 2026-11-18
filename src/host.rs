//! Running things on a device: over ssh with the device's password (sshpass), or locally when host is "local".
//! Agent commands run through the user's interactive shell (`$SHELL -ic`), so their own functions (cc0, cx3…) work.
use serde_json::Value;
use std::process::{Command, Output, Stdio};

pub fn quote(s: &str) -> String { format!("'{}'", s.replace('\'', "'\\''")) }

fn is_local(dev: &Value) -> bool { matches!(dev["host"].as_str().unwrap_or(""), "" | "local" | "localhost") }

fn ssh_opts() -> Vec<String> {
    let kh = crate::store::path("known_hosts");
    ["-o", "StrictHostKeyChecking=accept-new", "-o", &format!("UserKnownHostsFile={kh}"), "-o", "PubkeyAuthentication=no",
     "-o", "PreferredAuthentications=password,keyboard-interactive", "-o", "ServerAliveInterval=30", "-o", "ConnectTimeout=20"]
        .iter().map(|s| s.to_string()).collect()
}


pub fn password(dev: &Value) -> String { dev["password"].as_str().and_then(crate::crypt::open).unwrap_or_default() }

/// A shell line run on the device.
pub fn line(dev: &Value, script: &str) -> Command {
    if is_local(dev) {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", script]);
        return c;
    }
    let port = dev["port"].as_u64().unwrap_or(22).to_string();
    let who = format!("{}@{}", dev["user"].as_str().unwrap_or(""), dev["host"].as_str().unwrap_or(""));
    let mut c = Command::new("sshpass");
    c.arg("-e").arg("ssh").args(["-p", &port]).args(ssh_opts()).arg(&who).arg(script);
    c.env("SSHPASS", password(dev));
    c
}

/// An agent command: `cd dir && <env> $SHELL -ic '<command> <args>'`.
pub fn agent(dev: &Value, dir: &str, env: &[(&str, String)], command: &str, args: &[String]) -> Command {
    let mut inner = command.to_string();
    for a in args { inner.push(' '); inner.push_str(&quote(a)); }
    let mut s = format!("mkdir -p {d} && cd {d} && exec env", d = quote(dir));
    for (k, v) in env { s.push_str(&format!(" {k}={}", quote(v))); }
    s.push_str(&format!(" \"${{SHELL:-/bin/sh}}\" -ic {}", quote(&inner)));
    line(dev, &s)
}

/// Run a short script on the device and wait (with a time limit).
pub fn run(dev: &Value, script: &str, secs: u64) -> Result<String, String> {
    let mut c = line(dev, script);
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = c.spawn().map_err(|e| format!("无法执行：{e}"))?;
    let out = wait(child, secs)?;
    let so = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let se = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if out.status.success() { Ok(so) } else { Err(if se.is_empty() { format!("退出码 {:?}；{so}", out.status.code()) } else { se }) }
}

pub fn wait(mut child: std::process::Child, secs: u64) -> Result<Output, String> {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().map_err(|e| e.to_string()),
            Ok(None) if start.elapsed().as_secs() >= secs => { let _ = child.kill(); return Err(format!("超时（{secs} 秒）")); }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(200)),
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Write a file on the device (always replaces it).
pub fn write(dev: &Value, path: &str, body: &str, mode: &str) -> Result<String, String> {
    let dir = path.rsplit_once('/').map(|x| x.0).unwrap_or("/");
    run(dev, &format!("mkdir -p {d} && printf '%s' {b} > {p} && chmod {mode} {p}", d = quote(dir), p = quote(path), b = quote(body)), 30)
}

