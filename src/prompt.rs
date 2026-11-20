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
fn send_script(dev: &Value) -> String {
    format!(r#"#!/bin/sh
# exterior send <话题名> <内容>：经 exterior 给其他话题发消息，自动署名。内容里写 ![说明](路径) 可附图片或文件。
# 由 exterior 安装，每次启动时覆盖。
if [ "$1" != send ] || [ $# -lt 3 ]; then echo '用法：exterior send <话题名> <内容>' >&2; exit 2; fi
to=$2; shift 2
exec curl -sS --fail-with-body -H "Authorization: Bearer $(cat {key})" --data-urlencode "from=$EXTERIOR_TOPIC" --data-urlencode "to=$to" --data-urlencode "text=$*" {url}
"#, key = host::quote(&format!("{}/.exterior-key", root(dev))), url = host::quote(&format!("{}/api/send", self_url(dev))))
}

/// The same for PowerShell on Windows (`exterior.ps1`; `exterior.cmd` runs it from cmd). Agents running in Git Bash
/// (claude) use the sh form above.
fn send_ps1(dev: &Value) -> String {
    // with a byte order mark: Windows PowerShell reads a file without one in the local code page
    format!("\u{feff}{}", format!(r#"# exterior send <话题名> <内容>：经 exterior 给其他话题发消息，自动署名。内容里写 ![说明](路径) 可附图片或文件。
# 由 exterior 安装，每次启动时覆盖。
if ($args.Count -lt 3 -or $args[0] -ne 'send') {{ [Console]::Error.WriteLine('用法：exterior send <话题名> <内容>'); exit 2 }}
$to = [string]$args[1]
$text = ($args[2..($args.Count - 1)] | ForEach-Object {{ [string]$_ }}) -join ' '
$key = (Get-Content -Raw -Encoding UTF8 -LiteralPath {key}).Trim()
$e = {{ param($s) [Uri]::EscapeDataString([string]$s) }}
$body = 'from=' + (& $e $env:EXTERIOR_TOPIC) + '&to=' + (& $e $to) + '&text=' + (& $e $text)
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
