<div align="center">

<h1>SOLARIS</h1>
<h2><em>一个你可以继续搭建的终端 AI 助手</em></h2>

<p>
    <a href="https://github.com/zephyraluco/solaris"><img src="https://img.shields.io/badge/Built_with-Rust-CE4D2B?style=for-the-badge&logo=rust&logoColor=white" alt="使用 Rust 构建"></a>
    <a href="https://github.com/zephyraluco/solaris"><img src="https://img.shields.io/badge/Version-0.1.0-2E8B57?style=for-the-badge" alt="版本 0.1.0"></a>
    <a href="./LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue?style=for-the-badge" alt="MIT 许可证"></a>
</p>

<p>
    <a href="./README.md">English</a> · 简体中文
</p>

</div>

---

solaris 是一个用 Rust 从零写起的终端 AI 助手。它是一个全屏 TUI 聊天客户端 —— 流式对话记录、Markdown 渲染、一个好用的编辑器、浮层与主题 —— 并建立在一个小而分层的工作区之上：一个纯逻辑的领域 crate、一个后端抽象层，以及一个可复用的终端 UI 框架（本应用只是它的其中一个使用者）。

界面是完整的，接上 provider 之后真的会去调用它。接一个 provider（或者干脆只导出一个 API key），每一轮都会通过 SSE 从真实模型流式取回：首个 token 之前会重试，每一轮都会结算 token 与费用。

> [!IMPORTANT]
> **solaris 目前是 v0.1.0，接上 provider 之后真的会去调用它。** Anthropic、OpenAI、Google、OpenRouter、自建的 New API 网关，以及任何 OpenAI 兼容端点（包括本机的 Ollama 或 llama.cpp 运行时）都通过 SSE 真实流式返回：首个 token 之前会重试，每一轮都会结算 token 与费用。
>
> 没有任何离线替身：没有凭据时页脚写作 `unconnected`，发起一轮会直接告诉你该怎么办 —— 运行 `/connect`，或把 provider 的 key 放进环境变量。页脚始终标明当前回答的后端。

> [!NOTE]
> **现在已经具备：**
>
> - 目录所涵盖的三条 wire 协议都有真实客户端，按凭据自动选择
> - 全屏 TUI：保留式组件树、类 flex 布局栈，以及模态 / 内联浮层
> - 流式对话记录，支持 Markdown 渲染与可折叠的思考块
> - 多行编辑器，支持按词移动、`/` 命令补全，以及命令面板
> - 两套配色主题，运行时循环切换
> - Build 与 Plan 两种模式，各有独立强调色与系统提示词
> - 一枚由用户 id 掷出的终端伙伴，可由你命名并持久化
> - 欢迎框，含吉祥物、最近活动与轮换提示
> - 会话统计 —— 轮次、分类 token、费用与上下文占用
> - 凭据、伙伴与历史记录保存在平台配置目录下
>
> **尚未具备：** 工具调用、MCP、会话落盘，以及真实的设备码登录。

---

# 快速开始

## 环境要求

- **Rust 1.85 或更高版本** —— 工作区使用 2024 edition。
- 一个能渲染 24 位色的终端（Windows Terminal、iTerm2、Alacritty、Kitty 等）。

## 从源码构建

```bash
git clone https://github.com/zephyraluco/solaris.git
cd solaris

# debug 构建
cargo build

# 优化构建 —— 产物位于 target/release/solaris
cargo build --release
```

## 运行

```bash
# 在仓库内直接运行
cargo run --release

# 或直接运行构建好的二进制
./target/release/solaris
```

## 命令行参数

| 参数 | 说明 | 默认值 |
| --- | --- | --- |
| `--model <MODEL>` | 启动时使用的模型标识 | 所连 provider 的首个模型 |
| `--provider <PROVIDER>` | 本次运行改用该 provider，但不改动已保存的选择 | 当前 provider |
| `--theme <THEME>` | 启动时使用的配色主题（`dark`、`light`） | `dark` |
| `--plan` | 以 plan 模式启动，而非 build 模式 | 关闭 |
| `--print-config` | 打印解析后的配置并退出，不启动界面 | — |
| `-h`, `--help` | 打印帮助 | — |
| `-V`, `--version` | 打印版本 | — |

`--print-config` 是查看 solaris 最终解析结果的最快方式 —— 模型、主题、模式、凭据所在位置、当前由哪个后端回答、会往哪个端点发请求、凭据来自哪里，以及伙伴状态：

```bash
solaris --print-config
```

---

# 使用 solaris

输入框是界面上唯一的文本输入区。输入消息后按 `Enter` 发送，或输入 `/` 唤出命令列表 —— 按 `Tab` 可补全高亮的那条命令。

## 快捷键

| 按键 | 行为 |
| --- | --- |
| `Enter` | 发送输入 —— 或执行 `/` 下的命令 |
| `Alt+Enter` | 插入换行 |
| `Tab` | 切换 build / plan 模式 —— 或补全 `/` 命令 |
| `Ctrl+K` | 命令面板 |
| `Ctrl+T` | 循环切换主题 |
| `Ctrl+O` | 折叠 / 展开思考块 |
| `Ctrl+L` | 清空对话记录 |
| `PageUp` / `PageDown` | 滚动对话记录（鼠标滚轮同样可用） |
| `?` | 输入框为空时显示此帮助 |
| `Esc` | 取消当前打开的内容 |
| `Ctrl+C` | 复制选区、中断当前生成的回答 —— 连按两次退出 |
| `Ctrl+D` | 输入框为空时退出 |
| `Ctrl+V` | 从剪贴板粘贴 |

## 斜杠命令

| 命令 | 说明 |
| --- | --- |
| `/help` | 显示快捷键与命令 |
| `/connect` | 连接模型 provider |
| `/theme [dark\|light]` | 切换配色主题 |
| `/buddy [name <name>]` | 查看你的伙伴 |
| `/model [name]` | 切换当前模型 |
| `/mode` | 切换 build / plan 模式 |
| `/clear` | 清空对话记录 |
| `/stats` | 显示 token 与费用统计 |
| `/quit` | 退出 solaris |

## 模式

**Build** 与 **Plan** 是同一次对话，只是强调色不同、系统提示词里的一句话不同。`Tab` 或 `/mode` 在两者之间切换，`--plan` 让你直接以 plan 模式启动。

## 伙伴

每位用户都会有一枚终端伙伴，显示在欢迎框里，并会在若干待机帧之间做动画。它的*骨架* —— species、rarity、eyes、hat、shiny —— 由你的用户 id 经 FNV-1a 哈希确定性地掷出，因此你的伙伴是稳定的，也无法被手工篡改。它的*灵魂* —— 名字、性格与孵化时间 —— 会写入 `companion.json`，由你用 `/buddy name <name>` 来设置。

## Provider

`/connect` 打开的是一个内联向导，它会接管输入区，而不是弹出一个浮层。向导会依次让你选择 provider，并填上该 provider 认证所需的内容：API key、OpenAI 兼容的 base URL 加 key，或者对于本机运行时什么都不用填。目录中涵盖 Anthropic、Claude 订阅、OpenAI、Google、OpenRouter、自建的 New API 网关，以及本地 Ollama/llama.cpp 运行时。多数条目会列出自己提供的模型；网关与自定义端点则是让你自己填 URL 和 key，模型名由你指定。凭据保存在 `auth.json` 中，并在任何展示处以掩码显示。

三条 wire 协议就覆盖了整个目录。Anthropic Messages API 承载两个 Claude 条目；OpenAI Chat Completions API 是兼容性下限，Google、OpenRouter、New API、本机 Ollama/llama.cpp 以及任意自定义端点都走它；OpenAI 自己则使用更新的 Responses API。谁走哪条协议是目录表里的一列，不靠猜。

### 环境变量

环境里的 key 优先于 `auth.json`，而且只靠它就能跑：`ANTHROPIC_API_KEY=… solaris` 完全不需要 `/connect`。

| 变量 | Provider |
| --- | --- |
| `ANTHROPIC_API_KEY`、`ANTHROPIC_BASE_URL` | Anthropic |
| `OPENAI_API_KEY`、`OPENAI_BASE_URL` | OpenAI |
| `GOOGLE_API_KEY` 或 `GEMINI_API_KEY` | Google |
| `OPENROUTER_API_KEY`、`OPENROUTER_BASE_URL` | OpenRouter |
| `SOLARIS_LOCAL_BASE_URL` | 本机运行时（默认 `http://localhost:11434/v1`） |

### 预期行为

- **流式**。文本与思考随模型产出即时到达；流式过程中 `Ctrl+C` 会通过断开连接来取消本次请求。
- **重试**。`429`、`5xx` 或连接中断会退避重试最多三次，但**只在第一个 token 之前** —— 之后重试会重复输出，所以改为直接报告失败。
- **token 与费用**。`/stats` 会按输入、输出、缓存读 / 缓存写分类结算一个会话并计价。价格来自内置的牌价快照，请当作估算而非账单；目录里没有的模型按 0 计费。若服务端完全没有报告用量，这些数字是按字符估算的，`/stats` 会用 `≈` 标出。
- **prompt 缓存**。solaris 是**主动请求**缓存的：Anthropic 走 `cache_control` 断点（打在最后一条消息上），OpenAI 只在端点是 OpenAI 自家时发 `prompt_cache_key`。`/stats` 里那两行缓存数字之所以会有值就是因为这个；也是长对话每轮越来越便宜的原因 —— 不变的前缀会以缓存读的价格回来。
- **上下文**。`/stats` 分别给出 `session tokens`（本会话累计花掉的）和 `context used`。只有后者说明窗口有多满：它等于最后一轮上报的用量，加上其后新提交内容的估算量。把整个会话的轮次相加会无限增长，对「单次请求的窗口」毫无意义。
- **思考**。只有当上游真的推了推理内容（`thinking_delta`、`reasoning_content`、`reasoning`）时才会出现思考块；solaris 目前不下发 Anthropic 的 `thinking` 参数，因为不支持的模型会连带整个请求一起拒绝。
- **模型名。** 不指定时使用所连 provider 提供的第一个模型（空模型名 provider 不会接受），页脚会立刻显示出来；`--model` 或 `/model <name>` 指定过的名字则原样保留，哪怕目录里没有它。
- **错误。** key 被拒、模型不存在、触发限流，都会带上服务端自己的说法以及下一步该做什么。页脚始终显示当前回答的后端，所以没有凭据的会话会写 `unconnected`，而不会写出一个它从未调用过的 provider。

唯一的缺口是 `claude-subscription`：通过 OAuth 登录尚未实现，因此向导会直接说明这一点并要求你改用 API key。provider 条目、模型列表与凭据接缝都已就位，等这个流程落地即可接上。

---

# 配置

## 状态存放位置

solaris 会解析出一个统一的状态目录，优先级如下：

1. `$SOLARIS_CONFIG_DIR`（若已设置）—— 便于把临时会话与真实配置隔离开。
2. Windows 上为 `%APPDATA%\solaris`。
3. 其他平台为 `$XDG_CONFIG_HOME/solaris`，回退到 `~/.config/solaris`。

`--print-config` 会打印它实际解析到的路径。

## 文件

| 文件 | 内容 |
| --- | --- |
| `auth.json` | `/connect` 写入的凭据，以 provider id 为键，另含当前 provider 与模型。 |
| `companion.json` | 伙伴的灵魂：名字、性格与孵化时间。 |
| `recent.json` | 最近活动，用于欢迎框展示与提示轮换。 |

三者都是纯 JSON。文件缺失或不可读会被当作空处理，因此删掉其中一个只会重置那部分状态 —— 绝不会导致应用无法启动。

---

# 致谢

solaris 是从两个项目移植 / 借鉴设计的 Rust 实现，源码在相关位置都做了标注：

- [**claurst**](https://github.com/Kuberwastaken/claurst) —— 终端伙伴、两栏欢迎框、内联 `/connect` 流程，以及 provider 目录的形态，都沿用了它的设计。
- **`@earendil-works/pi-tui`** —— 保留式组件树的 TUI 设计（由应用持有焦点与浮层、类 flex 的布局栈）沿用了该库的做法；`solaris-tui` 中每个移植自它的模块都在文件头注明了出处。
