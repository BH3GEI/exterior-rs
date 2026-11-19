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
    // PATH is put in front of the device's own, not instead of it
    for (k, v) in env { if *k == "PATH" { s.push_str(&format!(" PATH={}:\"$PATH\"", quote(v))); } else { s.push_str(&format!(" {k}={}", quote(v))); } }
    s.push_str(&format!(" \"${{SHELL:-/bin/sh}}\" -ic {}", quote(&inner)));
    line(dev, &s)
}

/// Run a short script on the device and wait (with a time limit).
pub fn run(dev: &Value, script: &str, secs: u64) -> Result<String, String> {
    let mut c = line(dev, script);
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = c.spawn().map_err(|e| crate::text::exec_failed(&e.to_string()))?;
    let out = wait(child, secs)?;
    let so = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let se = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if out.status.success() { Ok(so) } else { Err(if se.is_empty() { crate::text::exit_code(out.status.code(), &so) } else { se }) }
}

pub fn wait(mut child: std::process::Child, secs: u64) -> Result<Output, String> {
    // read the output while waiting: a child whose output fills the pipe would otherwise block until the time limit
    fn drain<R: std::io::Read + Send + 'static>(r: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || { let mut v = Vec::new(); if let Some(mut r) = r { let _ = r.read_to_end(&mut v); } v })
    }
    let (so, se) = (drain(child.stdout.take()), drain(child.stderr.take()));
    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if start.elapsed().as_secs() >= secs => { let _ = child.kill(); let _ = child.wait(); return Err(crate::text::timed_out(secs)); }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(e) => return Err(e.to_string()),
        }
    };
    Ok(Output { status, stdout: so.join().unwrap_or_default(), stderr: se.join().unwrap_or_default() })
}

/// Write a file on the device (always replaces it).
pub fn write(dev: &Value, path: &str, body: &str, mode: &str) -> Result<String, String> {
    let dir = path.rsplit_once('/').map(|x| x.0).unwrap_or("/");
    run(dev, &format!("mkdir -p {d} && printf '%s' {b} > {p} && chmod {mode} {p}", d = quote(dir), p = quote(path), b = quote(body)), 30)
}


/// Write bytes to a file on the device (through stdin, so any size and any content).
pub fn write_bytes(dev: &Value, path: &str, data: &[u8]) -> Result<(), String> {
    let dir = path.rsplit_once('/').map(|x| x.0).unwrap_or("/");
    let mut c = line(dev, &format!("mkdir -p {d} && cat > {p}", d = quote(dir), p = quote(path)));
    c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().map_err(|e| crate::text::exec_failed(&e.to_string()))?;
    {
        use std::io::Write;
        let mut i = child.stdin.take().unwrap();
        i.write_all(data).map_err(|e| e.to_string())?;
    }
    let out = wait(child, 120)?;
    if out.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().to_string()) }
}

/// Read a file on the device, at most `max` + 1 bytes (so a caller can tell it was bigger). `path` is a shell word:
/// quoted already, or starting with "$HOME"/.
pub fn read_bytes(dev: &Value, path: &str, max: usize) -> Result<Vec<u8>, String> {
    let mut c = line(dev, &format!("test -f {path} || exit 3; head -c {} {path}", max + 1));
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = c.spawn().map_err(|e| crate::text::exec_failed(&e.to_string()))?;
    let out = wait(child, 300)?;
    match out.status.code() {
        Some(0) => Ok(out.stdout),
        Some(3) => Err(crate::text::FILE_MISSING.into()),
        _ => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn big_file_reads_at_once() {
        // more than a pipe holds: must not hang until the time limit
        let p = std::env::temp_dir().join("exterior-big-test.bin");
        std::fs::write(&p, vec![7u8; 400_000]).unwrap();
        let t = std::time::Instant::now();
        let b = super::read_bytes(&serde_json::json!({"host": "local"}), &super::quote(p.to_str().unwrap()), 1_000_000).unwrap();
        assert_eq!(b.len(), 400_000);
        assert!(t.elapsed().as_secs() < 5);
        assert_eq!(super::read_bytes(&serde_json::json!({"host": "local"}), "/nonexistent/x", 10).unwrap_err(), crate::text::FILE_MISSING);
    }
}
