//! exterior — Yao's outer brain: Telegram topics ↔ agent sessions (claude / pi / codex), shared memory.
//!
//!   exterior run              the daemon (on Lazycat; reaches the agents' machine via EXTERIOR_HOST)
//!   exterior ls               sessions: name, topic, agent        (for agents and for you)
//!   exterior send NAME TEXT   post into another session's topic as 「<this session> → TEXT」
mod agents;
mod brief;
mod daemon;
mod host;
mod memory;
mod note;

use serde_json::json;

const DEFAULT_ROOT: &str = "/Users/you/Desktop/code/agent/exterior-home";

fn root() -> String { std::env::var("EXTERIOR_ROOT").unwrap_or(DEFAULT_ROOT.into()) }

fn topics() -> Vec<serde_json::Value> {
    let extra: Vec<String> = std::env::var("EXTERIOR_EXTRA").unwrap_or_default().split(',').map(String::from).collect();
    brief::get("/api/bots").and_then(|v| v.as_array().cloned()).unwrap_or_default().into_iter()
        .filter(|t| t["deleted"] != true && (t["user_created"] == true || extra.iter().any(|e| t["chat"] == e.as_str())))
        .collect()
}

fn ls() {
    let me = std::env::var("EXTERIOR_SESSION").unwrap_or_default();
    let ts = topics();
    if ts.is_empty() { println!("（还没有会话）"); return; }
    for t in ts {
        let name = t["name"].as_str().unwrap_or("").trim_start_matches("✓ ");
        let chat = t["chat"].as_str().unwrap_or("");
        let agent = note::read(chat)["agent"].as_str().unwrap_or("").to_string();
        let mark = if name == me { "  ← 你" } else { "" };
        println!("{name}\t{chat}\t{}{mark}", if agent.is_empty() { "-".into() } else { agent });
    }
}

fn send(to: &str, text: &str) -> i32 {
    let from = std::env::var("EXTERIOR_SESSION").ok().filter(|s| !s.is_empty()).unwrap_or("外部".into());
    let ts = topics();
    let hit = ts.iter().find(|t| t["chat"] == to || t["name"].as_str().map(|n| n.trim_start_matches("✓ ") == to).unwrap_or(false));
    let Some(t) = hit else {
        eprintln!("没有叫「{to}」的会话。用 exterior ls 看有哪些。");
        return 1;
    };
    if t["name"].as_str().map(|n| n.trim_start_matches("✓ ")) == Some(from.as_str()) {
        eprintln!("不能发给自己。");
        return 1;
    }
    match brief::post("/api/send", json!({"chat": t["chat"], "text": brief::clip(&format!("{from} → {text}"))})) {
        Ok(_) => { println!("已发到「{}」", t["name"].as_str().unwrap_or(to)); 0 }
        Err((c, v)) => { eprintln!("发送失败 {c}: {v}"); 1 }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("run") => { daemon::run(&root()); 0 }
        Some("ls") => { ls(); 0 }
        Some("send") if args.len() >= 3 => send(&args[1], &args[2..].join(" ")),
        _ => {
            eprintln!("用法：\n  exterior ls                 有哪些会话\n  exterior send 名字 内容     给另一个会话发一句（走 Telegram，对方话题里能看到）\n  exterior run                运行 exterior（常驻）");
            2
        }
    };
    std::process::exit(code);
}
