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

pub const MENU_STOP: &str = "停止当前回合";
pub const MENU_EXIT: &str = "结束当前会话，下一条消息将开启新会话";
pub const MENU_AGENT: &str = "切换 agent";
pub const MENU_ACCOUNT: &str = "claude · 换账号继续当前会话";
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

pub fn builtin_desc(agent: &str, name: &str) -> String {
    let what = match name {
        "model" => "切换模型", "thinking" => "设置思考强度", "compact" => "压缩上下文", "new" => "开启新会话",
        "name" => "设置会话名称", "session" => "查看会话统计", "fork" => "从先前的消息分叉出新会话", "export" => "将会话导出为 HTML",
        "status" => "查看模型和用量", "review" => "审查代码改动", other => other,
    };
    format!("{agent} · {what}")
}

// ---- Telegram messages ----

pub const WARN: &str = "⚠️";
/// exterior's own notices, set apart from what the agent says: small grey text (a rich message footer).
pub fn sys(t: &str) -> String { footer("⚙️", t) }
pub fn sys_warn(t: &str) -> String { footer(WARN, t) }
/// a reply of exterior's own, as a notice unless it already is one
pub fn as_sys(t: &str) -> String { if t.starts_with("<footer>") { t.to_string() } else { sys(t) } }
fn footer(icon: &str, t: &str) -> String {
    let t = t.trim().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('\n', "<br>");
    format!("<footer>{icon} exterior · {t}</footer>")
}
pub const STOPPED_SUFFIX: &str = "本回合已停止。";
pub const CUT_SUFFIX: &str = "输出中断。";
pub const DONE_EMPTY: &str = "本回合已完成，没有文字输出。";
pub const NOTHING_RUNNING: &str = "当前没有正在执行的任务。";
pub const CHOOSE_MODEL: &str = "请选择模型：";
pub const NO_MODELS: &str = "该 agent 没有可选的模型。";
pub const CHOOSE_THINKING: &str = "请选择思考强度：";
pub fn choose_effort(cur: Option<&str>) -> String { format!("请选择 effort（当前：{}）：", cur.unwrap_or("auto")) }
pub const COMPACTED: &str = "上下文已压缩。";
pub const NEW_SESSION: &str = "已开启新会话。";
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
pub fn session_ended(agent: &str) -> String { format!("会话已结束。下一条消息将开启新的 {agent} 会话。") }
pub fn wrong_agent(owner: &str, cur: &str) -> String { format!("该命令属于 {owner}，当前话题使用的是 {cur}。") }
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
pub fn name_set(name: &str) -> String { format!("会话名称已设置为「{name}」。") }
pub fn exported(path: &str) -> String { format!("会话已导出至：{path}") }
pub const CHOOSE_FORK: &str = "请选择从哪条消息处分叉：";
pub const NO_FORK_POINTS: &str = "当前会话中还没有可以分叉的消息。";
pub fn forked(text: &str) -> String { format!("已从所选消息处分叉出新会话，之后的消息将在新会话中进行。所选消息原文：\n{text}") }
pub const FORK_CANCELLED: &str = "分叉已被扩展取消。";
pub fn review_started(target: &str) -> String { format!("已开始审查{target}，结果将在本话题中显示。") }
pub fn review_target(kind: &str, v: &str) -> String {
    match kind { "commit" => format!("提交 {v}"), "baseBranch" => format!("当前分支与 {v} 的差异"), _ => "尚未提交的改动".into() }
}
pub fn session_stats(d: &serde_json::Value) -> String {
    let n = |v: &serde_json::Value| v.as_u64().map(|x| x.to_string()).unwrap_or_else(|| "—".into());
    let t = &d["tokens"];
    let ctx = &d["contextUsage"];
    format!("会话 {}\n消息：你 {} 条，pi {} 条；工具调用 {} 次\ntoken：输入 {}，输出 {}，缓存读取 {}，缓存写入 {}，合计 {}\n费用：{}\n上下文：已用 {} / {}",
        d["sessionId"].as_str().unwrap_or("—"), n(&d["userMessages"]), n(&d["assistantMessages"]), n(&d["toolCalls"]),
        n(&t["input"]), n(&t["output"]), n(&t["cacheRead"]), n(&t["cacheWrite"]), n(&t["total"]),
        d["cost"].as_f64().map(|c| format!("${c:.2}")).unwrap_or_else(|| "—".into()), n(&ctx["tokens"]), n(&ctx["contextWindow"]))
}
pub fn codex_status(model: &str, r: &serde_json::Value) -> String {
    let mut s = format!("模型：{model}");
    for k in ["primary", "secondary"] {
        let w = &r[k];
        if let Some(p) = w["usedPercent"].as_i64() {
            let reset = w["resetsAt"].as_i64().map(|t| t - crate::store::now() as i64).filter(|l| *l > 0).map(|l| resets_in(l / 3600, l % 3600 / 60)).unwrap_or_default();
            s.push_str(&format!("\n{}用量：{p}%{reset}", window(w["windowDurationMins"].as_i64())));
        }
    }
    if r["credits"]["hasCredits"] == false && r["credits"]["unlimited"] != true { s.push_str("\n额度余额：无"); }
    if let Some(t) = r["rateLimitReachedType"].as_str() { s.push_str(&format!("\n已达上限：{t}")); }
    s
}
/// A rate-limit window's length, e.g. 「5 小时」「7 天」.
pub fn window(mins: Option<i64>) -> String {
    match mins {
        Some(m) if m > 0 && m % 1440 == 0 => format!("{} 天", m / 1440),
        Some(m) if m > 0 && m % 60 == 0 => format!("{} 小时", m / 60),
        Some(m) if m > 0 => format!("{m} 分钟"),
        _ => String::new(),
    }
}
pub const DEFAULT_MODEL: &str = "默认模型";
pub const NO_SESSIONS: &str = "没有找到其他会话。";
pub const CHOOSE_SESSION: &str = "请选择要恢复的会话：";
pub fn resumed(id: &str) -> String { format!("已切换到会话 {id}，之后的消息将在该会话中进行。") }
pub fn cloned(id: &str) -> String { format!("已复制当前会话，之后的消息将在副本 {id} 中进行。") }
pub fn renamed(name: &str) -> String { format!("会话已重命名为「{name}」。") }
pub fn archived(id: &str) -> String { format!("原会话已归档，之后的消息将在新会话 {id} 中进行。") }
pub const GOAL_CLEARED: &str = "目标已清除。";
pub fn goal_set(g: &str) -> String { format!("目标已设置为：{g}") }
pub const LOGGED_OUT: &str = "已退出该 codex 账号的登录。";
pub const QUIT: &str = "agent 进程已退出。下一条消息将恢复原会话。";
pub const NOTHING_TO_COPY: &str = "还没有可复制的回答。";
pub fn exported_file(name: &str) -> String { format!("会话已导出为文件 {name}。") }
pub const EMPTY_RESULT: &str = "（无内容）";
pub const NO_DIFF: &str = "工作目录中没有改动。";

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
pub fn model_set(v: &str) -> String { format!("已切换到模型 {v}。") }
pub fn model_failed(e: &str) -> String { sys_warn(&format!("模型切换失败：{e}")) }
pub fn thinking_set(v: &str) -> String { format!("思考强度已设置为 {v}。") }
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
pub fn clipped(n: usize) -> String { format!("…（已省略前 {n} 字）") }
pub fn menu_set(n: usize) -> String { format!("已设置 {n} 条命令") }

/// Folder name for a topic whose title has no usable characters.
pub const UNNAMED_TOPIC: &str = "话题";
/// Stands for a session id in the example resume lines of the start text.
pub const ID_PLACEHOLDER: &str = "<编号>";
/// Sample values for the 启动说明 page.
pub const SAMPLE_TOPIC: &str = "示例话题";
pub const SAMPLE_TID: &str = "tgbot:编号";
