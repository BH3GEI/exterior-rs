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
    let raw = host::run(dev, &format!("cat {}", host::quote(&path)), 30).map_err(|e| text::prompt_unreadable(&path, &e))?;
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

/// The address devices use to reach exterior.
pub fn self_url() -> String {
    let u = params::get("self_url");
    if !u.is_empty() { return u.trim_end_matches('/').to_string(); }
    std::env::var("LAZYCAT_APP_DOMAIN").map(|d| format!("https://{d}")).unwrap_or_default()
}

/// The key `exterior send` presents (made once, kept in config.json).
pub fn send_key() -> String {
    let mut c = crate::store::config();
    if let Some(k) = c["send_key"].as_str().filter(|k| !k.is_empty()) { return k.to_string(); }
    let mut b = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") { let _ = std::io::Read::read_exact(&mut f, &mut b); }
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
"#, key = host::quote(&format!("{}/.exterior-key", root(dev))), url = host::quote(&format!("{}/api/send", self_url())))
}

/// Put the brief key, the exterior key and `exterior send` on a device.
pub fn install(dev: &Value) -> Result<(), String> {
    let root = root(dev);
    if root.is_empty() { return Err(text::NO_ROOT.into()); }
    let k = crate::brief::key();
    if !k.is_empty() { host::write(dev, &format!("{root}/.brief-key"), &k, "600")?; }
    host::write(dev, &format!("{root}/.exterior-key"), &send_key(), "600")?;
    host::write(dev, &format!("{}/exterior", bin_dir(dev)), &send_script(dev), "755")?;
    Ok(())
}
