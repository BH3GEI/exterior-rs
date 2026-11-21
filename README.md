# exterior

把 Telegram 群组中的每个话题变成一个 agent 会话：在话题中发送的消息交给运行在你自己机器上的 agent（Claude Code、Codex 或 pi），agent 说的话按段发回同一话题：同一话题短时间内的几段合并成一条，所有消息排队发送，不会撞上 Telegram 的限流。

exterior 是单个 Rust 程序，可以直接运行在 Linux、macOS、Windows 上；也提供懒猫微服应用包。它通过 [brief](https://github.com/BH3GEI/wxbrief) 收发 Telegram 消息，在本机直接启动 agent，或通过 SSH 在其他设备上启动 agent。运行方式见「运行方式」。

## 工作方式

1. 在 Telegram 群组中新建一个话题并发送消息。brief 通过 Telegram Bot API 收到消息。
2. exterior 通过 brief 的 `/api/wait` 读取新消息，为每个话题在设备上建立一个文件夹，并在其中启动该话题的 agent（远程设备通过 SSH，本机设备直接启动）。agent 保持运行，多轮对话共用一个会话。
3. exterior 与 agent 通过标准输入输出交换 JSON：claude 使用 stream-json，pi 使用 rpc，codex 使用 app-server。agent 的回答在生成过程中持续写回话题。
4. 图片和文件：你发送的图片和文件保存到话题文件夹的 `files/` 中，并告知 agent 路径。agent 在回答中写 `![说明](路径)`，exterior 就把该文件按原位置发到话题中（代码块里的不算）。
5. 会话之间：agent 运行 `exterior send <话题名> <内容>`（exterior 装在设备的 `<根目录>/.exterior/bin` 中，并加入 agent 的 PATH），exterior 加上「名称 → 」署名后发到该话题，再转交给该话题的 agent。这一请求发往 exterior 的 `/api/send`，凭 `<根目录>/.exterior-key` 中的密钥访问；作为懒猫应用运行时，这是唯一不需要懒猫登录的地址。

6. 发送：每个话题有自己的发送顺序，一个话题发得慢不会耽误其他话题；所有话题共用一个限流队列（间隔见设置项 `tg_gap_ms`）。用户正在等的回答（话题的当前回合）优先于其他消息发出。队列拥堵时，仍在运行的话题把更长时间内说的话合并成一条（`busy_merge_s`）。思考、工具调用等折叠步骤和 agent 的回答分开成条，回答单独成一条消息。
7. 盯会话：agent 运行 `exterior watch <会话ID>`（会话ID 见 brief 的 `/api/chats`，例如以 `@chatroom` 结尾的微信群），exterior 自己向 brief 等这个会话的新消息，攒 20 秒（最多 2 分钟）一批，以「群名 → …」交给该话题的 agent；没有新消息时不打扰 agent。`exterior unwatch <会话ID>` 取消，`exterior watch` 列出正在盯的会话。exterior 本身不调用模型。
8. 定时消息：设置项 `schedules`，每条为 `HH:MM|话题名|内容`，多条用 `||` 隔开。exterior 每天到点（按所在机器的时区，到点后两小时内补发，重启不漏）把内容以「定时 → 内容」交给该话题的 agent，可用于每天整理 knowledge 等例行工作。

## 话题之间的协作

以下约定写在启动说明 `templates/exterior-session.md` 中，由 agent 执行，exterior 只提供 `exterior send` 和话题管理：

- 派任务：耗时较长或要动手的独立任务，agent 通过 brief 的 `/api/bots/user_topic` 开一个新话题，exterior 接管该话题并启动另一个 agent；用 `exterior send <话题名> <任务>` 交代背景、要求和交付物，对方完成后用 `exterior send <原话题> <结果>` 回报；完成后通过 brief 的 `/api/bots/release` 归档该话题。用户可以在新话题里看到全过程并插手。
- 话题笔记：每个话题文件夹中的 `NOTES.md` 记录目标、关键决定和进度。新会话开始、换 agent 或压缩上下文后，agent 先读它。
- knowledge：agent 把以后还用得上的信息追加到 `knowledge/inbox.md`（一行一条，「日期 · 话题名 · 内容」），不直接改 knowledge 的其他文件；可用定时消息让一个话题定期把 inbox 整理进对应文件。

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

## 运行方式

exterior 有两种运行方式，功能相同：

- 直接运行：在一台 Linux、macOS 或 Windows 电脑上运行 exterior 程序。
- 懒猫应用：作为应用安装在懒猫微服上。

两种方式都需要：

- 已部署、可以从运行 exterior 的机器访问的 brief，并已在其中建好 Telegram 机器人和群组。同一个 brief 只能接一个 exterior，否则两边会互相抢消息。
- 运行 agent 的设备：可以是运行 exterior 的这台电脑（设备地址填 `local`，直接运行时可用），也可以是能通过 SSH 登录的 Linux 或 macOS 设备。设备上装好要用的 agent（`claude`、`pi`、`codex`）和 `git`。

### 直接运行（Linux、macOS、Windows）

运行 exterior 的电脑上需要 `git` 和 `curl`；连接远程设备还需要 `ssh`（用密码登录时另需 `sshpass`）。Windows 需要 Windows 10 及以上（自带 `curl.exe` 和 PowerShell）；连接远程设备需要 OpenSSH 客户端（Windows 自带，可在「可选功能」中开启）。

1. 从本仓库的 GitHub Actions 或 Release 下载对应的压缩包并解压，或自行 `cargo build --release`：

   | 系统 | 压缩包 | 启动 |
   |---|---|---|
   | Linux x86_64 / aarch64 | `exterior-linux-x86_64.tar.gz`、`exterior-linux-aarch64.tar.gz`（静态链接，不依赖系统库） | 在终端中运行 `./run-linux.sh`，按 Ctrl-C 停止；常驻运行可用附带的 systemd 用户服务示例 `exterior.service`（使用方法写在文件开头） |
   | macOS Apple 芯片 / Intel | `exterior-macos-arm64.zip`、`exterior-macos-x86_64.zip` | 双击 `run-mac.command`（第一次运行时 macOS 可能阻止未签名的程序，在 Finder 中右键选择「打开」），关闭终端窗口即停止 |
   | Windows x86_64 | `exterior-windows-x86_64.zip` | 双击 `run-windows.bat`，关闭窗口即停止 |

2. 程序启动后打开管理页面 `http://127.0.0.1:3000`（Linux 在终端中显示地址，有桌面时自动打开浏览器）。
3. 「设置」：填写 brief 地址和密钥。
4. 「设备」：添加设备并点击「测试」。本机设备地址填 `local`，根目录填一个本机文件夹（例如 `/home/<用户名>/exterior`、`/Users/<用户名>/exterior` 或 `C:\Users\<用户名>\exterior`），不需要端口、用户和密码。添加 agent（名称、启动命令、类型），点击「测试」确认可用。
5. 在设备的根目录下准备 knowledge 目录（见下文「准备 knowledge」）。
6. 在 Telegram 群组中新建话题并发送消息。

数据和地址：

- 数据（设置、加密的密码和密钥、会话记录、日志）保存在 Linux 的 `$XDG_DATA_HOME/exterior`（默认 `~/.local/share/exterior`）、macOS 的 `~/Library/Application Support/exterior`、Windows 的 `%APPDATA%\exterior`。环境变量 `EXTERIOR_VAR` 可指定其他文件夹。
- 管理页面默认只在本机 `127.0.0.1:3000` 上可以访问，没有登录保护。环境变量 `PORT` 可改端口，`EXTERIOR_ADDR`（如 `0.0.0.0:3000`）可改监听地址；监听其他地址时，同一网络中的任何人都能打开管理页面。Telegram 消息都经过 brief 收发，exterior 本身不需要从外部访问。

本机设备：

- Linux、macOS：agent 通过用户的交互式 shell（`$SHELL -ic`）启动，可以使用 shell 函数（例如多账号的 `cc0`）。
- Windows：agent 通过 PowerShell 启动（有 PowerShell 7 时用 `pwsh`，否则用系统自带的 Windows PowerShell），加载用户的 PowerShell 配置文件，因此可以使用其中定义的函数；启动参数通过环境变量传入，不经过 shell 解析。Windows 的命令行最长约 32000 个字符，启动说明不能超过这个长度。
- 文件的读写和 knowledge 的提交在本机直接进行，不经过 shell。

远程设备：

- 填了密码的设备通过 `sshpass` 输入密码登录（Linux 用发行版的软件包安装，Mac 上可用 Homebrew 安装）。没有填密码，或这台电脑没有 `sshpass`（Windows 通常没有）时，使用当前用户自己的 SSH 密钥登录（`BatchMode`，不会询问密码），需要事先把公钥加入设备的 `~/.ssh/authorized_keys`。设备页的「测试连接」会注明改用了密钥登录。
- 远程设备需要是 Linux 或 macOS（通过 sh 执行命令）。

### 懒猫应用

```sh
cd pkg
./build.sh                      # 编译静态 Linux 程序，并打包所需的 Alpine 软件包（ssh、sshpass、git、curl）
lzc-cli project build
lzc-cli app install --apk n dist/bh3gei.lzcapp.exterior-v*.lpk
```

安装后打开应用页面，按「直接运行」的第 3–6 步设置（懒猫应用的容器中没有 agent，设备通过 SSH 连接）。应用的数据保存在 `/lzcapp/var`。

### 两种方式的区别

| 项目 | 直接运行 | 懒猫应用 |
|---|---|---|
| 管理页面的访问 | 默认只在本机 `127.0.0.1:3000`，没有登录保护 | 懒猫的应用域名，需要懒猫登录 |
| 数据位置 | 见上文「数据和地址」 | `/lzcapp/var` |
| `exterior send`（会话之间转发） | 本机设备访问 `http://127.0.0.1:<端口>`；远程设备只有在「设置」中把「设备访问 exterior 的地址」（`self_url`）填成它能访问到的地址后才能使用 | 所有设备访问懒猫的应用域名（`/api/` 不需要懒猫登录，凭密钥访问） |
| 定时消息的时区 | 运行 exterior 的电脑的时区 | 旧金山时间 |

Windows 本机设备另装有 `exterior.ps1` 和 `exterior.cmd`，`exterior send` 在 PowerShell、cmd 和 Git Bash 中都可以使用。`/exterior_account`（换 claude 账号）需要在设备上复制会话记录，Windows 本机设备不支持。codex 的 `/diff` 在 Windows 上需要 Git Bash 的 `sh` 在 PATH 中。

### 准备 knowledge

在设备的根目录下准备 knowledge 目录，把本仓库 `templates/` 中的三个说明文件复制进去（`exterior-session.md`：每个话题启动时交给 agent 的说明；`exterior-global.md`：「全局」话题额外的说明；`exterior-test.md`：设备页「测试」按钮的问题）。按需修改，花括号中的占位符由 exterior 填入。knowledge 需要是一个 git 仓库，exterior 每个回合结束后提交并推送其中的修改。

设备密码和 brief 密钥在 exterior 中加密保存。所有可调参数（名称、路径、时限、启动参数等）都在「设置」页中，清空即恢复默认值。

启动参数示例：设置项 `claude_args`（claude 的额外启动参数，按空格分隔）可以加上 `--disallowedTools`，限制 claude 可用的工具或 subagent 类型，例如 `--dangerously-skip-permissions --disallowedTools Agent(Explore)` 禁止使用 Explore 类型的 subagent（较旧的 claude 版本中该工具名为 `Task`）。

## 开发

```sh
cargo test                       # 单元测试（Windows 专用的测试只在 Windows 上运行；store::tests::lazycat_package 检查懒猫应用包的设置）
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
| `run-linux.sh`、`exterior.service`、`run-mac.command`、`run-windows.bat` | 直接运行时的启动脚本和 systemd 用户服务示例 |
| `pkg/` | 懒猫应用包（`build.sh`、`content/start.sh`、`lzc-manifest.yml`、`package.yml`） |
| `.github/workflows/build.yml` | GitHub Actions：在三个系统上测试，编译 Linux（x86_64、aarch64，静态 musl）、macOS 和 Windows 版，推送 `v*` 标签时发布到 Release |
