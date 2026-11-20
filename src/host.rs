//! Running things on a device: over ssh (with the device's password through sshpass; with the user's own ssh keys
//! when the device has no password or this machine has no sshpass), or on this machine when host is "local".
//! Agent commands run through the user's interactive shell (`$SHELL -ic`; on Windows PowerShell with the user's
//! profile), so their own functions (cc0, cx3…) work. Files on this machine are read and written directly.
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;

pub fn quote(s: &str) -> String { format!("'{}'", s.replace('\'', "'\\''")) }

pub fn is_local(dev: &Value) -> bool { matches!(dev["host"].as_str().unwrap_or(""), "" | "local" | "localhost") }

/// Whether a program is on PATH (on Windows also with the PATHEXT endings: .exe, .cmd…).
pub fn on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT".into()).split(';').map(str::to_string).chain([String::new()]).collect()
    } else { vec![String::new()] };
    std::env::split_paths(&path).any(|d| exts.iter().any(|e| d.join(format!("{name}{e}")).is_file()))
}

fn sshpass() -> bool { static S: OnceLock<bool> = OnceLock::new(); *S.get_or_init(|| on_path("sshpass")) }

pub fn password(dev: &Value) -> String { dev["password"].as_str().and_then(crate::crypt::open).unwrap_or_default() }

/// A remote device is logged into with the user's ssh keys (no password asked) when it has no password set, or
/// when this machine has no sshpass (Windows, a Mac without it).
pub fn uses_keys(dev: &Value) -> bool { password(dev).is_empty() || !sshpass() }

/// The device has a password, but it cannot be used here (no sshpass): keys are tried instead.
pub fn password_unused(dev: &Value) -> bool { !is_local(dev) && !password(dev).is_empty() && !sshpass() }

/// ssh's option for exterior's own known_hosts file (ssh splits option values at spaces unless quoted).
fn known_hosts_opt(path: &str) -> String {
    let p = if cfg!(windows) { path.replace('\\', "/") } else { path.to_string() };
    if p.contains(char::is_whitespace) { format!("UserKnownHostsFile=\"{p}\"") } else { format!("UserKnownHostsFile={p}") }
}

fn ssh_opts(keys: bool) -> Vec<String> {
    let kh = known_hosts_opt(&crate::store::path("known_hosts"));
    let auth: &[&str] = if keys { &["-o", "BatchMode=yes"] }
                        else { &["-o", "PubkeyAuthentication=no", "-o", "PreferredAuthentications=password,keyboard-interactive"] };
    let mut v: Vec<&str> = vec!["-o", "StrictHostKeyChecking=accept-new", "-o", &kh];
    v.extend(auth);
    v.extend(["-o", "ServerAliveInterval=30", "-o", "ConnectTimeout=20"]);
    v.into_iter().map(String::from).collect()
}

/// A shell line run on the device: sh on a remote device and on this machine (PowerShell on Windows).
pub fn line(dev: &Value, script: &str) -> Command {
    if is_local(dev) {
        if cfg!(windows) { return powershell(script, false); }
        let mut c = Command::new("/bin/sh");
        c.args(["-c", script]);
        return c;
    }
    let port = dev["port"].as_u64().unwrap_or(22).to_string();
    let who = format!("{}@{}", dev["user"].as_str().unwrap_or(""), dev["host"].as_str().unwrap_or(""));
    if uses_keys(dev) {
        let mut c = Command::new("ssh");
        c.args(["-p", &port]).args(ssh_opts(true)).arg(&who).arg(script);
        return c;
    }
    let mut c = Command::new("sshpass");
    c.arg("-e").arg("ssh").args(["-p", &port]).args(ssh_opts(false)).arg(&who).arg(script);
    c.env("SSHPASS", password(dev));
    c
}

// ---- PowerShell (Windows) ----

/// PowerShell 7 (pwsh) when installed, else Windows PowerShell; EXTERIOR_POWERSHELL picks one.
pub fn powershell_exe() -> String {
    if let Some(p) = std::env::var("EXTERIOR_POWERSHELL").ok().filter(|p| !p.is_empty()) { return p; }
    static P: OnceLock<String> = OnceLock::new();
    P.get_or_init(|| if on_path("pwsh") { "pwsh".into() } else { "powershell".into() }).clone()
}

/// A PowerShell script, handed over encoded (so no quoting of the script itself is involved). `profile`: load the
/// user's profile, where his own functions are (like `$SHELL -i`). Script files may run (for this process only).
pub fn powershell(script: &str, profile: bool) -> Command {
    let mut c = Command::new(powershell_exe());
    c.args(["-NoLogo", "-NonInteractive", "-ExecutionPolicy", "Bypass"]);
    if !profile { c.arg("-NoProfile"); }
    c.arg("-EncodedCommand").arg(ps_encode(&format!("$ProgressPreference = 'SilentlyContinue'\n{script}")));
    c
}

/// What -EncodedCommand takes: base64 of the UTF-16LE text.
pub fn ps_encode(script: &str) -> String {
    let b: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    crate::brief::base64(&b)
}

/// A PowerShell string literal: single quotes (PowerShell takes the typographic ones as quotes too) doubled.
pub fn ps_quote(s: &str) -> String {
    let mut o = String::from("'");
    for c in s.chars() { if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') { o.push(c); } o.push(c); }
    o.push('\'');
    o
}

/// The PowerShell script that starts an agent on Windows. The arguments come in the environment (EXTERIOR_ARGS, a JSON
/// list), so nothing in them is ever parsed by a shell. PowerShell before 7.3 passes quotes inside arguments to a
/// program unescaped; they are escaped first there.
pub fn ps_agent_script(command: &str) -> String {
    format!(r#"try {{ [Console]::OutputEncoding = [Text.Encoding]::UTF8 }} catch {{}}
$OutputEncoding = [Text.Encoding]::UTF8
$j = @()
if ($env:EXTERIOR_ARGS) {{ $j = ConvertFrom-Json $env:EXTERIOR_ARGS }}
Remove-Item Env:EXTERIOR_ARGS -ErrorAction SilentlyContinue
$a = @($j | ForEach-Object {{ [string]$_ }})
if ($PSVersionTable.PSVersion -lt [version]'7.3') {{
  $a = @($a | ForEach-Object {{ $s = $_ -replace '(\\*)"', '$1$1\"'; if ($s -match '\s') {{ $s = $s -replace '(\\+)$', '$1$1' }}; $s }})
}}
{command} @a
exit $LASTEXITCODE"#)
}

/// PATH with `front` put first (`:` between entries, `;` on Windows).
pub fn path_with(front: &str, cur: Option<std::ffi::OsString>) -> std::ffi::OsString {
    let rest: Vec<PathBuf> = cur.map(|c| std::env::split_paths(&c).collect()).unwrap_or_default();
    std::env::join_paths(std::iter::once(PathBuf::from(front)).chain(rest)).unwrap_or_else(|_| front.into())
}

/// An agent command: `cd dir && <env> $SHELL -ic '<command> <args>'`. On Windows (this machine): PowerShell with the
/// user's profile, in `dir`, with the environment set directly.
pub fn agent(dev: &Value, dir: &str, env: &[(&str, String)], command: &str, args: &[String]) -> Command {
    if is_local(dev) && cfg!(windows) {
        let _ = std::fs::create_dir_all(dir);
        let mut c = powershell(&ps_agent_script(command), true);
        c.current_dir(dir);
        for (k, v) in env {
            if *k == "PATH" { c.env("PATH", path_with(&v.replace('/', "\\"), std::env::var_os("PATH"))); } else { c.env(k, v); }
        }
        c.env("EXTERIOR_ARGS", serde_json::to_string(args).unwrap_or_default());
        return c;
    }
    let mut inner = command.to_string();
    for a in args { inner.push(' '); inner.push_str(&quote(a)); }
    let mut s = format!("mkdir -p {d} && cd {d} && exec env", d = quote(dir));
    // PATH is put in front of the device's own, not instead of it
    for (k, v) in env { if *k == "PATH" { s.push_str(&format!(" PATH={}:\"$PATH\"", quote(v))); } else { s.push_str(&format!(" {k}={}", quote(v))); } }
    s.push_str(&format!(" \"${{SHELL:-/bin/sh}}\" -ic {}", quote(&inner)));
    line(dev, &s)
}

// ---- running and waiting ----

/// Run a short script on the device and wait (with a time limit).
pub fn run(dev: &Value, script: &str, secs: u64) -> Result<String, String> { exec(line(dev, script), secs) }

/// Run a command and wait (with a time limit): Ok(stdout) if it succeeded, else Err(stderr, or the exit code and stdout).
pub fn exec(mut c: Command, secs: u64) -> Result<String, String> {
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = c.spawn().map_err(|e| crate::text::exec_failed(&e.to_string()))?;
    let out = wait(child, secs)?;
    let so = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let se = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if out.status.success() { Ok(so) } else { Err(if se.is_empty() { crate::text::exit_code(out.status.code(), &so) } else { se }) }
}

/// Like `exec`, with stderr taken as part of the output (`2>&1`).
pub fn exec_all(mut c: Command, secs: u64) -> Result<String, String> {
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = c.spawn().map_err(|e| crate::text::exec_failed(&e.to_string()))?;
    let out = wait(child, secs)?;
    let all = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)).trim().to_string();
    if out.status.success() { Ok(all) } else { Err(crate::text::exit_code(out.status.code(), &all)) }
}

/// Is the device there? (This machine always is.)
pub fn alive(dev: &Value) -> bool { is_local(dev) || run(dev, "true", 30).is_ok() }

/// End a process exterior started, with what it started in turn on Windows (there the agent runs under PowerShell,
/// and ending PowerShell alone would leave it running).
pub fn kill(child: &mut Child) {
    if cfg!(windows) {
        let _ = Command::new("taskkill").args(["/T", "/F", "/PID", &child.id().to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
    let _ = child.kill();
}

pub fn wait(mut child: Child, secs: u64) -> Result<Output, String> {
    // read the output while waiting: a child whose output fills the pipe would otherwise block until the time limit
    fn drain<R: std::io::Read + Send + 'static>(r: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || { let mut v = Vec::new(); if let Some(mut r) = r { let _ = r.read_to_end(&mut v); } v })
    }
    let (so, se) = (drain(child.stdout.take()), drain(child.stderr.take()));
    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if start.elapsed().as_secs() >= secs => { kill(&mut child); let _ = child.wait(); return Err(crate::text::timed_out(secs)); }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(e) => return Err(e.to_string()),
        }
    };
    Ok(Output { status, stdout: so.join().unwrap_or_default(), stderr: se.join().unwrap_or_default() })
}

/// Check a device for the 设备 page: its name, and that its root folder can be written.
pub fn check(dev: &Value, root: &str) -> Result<String, String> {
    if !is_local(dev) { return run(dev, &format!("uname -sn; mkdir -p {r} && test -w {r} && echo 根目录可写", r = quote(root)), 40); }
    if root.is_empty() { return Err(crate::text::NO_ROOT.into()); }
    let name = if cfg!(windows) { format!("Windows {}", std::env::var("COMPUTERNAME").unwrap_or_default()) }
               else { let mut c = Command::new("uname"); c.arg("-sn"); exec(c, 10).unwrap_or_else(|_| std::env::consts::OS.into()) };
    let probe = Path::new(root).join(".exterior-write-test");
    std::fs::create_dir_all(root).and_then(|_| std::fs::write(&probe, b"")).map_err(|e| format!("{root}: {e}"))?;
    let _ = std::fs::remove_file(&probe);
    Ok(format!("{name}\n根目录可写"))
}

// ---- files ----

/// The user's home folder on this machine.
pub fn home() -> PathBuf {
    let v = if cfg!(windows) { std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) } else { std::env::var_os("HOME") };
    v.map(PathBuf::from).unwrap_or_default()
}

/// A path on this machine the way an agent or a setting writes it: "~/…" in the home folder, absolute, or relative to `dir`.
pub fn local_path(path: &str, dir: &str) -> PathBuf {
    if let Some(r) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) { return home().join(r); }
    if path.starts_with('/') || Path::new(path).is_absolute() || dir.is_empty() { return PathBuf::from(path); }
    PathBuf::from(format!("{}/{path}", dir.trim_end_matches('/')))
}

fn write_local(path: &str, data: &[u8], mode: Option<&str>) -> Result<(), String> {
    let p = Path::new(path);
    if let Some(d) = p.parent().filter(|d| !d.as_os_str().is_empty()) { std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?; }
    std::fs::write(p, data).map_err(|e| format!("{path}: {e}"))?;
    #[cfg(unix)]
    if let Some(m) = mode.and_then(|m| u32::from_str_radix(m, 8).ok()) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).map_err(|e| format!("{path}: {e}"))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok(())
}

/// Write a file on the device (always replaces it).
pub fn write(dev: &Value, path: &str, body: &str, mode: &str) -> Result<String, String> {
    if is_local(dev) { return write_local(path, body.as_bytes(), Some(mode)).map(|_| String::new()); }
    let dir = path.rsplit_once('/').map(|x| x.0).unwrap_or("/");
    run(dev, &format!("mkdir -p {d} && printf '%s' {b} > {p} && chmod {mode} {p}", d = quote(dir), p = quote(path), b = quote(body)), 30)
}

/// Write bytes to a file on the device (through stdin, so any size and any content).
pub fn write_bytes(dev: &Value, path: &str, data: &[u8]) -> Result<(), String> {
    if is_local(dev) { return write_local(path, data, None); }
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

/// A file an agent pointed to ("~/…", absolute, or relative to its folder `dir`), at most `max` + 1 bytes.
pub fn read_file(dev: &Value, path: &str, dir: &str, max: usize) -> Result<Vec<u8>, String> {
    if is_local(dev) {
        use std::io::Read;
        let p = local_path(path, dir);
        if !p.is_file() { return Err(crate::text::FILE_MISSING.into()); }
        let mut v = Vec::new();
        std::fs::File::open(&p).and_then(|f| f.take(max as u64 + 1).read_to_end(&mut v)).map_err(|e| e.to_string())?;
        return Ok(v);
    }
    let word = if let Some(r) = path.strip_prefix("~/") { format!("\"$HOME\"/{}", quote(r)) }
               else if path.starts_with('/') { quote(path) }
               else { quote(&format!("{}/{path}", dir.trim_end_matches('/'))) };
    read_bytes(dev, &word, max)
}

/// A text file on this machine ("~/…" is in the home folder).
pub fn read_local(path: &str) -> Result<String, String> {
    let p = local_path(path, "");
    std::fs::read_to_string(&p).map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { crate::text::FILE_MISSING.to_string() } else { e.to_string() })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn big_file_reads_at_once() {
        // more than a pipe holds: must not hang until the time limit
        let p = std::env::temp_dir().join("exterior-big-test.bin");
        std::fs::write(&p, vec![7u8; 400_000]).unwrap();
        let t = std::time::Instant::now();
        let local = json!({"host": "local"});
        let b = super::read_file(&local, p.to_str().unwrap(), "", 1_000_000).unwrap();
        assert_eq!(b.len(), 400_000);
        assert_eq!(super::read_file(&local, p.to_str().unwrap(), "", 1000).unwrap().len(), 1001);
        let (dir, name) = (p.parent().unwrap().to_str().unwrap(), p.file_name().unwrap().to_str().unwrap());
        assert_eq!(super::read_file(&local, name, dir, 1_000_000).unwrap().len(), 400_000);
        assert!(t.elapsed().as_secs() < 5);
        assert_eq!(super::read_file(&local, "/nonexistent/x", "", 10).unwrap_err(), crate::text::FILE_MISSING);
        // through the shell, the way a remote device is read
        #[cfg(unix)]
        {
            let b = super::read_bytes(&local, &super::quote(p.to_str().unwrap()), 1_000_000).unwrap();
            assert_eq!(b.len(), 400_000);
            assert_eq!(super::read_bytes(&local, "/nonexistent/x", 10).unwrap_err(), crate::text::FILE_MISSING);
        }
    }

    #[test]
    fn local_files() {
        let d = std::env::temp_dir().join(format!("exterior-files-{}", std::process::id()));
        let local = json!({"host": "local"});
        let f = d.join("a b").join("k");
        super::write(&local, f.to_str().unwrap(), "秘密'\"", "600").unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "秘密'\"");
        #[cfg(unix)]
        { use std::os::unix::fs::PermissionsExt; assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600); }
        super::write_bytes(&local, d.join("files").join("x.bin").to_str().unwrap(), &[0, 1, 2]).unwrap();
        assert_eq!(std::fs::read(d.join("files").join("x.bin")).unwrap(), vec![0, 1, 2]);
        assert_eq!(super::read_local(f.to_str().unwrap()).unwrap(), "秘密'\"");
        assert!(super::check(&local, d.join("root").to_str().unwrap()).unwrap().ends_with("根目录可写"));
        assert!(super::alive(&local));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn paths_and_quoting() {
        assert_eq!(super::local_path("~/x/y", "/d"), super::home().join("x/y"));
        assert_eq!(super::local_path("/abs/f", "/d"), std::path::PathBuf::from("/abs/f"));
        assert_eq!(super::local_path("f.png", "/d/"), std::path::PathBuf::from("/d/f.png"));
        assert_eq!(super::ps_quote("it's ‘x’"), "'it''s ‘‘x’’'");
        // "AB" in UTF-16LE is 41 00 42 00
        assert_eq!(super::ps_encode("AB"), "QQBCAA==");
        assert_eq!(super::known_hosts_opt("/lzcapp/var/known_hosts"), "UserKnownHostsFile=/lzcapp/var/known_hosts");
        let sep = if cfg!(windows) { ";" } else { ":" };
        let joined = super::path_with("/front", Some(std::env::join_paths(["/a", "/b"]).unwrap()));
        assert_eq!(joined.to_str().unwrap(), format!("/front{sep}/a{sep}/b"));
        #[cfg(not(windows))]
        assert_eq!(super::known_hosts_opt("/Users/a/Library/Application Support/exterior/known_hosts"),
                   "UserKnownHostsFile=\"/Users/a/Library/Application Support/exterior/known_hosts\"");
        #[cfg(windows)]
        assert_eq!(super::known_hosts_opt(r"C:\Users\a b\AppData\Roaming\exterior/known_hosts"), "UserKnownHostsFile=\"C:/Users/a b/AppData/Roaming/exterior/known_hosts\"");
    }

    /// The ssh command for a remote device: password through sshpass (as on Lazycat), else keys with BatchMode.
    #[test]
    fn ssh_command() {
        let _g = crate::store::TEST.lock().unwrap_or_else(|e| e.into_inner());
        let args = |c: &std::process::Command| c.get_args().map(|a| a.to_string_lossy().to_string()).collect::<Vec<_>>();
        let c = super::line(&json!({"host": "h", "user": "u", "port": 2222}), "echo hi");
        assert_eq!(c.get_program(), "ssh");
        let a = args(&c);
        assert!(a.contains(&"BatchMode=yes".to_string()) && !a.contains(&"PubkeyAuthentication=no".to_string()), "{a:?}");
        assert_eq!(&a[..2], ["-p", "2222"]);
        assert_eq!(&a[a.len() - 2..], ["u@h", "echo hi"]);
        // with a password (Lazycat): the same options as always
        let kh = format!("UserKnownHostsFile={}", crate::store::path("known_hosts"));
        if !kh.contains(' ') && !cfg!(windows) {
            assert_eq!(super::ssh_opts(false), ["-o", "StrictHostKeyChecking=accept-new", "-o", &kh, "-o", "PubkeyAuthentication=no",
                "-o", "PreferredAuthentications=password,keyboard-interactive", "-o", "ServerAliveInterval=30", "-o", "ConnectTimeout=20"]);
        }
    }

    /// Agents on Windows: arguments with quotes, spaces, new lines, backslashes and non-ASCII text arrive unchanged,
    /// in the right folder, with the environment and PATH set; with both Windows PowerShell and PowerShell 7.
    #[cfg(windows)]
    #[test]
    fn windows_agent_arguments() {
        let d = std::env::temp_dir().join(format!("exterior-agent-{}", std::process::id()));
        let dir = d.join("topic dir");
        let args: Vec<String> = ["plain", "two words", "quote\"inside", "\"wrapped\"", "multi\nline", "中文 text", r"C:\dir with space\", r"back\\slash", "$HOME %PATH% `t 'single'", "end\\"]
            .iter().map(|s| s.to_string()).collect();
        let py = "python -c \"import sys,os,json; print(json.dumps([sys.argv[1:], os.getcwd(), os.environ.get('EXTERIOR_TOPIC'), os.environ['PATH'].split(';')[0], 'EXTERIOR_ARGS' in os.environ]))\"";
        for ps in ["powershell", "pwsh"] {
            if !super::on_path(ps) { continue; }
            std::env::set_var("EXTERIOR_POWERSHELL", ps);
            let c = super::agent(&json!({"host": "local"}), dir.to_str().unwrap(), &[("EXTERIOR_TOPIC", "t:1".into()), ("PATH", r"C:\x\bin".into())], py, &args);
            let out = super::exec(c, 60).unwrap();
            let v: serde_json::Value = serde_json::from_str(out.lines().last().unwrap()).unwrap_or_else(|_| panic!("{ps}: {out}"));
            assert_eq!(v[0], json!(args), "{ps}");
            // the temporary folder may come back in its short (8.3) form
            assert!(v[1].as_str().unwrap().to_lowercase().ends_with(&format!("exterior-agent-{}\\topic dir", std::process::id())), "{ps}: {}", v[1]);
            assert_eq!(v[2], "t:1");
            assert_eq!(v[3], r"C:\x\bin");
            assert_eq!(v[4], false);
        }
        std::env::remove_var("EXTERIOR_POWERSHELL");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(windows)]
    #[test]
    fn windows_short_scripts() {
        let local = json!({"host": "local"});
        assert_eq!(super::run(&local, "Write-Output ('a' + 'b')", 60).unwrap(), "ab");
        assert!(super::run(&local, "exit 4", 60).is_err());
    }
}
