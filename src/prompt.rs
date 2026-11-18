//! What exterior tells an agent when it starts it. Passed on the command line (claude and pi:
//! --append-system-prompt; codex: -c developer_instructions), so nothing is written into the folders.
//! Everything about Yao lives in agent-infra; the agent looks things up there when it needs them.
use serde_json::Value;

/// Where agent-infra lives on a device.
pub fn infra(dev: &Value) -> String {
    dev["infra"].as_str().filter(|s| !s.is_empty()).unwrap_or("/Users/you/Desktop/agent-infra").trim_end_matches('/').to_string()
}

pub fn root(dev: &Value) -> String { dev["root"].as_str().unwrap_or("").trim_end_matches('/').to_string() }

pub fn session_text(dev: &Value, name: &str, tid: &str, dir: &str, global: bool) -> String {
    let (root, infra) = (root(dev), infra(dev));
    let brief = crate::store::config()["brief"]["url"].as_str().unwrap_or("").trim_end_matches('/').to_string();
    let mut t = format!(r#"# exterior

你在 exterior 里，对应 Telegram 话题「{name}」（{tid}）。Yao 在话题里对你说话，你的回答直接发到话题里；他多半在手机上看，回答要短、直接。启动目录 {dir} 放这件事的文件。

- 第一条消息如果只是一条恢复会话的命令（如 `cc3 --resume <编号>`、`pi --session <编号>`、`cx3 resume <编号>`），表示 Yao 把这个话题从那个会话换到了你这里：找到那个会话的记录读一下，接着做，不用问他。
- 开头是「某某 → 」的消息来自另一个会话，不是 Yao。
- 别的会话、微信和 Telegram 消息：用 brief API {brief}，请求头 `Authorization: Bearer $(cat {root}/.brief-key)`，先看 `GET /api/help?for=all`。给别的会话发话用 `POST /api/send`，文字以「{name} → 」开头。往外给别人发消息只会进草稿，要 Yao 确认。
- 关于 Yao 的一切（规矩、机器、项目、工具用法）在 {infra}，用到时先看那一层的 README.md 再翻具体文件，不用提前读。
- 学到以后还用得上的事（Yao 纠正你的、机器和项目的新事实），直接改 {infra} 里对应的那个文件；新机器、新项目才新建文件，并更新那层的 README.md。不要另起记忆文件，不要自己跑 git，exterior 会提交。
"#);
    if global {
        t.push_str("\n你是「全局」：Yao 在这里闲聊、问各话题的进度。聊出值得单独做的事，建议他开新话题，并把来龙去脉发过去，自己不在这里埋头干大活。\n");
    }
    t
}

/// Put the brief key on a device.
pub fn install(dev: &Value) -> Result<(), String> {
    let root = root(dev);
    if root.is_empty() { return Err("没有设置根目录".into()); }
    let k = crate::brief::key();
    if !k.is_empty() { crate::host::write(dev, &format!("{root}/.brief-key"), &k, "600")?; }
    Ok(())
}
