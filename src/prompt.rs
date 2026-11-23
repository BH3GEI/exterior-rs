//! What exterior tells an agent when it starts it. Passed on the command line (claude and pi:
//! --append-system-prompt; codex: developerInstructions), so nothing is written into the topic folders.
//! The text is not in this program: it is a file in knowledge on the device (see the session_prompt and
//! global_prompt settings), read at every start. exterior only fills in the placeholders.
use crate::agents::Kind;
use crate::{host, params, text};
use serde_json::Value;

/// Where knowledge lives on a device.
pub fn infra(dev: &Value) -> String {
    dev["infra"].as_str().filter(|s| !s.is_empty()).map(String::from)
        .unwrap_or_else(|| format!("{}/{}", root(dev), params::get("knowledge_dir"))).trim_end_matches('/').to_string()
}

pub fn root(dev: &Value) -> String { dev["root"].as_str().unwrap_or("").trim_end_matches('/').to_string() }

/// The folder of the global topic on a device.
pub fn global_dir(dev: &Value) -> String { format!("{}/{}", root(dev), params::get("global_dir")) }

/// The full path of a text file in knowledge named by a setting ("session_prompt", "global_prompt", "test_prompt").
pub fn file(dev: &Value, which: &str) -> String { format!("{}/{}", infra(dev), params::get(which).trim_start_matches('/')) }

/// Read a template from the device, without its <!-- --> comments.
pub fn read(dev: &Value, which: &str) -> Result<String, String> {
    let path = file(dev, which);
    let raw = if host::is_local(dev) { host::read_local(&path) } else { host::run(dev, &format!("cat {}", host::quote(&path)), 30) }
        .map_err(|e| text::prompt_unreadable(&path, &e))?;
    let mut out = String::new();
    let mut rest = raw.as_str();
    while let Some(i) = rest.find("<!--") {
        out.push_str(&rest[..i]);
        rest = rest[i..].find("-->").map(|j| &rest[i + j + 3..]).unwrap_or("");
    }
    out.push_str(rest);
    Ok(out.trim().to_string())
}

/// Example resume lines, one per kind of agent the device has, written the way each agent writes them.
fn resume_examples(dev: &Value) -> String {
    Kind::ALL.iter().filter_map(|k| {
        let row = dev["agents"].as_array().into_iter().flatten().find(|a| a["kind"] == k.name())?;
        Some(format!("`{}`", crate::agents::resume_line(*k, row["name"].as_str()?, crate::text::ID_PLACEHOLDER)))
    }).collect::<Vec<_>>().join("、")
}

pub fn session_text(dev: &Value, name: &str, tid: &str, dir: &str, global: bool) -> Result<String, String> {
    let brief = crate::store::config()["brief"]["url"].as_str().unwrap_or("").trim_end_matches('/').to_string();
    let mut t = read(dev, "session_prompt")?;
    if global { t = format!("{t}\n\n{}", read(dev, "global_prompt")?); }
    let vars = [("{name}", name.to_string()), ("{tid}", tid.to_string()), ("{dir}", dir.to_string()), ("{brief}", brief),
                ("{root}", root(dev)), ("{infra}", infra(dev)), ("{user}", params::get("user_name")), ("{mark}", params::get("relay_mark")),
                ("{global_topic}", params::get("global_topic")), ("{resume_examples}", resume_examples(dev))];
    for (k, v) in vars { t = t.replace(k, &v); }
    Ok(t + "\n")
}

/// Where `exterior send` lives on a device (put in front of the agents' PATH).
pub fn bin_dir(dev: &Value) -> String { format!("{}/.exterior/bin", root(dev)) }

/// The address a device uses to reach exterior: the self_url setting; on Lazycat the app's domain; elsewhere, for
/// this machine, the address the interface listens on. Empty when there is none (a remote device, not on Lazycat).
pub fn self_url(dev: &Value) -> String {
    url_for(&params::get("self_url"), std::env::var("LAZYCAT_APP_DOMAIN").ok().as_deref(), crate::store::on_lazycat(), host::is_local(dev), crate::web::listening())
}

pub fn url_for(set: &str, domain: Option<&str>, lazycat: bool, local: bool, listen: Option<std::net::SocketAddr>) -> String {
    if !set.is_empty() { return set.trim_end_matches('/').to_string(); }
    if let Some(d) = domain { return format!("https://{d}"); }
    if lazycat || !local { return String::new(); }
    listen.map(|a| {
        let ip = match a.ip() { std::net::IpAddr::V4(i) if i.is_unspecified() => std::net::Ipv4Addr::LOCALHOST.into(),
                                std::net::IpAddr::V6(i) if i.is_unspecified() => std::net::Ipv6Addr::LOCALHOST.into(), ip => ip };
        format!("http://{}", std::net::SocketAddr::new(ip, a.port()))
    }).unwrap_or_default()
}

/// The key `exterior send` presents (made once, kept in config.json).
pub fn send_key() -> String {
    let mut c = crate::store::config();
    if let Some(k) = c["send_key"].as_str().filter(|k| !k.is_empty()) { return k.to_string(); }
    let mut b = [0u8; 16];
    let _ = getrandom::getrandom(&mut b);
    let k: String = b.iter().map(|x| format!("{x:02x}")).collect();
    c["send_key"] = serde_json::json!(k);
    crate::store::set_config(&c);
    k
}

/// `exterior send <话题> <内容>`: hands a message for another topic to exterior, which signs it and sends it.
fn send_script(dev: &Value) -> String { send_sh(dev, host::is_local(dev) && cfg!(windows)) }

/// The sh form. `windows`: for Git Bash on this Windows machine, where curl is a Windows program that receives its
/// command line in the local code page, so Chinese in it arrives garbled. There nothing but ASCII goes on curl's
/// command line: the topic name is percent-encoded byte by byte by the script, and the text goes in through stdin.
fn send_sh(dev: &Value, windows: bool) -> String {
    let (key, url) = (host::quote(&format!("{}/.exterior-key", root(dev))), host::quote(&format!("{}/api/send", self_url(dev))));
    let post = if windows {
        format!(r#"enc() {{ printf '%s' "$1" | od -An -v -tx1 | tr -d ' \n' | sed 's/../%&/g'; }}
printf '%s' "$text" | curl -sS --fail-with-body -H "Authorization: Bearer $(cat {key})" --data "cmd=$cmd" --data "from=$(enc "$EXTERIOR_TOPIC")" --data "to=$(enc "$to")" --data-urlencode "text@-" {url}"#)
    } else {
        format!(r#"exec curl -sS --fail-with-body -H "Authorization: Bearer $(cat {key})" --data-urlencode "cmd=$cmd" --data-urlencode "from=$EXTERIOR_TOPIC" --data-urlencode "to=$to" --data-urlencode "text=$text" {url}"#)
    };
    format!(r#"#!/bin/sh
# exterior send <话题名> <内容>：经 exterior 给其他话题发消息，自动署名。内容里写 ![说明](路径) 可附图片或文件。
# exterior watch <会话ID> / exterior unwatch <会话ID>：让 exterior 盯住一个会话（如微信群），新消息自动交给你。
# exterior watch --all [来源]：盯某个来源（如 wechat）的全部会话，含以后新出现的；不写来源就是全部来源。exterior watch 或 --list：列出在盯什么。
# exterior usage [5h|24h|7d]：各订阅当前的额度窗口，以及这段时间各话题、各订阅用了多少。
# exterior topic open <话题名>：开一个新话题交给另一个 agent（返回 chat）；exterior topic close <话题名或 chat>：任务完成后关闭。
# exterior draft <会话ID或名字> <内容>：写一条 brief 草稿等 Yao 处理；exterior draft list；exterior draft rm <编号>。
# 由 exterior 安装，每次启动时覆盖。
usage() {{ echo '用法：exterior send <话题名> <内容> | exterior topic open|close <话题名> | exterior draft <会话> <内容> | exterior draft list | exterior draft rm <编号> | exterior watch [会话ID | --all [来源] | --list] | exterior unwatch <会话ID | --all [来源]> | exterior usage [5h|24h|7d]' >&2; exit 2; }}
case "$1" in
  send) [ $# -ge 3 ] || usage; to=$2; shift 2; cmd=send; text="$*" ;;
  watch) [ $# -le 3 ] || usage; cmd=watch; shift; to="$*"; text= ;;
  unwatch) [ $# -ge 2 ] && [ $# -le 3 ] || usage; cmd=unwatch; shift; to="$*"; text= ;;
  usage) [ $# -le 2 ] || usage; cmd=usage; to=${{2:-}}; text= ;;
  topic) [ $# -ge 3 ] || usage; cmd=topic; to=$2; shift 2; text="$*" ;;
  draft) [ $# -ge 2 ] || usage; cmd=draft; to=$2; shift 2; text="$*" ;;
  *) usage ;;
esac
{post}
"#)
}

/// The same for PowerShell on Windows (`exterior.ps1`; `exterior.cmd` runs it from cmd). Agents running in Git Bash
/// (claude) use the sh form above.
fn send_ps1(dev: &Value) -> String {
    // with a byte order mark: Windows PowerShell reads a file without one in the local code page
    format!("\u{feff}{}", format!(r#"# exterior send <话题名> <内容>：经 exterior 给其他话题发消息，自动署名。内容里写 ![说明](路径) 可附图片或文件。
# 由 exterior 安装，每次启动时覆盖。
$usage = '用法：exterior send <话题名> <内容> | exterior topic open|close <话题名> | exterior draft <会话> <内容> | exterior draft list | exterior draft rm <编号> | exterior watch [会话ID | --all [来源] | --list] | exterior unwatch <会话ID | --all [来源]> | exterior usage [5h|24h|7d]'
$cmd = [string]$args[0]; $to = ''; $text = ''
if ($cmd -eq 'send' -and $args.Count -ge 3) {{ $to = [string]$args[1]; $text = ($args[2..($args.Count - 1)] | ForEach-Object {{ [string]$_ }}) -join ' ' }}
elseif ($cmd -eq 'watch' -and $args.Count -le 3) {{ if ($args.Count -ge 2) {{ $to = ($args[1..($args.Count - 1)] | ForEach-Object {{ [string]$_ }}) -join ' ' }} }}
elseif ($cmd -eq 'unwatch' -and $args.Count -ge 2 -and $args.Count -le 3) {{ $to = ($args[1..($args.Count - 1)] | ForEach-Object {{ [string]$_ }}) -join ' ' }}
elseif ($cmd -eq 'usage' -and $args.Count -le 2) {{ if ($args.Count -eq 2) {{ $to = [string]$args[1] }} }}
elseif (($cmd -eq 'topic' -and $args.Count -ge 3) -or ($cmd -eq 'draft' -and $args.Count -ge 2)) {{ $to = [string]$args[1]; if ($args.Count -ge 3) {{ $text = ($args[2..($args.Count - 1)] | ForEach-Object {{ [string]$_ }}) -join ' ' }} }}
else {{ [Console]::Error.WriteLine($usage); exit 2 }}
$key = (Get-Content -Raw -Encoding UTF8 -LiteralPath {key}).Trim()
$e = {{ param($s) [Uri]::EscapeDataString([string]$s) }}
$body = 'cmd=' + (& $e $cmd) + '&from=' + (& $e $env:EXTERIOR_TOPIC) + '&to=' + (& $e $to) + '&text=' + (& $e $text)
try {{ [Console]::OutputEncoding = [Text.Encoding]::UTF8 }} catch {{}}
try {{
  $r = Invoke-WebRequest -UseBasicParsing -Method Post -Uri {url} -Headers @{{ Authorization = "Bearer $key" }} -ContentType 'application/x-www-form-urlencoded; charset=utf-8' -Body ([Text.Encoding]::UTF8.GetBytes($body))
  [Console]::Out.Write([Text.Encoding]::UTF8.GetString($r.RawContentStream.ToArray()))
}} catch {{
  $m = $_.ErrorDetails.Message; if (-not $m) {{ $m = $_.Exception.Message }}
  [Console]::Error.WriteLine($m); exit 1
}}
"#, key = host::ps_quote(&format!("{}/.exterior-key", root(dev))), url = host::ps_quote(&format!("{}/api/send", self_url(dev)))))
}

const SEND_CMD: &str = "@echo off\r\nrem exterior send: runs exterior.ps1 next to this file (installed by exterior)\r\npowershell -NoProfile -ExecutionPolicy Bypass -File \"%~dp0exterior.ps1\" %*\r\n";

/// Put the brief key, the exterior key and `exterior send` on a device.
pub fn install(dev: &Value) -> Result<(), String> {
    let root = root(dev);
    if root.is_empty() { return Err(text::NO_ROOT.into()); }
    let k = crate::brief::key();
    if !k.is_empty() { host::write(dev, &format!("{root}/.brief-key"), &k, "600")?; }
    host::write(dev, &format!("{root}/.exterior-key"), &send_key(), "600")?;
    host::write(dev, &format!("{}/exterior", bin_dir(dev)), &send_script(dev), "755")?;
    if host::is_local(dev) && cfg!(windows) {
        host::write(dev, &format!("{}/exterior.ps1", bin_dir(dev)), &send_ps1(dev), "755")?;
        host::write(dev, &format!("{}/exterior.cmd", bin_dir(dev)), SEND_CMD, "755")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::url_for;

    /// A form body as name → value.
    #[cfg(unix)]
    fn form(b: &str) -> std::collections::HashMap<String, String> {
        let dec = |s: &str| {
            let (s, mut out, mut i) = (s.as_bytes(), Vec::new(), 0);
            while i < s.len() {
                match s[i] {
                    b'+' => out.push(b' '),
                    b'%' => { out.push(u8::from_str_radix(std::str::from_utf8(&s[i + 1..i + 3]).unwrap(), 16).unwrap()); i += 2; }
                    c => out.push(c),
                }
                i += 1;
            }
            String::from_utf8(out).unwrap()
        };
        let mut f: std::collections::HashMap<String, String> = b.split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (dec(k), dec(v))).collect();
        // a field curl leaves out (empty stdin) reads as empty on exterior's side
        for k in ["cmd", "from", "to", "text"] { f.entry(k.to_string()).or_default(); }
        f
    }

    #[cfg(unix)]
    #[test]
    fn send_script_commands() {
        let dir = std::env::temp_dir().join(format!("exterior-script-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join(".exterior-key"), "k").unwrap();
        // curl as Git Bash on Windows runs it: what it is given on its command line is written down
        let curl = String::from_utf8(std::process::Command::new("sh").args(["-c", "command -v curl"]).output().unwrap().stdout).unwrap();
        std::fs::write(dir.join("bin/curl"), format!("#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$ARGS_LOG\"\nexec {} \"$@\"\n", curl.trim())).unwrap();
        std::process::Command::new("chmod").args(["755", dir.join("bin/curl").to_str().unwrap()]).status().unwrap();
        let srv = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}/api/send", srv.server_addr().to_ip().unwrap().port());
        let got = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        { let got = got.clone(); std::thread::spawn(move || for mut r in srv.incoming_requests() {
            let mut b = String::new(); let _ = std::io::Read::read_to_string(r.as_reader(), &mut b);
            got.lock().unwrap().push(b); let _ = r.respond(tiny_http::Response::from_string("ok"));
        }); }
        let dev = serde_json::json!({"root": dir.to_str().unwrap(), "host": "local"});
        // the form used everywhere, and the one for Git Bash on Windows
        for windows in [false, true] {
            got.lock().unwrap().clear();
            let log = dir.join(format!("args-{windows}"));
            let script = super::send_sh(&dev, windows);
            let start = script.rfind(" '").unwrap();
            let script = format!("{} '{url}'\n", &script[..start]);
            let f = dir.join("exterior");
            std::fs::write(&f, script).unwrap();
            let path = format!("{}:{}", dir.join("bin").display(), std::env::var("PATH").unwrap_or_default());
            let run = |args: &[&str]| std::process::Command::new("sh").arg(&f).args(args).env("EXTERIOR_TOPIC", "tgbot:1").env("PATH", &path).env("ARGS_LOG", &log)
                .output().unwrap().status.code();
            assert_eq!(run(&["watch", "wx@chatroom"]), Some(0));
            assert_eq!(run(&["send", "全局", "a", "b"]), Some(0));
            assert_eq!(run(&["unwatch"]), Some(2));
            assert_eq!(run(&["bogus"]), Some(2));
            assert_eq!(run(&["send", "exterior 开发", "中文 & 符号 = %20 + \"引号\"\n第二行", "'单引号'"]), Some(0));
            assert_eq!(run(&["watch"]), Some(0));
            assert_eq!(run(&["usage", "7d"]), Some(0));
            assert_eq!(run(&["usage", "7d", "x"]), Some(2));
            assert_eq!(run(&["watch", "--all", "wechat"]), Some(0));
            assert_eq!(run(&["topic", "open", "新任务"]), Some(0));
            assert_eq!(run(&["draft", "list"]), Some(0));
            assert_eq!(run(&["draft", "123@chatroom", "你好", "明天见"]), Some(0));
            assert_eq!(run(&["topic", "open"]), Some(2));
            let g: Vec<_> = got.lock().unwrap().iter().map(|b| form(b)).collect();
            let want = |cmd: &str, to: &str, text: &str| [("cmd", cmd), ("from", "tgbot:1"), ("to", to), ("text", text)].map(|(k, v)| (k.to_string(), v.to_string())).into_iter().collect::<std::collections::HashMap<_, _>>();
            assert_eq!(g, vec![want("watch", "wx@chatroom", ""), want("send", "全局", "a b"),
                               want("send", "exterior 开发", "中文 & 符号 = %20 + \"引号\"\n第二行 '单引号'"), want("watch", "", ""), want("usage", "7d", ""),
                               want("watch", "--all wechat", ""), want("topic", "open", "新任务"), want("draft", "list", ""), want("draft", "123@chatroom", "你好 明天见")], "windows {windows}");
            // on Windows nothing but ASCII may reach curl's command line
            let args = std::fs::read_to_string(&log).unwrap();
            assert_eq!(args.is_ascii(), windows, "{args}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn address_for_devices() {
        let any: std::net::SocketAddr = "0.0.0.0:3456".parse().unwrap();
        let one: std::net::SocketAddr = "192.168.1.5:3000".parse().unwrap();
        // Lazycat: the setting, else the app's domain, for every device
        assert_eq!(url_for("https://x.example/", Some("ext.box.heiyu.space"), true, false, Some(any)), "https://x.example");
        assert_eq!(url_for("", Some("ext.box.heiyu.space"), true, false, Some(any)), "https://ext.box.heiyu.space");
        assert_eq!(url_for("", None, true, true, Some(any)), "");
        // elsewhere: this machine reaches the interface directly; a remote device only through the setting
        assert_eq!(url_for("", None, false, true, Some(any)), "http://127.0.0.1:3456");
        assert_eq!(url_for("", None, false, true, Some(one)), "http://192.168.1.5:3000");
        assert_eq!(url_for("", None, false, true, Some("[::]:80".parse().unwrap())), "http://[::1]:80");
        assert_eq!(url_for("", None, false, false, Some(any)), "");
        assert_eq!(url_for("http://pc.lan:3000", None, false, false, Some(any)), "http://pc.lan:3000");
    }
}
