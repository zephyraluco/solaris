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

界面是完整的，并且完全离线可跑。solaris 自带一个确定性的 **mock 后端**，它会把一段预置的思考轨迹和一段 Markdown 回复逐块地流式发出，因此流式渲染、滚动、Markdown 排版与 token 统计都能在没有网络、也没有 API key 的情况下被完整地跑通。

> [!IMPORTANT]
> **solaris 目前是 v0.1.0，并且只带了 mock 后端。** `/connect` 向导会收集并保存凭据，也允许你选择 provider 与模型，但还没有实现任何 provider 客户端 —— 每一轮回答仍然由 mock 产生。请把 provider 目录理解为 API 应有的形状，而不是已经可用的接入。

> [!NOTE]
> **现在已经具备：**
>
> - 全屏 TUI：保留式组件树、类 flex 布局栈，以及模态 / 内联浮层
> - 流式对话记录，支持 Markdown 渲染与可折叠的思考块
> - 多行编辑器，支持按词移动、`/` 命令补全，以及命令面板
> - 两套配色主题，运行时循环切换
> - Build 与 Plan 两种模式，各有独立强调色与系统提示词
> - 一枚由用户 id 掷出的终端伙伴，可由你命名并持久化
> - 欢迎框，含吉祥物、最近活动与轮换提示
> - 会话统计 —— 轮次、token、费用与上下文占用
> - 凭据、伙伴与历史记录保存在平台配置目录下

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
| `--model <MODEL>` | 启动时使用的模型标识 | `solaris-mock-1` |
| `--theme <THEME>` | 启动时使用的配色主题（`dark`、`light`） | `dark` |
| `--plan` | 以 plan 模式启动，而非 build 模式 | 关闭 |
| `--chunk-delay-ms <MS>` | mock 后端每块流式输出的延迟（毫秒） | `24` |
| `--print-config` | 打印解析后的配置并退出，不启动界面 | — |
| `-h`, `--help` | 打印帮助 | — |
| `-V`, `--version` | 打印版本 | — |

`--print-config` 是查看 solaris 最终解析结果的最快方式 —— 模型、主题、模式、凭据所在位置、当前 provider 以及伙伴状态：

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

`/connect` 打开的是一个内联向导，它会接管输入区，而不是弹出一个浮层。向导会依次让你选择 provider，并填上该 provider 认证所需的内容：API key、OpenAI 兼容的 base URL 加 key、设备码登录，或者对于本机运行时什么都不用填。目录中涵盖 Anthropic、Claude 订阅、OpenAI、Google、Groq、OpenRouter，以及本地 Ollama/llama.cpp 运行时，并列出各自提供的模型。

凭据保存在 `auth.json` 中，并在任何展示处以掩码显示。如上所述，目前还没有实现 provider 客户端，所以连接 provider 并不会改变回答的来源。

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
