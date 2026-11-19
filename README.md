# exterior

把 Telegram 群组中的每个话题变成一个 agent 会话：在话题中发送的消息交给运行在你自己机器上的 agent（Claude Code、Codex 或 pi），agent 的回答实时写回同一话题。

exterior 是运行在懒猫微服上的一个应用（单个 Rust 程序），通过 [brief](https://github.com/BH3GEI/wxbrief) 收发 Telegram 消息，通过 SSH 在设备上启动 agent。

## 工作方式

1. 在 Telegram 群组中新建一个话题并发送消息。brief 通过 Telegram Bot API 收到消息。
2. exterior 通过 brief 的 `/api/wait` 读取新消息，为每个话题在设备上建立一个文件夹，并通过 SSH 在其中启动该话题的 agent。agent 保持运行，多轮对话共用一个会话。
3. exterior 与 agent 通过标准输入输出交换 JSON：claude 使用 stream-json，pi 使用 rpc，codex 使用 app-server。agent 的回答在生成过程中持续写回话题。
4. 图片和文件：你发送的图片和文件保存到话题文件夹的 `files/` 中，并告知 agent 路径；agent 可以通过 brief 的 `/api/send_file` 发送图片和文件。
5. 会话之间：agent 可以通过 brief 向另一个话题发送以「名称 → 」开头的消息，exterior 将其转交给该话题的 agent。

## 话题中的命令

原则：agent 自己有的命令，在话题中以相同的名称提供，前面加上表示 agent 的前缀（claude 为 `cc_`，pi 为 `pi_`，codex 为 `cx_`），含义与 agent 自身的命令相同。exterior 自己只有三个命令：

| 命令 | 作用 |
|---|---|
| `/stop` | 停止当前回合（相当于在 agent 中按 Esc） |
| `/exit` | 结束 agent 进程，并删除该话题记录的会话编号，下一条消息开启全新的会话 |
| `/agent` | 切换到另一个 agent；新 agent 会读取原会话的记录并继续 |

agent 的命令分两类：

1. agent 在无界面模式下自己能处理的命令（claude 的 `/compact`、`/context`、`/usage`，pi 的扩展和技能命令等）：exterior 启动时向 agent 读取列表，原样转交。
2. agent 只在交互界面中提供、但协议中有对应请求的命令：exterior 转换为协议请求。目前包括：
   - claude：`/cc_model`、`/cc_status`、`/cc_cost`、`/cc_export`、`/cc_hooks`、`/cc_permissions`、`/cc_memory`、`/cc_exit`
   - pi：`/pi_model`、`/pi_thinking`、`/pi_compact`、`/pi_new`、`/pi_name`、`/pi_session`、`/pi_fork`、`/pi_clone`、`/pi_tree`、`/pi_export`、`/pi_copy`、`/pi_resume`、`/pi_import`、`/pi_quit`
   - codex：`/cx_model`、`/cx_compact`、`/cx_new`、`/cx_clear`、`/cx_status`、`/cx_usage`、`/cx_review`、`/cx_diff`、`/cx_rename`、`/cx_resume`、`/cx_fork`、`/cx_archive`、`/cx_goal`、`/cx_mcp`、`/cx_apps`、`/cx_plugins`、`/cx_skills`、`/cx_hooks`、`/cx_copy`、`/cx_logout`、`/cx_quit`、`/cx_exit`

Telegram 的「/」菜单最多显示 100 条命令，超出的命令不显示在菜单中，但直接输入仍然有效。

agent 的报错、重试、用量限制、模型切换等情况都会在话题中显示。

## 启动说明与 knowledge

exterior 不在程序中保存任何发给 agent 的文字。启动 agent 时，它从设备上的 knowledge 目录读取说明文件（默认 `knowledge/rules/exterior-session.md`，「全局」话题另加 `rules/exterior-global.md`），填入话题名、文件夹等信息后交给 agent。agent 在 knowledge 中查阅和修改关于用户的信息；每个回合结束后，exterior 提交并推送 knowledge 中的修改。

名为「全局」的话题由 exterior 在启动时通过 brief 以用户账号创建并置顶，用于一般性交流和询问各话题进展。

## 部署

需要：懒猫微服、已部署的 brief（并已在其中建好 Telegram 机器人和群组）、一台可以 SSH 登录且装有 agent 的设备。

```sh
cd pkg
./build.sh                      # 编译静态 Linux 程序，并打包所需的 Alpine 软件包（ssh、sshpass、git、curl）
lzc-cli project build
lzc-cli app install --apk n dist/bh3gei.lzcapp.exterior-v*.lpk
```

安装后打开应用页面：

1. 「设置」：填写 brief 地址和密钥。
2. 「设备」：添加设备（地址、端口、用户、密码、根目录），添加 agent（名称、启动命令、类型），点击「测试」确认可用。启动命令在设备上通过交互式 shell 执行，因此可以使用 shell 函数（例如多账号的 `cc0`）。
3. 在设备的根目录下准备 knowledge 目录和其中的启动说明文件。
4. 在 Telegram 群组中新建话题并发送消息。

设备密码和 brief 密钥在 exterior 中加密保存。所有可调参数（名称、路径、时限、启动参数等）都在「设置」页中，清空即恢复默认值。

## 开发

```sh
cargo test                       # 单元测试
cargo run -- probe pi pi /tmp/probe "只回复：在" [模型]   # 在本机直接驱动一个 agent，不经过 Telegram
```

代码结构：

| 文件 | 内容 |
|---|---|
| `src/daemon.rs` | 话题与会话、消息分发、命令处理、输出写回 |
| `src/agents.rs` | 三种 agent 的协议驱动、报错解析 |
| `src/prompt.rs` | 读取并填写启动说明 |
| `src/infra.rs` | knowledge 的提交、推送、撤回 |
| `src/web.rs` | 管理页面 |
| `src/text.rs` | 所有显示给用户的文字 |
| `src/params.rs` | 所有可调参数及默认值 |
| `src/host.rs`、`src/brief.rs`、`src/store.rs`、`src/crypt.rs` | SSH 执行、brief 接口、本地存储、加密 |
