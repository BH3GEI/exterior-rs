//! Every sentence exterior shows to people (Telegram messages, errors, status names, menu descriptions).
//! Written in standard written Chinese. The web page's own labels are in web.rs.

// ---- session status: stored as the key, shown as the label ----

pub mod status {
    pub const NEW: &str = "new";
    pub const RUNNING: &str = "running";
    pub const MOVING: &str = "moving";
    pub const IDLE: &str = "idle";
    pub const ERROR: &str = "error";
    pub const STOPPED: &str = "stopped";
    pub const DELETED: &str = "deleted";

    pub fn label(key: &str) -> &'static str {
        match key {
            RUNNING => "运行中",
            MOVING => "转移中",
            IDLE => "空闲",
            ERROR => "出错",
            STOPPED => "进程已关闭",
            DELETED => "已删除",
            _ => "未启动",
        }
    }

    /// Values written by versions before 0.7, which stored the label itself.
    pub fn from_legacy(v: &str) -> Option<&'static str> {
        Some(match v {
            "未启动" => NEW, "运行中" => RUNNING, "转移中" => MOVING, "空闲" => IDLE,
            "出错" => ERROR, "未运行" => STOPPED, "已删除" => DELETED, _ => return None,
        })
    }
}

// ---- "/" menu ----

pub const MENU_STOP: &str = "exterior · 停止当前回合";
pub const MENU_EXIT: &str = "exterior · 结束当前会话，下一条消息将开启新会话";
pub const MENU_AGENT: &str = "exterior · 切换 agent";
pub const MENU_ACCOUNT: &str = "exterior · 换一个 claude 账号继续当前会话";
pub fn exhausted(acct: &str, why: &str) -> String { format!("{acct} 的额度已用完：{why}。可以换一个账号继续当前会话，上一条消息会自动重发；也可以等额度重置。") }
pub fn choose_account(cur: &str, age_min: u64) -> String { format!("当前账号 {cur}。选择要换到的账号（用量是 {age_min} 分钟前的数据）：") }
pub const NO_ACCOUNTS: &str = "没有其他可用的 claude 账号。";
pub fn accounts_failed(e: &str) -> String { format!("读不到各账号的用量：{e}") }
pub fn account_label(n: &str, h5: &str, d7: &str) -> String { format!("cc{n} · 5h {h5}% · 7d {d7}%") }
pub fn no_such_account(a: &str) -> String { format!("没有名为 {a} 的 claude 账号。") }
pub fn already_account(a: &str) -> String { format!("当前已在使用 {a}。") }
pub fn account_switched(a: &str) -> String { format!("已换到 {a}，会话保持不变。") }
pub fn account_switched_resend(a: &str) -> String { format!("已换到 {a}，会话保持不变，正在重发上一条消息。") }
pub fn account_copy_failed(e: &str) -> String { format!("换账号失败，会话记录复制不过去：{e}") }

/// What the commands of exterior's table (pi, codex) do; the agents' own commands come with their own descriptions.
pub fn builtin_desc(name: &str) -> String {
    match name {
        "model" => "切换模型", "thinking" => "设置思考强度", "compact" => "压缩上下文", "new" | "clear" => "开启新会话",
        "name" => "设置会话名称", "rename" => "重命名会话", "session" => "查看会话统计", "fork" => "分叉出新会话",
        "clone" => "复制当前会话", "tree" => "查看会话树", "export" => "导出会话", "copy" => "复制上一条回答",
        "resume" => "恢复其他会话", "import" => "导入会话文件", "quit" | "exit" => "退出 agent 进程（会话保留）",
        "status" => "查看模型和用量", "usage" => "查看用量", "review" => "审查代码改动", "diff" => "查看工作目录的改动",
        "archive" => "归档当前会话", "goal" => "查看或设置目标", "mcp" => "查看 MCP 服务", "apps" => "查看应用",
        "plugins" => "查看插件", "skills" => "查看技能", "hooks" => "查看 hook", "logout" => "退出登录",
        other => other,
    }.to_string()
}

// ---- Telegram messages ----

pub const WARN: &str = "⚠️";
/// a tool call the turn ended without a result for; the result follows later under the same title
pub fn tool_waiting(title: &str) -> String { format!("{title}（结果稍后）") }
/// exterior's own "/effort auto" to claude at start (claude has no command-line form for it), and claude's reply
pub const EFFORT_AUTO_SENT: &str = "exterior → claude（启动时设置 effort auto，claude 没有对应的启动参数）";
pub const EFFORT_AUTO_REPLY: &str = "claude 对 /effort auto 的回复";
/// Folded: what claude says again, after the user's /compact, of what its last local command said.
pub const COMPACT_REPEAT: &str = "claude 在压缩回合中重复输出的内容（不是回答）";
/// pi's agent_end in short: the conversation it carries was shown message by message already.
pub fn agent_end(n: usize, last: Option<(&str, String, bool)>, rest: &str) -> String {
    let mut s = format!("本回合结束，会话共 {n} 条消息（各条已逐条转发，此处不重复）。");
    if let Some((role, head, more)) = last { s.push_str(&format!("\n最后一条（{role}）：{head}{}", if more { "…" } else { "" })); }
    if !rest.is_empty() { s.push_str(&format!("\n{rest}")); }
    s
}
pub const THINKING: &str = "💭 思考";
pub const STDERR: &str = "🖥 stderr";
pub const STDOUT: &str = "🖥 stdout";
pub const SUBAGENT: &str = "🤖 子 agent";
pub const THINKING_HIDDEN: &str = "（模型没有给出思考内容）";
pub fn event_title(kind: &str) -> String { format!("ℹ️ {kind}") }
/// exterior's own notices, set apart from what the agent says: small grey text (a rich message footer).
pub fn sys(t: &str) -> String { footer("⚙️", t) }
pub fn sys_warn(t: &str) -> String { footer(WARN, t) }
/// a reply of exterior's own, as a notice unless it already is one
pub fn as_sys(t: &str) -> String { if t.starts_with("<footer>") { t.to_string() } else { sys(t) } }
fn footer(icon: &str, t: &str) -> String {
    let t = t.trim();
    // a long or multi-line notice: its first line as the footer, the whole text folded below
    if t.contains('\n') || t.chars().count() > 200 {
        let first: String = t.lines().next().unwrap_or("").chars().take(150).collect();
        return format!("<footer>{icon} exterior · {}</footer>\n\n{}", esc(&first), step(DETAILS, t).join("\n\n"));
    }
    format!("<footer>{icon} exterior · {}</footer>", esc(t))
}

fn esc(t: &str) -> String { t.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;") }

pub const DETAILS: &str = "完整内容";
/// Longest text one message carries (Telegram allows 32768 characters in a rich message; markup needs room too).
pub const MESSAGE_MAX: usize = 30000;
const CHUNK: usize = 28000;

/// A step shown folded: a small italic title, the full content in a code block below it. Nothing is cut: content
/// too long for one message becomes several folded parts, (1/3), (2/3)…
pub fn step(title: &str, body: &str) -> Vec<String> {
    let body = body.trim_end();
    let mut run = 0; let mut longest = 0;
    for c in body.chars() { if c == '`' { run += 1; longest = longest.max(run); } else { run = 0; } }
    let fence = "`".repeat(longest.max(2) + 1);
    let parts = split_text(body, CHUNK);
    let n = parts.len();
    parts.iter().enumerate().map(|(i, p)| {
        let part = if n > 1 { format!("（{}/{n}）", i + 1) } else { String::new() };
        if p.trim().is_empty() { format!("<details><summary><sub><i>{}{part}</i></sub></summary>\n\n（无内容）\n\n</details>", esc(title)) }
        else { format!("<details><summary><sub><i>{}{part}</i></sub></summary>\n\n{fence}\n{p}\n{fence}\n\n</details>", esc(title)) }
    }).collect()
}

/// Cut text into pieces of at most `n` characters, at a paragraph or line break where possible. Nothing is dropped.
pub fn split_text(t: &str, n: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest: Vec<char> = t.chars().collect();
    while rest.len() > n {
        let head: String = rest[..n].iter().collect();
        // a blank line, else a line break, if one is in the second half; only then mid-line
        let late = |b: Option<usize>| b.map(|b| head[..b].chars().count()).filter(|c| *c > n / 2);
        let cut = late(head.rfind("\n\n")).or_else(|| late(head.rfind('\n'))).unwrap_or(n);
        out.push(rest[..cut].iter().collect::<String>());
        rest = rest[cut..].to_vec();
        while rest.first() == Some(&'\n') { rest.remove(0); }
    }
    if !rest.is_empty() || out.is_empty() { out.push(rest.into_iter().collect()); }
    out
}

/// Pieces (paragraphs, folded steps) → messages of at most MESSAGE_MAX characters, in order. Folded steps and what
/// the agent says are never in the same message: an answer after a long run of steps would be easy to miss.
pub fn pack(pieces: Vec<String>) -> Vec<String> {
    let mut msgs: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_step = false;
    for p in pieces {
        let step = p.starts_with("<details>");
        if !cur.is_empty() && step != cur_step { msgs.push(std::mem::take(&mut cur)); }
        cur_step = step;
        for p in if p.chars().count() > MESSAGE_MAX { split_text(&p, MESSAGE_MAX) } else { vec![p] } {
            if !cur.is_empty() && cur.chars().count() + 2 + p.chars().count() > MESSAGE_MAX { msgs.push(std::mem::take(&mut cur)); }
            if !cur.is_empty() { cur.push_str("\n\n"); }
            cur.push_str(&p);
        }
    }
    if !cur.is_empty() { msgs.push(cur); }
    msgs
}
/// A message as it goes out: text, or a file the agent pointed to with `![说明](路径)`.
#[derive(Debug, PartialEq)]
pub enum Piece { Text(String), File { alt: String, path: String } }

/// Split a message at the agent's `![说明](路径)` references to local files (not inside code), so each file goes out
/// in its place. Web addresses are left as they are. A message without such references comes back whole.
pub fn pieces(msg: &str) -> Vec<Piece> {
    fn find(line: &str) -> Option<(usize, usize, String, String)> {
        let mut from = 0;
        while let Some(i) = line[from..].find("![").map(|i| i + from) {
            from = i + 2;
            // inside `inline code`
            if line[..i].matches('`').count() % 2 == 1 { continue; }
            let Some(j) = line[i + 2..].find("](").map(|j| j + i + 2) else { return None };
            let rest = &line[j + 2..];
            let (path, len) = if let Some(r) = rest.strip_prefix('<') {
                match r.find(">)") { Some(k) => (&r[..k], k + 3), None => continue }
            } else {
                match rest.find(')') { Some(k) => (&rest[..k], k + 1), None => continue }
            };
            let path = path.trim();
            if path.is_empty() || path.contains("://") || path.starts_with("data:") || path.starts_with('#') { continue; }
            return Some((i, j + 2 + len, line[i + 2..j].to_string(), path.to_string()));
        }
        None
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut fence: Option<usize> = None;
    for line in msg.split_inclusive('\n') {
        let t = line.trim_start();
        let ticks = t.chars().take_while(|c| *c == '`').count();
        if let Some(n) = fence {
            if ticks >= n && t.trim_end().chars().all(|c| c == '`') { fence = None; }
            cur.push_str(line);
            continue;
        }
        if ticks >= 3 { fence = Some(ticks); cur.push_str(line); continue; }
        let mut rest = line;
        while let Some((a, b, alt, path)) = find(rest) {
            cur.push_str(&rest[..a]);
            if !cur.trim().is_empty() { out.push(Piece::Text(cur.trim().to_string())); }
            cur.clear();
            out.push(Piece::File { alt: alt.trim().to_string(), path });
            rest = &rest[b..];
        }
        cur.push_str(rest);
    }
    if out.is_empty() { return vec![Piece::Text(msg.to_string())]; }
    if !cur.trim().is_empty() { out.push(Piece::Text(cur.trim().to_string())); }
    out
}

pub const STOPPED_SUFFIX: &str = "本回合已停止。";
pub const CUT_SUFFIX: &str = "输出中断。";
pub const DONE_EMPTY: &str = "本回合已完成，没有文字输出。";
pub const NOTHING_RUNNING: &str = "当前没有正在执行的任务。";
pub const CHOOSE_MODEL: &str = "请选择模型：";
pub const NO_MODELS: &str = "该 agent 没有可选的模型。";
pub const CHOOSE_THINKING: &str = "请选择思考强度：";
pub const RESTART_CUT: &str = "exterior 重启时本回合尚未完成，请重新发送该消息。";
pub const RESTART_RESEND: &str = "exterior 重启打断了本回合，已自动重新发送上一条消息。";
/// what the agent gets when its turn is re-sent after a restart
pub fn resent(msg: &str) -> String { format!("（exterior 重启打断了上一回合，下面是同一条消息，请接着处理，已做完的部分不必重做。）\n\n{msg}") }

pub fn exited_waiting(why: &str) -> String {
    let tail = if why.is_empty() { String::new() } else { format!("\n\n{why}") };
    sys_warn(&format!("agent 进程中断（可能是设备重启或断网）。设备恢复后会自动接着处理上一条消息，不必重发。{tail}"))
}
pub const DEVICE_BACK: &str = "设备已恢复，正在接着处理上一条消息。";
pub fn exited(why: &str) -> String {
    let tail = if why.is_empty() { String::new() } else { format!("\n\n{why}") };
    sys_warn(&format!("agent 进程意外退出。下一条消息将恢复原会话。{tail}"))
}
/// a table command the agent answered with nothing
pub fn no_output(agent: &str, name: &str) -> String { format!("{agent} 执行了 /{name}，没有返回内容。") }
/// an agent's structured answer, shown as it is (key: value lines)
pub fn shown(t: &str) -> String { format!("```\n{t}\n```") }
pub fn codex_model_kept(v: &str) -> String { format!("codex 没有切换模型的请求，模型随每一轮发送：从下一轮起使用 {v}。") }
pub fn file_not_sent(path: &str, e: &str) -> String { sys_warn(&format!("文件没有发出去：{path}：{e}")) }
pub const FILE_MISSING: &str = "文件不存在";
/// A file upload that brief received but did not answer in time: brief may still send it, so it is not sent again.
pub const FILE_MAYBE_SENT: &str = "上传后没有收到 brief 的应答，文件可能已经发出；为避免重复发送，没有重发";
pub fn file_unsure(path: &str) -> String { sys_warn(&format!("文件可能没有发出去：{path}：{FILE_MAYBE_SENT}")) }
/// Shown before a remote device's test result when its password cannot be used on this machine.
pub const NO_SSHPASS: &str = "本机没有 sshpass，无法使用密码，已改用 SSH 密钥登录；";
pub const ACCOUNT_SWITCH_WINDOWS: &str = "切换 claude 账号需要复制会话记录，目前只支持 Linux 和 macOS 设备";
pub fn file_too_big(mb: usize) -> String { format!("文件超过 {mb} MB") }
pub fn relay_from_unknown(from: &str) -> String { format!("exterior 不认识发送方话题「{from}」（`exterior send` 要在 exterior 启动的 agent 中运行）") }
pub fn relay_to_unknown(to: &str, names: &str) -> String { format!("没有名为「{to}」的话题。现有话题：{names}") }
pub const RELAY_EMPTY: &str = "内容为空";
pub fn relayed(to: &str) -> String { format!("已发送到「{to}」") }
pub const NOT_READY: &str = "exterior 尚未就绪";
/// a scheduled message handed to a topic's agent (the 定时消息 setting)
pub fn scheduled(content: &str) -> String { format!("定时消息已交给 agent：{content}") }
pub const SCHEDULE_FROM: &str = "定时";
pub fn watching(list: &[String]) -> String { if list.is_empty() { "没有在盯的会话".into() } else { format!("正在盯：{}", list.join("、")) } }
pub fn watch_on(c: &str) -> String { format!("开始盯 {c}：有新消息时，exterior 会攒一小批交给你") }
pub fn watch_off(c: &str) -> String { format!("不再盯 {c}") }
pub const WATCH_TOPIC: &str = "Telegram 话题之间请用 exterior send";
pub const WATCH_HEADER: &str = "（exterior watch 收到的新消息）";
pub fn idle_compact(min: u64, ctx: u64) -> String { format!("exterior 闲置压缩：本话题已闲置 {min} 分钟，上下文约 {} 万 token，在缓存过期前发送 /compact（设置项 idle_compact_min、idle_compact_tokens）", ctx / 10000) }
pub fn session_ended(agent: &str) -> String { format!("会话已结束。下一条消息将开启新的 {agent} 会话。") }
pub fn wrong_agent(owner: &str, cur: &str) -> String { format!("该命令只适用于 {owner}，当前话题使用的是 {cur}。") }
pub fn start_failed(agent: &str, e: &str) -> String { sys_warn(&format!("无法启动 {agent}：{e}")) }
pub fn current_agent(cur: &str) -> String { format!("当前使用 {cur}。请选择要切换到的 agent：") }
pub fn no_such_agent(want: &str, list: &str) -> String { format!("不存在名为「{want}」的 agent。可用的 agent：{list}") }
pub fn already_agent(cur: &str) -> String { format!("当前已在使用 {cur}。") }
pub fn switched_agent(want: &str) -> String { format!("已切换到 {want}。") }
pub fn transferring(want: &str) -> String { format!("正在将上下文转交给 {want}。") }
pub fn no_such_command(head: &str) -> String { format!("不存在命令 /{head}。") }
pub fn say_failed(e: &str) -> String { sys_warn(&format!("无法将消息交给 agent：{e}")) }
pub fn models_failed(e: &str) -> String { sys_warn(&format!("无法获取模型列表：{e}")) }
pub fn no_model_at(i: usize, cmd: &str) -> String { sys_warn(&format!("不存在第 {i} 个模型，请重新发送 /{cmd} 进行选择。")) }
pub fn name_needed(cmd: &str) -> String { format!("请在命令后写出名称，例如：/{cmd} 项目调研") }
pub const CHOOSE_FORK: &str = "请选择从哪条消息处分叉：";
/// A rate-limit window's length, e.g. 「5 小时」「7 天」.
pub fn window(mins: Option<i64>) -> String {
    match mins {
        Some(m) if m > 0 && m % 1440 == 0 => format!("{} 天", m / 1440),
        Some(m) if m > 0 && m % 60 == 0 => format!("{} 小时", m / 60),
        Some(m) if m > 0 => format!("{m} 分钟"),
        _ => String::new(),
    }
}
pub const NO_SESSIONS: &str = "没有找到其他会话。";
pub const CHOOSE_SESSION: &str = "请选择要恢复的会话：";
pub const QUIT: &str = "agent 进程已退出。下一条消息将恢复原会话。";
pub const NOTHING_TO_COPY: &str = "还没有可复制的回答。";
pub const EMPTY_RESULT: &str = "（无内容）";

/// A result from an agent, shown as indented key: value lines (for the commands that only display information).
pub fn show(v: &serde_json::Value) -> String {
    fn walk(v: &serde_json::Value, depth: usize, out: &mut String) {
        let pad = "  ".repeat(depth);
        match v {
            serde_json::Value::Object(m) => for (k, x) in m {
                if k.starts_with('_') || x.is_null() { continue; }
                if x.is_object() || x.is_array() { out.push_str(&format!("{pad}{k}：\n")); walk(x, depth + 1, out); }
                else { out.push_str(&format!("{pad}{k}：{}\n", scalar(x))); }
            },
            serde_json::Value::Array(a) => for x in a {
                if x.is_object() || x.is_array() { out.push_str(&format!("{pad}-\n")); walk(x, depth + 1, out); }
                else { out.push_str(&format!("{pad}- {}\n", scalar(x))); }
            },
            x => out.push_str(&format!("{pad}{}\n", scalar(x))),
        }
    }
    fn scalar(x: &serde_json::Value) -> String { x.as_str().map(String::from).unwrap_or_else(|| x.to_string()) }
    let mut out = String::new();
    walk(v, 0, &mut out);
    let out = out.trim_end().to_string();
    if out.is_empty() { EMPTY_RESULT.into() } else { out }
}
pub fn model_failed(e: &str) -> String { sys_warn(&format!("模型切换失败：{e}")) }
pub fn thinking_failed(e: &str) -> String { sys_warn(&format!("无法获取思考强度选项：{e}")) }
pub fn failed(e: &str) -> String { sys_warn(e) }
pub fn note(t: &str) -> String { sys_warn(t) }

pub fn file_kind(image: bool) -> &'static str { if image { "图片" } else { "文件" } }
pub fn file_saved(user: &str, kind: &str, path: &str) -> String { format!("[{user} 发送了一个{kind}，已保存至：{path}]") }
pub fn file_failed(user: &str, kind: &str, e: &str) -> String { format!("[{user} 发送了一个{kind}，但 exterior 未能下载：{e}]") }

// ---- connection status (会话 and 设置 pages) ----

pub const TG_ADOPTED: &str = "已接管群组中新建的话题";
pub const TG_CONNECTED: &str = "已连接";
pub const TG_NOT_SET: &str = "尚未设置 brief 地址和密钥，或尚未添加设备";
pub fn tg_adopt_failed(e: &str) -> String { format!("接管话题失败：{e}") }
pub fn tg_unreachable(e: &str) -> String { format!("无法连接 brief：{e}") }
pub fn global_ready(name: &str) -> String { format!("「{name}」话题已存在并已置顶") }
pub fn global_failed(name: &str, e: &str) -> String { format!("无法创建或置顶「{name}」话题：{e}") }

// ---- errors ----

pub const NO_DEVICE: &str = "尚未添加设备";
pub const BAD_KIND: &str = "agent 类型无效";
pub const NO_COMMANDS: &str = "未能获取 agent 的命令列表：agent 没有响应";
pub const NOT_RUNNING: &str = "agent 未在运行";
pub const AGENT_ERROR: &str = "agent 返回了错误，但未说明原因";
pub const MODEL_ERROR: &str = "模型返回了错误，但未说明原因";
pub const PI_REJECTED: &str = "pi 拒绝了该请求";
pub const CODEX_UNHANDLED: &str = "exterior 不处理该类请求";
pub const CODEX_ERROR: &str = "codex 返回了错误，但未说明原因";
pub const CODEX_NO_THREAD: &str = "codex 未返回会话编号";
pub const CODEX_NOT_READY: &str = "codex 尚未就绪";
pub const SET_MODEL_FAILED: &str = "agent 拒绝了模型切换，但未说明原因";
pub const NOT_SUPPORTED: &str = "该 agent 不支持此命令";
pub const BUILTIN_FAILED: &str = "命令执行失败，但 agent 未说明原因";
pub const NO_ROOT: &str = "设备未设置根目录";
pub const BRIEF_NOT_SET: &str = "brief 尚未设置";
pub const BAD_COMMIT: &str = "提交编号无效";
pub const PUSH_OK: &str = "推送检查通过";
pub const PUSHED: &str = "已推送";
pub const REVERTED: &str = "已撤回并推送";
pub const KEY_WRITTEN: &str = "已写入";

pub fn device_missing(d: &str) -> String { format!("设备「{d}」不存在") }
pub fn agent_missing(d: &str, a: &str) -> String { format!("设备「{d}」上没有名为「{a}」的 agent") }
pub fn no_response(secs: u64) -> String { format!("agent 在 {secs} 秒内没有响应") }
pub fn codex_start_failed(e: &str) -> String { format!("codex 启动失败：{e}") }
pub fn agy_start_failed(e: &str) -> String { format!("agy 启动失败：{e}") }
pub fn agy_resume_failed(id: &str, e: &str) -> String { format!("agy 无法续接对话 {id}（{e}），已开新对话") }
pub fn agy_model_set(v: &str) -> String { format!("已切换到 {v}，本话题之后都用它") }
pub fn agy_stopped_by(why: &str) -> String { format!("agy 结束了这一轮：{why}") }
pub const AGY_NO_SESSION: &str = "agy 未返回会话编号";
pub const AGY_NOT_READY: &str = "agy 尚未就绪";
pub fn model_unknown(agent: &str, v: &str) -> String { format!("{agent} 的模型列表中没有「{v}」") }
/// Whose request failed, for retry notices.
pub enum Source { Model, Api, Codex, Compaction }

pub fn retrying(src: Source, a: i64, max: i64, e: &str) -> String {
    let who = match src { Source::Model => "模型", Source::Api => "接口", Source::Codex => "codex ", Source::Compaction => "上下文压缩" };
    if max > 0 { format!("{who}请求失败，正在进行第 {a}/{max} 次重试：{e}") } else { format!("{who}请求失败，正在重试：{e}") }
}
pub fn extension_error(e: &str) -> String { format!("pi 扩展出错：{e}") }
pub fn extension_notice(m: &str) -> String { format!("pi 扩展提示：{m}") }
pub fn dialog_cancelled(kind: &str, title: &str) -> String {
    let what = match kind { "select" => "选择", "confirm" => "确认", _ => "输入" };
    format!("pi 扩展请求你进行{what}：「{title}」。exterior 目前无法转交此类请求，已将其取消。")
}
pub fn compaction_failed(e: &str) -> String { format!("上下文压缩失败：{e}") }
pub fn codex_request_declined(method: &str) -> String { format!("codex 发出了 exterior 不支持的请求（{method}），已拒绝。") }
pub fn mcp_failed(name: &str, e: &str) -> String { if e.is_empty() { format!("MCP 服务 {name} 启动失败") } else { format!("MCP 服务 {name} 启动失败：{e}") } }
pub fn auth_recovery(provider: &str, m: &str) -> String { format!("模型服务 {provider} 认证失效，正在恢复：{m}") }
pub fn auth_error(e: &str) -> String { format!("claude 登录状态出错：{e}") }
pub fn hook_failed(name: &str, e: &str) -> String { format!("hook {name} 执行失败：{e}") }
pub fn instructions_too_long(total: u64, limit: u64) -> String { format!("说明文件总长度为 {total} 字符，超过上限 {limit} 字符") }
pub fn tool_denied(tool: &str, why: &str) -> String { if why.is_empty() { format!("claude 拒绝调用工具 {tool}") } else { format!("claude 拒绝调用工具 {tool}（{why}）") } }
pub fn writes_lost(code: &str) -> String { format!("会话记录写入失败（{code}），部分记录已丢失") }
pub fn mirror_failed(e: &str) -> String { format!("会话记录同步失败：{e}") }
pub const REFUSED_NO_FALLBACK: &str = "模型拒绝了该请求，且没有可用的备用模型";
pub const CODEX_SYSTEM_ERROR: &str = "codex 会话出现系统错误";
pub fn result_error(subtype: &str) -> String {
    match subtype {
        "error_max_turns" => "已达到最大回合数".into(),
        "error_max_budget_usd" => "已达到费用上限".into(),
        "error_max_structured_output_retries" => "结构化输出的重试次数已用完".into(),
        "error_during_execution" => "执行过程中出错".into(),
        _ => AGENT_ERROR.into(),
    }
}
pub fn fallback(from: &str, to: &str) -> String { format!("模型已从 {from} 切换为 {to}") }
pub fn limit_reached(kind: &str, when: &str) -> String { format!("用量已达上限（{kind}）{when}") }
pub fn limit_near(kind: &str, pct: &str, when: &str) -> String { format!("用量接近上限（{kind}）{pct}{when}") }
pub fn resets_in(h: i64, m: i64) -> String { format!("，预计 {h} 小时 {m} 分钟后恢复") }
pub fn used_pct(p: f64) -> String { format!("，已使用 {p:.0}%") }
pub fn picked(command: &str, n: &str) -> String { format!("（{command} 选用了 {n}）") }
pub fn exit_code(code: Option<i32>, tail: &str) -> String {
    match code { Some(c) => format!("退出码 {c}：{tail}"), None => format!("进程被终止：{tail}") }
}
pub fn exec_failed(e: &str) -> String { format!("命令无法执行：{e}") }
pub fn timed_out(secs: u64) -> String { format!("执行超时（{secs} 秒）") }
pub fn download_failed(code: Option<i32>) -> String { format!("文件下载失败（curl 退出码 {code:?}）") }
pub fn read_failed(e: &str) -> String { format!("读取失败：{e}") }
pub fn prompt_unreadable(path: &str, e: &str) -> String { format!("无法读取启动说明 {path}：{e}") }
pub fn menu_set(n: usize) -> String { format!("已设置 {n} 条命令") }

/// Folder name for a topic whose title has no usable characters.
pub const UNNAMED_TOPIC: &str = "话题";
/// Stands for a session id in the example resume lines of the start text.
pub const ID_PLACEHOLDER: &str = "<编号>";
/// Sample values for the 启动说明 page.
pub const SAMPLE_TOPIC: &str = "示例话题";
pub const SAMPLE_TID: &str = "tgbot:编号";

pub const USAGE_SPAN: &str = "时间范围写成 5h、24h、7d 这样（数字加 m、h 或 d）";
