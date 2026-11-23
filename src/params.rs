//! Adjustable settings. Each one has a default here and can be changed on the 设置 page
//! (stored in config.json under "params"). Nothing else in the code carries these values.

pub struct Param { pub key: &'static str, pub label: &'static str, pub default: &'static str }

pub const PARAMS: &[Param] = &[
    Param { key: "user_name", label: "用户称呼（启动说明和文件提示中使用）", default: "Yao" },
    Param { key: "global_topic", label: "全局话题的名称（exterior 启动时确保它存在并置顶；它的说明由 topic_roles 决定）", default: "全局" },
    Param { key: "topic_aliases", label: "话题别名：改名后旧名仍可用（exterior send、定时消息），格式「旧名=新名」，多条用；隔开", default: "" },
    Param { key: "global_dir", label: "全局话题的文件夹名", default: "overview" },
    Param { key: "knowledge_dir", label: "knowledge 文件夹名（设备未单独指定时使用）", default: "knowledge" },
    Param { key: "session_prompt", label: "启动说明文件（相对 knowledge）", default: "exterior-session.md" },
    Param { key: "topic_roles", label: "话题角色：「话题名=角色」，多条用；隔开（如 Polaris=global）。启动该话题时在启动说明后追加 knowledge/exterior-<角色>.md；话题别名同样适用", default: "全局=global" },
    Param { key: "test_prompt", label: "测试 agent 时发送的问题所在文件（相对 knowledge）", default: "exterior-test.md" },
    Param { key: "self_url", label: "设备访问 exterior 的地址（`exterior send` 使用；留空时，作为懒猫应用运行则用应用域名，直接运行则本机设备用 http://127.0.0.1:端口，远程设备无法使用）", default: "" },
    Param { key: "schedules", label: "定时消息：每条「HH:MM|话题名|内容」，多条用 || 隔开；按 exterior 所在机器的时区（懒猫应用为旧金山时间），每天一次，exterior 到点把内容交给该话题的 agent", default: "" },
    Param { key: "idle_compact_min", label: "闲置压缩：一轮结束后闲置多少分钟，exterior 让 agent 执行一次 /compact（0 为关闭；Claude Code 自己用 1 小时缓存的 0.9，即 54 分钟）", default: "54" },
    Param { key: "idle_compact_tokens", label: "闲置压缩：上下文累积到多少 token 才压缩（压缩后从头累积；Claude Code 自己的默认值是 20 万）", default: "200000" },
    Param { key: "relay_mark", label: "会话之间转发消息的分隔符", default: "→" },
    Param { key: "pick_mark", label: "启动命令选定账号时输出的前缀", default: "→" },
    Param { key: "claude_args", label: "claude 的额外启动参数", default: "--dangerously-skip-permissions" },
    Param { key: "claude_fixed_args", label: "claude 的固定启动参数（让子 agent 的话也转发出来，exterior 折叠显示）", default: "--forward-subagent-text" },
    Param { key: "agent_env", label: "给所有 agent 的环境变量（KEY=值，空格隔开；默认关掉 claude 的自动记忆）", default: "CLAUDE_CODE_DISABLE_AUTO_MEMORY=1" },
    Param { key: "codex_auto_approve", label: "codex 发来审批请求时自动同意（1 同意，0 拒绝）", default: "1" },
    Param { key: "agy_auto_allow", label: "agy 发来权限请求时自动允许（1 允许，0 拒绝；agy_args 带 --dangerously-skip-permissions 时一般不会发）", default: "1" },
    Param { key: "agy_args", label: "agy（Antigravity CLI）的额外启动参数", default: "--dangerously-skip-permissions" },
    Param { key: "agy_quota_min", label: "agy 一轮结束后查一次额度（agy -p /usage），同一账号最多每多少分钟一次（0 为不查）", default: "10" },
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
    Param { key: "limit_warn_pct", label: "订阅窗口用量达到多少百分比时提醒（codex）并标为接近上限", default: "90" },
    Param { key: "usage_keep_days", label: "用量记录保留多少天", default: "14" },
    Param { key: "usage_max_mb", label: "用量记录文件最大多少 MB（超过时只留最新的一半）", default: "20" },
    Param { key: "stuck_min", label: "卡死提醒：回合多少分钟没有任何输出，或 agent 中途退出多少分钟没续上，就提醒全局话题一次（0 为关闭）", default: "30" },
    Param { key: "drain_s", label: "重启或安装前，最多等各话题把已生成的输出发完多少秒", default: "180" },
    Param { key: "refresh_s", label: "同步话题列表的间隔（秒）", default: "60" },
    Param { key: "wait_s", label: "向 brief 等待新消息的时长（秒）", default: "20" },
    Param { key: "brief_timeout_s", label: "向 brief 发出一个请求的时限（秒）；上传文件时另加按 1 Mbps 估算的上传时间；须大于等待新消息的时长", default: "70" },
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
/// The settings that let the agents run unattended (permissions, approvals, sandbox, fixed arguments, environment):
/// shown together on the settings page.
pub const UNATTENDED: &[&str] = &["claude_args", "claude_fixed_args", "claude_thinking", "agent_env", "codex_approval", "codex_sandbox",
                                  "codex_auto_approve", "agy_args", "agy_auto_allow"];

pub fn args(key: &str) -> Vec<String> { get(key).split_whitespace().map(String::from).collect() }

pub fn secs(key: &str) -> std::time::Duration { std::time::Duration::from_secs(num(key)) }
