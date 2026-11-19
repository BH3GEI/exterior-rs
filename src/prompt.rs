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

/// Put the brief key on a device.
pub fn install(dev: &Value) -> Result<(), String> {
    let root = root(dev);
    if root.is_empty() { return Err(text::NO_ROOT.into()); }
    let k = crate::brief::key();
    if !k.is_empty() { host::write(dev, &format!("{root}/.brief-key"), &k, "600")?; }
    Ok(())
}
