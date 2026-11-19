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
pub const MENU_MODEL: &str = "切换模型";

pub fn builtin_desc(agent: &str, name: &str) -> String {
    let what = match name { "thinking" => "设置思考强度", "compact" => "压缩上下文", "new" => "开启新会话", other => other };
    format!("{agent} · {what}")
}

// ---- Telegram messages ----

pub const WARN: &str = "⚠️";
pub const STOPPED_SUFFIX: &str = "（已停止）";
pub const CUT_SUFFIX: &str = "（输出中断）";
pub const DONE_EMPTY: &str = "本回合已完成，没有文字输出。";
pub const NOTHING_RUNNING: &str = "当前没有正在执行的任务。";
pub const CHOOSE_MODEL: &str = "请选择模型：";
pub const NO_MODELS: &str = "该 agent 没有可选的模型。";
pub const CHOOSE_THINKING: &str = "请选择思考强度：";
pub const COMPACTED: &str = "上下文已压缩。";
pub const NEW_SESSION: &str = "已开启新会话。";
pub const RESTART_CUT: &str = "exterior 重启时本回合尚未完成，请重新发送该消息。";

pub fn exited(why: &str) -> String {
    let tail = if why.is_empty() { String::new() } else { format!("\n\n{why}") };
    format!("{WARN} agent 进程意外退出。下一条消息将恢复原会话。{tail}")
}
pub fn session_ended(agent: &str) -> String { format!("会话已结束。下一条消息将开启新的 {agent} 会话。") }
pub fn wrong_agent(owner: &str, cur: &str) -> String { format!("该命令属于 {owner}，当前话题使用的是 {cur}。") }
pub fn start_failed(agent: &str, e: &str) -> String { format!("{WARN} 无法启动 {agent}：{e}") }
pub fn current_agent(cur: &str) -> String { format!("当前使用 {cur}。请选择要切换到的 agent：") }
pub fn no_such_agent(want: &str, list: &str) -> String { format!("不存在名为「{want}」的 agent。可用的 agent：{list}") }
pub fn already_agent(cur: &str) -> String { format!("当前已在使用 {cur}。") }
pub fn switched_agent(want: &str) -> String { format!("已切换到 {want}。") }
pub fn transferring(want: &str) -> String { format!("正在将上下文转交给 {want}。") }
pub fn no_such_command(head: &str) -> String { format!("不存在命令 /{head}。") }
pub fn say_failed(e: &str) -> String { format!("{WARN} 无法将消息交给 agent：{e}") }
pub fn models_failed(e: &str) -> String { format!("{WARN} 无法获取模型列表：{e}") }
pub fn no_model_at(i: usize) -> String { format!("{WARN} 不存在第 {i} 个模型，请重新发送 /model 进行选择。") }
pub fn model_set(v: &str) -> String { format!("已切换到模型 {v}。") }
pub fn model_failed(e: &str) -> String { format!("{WARN} 模型切换失败：{e}") }
pub fn thinking_set(v: &str) -> String { format!("思考强度已设置为 {v}。") }
pub fn thinking_failed(e: &str) -> String { format!("{WARN} 无法获取思考强度选项：{e}") }
pub fn failed(e: &str) -> String { format!("{WARN} {e}") }
pub fn note(t: &str) -> String { format!("{WARN} {t}") }

pub fn file_kind(image: bool) -> &'static str { if image { "图片" } else { "文件" } }
pub fn file_saved(user: &str, kind: &str, path: &str) -> String { format!("[{user} 发送了一个{kind}，已保存至：{path}]") }
pub fn file_failed(user: &str, kind: &str, e: &str) -> String { format!("[{user} 发送了一个{kind}，但 exterior 未能下载：{e}]") }

// ---- connection status (会话 and 设置 pages) ----

pub const TG_ADOPTED: &str = "已接管群组中新建的话题";
pub const TG_CONNECTED: &str = "已连接";
pub const TG_NOT_SET: &str = "尚未设置 brief 地址和密钥，或尚未添加设备";
pub fn tg_adopt_failed(e: &str) -> String { format!("接管话题失败：{e}") }
pub fn tg_unreachable(e: &str) -> String { format!("无法连接 brief：{e}") }

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
pub enum Source { Model, Api, Codex }

pub fn retrying(src: Source, a: i64, max: i64, e: &str) -> String {
    let who = match src { Source::Model => "模型", Source::Api => "接口", Source::Codex => "codex " };
    if max > 0 { format!("{who}请求失败，正在进行第 {a}/{max} 次重试：{e}") } else { format!("{who}请求失败，正在重试：{e}") }
}
pub fn extension_error(e: &str) -> String { format!("pi 扩展出错：{e}") }
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
