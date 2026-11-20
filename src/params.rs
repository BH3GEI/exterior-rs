//! Adjustable settings. Each one has a default here and can be changed on the 设置 page
//! (stored in config.json under "params"). Nothing else in the code carries these values.

pub struct Param { pub key: &'static str, pub label: &'static str, pub default: &'static str }

pub const PARAMS: &[Param] = &[
    Param { key: "user_name", label: "用户称呼（启动说明和文件提示中使用）", default: "Yao" },
    Param { key: "global_topic", label: "全局话题的名称", default: "全局" },
    Param { key: "global_dir", label: "全局话题的文件夹名", default: "overview" },
    Param { key: "knowledge_dir", label: "knowledge 文件夹名（设备未单独指定时使用）", default: "knowledge" },
    Param { key: "session_prompt", label: "启动说明文件（相对 knowledge）", default: "exterior-session.md" },
    Param { key: "global_prompt", label: "全局话题追加说明文件（相对 knowledge）", default: "exterior-global.md" },
    Param { key: "test_prompt", label: "测试 agent 时发送的问题所在文件（相对 knowledge）", default: "exterior-test.md" },
    Param { key: "self_url", label: "设备访问 exterior 的地址（`exterior send` 使用；留空则用懒猫应用域名，不在懒猫上时本机设备用 http://127.0.0.1:端口）", default: "" },
    Param { key: "schedules", label: "定时消息：每条「HH:MM|话题名|内容」，多条用 || 隔开；按 exterior 所在时区（懒猫上为旧金山时间），每天一次，exterior 到点把内容交给该话题的 agent", default: "" },
    Param { key: "relay_mark", label: "会话之间转发消息的分隔符", default: "→" },
    Param { key: "pick_mark", label: "启动命令选定账号时输出的前缀", default: "→" },
    Param { key: "claude_args", label: "claude 的额外启动参数", default: "--dangerously-skip-permissions" },
    Param { key: "codex_approval", label: "codex 的审批策略（approvalPolicy）", default: "never" },
    Param { key: "codex_sandbox", label: "codex 的沙箱模式（sandbox）", default: "danger-full-access" },
    Param { key: "ack_emoji", label: "收到消息时添加的回应表情", default: "👀" },
    Param { key: "wait_hours", label: "agent 中断后，最多等设备恢复多少小时再自动续上", default: "24" },
    Param { key: "claude_dir", label: "claude 各账号的配置目录（{name} 为账号名，如 cc3）", default: "~/.claude-{name}" },
    Param { key: "cc_usage", label: "claude 各账号用量缓存文件（每行：编号 5小时% 7天% 状态）", default: "~/.agents/cc-usage.cache" },
    Param { key: "idle_hours", label: "会话空闲多少小时后关闭进程", default: "2" },
    Param { key: "pause_ms", label: "agent 停顿多久算说完一段（毫秒）", default: "2000" },
    Param { key: "merge_s", label: "同一话题多少秒内说的话合并成一条发送", default: "10" },
    Param { key: "busy_merge_s", label: "Telegram 发送队列拥堵时，仍在运行的话题多少秒内说的话合并成一条发送", default: "60" },
    Param { key: "tg_gap_ms", label: "向 Telegram 发送、编辑消息的最短间隔（毫秒）；所有话题共用一个队列，避免限流", default: "3000" },
    Param { key: "ask_timeout_s", label: "等待 agent 响应请求的时间（秒）", default: "30" },
    Param { key: "codex_ready_s", label: "等待 codex 就绪的时间（秒）", default: "40" },
    Param { key: "claude_thinking", label: "claude 的 --thinking-display（summarized 显示思考摘要；留空则不传）", default: "summarized" },
    Param { key: "limit_warn_pct", label: "codex 用量达到多少百分比时提醒", default: "90" },
    Param { key: "refresh_s", label: "同步话题列表的间隔（秒）", default: "60" },
    Param { key: "wait_s", label: "向 brief 等待新消息的时长（秒）", default: "20" },
    Param { key: "retry_wait_s", label: "发送失败后等待多少秒再重试", default: "3" },
    Param { key: "reconnect_s", label: "brief 连接失败后等待多少秒再重连", default: "5" },
    Param { key: "menu_retry_min", label: "命令菜单生成失败后的重试间隔（分钟）", default: "5" },
    Param { key: "commands_timeout_s", label: "读取 agent 命令列表的时限（秒）", default: "90" },
    Param { key: "test_timeout_s", label: "agent 测试的时限（秒）", default: "180" },
    Param { key: "git_timeout_s", label: "knowledge 提交和推送的时限（秒）", default: "90" },
];

/// The current value of a setting (what the 设置 page saved, else the default).
pub fn get(key: &str) -> String {
    let saved = crate::store::config()["params"][key].as_str().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    saved.unwrap_or_else(|| PARAMS.iter().find(|p| p.key == key).map(|p| p.default.to_string()).unwrap_or_default())
}

pub fn num(key: &str) -> u64 {
    get(key).parse().unwrap_or_else(|_| PARAMS.iter().find(|p| p.key == key).and_then(|p| p.default.parse().ok()).unwrap_or(0))
}

/// A setting that is a list of command-line arguments, split at spaces.
pub fn args(key: &str) -> Vec<String> { get(key).split_whitespace().map(String::from).collect() }

pub fn secs(key: &str) -> std::time::Duration { std::time::Duration::from_secs(num(key)) }
