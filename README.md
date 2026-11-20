# exterior

把 Telegram 群组中的每个话题变成一个 agent 会话：在话题中发送的消息交给运行在你自己机器上的 agent（Claude Code、Codex 或 pi），agent 说的话按段发回同一话题：同一话题短时间内的几段合并成一条，所有消息排队发送，不会撞上 Telegram 的限流。

exterior 是单个 Rust 程序，作为应用运行在懒猫微服上，也可以直接运行在 Mac 或 Windows 电脑上（见「在 Mac / Windows 上运行」）。它通过 [brief](https://github.com/BH3GEI/wxbrief) 收发 Telegram 消息，通过 SSH 在设备上启动 agent，或直接在本机启动 agent。

## 工作方式

1. 在 Telegram 群组中新建一个话题并发送消息。brief 通过 Telegram Bot API 收到消息。
2. exterior 通过 brief 的 `/api/wait` 读取新消息，为每个话题在设备上建立一个文件夹，并通过 SSH 在其中启动该话题的 agent。agent 保持运行，多轮对话共用一个会话。
3. exterior 与 agent 通过标准输入输出交换 JSON：claude 使用 stream-json，pi 使用 rpc，codex 使用 app-server。agent 的回答在生成过程中持续写回话题。
4. 图片和文件：你发送的图片和文件保存到话题文件夹的 `files/` 中，并告知 agent 路径。agent 在回答中写 `![说明](路径)`，exterior 就把该文件按原位置发到话题中（代码块里的不算）。
5. 会话之间：agent 运行 `exterior send <话题名> <内容>`（exterior 装在设备的 `<根目录>/.exterior/bin` 中，并加入 agent 的 PATH），exterior 加上「名称 → 」署名后发到该话题，再转交给该话题的 agent。这是 exterior 唯一不需要懒猫登录的地址（`/api/send`），凭 `<根目录>/.exterior-key` 中的密钥访问。

## 话题中的命令

原则：exterior 只转达和转发。不带前缀的命令都交给话题当前的 agent；exterior 自己的命令以 `exterior_` 开头：

| 命令 | 作用 |
|---|---|
| `/exterior_stop` | 停止当前回合（相当于在 agent 中按 Esc） |
| `/exterior_exit` | 结束 agent 进程，并删除该话题记录的会话编号，下一条消息开启全新的会话 |
| `/exterior_agent` | 切换到另一个 agent；新 agent 会读取原会话的记录并继续 |
| `/exterior_account` | claude：换一个账号继续当前会话 |

agent 的命令：

1. claude、pi 自己报告的命令（claude 的 `/compact`、`/model`、`/effort`、`/goal`，pi 的扩展、模板和技能命令等）：exterior 启动时向 agent 读取列表和说明，原样转交，agent 的回答原样转发。
2. pi 和 codex 只在交互界面中提供、但协议中有对应请求的命令，由 exterior 的表（`agents::BUILTINS`）转换为协议请求，agent 的回答原样显示：
   - pi：`/model`、`/thinking`、`/compact`、`/new`、`/name`、`/session`、`/fork`、`/clone`、`/tree`、`/export`、`/copy`、`/resume`、`/import`、`/quit`
   - codex：`/model`、`/compact`、`/new`、`/clear`、`/status`、`/usage`、`/review`、`/diff`、`/rename`、`/resume`、`/fork`、`/archive`、`/goal`、`/mcp`、`/apps`、`/plugins`、`/skills`、`/hooks`、`/copy`、`/logout`、`/quit`、`/exit`
3. 当前 agent 没有的命令，作为普通消息原样交给它。

菜单中同名的命令只列一次，灰色说明的开头写明哪些 agent 支持（例如「Claude / Pi / Codex · 压缩上下文」）。命令名中 Telegram 不允许的字符换成 `_`（例如 `design-sync` → `/design_sync`）。

Telegram 的「/」菜单最多显示 100 条命令，超出的命令不显示在菜单中，但直接输入仍然有效。

agent 的报错、重试、用量限制、模型切换等情况都会在话题中显示。

## 启动说明与 knowledge

exterior 不在程序中保存任何发给 agent 的文字。启动 agent 时，它从设备上的 knowledge 目录读取说明文件（默认 `knowledge/exterior-session.md`，「全局」话题另加 `exterior-global.md`），填入话题名、文件夹等信息后交给 agent。agent 在 knowledge 中查阅关于用户的信息，并把新学到的信息追加到 `knowledge/inbox.md`（按 `templates/exterior-session.md` 的说明）；每个回合结束后，exterior 提交并推送 knowledge 中的修改。

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
3. 在设备的根目录下准备 knowledge 目录，把本仓库 `templates/` 中的三个说明文件复制进去（`exterior-session.md`：每个话题启动时交给 agent 的说明；`exterior-global.md`：「全局」话题额外的说明；`exterior-test.md`：设备页「测试」按钮的问题）。按需修改，花括号中的占位符由 exterior 填入。
4. 在 Telegram 群组中新建话题并发送消息。

设备密码和 brief 密钥在 exterior 中加密保存。所有可调参数（名称、路径、时限、启动参数等）都在「设置」页中，清空即恢复默认值。

## 在 Mac / Windows 上运行

不使用懒猫微服时，exterior 可以直接运行在自己的 Mac 或 Windows 电脑上，agent 在同一台电脑上运行（设备地址填 `local`），也可以通过 SSH 连接其他设备。

需要：

- 已部署、可以从这台电脑访问的 brief（并已在其中建好 Telegram 机器人和群组）。同一个 brief 只能接一个 exterior，否则两边会互相抢消息。
- 这台电脑上装好要用的 agent（`claude`、`pi`、`codex`），以及 `git` 和 `curl`。
- Windows：Windows 10 及以上（自带 `curl.exe` 和 PowerShell）；连接远程设备还需要 OpenSSH 客户端（Windows 自带，可在「可选功能」中开启）。

启动：

1. 从本仓库的 GitHub Actions 或 Release 下载对应的压缩包（`exterior-macos-arm64`、`exterior-macos-x86_64`、`exterior-windows-x86_64`）并解压；或自行 `cargo build --release`。
2. Mac：双击 `run-mac.command`（第一次运行时 macOS 可能阻止未签名的程序，在 Finder 中右键选择「打开」）。Windows：双击 `run-windows.bat`。程序启动后会打开管理页面 `http://127.0.0.1:3000`，关闭终端窗口即停止 exterior。
3. 在管理页面中按「部署」一节的第 1–4 步设置。添加本机设备时地址填 `local`，根目录填一个本机文件夹（例如 `/Users/<用户名>/exterior` 或 `C:\Users\<用户名>\exterior`），不需要端口、用户和密码。
4. 把 `templates/` 中的三个说明文件复制到 `<根目录>/knowledge/` 中。knowledge 需要是一个 git 仓库，exterior 每个回合结束后提交并推送其中的修改。

数据和地址：

- 数据（设置、加密的密码和密钥、会话记录、日志）保存在 macOS 的 `~/Library/Application Support/exterior`、Windows 的 `%APPDATA%\exterior`（Linux 为 `~/.local/share/exterior`）。环境变量 `EXTERIOR_VAR` 可指定其他文件夹。
- 管理页面默认只在本机 `127.0.0.1:3000` 上可以访问，没有登录保护。环境变量 `PORT` 可改端口，`EXTERIOR_ADDR`（如 `0.0.0.0:3000`）可改监听地址；监听其他地址时，同一网络中的任何人都能打开管理页面。

本机设备：

- Mac：agent 和懒猫上一样，通过用户的交互式 shell（`$SHELL -ic`）启动，可以使用 shell 函数。
- Windows：agent 通过 PowerShell 启动（有 PowerShell 7 时用 `pwsh`，否则用系统自带的 Windows PowerShell），加载用户的 PowerShell 配置文件，因此可以使用其中定义的函数；启动参数通过环境变量传入，不经过 shell 解析。Windows 的命令行最长约 32000 个字符，启动说明不能超过这个长度。
- 文件的读写和 knowledge 的提交在本机直接进行，不经过 shell。

远程设备：

- 填了密码的设备通过 `sshpass` 输入密码登录（Mac 上可用 Homebrew 安装 `sshpass`）。没有填密码，或这台电脑没有 `sshpass`（Windows 通常没有）时，使用当前用户自己的 SSH 密钥登录（`BatchMode`，不会询问密码），需要事先把公钥加入设备的 `~/.ssh/authorized_keys`。设备页的「测试连接」会注明改用了密钥登录。
- 远程设备需要是 Linux 或 macOS（通过 sh 执行命令）。

与懒猫上的区别：

- 没有懒猫的登录保护和公网地址，Telegram 消息都经过 brief 收发，exterior 本身不需要从外部访问。
- `exterior send`（会话之间转发）通过 exterior 的 `/api/send` 发出。本机设备直接访问 `http://127.0.0.1:<端口>`；远程设备只有在「设置」中把「设备访问 exterior 的地址」（`self_url`）填成它能访问到的地址后才能使用。Windows 本机设备另装有 `exterior.ps1` 和 `exterior.cmd`，在 PowerShell、cmd 和 Git Bash 中都可以使用。
- `/exterior_account`（换 claude 账号）需要在设备上复制会话记录，Windows 本机设备不支持。codex 的 `/diff` 在 Windows 上需要 Git Bash 的 `sh` 在 PATH 中。

## 开发

```sh
cargo test                       # 单元测试（Windows 专用的测试只在 Windows 上运行）
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
| `src/host.rs` | 在设备上执行：SSH（密码或密钥）、本机（sh 或 PowerShell）、本机文件读写 |
| `src/brief.rs`、`src/store.rs`、`src/crypt.rs` | brief 接口、本地存储（数据文件夹的位置）、加密 |
| `run-mac.command`、`run-windows.bat`、`.github/workflows/build.yml` | Mac / Windows 的启动脚本；GitHub Actions 在三个系统上测试，并编译 Mac 和 Windows 版（推送 `v*` 标签时发布到 Release） |
