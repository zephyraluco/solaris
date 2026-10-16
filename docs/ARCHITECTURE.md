# solaris 架构

本文描述 solaris 的分层结构、运行时数据流与扩展点。内容与仓库代码一一对应，对应版本 `0.1.0`。

---

## 1. 总览

solaris 是一个用 Rust 编写的终端 AI 助手：一个全屏 TUI 聊天客户端，外带一套可复用的终端 UI 框架。仓库是一个 Cargo workspace（edition 2024，Rust 1.85+），由六个 crate 组成，依赖严格单向：

```text
solaris                  应用：根组件、对话框、/connect 向导、转录渲染、按键映射、CLI 入口
├── solaris-tui          可复用终端 UI 框架                        【无工作区依赖】
├── solaris-provider     平台层：provider 目录、鉴权、模型与价格，把凭据解析成一个后端
│   └── solaris-backend  传输层与对话词汇（消息、事件、工具声明、计费）  【无工作区依赖】
└── solaris-agent        agent 会话：多轮工具循环、审批钩子、模式与默认工具集
    ├── solaris-tools    工具层：八个内建工具、注册表、runner、截断      【无工作区依赖】
    ├── solaris-provider
    └── solaris-backend
```

三条硬性约束：

1. **依赖只能向下**。`solaris-backend` 与 `solaris-tools` 是两片叶子：它们互不依赖，也不依赖工作区里任何其他 crate。`solaris-provider` 建在 `solaris-backend` 之上——平台层必须知道「哪个模型讲哪种协议」；`solaris-agent` 把后端、工具与平台三者组起来；应用层只向上组合 `solaris-agent`、`solaris-provider` 与 `solaris-tui`。
2. **UI 不认识任何 provider，也不认识任何工具**。界面把一次请求交给 [`Agent`](../crates/solaris-agent/src/agent.rs)，只消费它回流的 `AgentEvent` 流；`solaris` 的依赖里既没有 `solaris-tools`，也没有 `solaris-backend`。
3. **传输层与工具层互不相识**。`solaris-backend` 不认识工具，`solaris-tools` 不认识模型、消息或 provider。把工具声明翻成某条 wire 的请求形状、把工具结果写回历史，都是 `solaris-agent` 的事。

| Crate | 目录 | 依赖 | 职责 |
| --- | --- | --- | --- |
| `solaris-backend` | [`crates/solaris-backend`](../crates/solaris-backend) | `async-trait`、`futures`、`tokio`、`reqwest`（rustls）、`bytes`、`serde`、`serde_json`、`thiserror` | 传输层，外加一轮对话的词汇：`AgentBackend` 统一接口、`HttpBackend`（三条协议的请求体与流解析）、SSE 解码、重试与计费；`message`（`Message`/`Content`/`Role`）、`event`（`AgentEvent`/`TurnRequest`/`BackendError`）、`tool`（`ToolSpec`/`ToolCall`/`ToolResult`）、`usage`（`Usage`/`Price`）。它只接收一个已经解析好的请求，因此既不认识平台也不认识工具。 |
| `solaris-tools` | [`crates/solaris-tools`](../crates/solaris-tools) | `async-trait`、`futures`、`tokio`（`process`/`io-util`/`fs`）、`serde_json`、`thiserror`、`libc`（仅 Unix） | 工具层：八个内建工具（`read`/`write`/`edit`/`ls`/`grep`/`find` 与所在平台的 shell）、`ToolRegistry`、命令 runner、截断与文件写入串行化、`edit` 的 diff。不依赖工作区里任何其他 crate。 |
| `solaris-provider` | [`crates/solaris-provider`](../crates/solaris-provider) | `solaris-backend`、`async-trait`、`serde`、`serde_json`、`thiserror` | 平台层：`providers`（provider 目录——认证方式、wire、端点、环境变量、每个模型的上下文窗口/最大输出/单价）、`choose`（`Credential` 解析成后端，`AuthStore`、`mask_secret`）。向导步骤、模型选择器与价格表都从这一张表读。 |
| `solaris-agent` | [`crates/solaris-agent`](../crates/solaris-agent) | `solaris-backend`、`solaris-tools`、`solaris-provider`、`async-trait`、`futures`、`serde`、`tokio` | agent 会话：`Agent`（把解析好的后端包进多轮工具循环）、`Mode` 与它声明给一轮的默认工具集、`Approver` 审批钩子，并把应用需要的词汇（消息、事件、工具、用量、注册表与选择）再导出。 |
| `solaris-tui` | [`crates/solaris-tui`](../crates/solaris-tui) | `ratatui`、`crossterm`、`unicode-width`、`unicode-segmentation` | 终端 UI 框架：组件模型、类 flex 布局、浮层、主题、按键匹配、组件集、终端生命周期。 |
| `solaris` | [`crates/solaris`](../crates/solaris) | `solaris-agent`、`solaris-provider`、`solaris-tui` + `ratatui`、`crossterm`、`futures`、`anyhow`、`clap`、`tokio`、`serde`、`serde_json`、`thiserror` | 应用本体，同时产出库与 `solaris` 二进制；斜杠命令表、偏好设置、伙伴与提示语也住在这里。 |

---

## 2. solaris-backend：传输层与对话词汇

### 2.1 一轮对话的词汇

传输层同时持有「一轮对话是用什么写的」——这些类型正是三条 wire 要序列化的东西，所以它们住在讲协议的同一个 crate 里，而不是另起一层：

| 模块 | 内容 |
| --- | --- |
| `message` | `Message` / `Role` / `Content`：一条消息是**内容块列表**——`Text`、`ToolUse`、`ToolResult`。工具结果放在 user 的 `Content::ToolResult` 里，正是 Messages API 要求的位置；纯文本消息只有一个 `Text` 块，所以不碰工具的路径与从前一样。 |
| `event` | `AgentEvent`、`TurnRequest`、`BackendError` —— 后端与其调用方之间的唯一契约。 |
| `tool` | `ToolSpec`（下发给模型的声明）、`ToolCall`（模型的请求）、`ToolResult`（执行结果）。纯数据：谁声明工具是 `solaris-agent` 的事，谁执行调用是 `solaris-tools` 的事。 |
| `usage` | `Usage` / `Price`：四个互不重叠的 token 桶（普通输入、输出、缓存读、缓存写）外加「这是估算」标记；`Price` 是 USD / 百万 token 的单价。`Usage::merge` 把一轮里的多次模型调用加起来（工具往返就是这种情况）。 |

斜杠命令表、偏好设置、伙伴与提示语不属于「一轮对话的词汇」，它们住在应用层（`solaris` 的 `commands` / `config` / `buddy` / `recent` / `tips`，见 §6.1）；模式 `Mode` 属于 agent（见 §3）。

`AgentEvent` 是流式协议的核心：

```rust
enum AgentEvent {
    ThinkingDelta(String),                    // 思考轨迹分片
    TextDelta(String),                        // 可见回复分片
    Status(String),                           // 瞬时状态行
    ToolCall(ToolCall),                       // 模型请求一次工具调用（非终态）
    ToolResult { result: ToolResult, duration_ms: u64 }, // 这次调用的结果（非终态）
    OutputTruncated,                          // 回复撞上输出上限，可能带着残缺的调用（非终态）
    TurnComplete { usage: Usage, cost_usd: f64 }, // 终态：成功
    Error(String),                            // 终态：失败
}
```

`TurnRequest { history, prompt, tools }` 是输入侧：历史（含一条描述当前模式的 system 消息）、本轮提示词、本轮声明的工具。**模式不在里面**：一轮能用哪些工具是 agent 在构造请求之前就定下的事（见 §3），到传输层时 `tools` 已经确定。

**`prompt` 为空时协议层不追加 user 消息**：这是给工具往返用的——第 2 轮起，用户的提示词已经在 `history` 里了，接着往下写的是「助手请求调用 → 工具结果」这一串，而不是把同一句话再问一遍。

### 2.2 协议与客户端

```rust
pub type AgentEventStream = Pin<Box<dyn Stream<Item = AgentEvent> + Send>>;

#[async_trait]
pub trait AgentBackend: Send + Sync {
    async fn run_turn(&self, request: TurnRequest) -> Result<AgentEventStream, BackendError>;
    fn label(&self) -> &str { "backend" }   // 状态栏显示
}
```

- 一次 `run_turn` 返回一个装箱的 `Send` 流，UI 侧通过 channel 逐块消费。
- **`HttpBackend`** 是真实客户端。它只认识 `Wire`，于是三条协议各一个模块，统一放在 [`protocols/`](../crates/solaris-backend/src/protocols) 下：
  - [`anthropic`](../crates/solaris-backend/src/protocols/anthropic.rs) —— Messages API（`POST /v1/messages`，`x-api-key` + `anthropic-version`）。注意 system 提示词必须提到顶层 `system` 字段，不能留在 `messages` 里，所以历史里的 system 条目会被摘出去。另外最后一条消息会带一个 `cache_control: {type:"ephemeral"}` 断点：缓存是逐块开关的，断点会把它之前的一切（系统提示词 + 已发出的轮次）缓存下来，于是第二轮起不变的前缀按缓存读计价。低于 provider 的最小可缓存长度时它会被忽略，短对话不多花钱。
  - [`openai`](../crates/solaris-backend/src/protocols/openai.rs) —— Chat Completions（`POST /chat/completions`，Bearer），兼容性下限。New API、OpenRouter、Google 的兼容端点、本机 Ollama 与 llama.cpp 走的都是这条。因为「OpenAI 兼容」描述的是**响应**形状而请求字段各家不同，这个模块还带一张**修补表**：服务端用 400 指名它不认识的字段（`stream_options`、`max_tokens`）时，改一次请求体重发，而不是预先猜——每条修补每轮最多应用一次。
  - [`responses`](../crates/solaris-backend/src/protocols/responses.rs) —— Responses API（`POST /responses`，Bearer），OpenAI 自己走这条。system 提示词与 Messages API 一样提到顶层，但字段名是 `instructions`，转录是 `input` 条目列表，长度上限叫 `max_output_tokens`（不是 `max_tokens`）。请求带 `store: false`：终端会话不该把转录留在服务端。它比 Chat Completions 宽进严出——每条事件既在 `event:` 上命名、又在负载的 `type` 里重述一遍，所以网关丢掉前者也读得出来。
  - 顺带：`reasoning_content` / `reasoning`（Chat Completions）与 `response.reasoning_text.delta` / `response.reasoning_summary_text.delta`（Responses）若出现就映射成思考分片，不出现就跳过；`thinking` 参数本身不下发，因为不支持的模型会直接 400。
- 三条协议共用的部分：
  - [`sse`](../crates/solaris-backend/src/sse.rs) —— 跨 chunk 的 SSE 帧解码，**按字节**缓冲，所以帧切在多字节字符中间也不会损坏。
  - [`http`](../crates/solaris-backend/src/http.rs) —— 共享的 `reqwest` 客户端（10s 连接超时，无整请求超时，因为一轮本来就要流几分钟）、状态码 → 可操作的错误文案、退避重试。
  - [`wire`](../crates/solaris-backend/src/wire.rs) —— 按 `Wire` 分发请求体构造与流解析。跨协议但**只有 OpenAI 需要**的字段在这里补上：`prompt_cache_key` 只在 `Wire::is_openai()` 且端点主机就是 `api.openai.com` 时发送（OpenAI 的缓存本来就是自动的，这个键只影响路由，而兼容网关可能因为不认识它而拒掉整个请求，不值得冒险）。
  - **工具调用**：请求体里的 `tools` 只在调用方声明过工具时出现——空列表连字段都不写，所以不用工具的那条路径逐字节不变。三条协议各写各的形状（Anthropic 是 `input_schema`，Chat Completions 在 `function` 里包一层，Responses 是扁平的 `name`/`parameters`）。**调用一律在流结束时一次性交付**：三条 wire 都把一次调用拆成片段（名字先到，参数作为 JSON 文本分片随后抵达），解析器因此各自按索引或条目累积，只在流末 `flush` 出去，被执行的总是一个完整调用；流中途报错则不交付。历史里的 `ToolUse`/`ToolResult` 也按对方形状转换：Chat Completions 拆成 `tool_calls` 与 `role:"tool"` 消息，Responses 拆成 `function_call`/`function_call_output` 条目，Messages API 把结果并进紧随其后的 **user** 消息，并在合并同角色消息时把 `tool_result` 排在最前——那边的 API 两件事都不允许：角色不能重复，结果必须领起它所在的那条消息。
  - 参数文本解析不出 JSON 时进 `null`，工具层会把它当作「参数不是对象」回给模型，而不是静默地当成空参数——一个能自我纠正的模型需要知道它写错了什么。
- **模型发现**：`AgentBackend::models()` 是可选能力（默认实现直接报「本后端不会列模型」），`HttpBackend` 用 `Endpoint::models_url()`（Messages API 走 `/v1/models?limit=1000`，OpenAI 兼容走 `/models`）发一次 GET，复用同一套认证；`parse_models` 从 `{"data":[{"id":…}]}` 里取 id——两条 wire 的清单形状相同。**不重试**：它只喂选择器，慢或不可达的代价应当是退回目录，而不是让用户等。**网关的 `/v1` 在解析层补齐**：OpenAI 兼容网关的 API 挂在 `/v1` 下，而 `/connect` 收集来的 URL 常常只写到主机，于是请求打到的是网关的网页前端——它用 200 + HTML 回答，`parse_models` 读不出任何 id，于是「没有模型」而不是一个看得见的错误。所以 `resolve()` 会给没有路径的 base URL 补上 `/v1`（带路径的一律按原样使用：只有部署者知道 API 挂在哪），Messages API 不在其列——它的 base 就是主机本身。**200 但不成清单的回复是一种要报出来的失败**：`parse_models` 返回 `Option`，`None` 表示这压根不是清单（代理的 HTML 页、被包进 200 的错误），此时错误信息指名 URL 并转述网关自己的说法，而不是当成一个没有模型的 provider。真正空的清单（`{"data":[]}`）则是答案——网关在说这个凭据够不到任何模型；没有目录可退时这一条也会告知用户。这两者必须分得开，否则「URL 打错地方」和「网关不给模型」看起来一模一样。
- **重试只在第一个事件发出之前**（429 / 5xx / 传输错误；指数退避并尊重 `Retry-After`，最多 3 次）。已经吐过字就绝不再试，否则会重复输出。中断靠丢掉接收端：app 放弃 `rx` 后 `send` 失败、任务结束、`reqwest` 的流随之被 drop，请求被取消。
- **计费**：`Usage` 把各家的报告归一成四个互不重叠的桶（两家 OpenAI 协议都把缓存读计入 `input_tokens` / `prompt_tokens`，会被减掉）；提供商什么都没报时退回按字符估算并置位 `estimated`，`/stats` 会据此显示 `≈`。价格来自 `provider` 表的内置快照，未知模型不收费。生成的 token 还是 0 的 `usage` 块不算「报告过」。
- **上下文 vs 会话总量**：`SessionState::total_tokens()` 是全会话累计（帧脚把它和累计费用并排显示），`SessionState::context_tokens()` 才是「窗口有多满」——取最后一轮有上报的 usage，加上其后新提交内容的估算；没有任何上报时退化为对提示词取估算。把累计量拿去比窗口是错的，因为窗口是每请求的，而累计会无限增长。
- **没有可用凭据时**，`solaris-provider` 的 `UnconnectedBackend` 接管：它不发任何请求，而是把每一轮直接变成一条可操作的错误（「run /connect」，或指名该 provider 的环境变量），页脚也会写 `unconnected`。这样既不会假装有回复，也不会发一个必然 401 的请求。
- [`solaris-provider/src/choose.rs`](../crates/solaris-provider/src/choose.rs) 的 `choose_backend(auth, model, options)` 是解析凭据的入口（应用通过 `backend_factory` 调它，`solaris` 的 `AppOptions::new()` 则把这个工厂钉成一个固定后端）：按「环境变量 > `auth.json` > 本机运行时无需凭据」解析出一个 `BackendChoice`（后端 + provider id + 端点 + 凭据来源）。解析不出可用凭据就交给 `UnconnectedBackend`（`CredentialSource::Missing`）。模型名为空时（用户还没选过模型）换成该 provider 提供的第一个模型；用户明确给过的名字则原样送出。**目录里也没有名字时**（网关与自定义端点就是这样）凭据一样解析出端点，只是包成 `NamelessBackend`：turn 照旧拒绝——空模型名只会换来 400——但 `models()` 直通真正的客户端，因为那份清单正是名字的唯一来源，否则 `/model` 会永远空着（`CredentialSource::NoModel`，provider id 仍然给出来）。`BackendOptions` 里的 `environment` 可注入，测试因此既不继承 shell 里的 key、也碰不到网络。
- `AgentBackend` 是唯一扩展点：再实现一个即可接入新协议，UI 零改动。

---

## 3. solaris-agent：agent 会话

模型只能「请求」，真正把「请求 → 执行 → 再请求」串起来的是这一层。它同时是应用唯一需要认识的 crate：`Agent` 包住一个已经解析好的后端，持有整段对话，按模式决定一轮声明哪些工具，跑完本轮的调用，把结果写回对话，再问一次。

```rust
pub struct Agent {
    choice: BackendChoice, registry: Arc<ToolRegistry>, options: AgentOptions,
    cancel: Cancel, conversation: Arc<Conversation>, steering: Queue, follow_up: Queue,
}

impl Agent {
    pub fn new(choice: BackendChoice, registry: Arc<ToolRegistry>, options: AgentOptions) -> Self;
    pub fn cancel_handle(&self) -> Cancel;
    pub fn steering_handle(&self) -> Queue;      // 纠正正在跑的这一轮
    pub fn follow_up_handle(&self) -> Queue;     // 这一轮之后另起一轮
    pub fn clear(&self);                         // /clear
    pub fn conversation_len(&self) -> usize;
    pub async fn compact(&self) -> Option<usize>;  // /compact
    pub fn run_turn(&self, prompt: impl Into<String>, mode: Mode) -> AgentEventStream;
}
```

### 3.0 对话由 agent 持有

**这是这一层最重要的一条不变量**：模型看到的那段对话住在 `Agent` 里（[`conversation.rs`](../crates/solaris-agent/src/conversation.rs)），不是应用每轮递给它、也不是从转录里重建出来的。

理由有三条，都是实际踩过的：

1. **摘要必须活过它所在的那一轮。** 早先的实现在一轮内部压缩，轮次一结束结果就被丢掉，于是下一轮又把整段没压缩的对话发出去、再付一次摘要费。
2. **插话落在哪里是事实，不是可以重建的东西。** 消息要待在被模型读到的位置；从「一轮的提示词 + 一串工具步骤」反推插入点需要额外记录，而直接留着根本不需要。
3. **模型自己的话也是对话的一部分。** 转录是给用户看的（轮次、工具步骤、耗时、思考），对话是给模型看的（消息与内容块），两者形状不同：转录里没有「助手说了什么」这一等公民，而对话里必须有。

`Conversation` 只做四件事：`snapshot()`（构造一次请求，复制而非持锁跨越 await）、`push`、`replace_with`（压缩用）、`repair()`（一轮被取消或失败时收回没人应答的工具请求——三条 wire 都不接受没有结果的调用）。它**不含 system 消息**：提示词每轮现写（§3.1），所以压缩永远碰不到指令。

`Agent::run_turn(prompt, mode)` 只收新的话：提示词在循环开始前就写进对话，因此一轮即使被取消或失败，用户说过的话也留下来了。`/clear` 是唯一会忘记它的事——而且必须同时忘记转录，否则下一轮会回答一段用户已经看不见的对话。

### 3.1 提示词系统

系统提示词每轮从当前资源快照现写一次（[`prompt.rs`](../crates/solaris-agent/src/prompt.rs)）：

```text
You are solaris, a terminal assistant. …
# Working directory   ← AgentOptions.cwd：实际工作目录
# Working mode        ← Mode::instructions()：这一轮能做什么、不能做什么
# Using the tools     ← 每个工具自己的 Tool::guideline()，只列这一轮声明的
# Skills              ← 每个技能的名字、描述与 SKILL.md 路径，说明留在文件里
# How to work         ← 调查先于修改、最小改动、不重排无关代码、失败要读原因
# This endpoint       ← ProviderSpec::prompt，仅当平台确实需要不同措辞
# Project instructions ← 全局规则，再按祖先到工作目录排列的项目规则
# Additional instructions ← 用户与项目 APPEND_SYSTEM.md
```

四个设计要点：

- **工具自带指南**。`Tool::guideline()` 返回 `Option<&'static str>`，默认 `None`。`description` 决定模型**要不要**用它，指南决定用得**好不好**。放在 trait 上而不是集中查表，是为了让 `ToolRegistry::with_tools` 注册的自定义工具也能自带指南。
- **只写这一轮存在的工具**。Plan 模式撤掉 shell / `write` / `edit` 之后，提示词里也不提它们（§3.2）。
- **技能只列清单，不列内容**。`Skills::prompt_section` 只写名字、描述与 `SKILL.md` 的绝对路径，说明本身留在文件里等模型去读 —— 把每个技能的说明都塞进提示词，就是在为可能永远不做的活儿预先付上下文。要读文件就得有读文件的工具，所以这一段只在**这一轮真的声明了 `read`** 时才点名它（[`skills.rs`](../crates/solaris-agent/src/skills.rs)）。
- **平台的事实不重复**。端点、模型 id、工具 schema 都在请求体里，写进提示词就是第二个可能不一致的来源；只有**改变模型该被如何对待**的事才写（网关可能路由到任何模型家族、本机跑的是小模型、自建端点行为由部署者决定）。

[`resources.rs`](../crates/solaris-agent/src/resources.rs) 的 `ResourcePaths::load` 生成 `ResourceSnapshot`，包含技能、`PromptContext` 和诊断。规则同目录按 `AGENTS.override.md` → `AGENTS.md` → `CLAUDE.md` 选择一个，项目祖先遍历止于仓库根，无仓库时止于文件系统根。追加说明来自用户配置目录及工作目录 `.solaris/APPEND_SYSTEM.md`，保留来源路径。`/reload` 在空闲边界通过 `Agent::replace_resources` 替换快照、更新补全，不重建 Agent，也不清空对话；活动 stream 或压缩结束前延后应用。

### 3.2 模式与默认工具集

```text
Mode::Build  →  read + shell + edit + write
Mode::Plan   →  read + grep + find + ls
```

`Mode`（`label()` 给状态栏徽章，`next()` 用于 Tab 切换）与这张表都住在 `solaris-agent`：哪些工具算「只看」是决定一轮声明什么的那一层知道的事，`solaris-tools` 只提供工具本身。`Mode::instructions()` 与这张表住在同一个文件里不是巧合——撤掉一个工具却不在提示词里说明，模型会把「工具不见了」读成「工具坏了」，转而描述它做不了的那次修改。

`ToolSelection{ only, add, remove }` 在其上做增删（`--tools` / `--exclude-tools`）：`only` 是白名单，替换默认集合；整份列表只由 `+name`/`-name` 组成时改为在默认集合上增删（两种写法不能混用，混用直接报错而不是猜）；条目可以是 `*` 模式；`ToolRegistry::unmatched()` 会报出匹配不到任何工具的名字。

### 3.3 一轮的流程

1. **prompt 先写进对话**，在任何请求之前：一轮被取消或失败时，用户说过的话仍然留下。
2. 排空 `steering`（§3.5）并追加为 user 消息。「先写 prompt」这一步在这里才有意义：插话必须排在它要纠正的那句话**之后**。
3. 预算超了就压缩（§3.6），用上一次 provider 报的 usage 判断。
4. 构造本轮 `TurnRequest`（`history` = system 提示词 + 对话快照，`prompt` 留空——它已经在对话里了），调内层后端；转发 `ThinkingDelta`/`TextDelta`/`Status`/`ToolCall`/`OutputTruncated`，拦下 `TurnComplete` 以累计用量与费用。
5. **流结束时必须已经收到 `TurnComplete`**：只收到一半事件就断流的回合算失败（终态错误），不会把半截回答当成完整回答报成功。
6. 本轮没有工具调用 → 记录模型这句话（空话不记，部分 wire 拒绝空文本块）→ 再排空一次 `steering`（模型回答期间到达的插话就是「别结束，改成这个」）→ 再排空一次 `follow_up`（§3.5）；有一个非空就继续，都空才发自己的 `TurnComplete`（累计后的数字）并结束。
7. 有调用 → 跑这一批调用（见 §3.4），把 `assistant(tool_use…)` 与 `user(tool_result…)` 追加进对话，回到第 2 步。

**窗口已满的恢复**：第 4 步的请求可能被明确拒绝（`AgentEvent::Error` / `BackendError`），也可能被接受却悄悄截断输入。前者按一批跨平台的措辞识别（[`overflow.rs`](../crates/solaris-agent/src/overflow.rs)，见 §3.6），后者由 provider 自己报的 usage 看出来。两条路都走同一处理：**强制压缩一次，然后把同一轮重新问一遍**——只重试一次，且压缩关掉时就直接把错误交给用户。这把「上下文过长」从一个用户无能为力的错误变成了一个能自己收场的回合。

**没有轮数上限**：模型一直问到它不再请求、出错、或调用方取消为止。因此「取消」是一轮花销的唯一上界，这也是为什么循环在每一轮开始前、以及每轮调用跑完之后都要检查一次。这里**不是**恢复了 pi 的 `while (true)` 就完事：先前的 24 轮上限会在一次正常的长时间 agentic 工作（反复读—改—跑）中途把回合砍掉，那代价高于它防住的跑飞风险；取消则是用户看得见、随时可用的那条线。

### 3.4 一轮内的并行与取消

**调用默认并行**：上限是 `max_parallel`（`DEFAULT_MAX_PARALLEL = 4`），结果按模型请求的顺序回填给模型；只要该批里任一工具声明了 `execution_mode() == Sequential`（`write`、`edit` 与 shell 都是），整批退回逐个执行——写入类工具的按文件锁保证的是不互相踩，串行保证的是「同一批里能读到刚写下的东西」。

**被输出上限截断的调用不执行**：协议层把 `stop_reason` 为 `length` / `max_tokens` / `incomplete` 变成 `AgentEvent::OutputTruncated`，循环据此拒绝这一批全部调用，并告诉模型「撞到输出上限、参数可能残缺，请重新完整给出」；残缺参数被解析成 `null` 时那句含糊的「参数不是 JSON 对象」不再是唯一线索。

**取消按轮次隔离**：`Cancel` 在每次 `run_turn` 开始时 `reset()`，上一轮的取消不会波及下一轮；应用层持有 `Agent::cancel_handle()`，`Ctrl+C` 因此能杀掉正在跑的命令，而不只是不再读事件。循环在独立任务里跑，返回的流被丢弃（用户取消）就等于取消这一轮——任务发现发送端已关闭便不再继续。

### 3.5 两个队列：steering 与 follow-up

一轮可能跑很久，而用户想说的话有两种，它们不是同一件事：

| | steering | follow-up |
| --- | --- | --- |
| 意思 | 「不对，是另一个文件」 | 「这轮弄完再跑测试」 |
| 何时被读 | 每轮开始前，以及模型看似答完时 | 这一轮真的要结束时 |
| 落在哪里 | 并进正在跑的这一轮 | 成为**下一轮**（转录里另起一个 Turn） |

```rust
pub struct Queue { /* Arc<Mutex<Vec<String>>> */ }
impl Queue {
    pub fn push(&self, message: impl Into<String>) -> bool;  // 空白消息不入队
    pub fn drain(&self) -> Vec<String>;                      // 取走，只取一次
}
```

同一个类型，两个实例（`Agent::steering_handle()` / `follow_up_handle()`）——区分它们的是**agent 何时去读**，多一个类型只会把这四个方法再抄一遍。

- **共享队列而不是 channel**：句柄的生命周期比任何一轮都长（应用整个会话持有一份），而排空它的一方只活一轮，并且必须能**不阻塞地看一眼**。
- **steering 的第二次排空是关键**：模型答完最后一段话时再读一次，否则「别结束了，改成 X」会被丢在刚结束的那一轮里。
- **follow-up 让循环继续跑**：它不是「等这一轮结束应用再发一次请求」，而是同一个循环里接着下一轮，所以对话是接着长的。循环为此报 `AgentEvent::FollowUp { prompt }`，应用据此把上一轮标记完成、并为这个提示词新开一个 Turn——从这一刻起流出来的东西都属于它。
- **记录由循环回报**：`AgentEvent::Steered { text }` 在消息**真正被读到**时发出，应用据此写进当前轮的 `Turn.steering`（附 `after_steps`，即当时已完成的调用数）。如果按「用户按下回车时」记录，那么刚好在模型收尾时到达的插话——读到它的可能是**下一轮**——就会在转录里挂错轮次。
- **显示位置**：[`state.rs`](../crates/solaris/src/state.rs) 用 `after_steps` 把消息画在它当时的位置（`↳ steered …`），因为「纠正落在哪里」才是这件事的重点。

应用侧的用法：流式期间直接回车 = steering（默认，也是最常用的）；`/queue <prompt>` = follow-up。后者用命令而不是快捷键，因为 `alt+enter` 已经被换行占用，而命令表顺带让 `/help` 与补全都能发现它。

### 3.6 上下文压缩

长会话必然撞窗口，而撞了之后没有降级路径就是把错误留给用户（`context_window` 从前只喂给页脚的占比条）。现在由 agent 自己管：

```rust
pub struct Compaction { pub enabled: bool, pub reserve_tokens: u32, pub keep_recent_tokens: u32 }
// 默认：enabled = true、reserve_tokens = 16384、keep_recent_tokens = 20000
```

**两个数都是绝对量，不是窗口的比例**，这是刻意的：

- `reserve_tokens` 是留给回复的空间。回复不会因为窗口变大而变长，所以按比例留是错的单位——1M 窗口留十分之一下来，就是 100k 永远用不到的空间；小窗口留十分之一，可能还不够一次回答。
- `keep_recent_tokens` 是永不摘要的那段最近的对话。用 token 而不是条数同理：8 条可能是一段话，也可能是一整个文件。

触发条件是 `context_tokens > context_window - reserve_tokens`，而 `context_tokens` **优先取 provider 上一轮自己报的 usage**（输入+输出四桶之和），估算是退路。用真数是因为估算会偏，而且偏多少不可知。

其余的规则：

- **系统提示词不参与**：它不在对话里（§3.0），所以摘要永远碰不到指令。
- **裁切点只落在安全边界**：从尾部往回按 token 预算累加，然后向前跳过 `is_tool_results()` 的消息——一条没有前置 `tool_use` 的工具结果，三条 wire 都会拒收（[`compaction.rs`](../crates/solaris-agent/src/compaction.rs) 的 `cut_point`）。
- **已有摘要就「更新」而不是「重写」**：第一次用 `COMPACT_PROMPT`（固定的 Goal / Constraints / Progress / Key Decisions / Next Steps / Critical Context 六段），之后用 `UPDATE_COMPACT_PROMPT`，明确要求「保留摘要里已有的一切，只加新的」。**否则每次压缩都是「对上上次摘要的摘要」，开头的信息一代代衰减。**
- **摘要是固定结构的，不是散文**：读它的模型要据此**行动**——Goal 与 Next Steps 是它接手的部分，Critical Context 是它无法从文件里重新读出的部分，Done/In Progress/Blocked 的三分法防的是它重做已完成的工作。自由散文总会丢掉其中一段，丢哪段不可预测。
- **摘要调用是最朴素的一轮**：单独发提示词 + 对话文本，声明零工具，它的输出被就地读走而不转发（调用方要的是一轮的回答，不是沿途的笔记）。它**观察本轮取消**：一段长对话的摘要不值得在用户已经放弃之后还等完。
- **摘要失败就原样发送**：宁可让模型读得多，也不能因为摘要崩了就把对话丢掉；此时不发 `Compacted` 事件，所以界面上也不会声称压过。
- **窗口未知（0）什么都不做**：没人报过窗口大小时，按一个不存在的预算丢对话是纯损失。

压缩发生时报 `AgentEvent::Compacted { dropped }`：应用弹一条通知并在转录里留一行（`⌁ N earlier messages summarized to make room`）。突然「记性变差」必须能追溯到原因，否则它看起来就像模型退化了。

`/compact` 走同一条路但不带强制与重试：没有要保护的回合，所以对话里没有值得摘要的东西时直接回答「没有可摘要的内容」，而不是白打一次模型调用。应用侧它在事件循环之外跑（和模型发现同一套 channel 手法），完成后报出压掉了几条、还剩几条。

### 3.7 审批

```rust
pub enum Approval { Approved, Denied }
#[async_trait] pub trait Approver { async fn approve(&self, call: &ToolCall) -> Approval; }
```

异步是必须的：循环跑在后台任务里，提示要送到画屏幕的地方，答案要再送回来。`ChannelApprover` 就是这条通路——`approve()` 把 `ApprovalRequest { call, reply }` 交给 UI，然后等一个 `oneshot`；UI 已经不在了就按拒绝处理，失败方向要安全。

`App` 侧只读四件套（`read`/`grep`/`find`/`ls`）直接放行，其余弹一个 `ConfirmDialog`，答案经 `dialog_rx` 回到等待中的调用。默认仍是 `AlwaysApprove`；`--confirm-tools` 打开后才换成 `ChannelApprover`。被拒绝的调用会作为错误结果回到模型，而不是中断这一轮。

### 3.8 对应用层的透明性

`solaris` 从不 `use solaris_tools`，也从不 `use solaris_backend`：它需要的词汇（消息、事件、工具调用与结果、用量、注册表与选择）由 `solaris-agent` 再导出。应用侧因此几乎不必为工具改动：`App` 的 turn 流程、channel 与事件循环照旧，只是多消费两种事件。

### 3.9 技能

一段专门针对某类工作的说明，放在一个装有 `SKILL.md` 的目录里 —— 前面是 frontmatter（名字、描述），后面是说明本身，旁边通常还有脚本与参考资料。格式遵循 [Agent Skills](https://agentskills.io/specification) 规范，因此为其它 harness 写的技能无需改动即可使用（[`skills.rs`](../crates/solaris-agent/src/skills.rs)）。

装载分两步，这也是「技能」值得存在的全部理由：

1. **开机扫描，只把清单写进提示词**（§3.1 的 `# Skills` 段）：名字、描述、`SKILL.md` 的绝对路径。模型据此判断某个技能是否对得上手头这件事。
2. **要用时才读**。模型自己用 `read` 打开那个文件；用户也可以用 `/skill:<name> [request]` 直接点名，此时说明会作为这一轮的提示词送出去，`request` 追加在后面，而转录里显示的仍是那行调用本身。

几条边界：

- **发现规则照规范来**：含 `SKILL.md` 的目录就是一个技能，不再往里找；只有被配置的顶层目录里散落的 `.md` 才可能自成技能，且必须有描述。隐藏目录与 `node_modules` 跳过。
- **冲突与失格都不致命**：同名保留先发现的（目录顺序即优先级，用户先于项目），没有描述的不装载，其余问题（名字不合规范、描述过长）只报告不拒绝。全部经 `SkillWarning` 报出，由 [`main.rs`](../crates/solaris/src/main.rs) 打到 stderr。
- **frontmatter 使用 `serde_yaml_ng`**：正确解析 YAML 转义、块标量和嵌套结构。读取 `name` / `description` / `disable-model-invocation`，其它键不影响调用；格式错误给出诊断并跳过。
- **`disable-model-invocation: true` 把技能挡在模型清单之外**，只留给 `/skill:<name>`：给那些不该被自动挑中的技能用。

`ResourcePaths::load` 按「用户 Solaris 目录 → 用户 `.agents/skills` → 工作目录 `.solaris/skills` → 工作目录及祖先 `.agents/skills` → `--skill`」装载，祖先技能从近到远，用户技能保持最高优先级。扫描按名字排序、以真实路径去重并检测目录循环。共享的 [`input.rs`](../crates/solaris-agent/src/input.rs) 负责 skill 命令展开；`Agent::run_turn` 和 Agent 的两类 `Queue::try_push` 都使用它，入队时展开、正文不递归当作命令。显式调用重新读文件，携带绝对路径和资源目录；文件损坏或删除时报告错误。隐藏技能不进入模型清单，但仍进入用户补全。

---

## 4. solaris-tools：工具层

模型只能「请求」，真正动手的是这一层。它由四部分组成：工具本身、持有它们的注册表、拿起子进程的 runner，以及输出截断。**它不认识模型，也不认识 provider**：声明是 JSON Schema，执行在本地；谁把声明下发给模型、谁把结果写回历史，都不是它的事。

与参考实现 pi 的逐项差异、以及已知缺口，见 [`PI-COMPARISON.md`](./PI-COMPARISON.md)。

### 4.1 工具契约

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;                                  // JSON Schema
    fn guideline(&self) -> Option<&'static str> { None }            // 写进系统提示词的用法指南
    async fn run(&self, input: Value, ctx: &ToolContext) -> ToolOutput;
}
```

`guideline()` 是工具对**如何用好自己**的一句话说明，由 agent 汇编进系统提示词（§3.1）。它在这里而不是在提示词模块里集中查表，是为了让 `ToolRegistry::with_tools` 注册的自定义工具也能自带指南；`description` 决定模型要不要用它，指南决定用得对不对。

两条约定决定了这一层的形状：

- **工具不会让一轮失败**。文件不存在、命令退出码非零、模型给的参数不合规，全部变成 `ToolOutput::error` 回到模型手里——能处置它的是模型，不是 UI。
- **工具只认自己**。声明是 JSON Schema，执行在本地；工具不知道模型、消息或 provider 的存在，连自己会被声明给谁都不知道。把声明翻成某条 wire 的请求形状是 `solaris-backend` 的事，把两者接起来是 `solaris-agent` 的事。

`ToolContext` 带三样东西：`cwd`（相对路径的解析基准）、`temp_dir`（被截断的命令输出落在这里）与 `Cancel`（应用取消一轮时，正在跑的命令要被杀掉，而不是留给一个已经没人听的模型）。`Tool::execution_mode()` 决定它能否与同一批调用并行：默认 `Parallel`；`write`/`edit`/shell 声明 `Sequential`，因为它们改动的东西正是同批其他调用可能读到的。

八个内建工具：

| 工具 | 说明 |
| --- | --- |
| `read` | 读文本文件，`offset`/`limit` 支持分段续读 |
| `write` | 新建或整体覆盖，自动建父目录 |
| `edit` | 精确文本替换，一次调用可改多处；`oldText` 必须唯一匹配**原始**文本 |
| `ls` | 列目录，目录带 `/` 后缀，含 dotfiles |
| `grep` | shell out 到 ripgrep，输出 `path:line: text` |
| `find` | shell out 到 fd，按 glob 找路径 |
| `bash` / `powershell` | 合并 stdout+stderr，超时可杀，输出尾部截断 |

两个 shell 是同一份实现的两套配置（`ShellConfig`），注册表按平台只放其中一个：Windows 放 `powershell`，其余放 `bash`——给 Windows 声明 `bash` 是在声明一个跑不起来的东西。`edit` 与 `write` 共用一张按路径的锁（`FileMutationQueue`）：一轮里模型完全可能同时要求改同一个文件的两处，两次「读—改—写」重叠就会丢掉一次改动。

### 4.2 可测的缝

需要外部程序的三个工具都通过 trait 拿起子进程：

```rust
pub trait CommandRunner { async fn run(&self, program: &str, args: &[String], cwd: &Path) -> Result<CommandOutput, String>; }
pub trait ShellRunner { async fn run(&self, config: ShellConfig, command: &str, ctx: &ToolContext, timeout: Option<Duration>) -> Result<ShellOutcome, String>; }
```

默认实现是真的子进程（`LocalRunner`/`LocalShell`），测试注入的是罐头输出。于是整套测试既不需要机器上装了 ripgrep 与 fd，也不会真的执行命令——只有两个测试例外，它们各起一次真进程，用来证明「接线是对的」。`read`/`write`/`edit`/`ls` 则直接在系统临时目录里建自己的夹具目录。

### 4.3 注册表

`ToolRegistry` 持有本平台的全部工具，并回答「按这份默认集合与这份选择，该声明哪些」：

```rust
pub fn select(&self, defaults: &[&str], selection: &ToolSelection) -> Vec<Arc<dyn Tool>>;
pub fn selected_names(&self, defaults: &[&str], selection: &ToolSelection) -> Vec<String>;
```

默认集合是**调用方的事**——这一层知道有哪些工具，不知道哪一轮该给哪些，所以由 `solaris-agent` 按模式给出（见 §3.1）：

```text
registry.select(default_tool_names(Mode::Plan), &selection)   // → read + grep + find + ls
```

名字在注册表里不存在（例如给 Windows 的默认集合写 `bash`）时只是不被声明，不会报错；`unmatched()` 则专门报出选择里打错的名字。

### 4.4 截断

读类取头部（文件开头才说明它是什么），命令类取尾部（失败信息在那里打印），上限 2000 行或 50KB，先到者为准。被截断的命令会把完整输出写进 `temp_dir` 下的文件并把路径写进结果；读取类则给出 `offset` 续读的提示。截断说明本身就是写给模型的，所以它总是跟在内容后面。

---

## 5. solaris-tui：终端 UI 框架

### 5.1 组件模型

```rust
pub trait Component {
    fn render(&mut self, buf: &mut Buffer, area: Rect);
    fn handle_key(&mut self, key: KeyEvent) -> KeyResult { KeyResult::Ignored }
    fn handle_mouse(&mut self, event: MouseEvent) -> MouseResult { MouseResult::Ignored }
    fn handle_paste(&mut self, text: &str) -> KeyResult { KeyResult::Ignored }
    fn tick(&mut self) -> bool { false }                       // 需要重绘则返回 true
    fn desired_height(&mut self, width: u16) -> Option<u16> { None }
    fn version(&self) -> u64 { 0 }                             // 渲染缓存的失效键
    fn invalidate(&mut self) {}
}
```

- `KeyResult`：`Handled` / `Ignored` / `Confirmed` / `Cancelled`。后两者表示「请求关闭」，由框架负责把它从浮层栈弹出。`Ignored` 才允许事件继续下传。
- `desired_height` 让内容自报高度：编辑器与内联向导据此决定输入区高度，`ScrollView` 据此决定离屏画布大小。
- `version` 是渲染缓存的失效键，`0` 表示「未知」——强制每帧重建。

### 5.2 布局

[`layout`](../crates/solaris-tui/src/layout.rs) 是一组纯几何函数，语义对齐 flex（对应 pi-tui 的 `VStack` / `HStack`）：

- `Axis::Vertical` / `Axis::Horizontal`
- `Basis::Cells | Percent | Auto`
- `Entry { basis, grow, shrink, min, max }`，配 `px` / `percent` / `auto` / `grow` / `shrink` / `min` / `max` 构造
- `split(area, axis, entries) -> Vec<Rect>`

框架与应用共用同一套布局，不存在两套排版逻辑。

### 5.3 浮层

[`overlay`](../crates/solaris-tui/src/overlay.rs) 负责几何：`Anchor`（默认居中，另有八向锚点）、`SizeValue::Cells | Percent`、`OverlayOptions`（宽高、`min_width`、最大宽高、锚点、偏移、外边距；默认宽 70%、`min_width` 30、最大宽 90%、最大高 80%、外边距 1），`resolve(area, options) -> Rect` 给出最终矩形。

绘制时浮层是**不透明**的：先 `Clear` 掉矩形区域，再绘制组件 —— 因此不存在半透明叠加的排版问题。

### 5.4 事件循环与输入路由

[`Tui`](../crates/solaris-tui/src/tui.rs) 持有根组件、浮层栈与两个句柄：

- `OverlayQueue = Rc<RefCell<Vec<(Box<dyn Component>, OverlayOptions)>>>` —— 根组件用它**请求**打开浮层，不在渲染中途直接改动栈。
- `QuitFlag = Rc<Cell<bool>>` —— 根组件用它请求退出。
- `overlay_flag: Rc<Cell<bool>>` —— 报告「当前有浮层」，根组件据此让编辑器失焦（即便浮层是被点击外部关闭的）。
- `SelectionHandle = Rc<RefCell<Selection>>` —— 全屏拖选状态，见 §5.5。

主循环（`poll_interval` 16 ms）：

```text
while !quit {
    dirty = 首帧 || 队列非空
    if poll(16ms) { 读取事件 → 按键/鼠标/粘贴/尺寸变化 → dirty = true }
    if tick() { dirty = true }          // 根组件与所有浮层各 tick 一次
    if dirty  { terminal.draw(render) } // ratatui 双缓冲差分，只刷变化区域
}
```

`render` 的顺序是：先把队列中的浮层提升进栈并刷新 `overlay_flag`，再绘根组件，最后自下而上逐个解析矩形、`Clear`、绘制浮层。

输入路由是**单一路径**，避免歧义：

- 只要有浮层，最顶层浮层吃掉**全部**按键、鼠标与粘贴事件；其余组件收不到任何输入。
- 无浮层时输入交给根组件。
- 鼠标左键点在顶层浮层之外 = 关闭该浮层。

**拖选**（[`selection`](../crates/solaris-tui/src/selection.rs)）不走这条单一路径，而是横切过去：

- `handle_mouse` 先让选区**观察**事件，再照常下传 —— 于是一次按下既会移动编辑器的光标，也会起一个选区；只有拖拽才把它变成真正的选择。滚轮事件选区不碰，仍由组件处理。
- **一个单元格也算选区**：只要这次按下变成过拖拽（或双击取词、三击取段），单格范围就会高亮、也能复制。单纯按一下（没有拖拽）不选中任何东西 —— 它只是点击，编辑器要用它移动光标，claurst 也是这么丢弃的。
- **松手不会取消选区**：释放后高亮留在原地，直到下一次按下另起一个选区、尺寸变化，或应用层主动清掉（例如 `/clear` 清空对话记录时）。框架也不会自动复制任何东西。
- `render` 的最后一步在**画完的帧**上重刷选中单元格的颜色，并记下每一行此刻显示的文字，供双击取词、三击取段使用。因为跑在最后，浮层里的单元格同样可选。
- 选区范围按行主序归一化并夹到帧内，拖到屏幕外会延伸到边缘而不是取消。
- 抽文本由应用层发起：`Selection::selected_text()` 给出当前选区的文字（按行去掉尾随空白；整块都是空白的选区则原样保留，所以缩进也能复制）。solaris 把它绑在 `Ctrl+C` 上，而且**只要有选区就复制** —— 空白选区不会掉进「清空输入框 / 退出」那一档（§6.4）。

### 5.5 组件集、主题与终端

- 组件集（[`components`](../crates/solaris-tui/src/components)）：`Editor`、`SelectList`（带模糊过滤）、`Markdown` 渲染、`ScrollView`、`Collapsible`（折叠内容）、`Panel`、`Text`、`Spacer`、`Loader`、`Welcome`（欢迎框）以及 `fuzzy` 匹配。
- 列表滚动（[`util::window_start`](../crates/solaris-tui/src/util.rs)）：三处会滚动的列表 —— 编辑器的 `/` 补全弹窗、`/model` 与 `/connect` 用的 [`InlineSelect`](../crates/solaris-tui/src/components/inline_select.rs)、Ctrl+K 面板与 `/theme` 用的 [`SelectList`](../crates/solaris-tui/src/components/select_list.rs) —— 共用这一个窗口函数，所以高亮不可能走出任何一处。差别只在「要不要记住窗口起点」：前两者每次都从高亮重算（`offset = 0`，往上走时窗口立刻跟回），`SelectList` 把上一次的起点传回来，所以窗口在回调路上会先停住。
- 拖选（[`selection`](../crates/solaris-tui/src/selection.rs)）：`Selection` 记锚点 / 焦点、按 `selection_bg` / `selection_fg` 给单元格上色、抽取文本；`Tui::selection()` 把句柄交给应用层。
- 主题（[`theme`](../crates/solaris-tui/src/theme.rs)）：`Theme` 含约二十个颜色槽位（含 build / plan 两种强调色），`Theme::NAMES = ["dark", "light"]`，未知名称回退到 dark。
- 按键（[`keys`](../crates/solaris-tui/src/keys.rs)）：`ctrl` / `alt` / `plain_char` / `is_submit` / `is_newline` 等匹配助手、`parse_key_spec`（支持 `ctrl+k`、`alt+enter`、`f1`…）、以及 action → 按键的 `Keybindings` 表。
- 终端生命周期（[`terminal`](../crates/solaris-tui/src/terminal.rs)）：进入 raw mode + 备用屏幕 + 鼠标捕获（Windows 上**不**开启 bracketed paste，因为 Windows Terminal 的转义序列不会被控制台后端解码为粘贴事件），并安装 panic hook —— 先恢复终端再交给原 hook，保证 panic 不会把用户留在 raw mode。`restore_terminal` 通过原子标志保证幂等。

---

### 5.6 可复用折叠组件

[`Collapsible`](../crates/solaris-tui/src/components/collapsible.rs) 实现 `Component`：调用方提供带样式的标题与正文行，以及 `Idle` / `Running` / `Failed` 状态。组件负责默认折叠、固定的 `→` / `↓`、缩进、保留样式的终端列宽截断、点击与键盘交互、高度与缓存版本，不认识子代理、工具调用或会话。组件没有转圈动画、帧状态或定时重绘。

保留式布局直接使用组件的 `render` 和 `handle_mouse`。聊天记录这样的逐行视图使用同一个 `lines(width)` 渲染器，将 `CollapsibleState` 留在记录中，再通过 `with_state` 恢复；更新标题、正文或完成状态不会重置展开选择。`CollapsibleInteraction<T>` 为多个虚拟标题提供共用的按下/释放、拖选和选区识别，调用方只映射命中坐标和记录 ID、处理视口滚动。

应用将普通工具组、Shell 命令与子代理都接到这个组件上：连续的普通调用合为 `→ Explored: 8 reads, 3 searches`，展开后显示各次调用的参数摘要与耗时，失败的调用标红；每个 Shell 命令独立显示为 `→ Shell: cargo test`，展开后显示输出。Shell、子代理与 steering 消息都会分隔普通工具组，不改变记录顺序。折叠标题始终显示 `→`，展开后显示 `↓`；不论内部工具是否已经失败，运行中的标题都不会变红。执行结束后才汇总错误：普通工具组检查组内调用，子代理检查最终结果与内部调用，Shell 检查命令结果；中断也视为失败。失败仅将标题标红，不追加错误或中断信息，不改变展开状态，不触发其他行为。

`ToolStep.disclosure` 保存通用展开状态，普通工具组由第一条调用持有；后续调用追加、结果到达、完成状态更新都不会重置展开选择。`SubagentStep` 只保存子任务数据。`transcript` 负责标题、统计、正文与命中几何，`App` 共用折叠手势与展开时的视口调整，不依赖具体工具或子代理执行器。

## 6. solaris：应用层

### 6.1 形态

同一个 crate 同时提供**库**与**二进制**：库（`lib.rs`）承载全部 UI 与状态逻辑，`main.rs` 只做参数解析与装配。这样集成测试可以 `use solaris::{App, AppOptions}` 直接驱动真实技术栈。

模块划分：`app`（根组件）、`state`（会话与通知）、`dialogs`（浮层对话框）、`connect`（内联向导，以及复用它渲染的 `/model` 选择器）、`transcript`（转录渲染与缓存）、`commands`（斜杠命令表：`parse_slash_command`、编辑器补全提示、Ctrl+K 面板项）、`keymap`（全局按键与页脚提示）、`clipboard`（平台剪贴板，藏在可注入的 `Clipboard` 后面），外加四个纯逻辑模块：`config`（`Config` / `Preferences`）、`buddy`（伙伴：`Companion` / `Bones` / `Soul` / `Species` / `Rarity` / `Hat`，由用户 id 经 FNV-1a 播种、用 Mulberry32 掷出骨架，因此稳定且不可手工篡改）、`recent`（历史提示词）与 `tips`（起步提示轮换）。

### 6.2 启动装配（`main.rs`）

1. `clap` 解析参数；构造 `Config`（`--plan` 映射到 `Mode::Plan`，`--model` 未给时模型名留空，由 provider 决定）。`--tools` / `--exclude-tools` 解析成 `ToolSelection`：白名单与 `+name`/`-name` 两种写法不能混用（混用直接报错，而不是猜），匹配不到任何工具的名字在启动时被指出来。
2. `config_dir()` 解析状态目录，优先级：`$SOLARIS_CONFIG_DIR` → Windows 的 `%APPDATA%\solaris` → `$XDG_CONFIG_HOME/solaris`，回退 `~/.config/solaris`。
3. 读取 `auth.json`、`companion.json`、`recent.json`、`settings.json`；**读取失败一律当作空**，绝不因为文件缺失而拒绝启动。随后 `ResourcePaths::load` 扫描技能并读取项目规则与追加说明，将单文件诊断输出到 stderr；单个坏资源不阻止启动。
4. `--provider`（可选）覆盖本次运行的 provider，只改内存里的 `auth`，不重写 `auth.json`；未知 id 直接报错并列出可选值。
5. 用 `choose_backend(&auth, &model, BackendOptions::default())` 解析出初始后端；显式给了 `--provider` 却解析不出凭据时，向 stderr 说明「先运行 /connect 或设置环境变量」，以免静默降级。解析出的后端随即由 `with_tools` 交给 `solaris_agent::Agent`（连同注册表、工具选择与审批钩子），注册表与工具选择在 `App` 里只建一次——`edit` 与 `write` 的按路径锁因此整个会话共用一张。
6. 若给了 `--print-config`，打印解析结果（含 backend / endpoint / 凭据来源）后直接返回，不进入界面。
7. 否则：建 tokio runtime 并 `enter()`，安装 panic hook，`setup_terminal()`，建 `Tui` 与 `App`。`AppOptions.backend_factory` 是一个闭包（生产环境就是 `choose_backend`），四个句柄 —— `quit`、`overlay_queue`、`overlay_flag`、`selection` —— 都取自 `Tui`，外加 `system_writer()` 作剪贴板；`tui.set_root(app)`、`tui.run(&mut terminal)`，最后无论如何都 `restore_terminal()`。

### 6.3 App 的状态组成

| 分组 | 字段 |
| --- | --- |
| 依赖与配置 | `agent: Arc<Agent>`（会话：后端 + 工具选择 + 审批 + 取消句柄，同时回答后端 label、provider id、真正会被询问的模型名与凭据来源）、`backend_factory`（按凭据与模型重新解析后端）、`config`、`theme` |
| 工具 | `tools: ToolSelection`（`--tools`/`--exclude-tools` 的结果，交给 `Agent`）、`registry: Arc<ToolRegistry>`（本平台的全部内建工具，整个会话一份）、`skills: Skills`（扫描到的技能：交给 `Agent` 进提示词，也供 `/skills` 与 `/skill:<name>` 使用）、`cwd`（相对路径的解析基准）、`approver`（每个重建的会话都接同一个）、`tool_cancel`（`Agent` 交出的取消句柄） |
| 会话 | `session: SessionState`、`transcript: TranscriptView`、`notifications: NotificationQueue` |
| 输入 | `editor: Editor`、`keybindings`、`inline: Option<ConnectFlow>` |
| 与框架的句柄 | `quit`、`overlay_queue`、`overlay_flag`、`selection` |
| 跨线程通道 | `events_rx`（后端事件）、`dialog_rx` / `dialog_tx`（对话框结果）、`models_rx`（模型发现）、`compact_rx`（`/compact` 的答案） |
| 持久化 | `auth` / `auth_path`、`buddy` / `buddy_path`、`recent` / `recent_path` |
| 剪贴板 | `clipboard: Clipboard`（`write` / `read` 两个可注入的闭包，生产环境是 `arboard`） |
| 展示与动画 | `version`、`greeting`、`tip`、`spinner_frame`、`buddy_step` / `buddy_started` |

`SessionState` 持有 `turns: Vec<Turn>`（每轮含提示词、被 steering 进来的消息 `steering: Vec<SteeringNote>`、回复、思考轨迹、工具步骤 `steps: Vec<ToolStep>`、被摘要掉的条数 `compacted`、`Usage`、费用、是否结束）、瞬时 `status`、滚动偏移、`follow_end`（是否吸附到最新一行）与单调递增的 `version`（渲染缓存的失效键）。`ToolStep` 是一次调用及其结果与耗时：`ToolCall` 事件把它加成「进行中」，对应的 `ToolResult` 事件把它补完，所以转录里既能看到正在跑的调用，也能看到它花了多久。`NotificationQueue` 是带存活时间的临时通知队列（Info / Warning / Error）。

**转录与对话是两件事**：`SessionState` 是给用户看的（轮次、工具步骤、耗时、被 steering 进来的消息、被摘要掉的条数），模型看到的那段对话住在 agent 里（§3.0）。所以这里没有「把转录压成请求」这一步——早先的实现每轮重建一次，代价是压缩的结果活不过一轮、插话要靠额外记录反推位置、以及模型自己的话需要另行补上。

**后端在会话中途会被重新解析**：`/connect` 写入凭据、`/model` 改模型时都调 `App::rebuild_backend()` → `resolve_backend()`。它做三件事，而不只是换一个后端：把凭据与模型交给 `backend_factory`；**采纳真正会被询问的模型**（模型名为空时换成该 provider 提供的第一个模型，因为 provider 不会接受空模型名）；按模型目录更新 `config.context_window`，footer 的占比条据此才诚实。拿到 `BackendChoice` 后再由 `with_tools` 建一个新的 `Agent`，并把取消句柄换新——`Agent` 本身不可变，重建就是它唯一的更新方式。`AppOptions::new()` 会把传进来的那个后端**钉死**成一个固定工厂（provider id 为空，模型原样回传），所以测试驱动的永远是它自己交给 app 的后端；生产环境则用真实的 `choose_backend`。

**模型发现在后台跑**：`App::new` 与 `/connect` 之后各调一次 `App::refresh_models()`，它把 `agent.models()` 丢进 tokio 任务，答案经 channel 在 `tick()` 里收。发现只做加法——没落地时选择器就用内置目录，永远不落地就一直用它。清单到手后：若模型还没命名，取其中的第一个并重建后端（与目录提供首个模型是同一套替换）；选择器改用它，并从目录里补上已知模型的描述与单价。`AppOptions::discover_models` 控制是否发起这次请求：**二进制里开着，`AppOptions::new()` 里关着**，所以测试即使驱动真实 `choose_backend` 也不会自己联网。**`/connect` 的模型步不等这份清单**：网关与自定义端点的目录里本来就没有名字，所以凭据一落地就进入 Model 步（列表还空着时由选择器的说明行交代正在向 provider 要），答案到手后 `App::fill_in_models()` 把它填进同一个选择器——否则这一步只会在有内置目录的 provider 上出现，网关用户存完 key 就被直接弹回会话。清单是空的、而目录也没得退时这一步没有东西可选，于是收掉向导，只留下那条「name one with /model <name>」的通知。

复制发生在 `handle_key`：`Ctrl+C` 命中 `quit` 绑定时，先看共享的 `selection` 句柄里有没有文本 —— 有就交给 [`clipboard`](../crates/solaris/src/clipboard.rs) 并通知结果，没有才真的退出。

### 6.4 输入优先级

根组件 `handle_key` 的分发顺序固定为：

```text
最顶层浮层（由框架拦截）  →  内联向导 / 选择器  →  会话（编辑器 + 全局快捷键）
```

内联向导虽然不是浮层，但在此期间它拥有键盘 —— 例如 `Ctrl+C` 是「取消向导」而不是「退出」。

全局快捷键里 `Ctrl+C` 走的是三家（claurst / Claude Code / pi）同一套阶梯：

```text
有选区            → 复制选区（pi-tui 把 ctrl+c 绑给 copy）
正在流式生成      → 中断本次生成（丢掉事件接收端，后端任务随之结束；
                     已生成的内容留在屏幕上）
否则              → 清空输入框，并提示 "press ctrl+c again to quit"，
                     2 秒内第二次按下才真的退出
```

`Ctrl+D` 是同一个手势的另一半，只在输入框为空时生效 —— 否则按键留给编辑器。浮层存在时按键由浮层吃下，所以要先关掉对话框才能复制。

`Ctrl+V` 走反方向：从剪贴板取文本，插进当前接受输入的地方 —— 内联向导的字段在场就给它，否则给输入框；剪贴板为空时提示 `clipboard is empty`。claurst 就是在同一个键（它还接受 `Cmd+V`）上读剪贴板、并对空剪贴板报警的。

### 6.5 渲染管线

`render_session` 用一次 `layout::split` 把整屏切成三段：

```text
┌──────────────────────────────┐
│ 转录（grow(1)，min 1）        │  ← 欢迎框或对话，可滚动
├──────────────────────────────┤
│ 输入区（px，内容自报高度）      │  ← 编辑器，或 /connect 内联向导
├──────────────────────────────┤
│ 页脚（px 1）                  │  ← 状态、backend、模型、模式、token 与费用、提示
└──────────────────────────────┘
```

输入区高度取 `inline.desired_height(width)` 或 `editor.desired_height(width)`，再夹在 `3..=(area.height - 2)` 之间；终端过小（宽 < 12 或高 < 4）时整屏跳过绘制。

转录本身由 [`TranscriptView`](../crates/solaris/src/transcript.rs) 负责，并做缓存：只有当**会话版本、区域尺寸、主题名、spinner 帧**变化时才重建行；唯一例外是转录为空时（欢迎框里的伙伴要做待机动画），此时每帧重建 —— 因为那也只有一个屏面。

### 6.6 一次对话的完整数据流

```text
编辑器提交
   │
   ▼
App::start_turn
   ├─ agent.run_turn(prompt, mode)
   │     （工具由 Agent 按模式选、对话由 Agent 持有、提示词由 Agent 写）
   ├─ 追加一个未完成的 Turn，follow_end = true，session.bump()
   ├─ 建 tokio unbounded channel，events_rx = Some(rx)
   └─ tokio::spawn：消费 Agent 的流，逐条 tx.send(event)
   │
   ▼  （后台任务，主线程不阻塞）
每帧 App::tick
   ├─ try_recv 排空 events_rx → apply_event 写入当前 Turn / status
   │     （ToolCall 追加一个进行中的 ToolStep，ToolResult 把它补完）
   ├─ 收到终态事件或通道断开 → 结束该轮、清 status、
   │                          该轮结束、状态清空
   ├─ 排空 dialog_rx → on_dialog_message（主题/模型/命令/确认）
   ├─ 排空 events_rx
   ├─ notifications.tick()、spinner 前进、伙伴动画步进
   └─ 任一变化则返回 dirty = true
   │
   ▼
Tui::render
   ├─ 提升队列中的浮层
   ├─ 根组件渲染（→ TranscriptView::lines 按需重建）
   └─ 逐个绘制浮层（Clear + render）
```

要点：

- **UI 侧永不阻塞**。后端跑在 tokio 任务上，主线程只用 `try_recv` 排空通道，因此流式输出、滚动与动画互不阻塞。
- **一轮可能是多次模型调用**。工具往返发生在 `Agent` 内部：它自己转发分片、自己累计用量，只在模型不再请求工具时发出一个 `TurnComplete`。所以 UI 看到的「一轮」始终是一次提问到一次回答，哪怕中间跑了十次命令。
- **按需重绘**。渲染只由 `dirty` 触发；`ratatui` 的双缓冲差分只把变化的单元写进终端，所以空闲帧是零成本的。
- **流式期间仍可输入，而且能改主意**。此时提交的提示词不再排到这一轮之后，而是由 steering 交给正在跑的循环（§3.5）；模型答完最后一句之前到达的插话还会让本该结束的一轮继续下去。
- **终态必达**。即使后端流意外结束且没有终态事件，`tick` 也会把该轮标记为完成并停掉 spinner。

### 6.7 对话框与内联向导

对话框（[`dialogs`](../crates/solaris/src/dialogs.rs)）有一条**唯一**的模态路径：每个对话框都是普通 `Component`，被压入框架的浮层栈，结果通过 `mpsc` 回传，而不是反向持有应用引用。

```rust
enum DialogMessage { Cancelled, Theme(String), Command(String),
                     Confirm { action: ConfirmAction, accepted: bool } }
```

应用侧只暴露 `open_help` / `open_stats` / `open_skills` / `open_buddy` / `open_palette` / `open_theme_dialog` / `open_clear_confirm`，内部统一走 `push_overlay`。技能的调用写作 `/skill:<name>`：冒号让名字留在同一个词里，编辑器因此能在 `/skill` 之后按名字把每个技能列出来（`commands::hints(skills)` 把固定的命令表与「每个技能一行」拼在一起），而 `/skill:` 单独出现时退化成 `/skills`。

**列表类选择器走内联**：`/connect` 与 `/model` 都不弹浮层，而是接管输入区（与 Claude Code 的 `/login` 视觉一致）。[`ConnectFlow`](../crates/solaris/src/connect.rs) 是一个状态机：

```text
ConnectStep:   Provider → ApiKey | CustomProvider → Model
ConnectOutcome: Handled | Closed | ProviderPicked | Submit | ModelPicked
```

`/model` 由 [`model.rs`](../crates/solaris/src/model.rs) 实现：它只写下这个命令的策略（列表说什么、选中意味着什么），列表本身是框架组件 [`InlineSelect`](../crates/solaris-tui/src/components/inline_select.rs) —— 标题、说明、问题行、`❯` 行、底部提示、按键、滚轮与命中测试都在那里，`/connect` 的 provider / model 两步用的也是它，所以两者样式不会漂移。`App` 用一个 `Inline` 枚举持有两者并转发事件。

Model 步可以先于清单打开：[`ConnectFlow::enter_models`](../crates/solaris/src/connect.rs) 接受空列表（此时给选择器加一行「正在问 provider」的说明），provider 的答案到达后同一个方法再调一次，把列表填上并把当前模型设为高亮行，回车即沿用——所以网关用户看到的第三步和内置目录的 provider 完全一样，只是晚半秒。

流程状态（第几步、输入框内容）归 `ConnectFlow`；副作用（写凭据、激活 provider、重新解析后端）归 `App`，因为那是应用状态而非流程状态。文本步骤会从 `AuthStore` 回填该 provider 已存的 URL 与 key（key 照常掩码显示），所以重新 `/connect` 看到的是已保存的内容而不是空字段，直接回车即沿用；`ctrl+u` 清空当前字段，用来换成另一个 key。

### 6.8 持久化

四个 JSON 文件都写在同一个状态目录下，全部由应用层负责读写（`auth.json` 的形状由 `solaris-provider` 的 `AuthStore` 定义，其余三个是应用自己的类型，它们只负责序列化）：

| 文件 | 内容 | 写入时机 |
| --- | --- | --- |
| `auth.json` | `AuthStore`：凭据（按 provider id）+ 当前 provider 与它上次用的模型 | `/connect` 产生变更后、以及用户选过模型后 |
| `settings.json` | `Preferences`：主题与模式 | `/theme`、Tab 切换模式后 |
| `companion.json` | 伙伴的「灵魂」：名字、性格、孵化时间 | `/buddy name <name>` 之后 |
| `recent.json` | 最近提示词 | 每次提交提示词后 |

写入走 `write_json`：先 `create_dir_all` 建父目录，再写文件。三个文件彼此独立，删掉任意一个只会重置对应那部分状态。

---

## 7. 扩展点

| 想做的事 | 改哪里 |
| --- | --- |
| 新增一个工具 | 在 [`crates/solaris-tools/src/tools`](../crates/solaris-tools/src/tools) 实现 `Tool`（name / description / parameters / guideline / run），在 [`registry.rs`](../crates/solaris-tools/src/registry.rs) 的 `with_runners` 里 `add` 一次，必要时把名字加进 `BUILTIN_TOOLS`，并在 [`solaris-agent/src/mode.rs`](../crates/solaris-agent/src/mode.rs) 的 `default_tool_names` 里把它列进某个模式。声明与执行是分开的，所以工具不需要认识任何 wire。 |
| 改工具的用法指南 | 实现该工具的 `Tool::guideline()`（默认 `None`）。它会被汇编进系统提示词的那一段「Using the tools」，且只在这一轮真的声明了该工具时出现（§3.1）。 |
| 调整系统提示词 | [`solaris-agent/src/prompt.rs`](../crates/solaris-agent/src/prompt.rs) 的 `build_system_prompt`；模式说明在 [`mode.rs`](../crates/solaris-agent/src/mode.rs)，来源文件由 `PromptContext` 提供。 |
| 调整技能与项目规则的查找位置或优先级 | [`resources.rs`](../crates/solaris-agent/src/resources.rs) 的 `ResourcePaths::load`，顺序决定同名技能优先级。 |
| 调整技能清单段或 frontmatter 解析 | [`skills.rs`](../crates/solaris-agent/src/skills.rs)：`Skills::prompt_section`、`Skill::read_invocation` / `invocation_prompt`、`parse_document` 与 `scan`；输入路由在 `input.rs`。 |
| 给某个平台加一句提示词 | [`solaris-provider/src/providers.rs`](../crates/solaris-provider/src/providers.rs) 里该条目的 `prompt` 字段；只有确实改变「模型该如何被对待」的事才值得写（网关、本机小模型、自建端点）。 |
| 调整压缩的时机或保留量 | [`compaction.rs`](../crates/solaris-agent/src/compaction.rs) 的 `Compaction`（`reserve_tokens` 默认 16384、`keep_recent_tokens` 默认 20000）；整轮关掉用 `Compaction::OFF`。裁切点在同一个文件的 `cut_point`，摘要提示词在 [`prompt.rs`](../crates/solaris-agent/src/prompt.rs) 的 `COMPACT_PROMPT` / `UPDATE_COMPACT_PROMPT`。 |
| 让消息在别处被读到 | 两个队列都由 agent 交出（`steering_handle()` / `follow_up_handle()`，§3.5）；非 UI 的调用方自己 `push()` 即可，记录由 `AgentEvent::Steered` / `FollowUp` 回报。 |
| 改变默认工具集 | [`solaris-agent/src/mode.rs`](../crates/solaris-agent/src/mode.rs) 的 `default_tool_names`（按模式给默认集合）。单次运行用 `--tools` / `--exclude-tools` 即可，不必改代码。 |
| 让工具执行前需要确认 | 实现 `Approver` 并在构造 `AgentOptions` 时用 `with_approver` 注入（[`crates/solaris/src/app.rs`](../crates/solaris/src/app.rs) 的 `with_tools`）。要做成交互式对话框，需要一条把「请确认」送回 UI 的通道，再让 `Approver` 等它。 |
| 调整工具的输出上限 | [`truncate.rs`](../crates/solaris-tools/src/truncate.rs) 的 `DEFAULT_MAX_LINES` / `DEFAULT_MAX_BYTES`，或给单个工具 `with_limits`。 |
| 限制一次工具往返的次数 | 没有上限：循环跑到模型不再请求为止（见 §3.3）。要加回来就在 [`solaris-agent/src/session.rs`](../crates/solaris-agent/src/session.rs) 的 `drive` 循环里计数并在 `AgentOptions` 上加一个字段。 |
| 换掉 ripgrep / fd | 实现 `CommandRunner` 并传给 `ToolRegistry::with_runners`（`grep` 与 `find` 都经它起子进程）。 |
| 接入新的 wire 协议 | 在 [`wire.rs`](../crates/solaris-backend/src/wire.rs) 的 `Wire` 加一个变体，在同一个 `wire.rs` 的 `request_body` / `WireStream` 各加一个分支，照 [`protocols/`](../crates/solaris-backend/src/protocols) 里既有的三个模块写一个只做「请求体 + 流解析」的模块，最后让 `PROVIDERS` 里对应的条目指向它——provider 自己的 `wire` 管它没列出的模型，模型的 `wire` 管它自己。UI 与 `choose_backend` 的调用方都不用动。 |
| 增删 provider 目录项 / 模型 | [`solaris-provider/src/providers.rs`](../crates/solaris-provider/src/providers.rs) 的 `PROVIDERS`（向导步骤、模型选择器、端点、环境变量、价格表，以及每个模型自己的 wire 都从这里读）。 |
| 调整重试 / 超时策略 | [`solaris-backend/src/http.rs`](../crates/solaris-backend/src/http.rs)：`BACKOFF`、`IDLE_TIMEOUT`、`CONNECT_TIMEOUT`。 |
| 新增斜杠命令 | [`crates/solaris/src/commands.rs`](../crates/solaris/src/commands.rs) 的命令表 + `App::execute_command` 加一个分支；补全、面板、帮助会自动跟随。 |
| 新增浮层对话框 | 在 [`dialogs.rs`](../crates/solaris/src/dialogs.rs) 实现 `Component`，经 `DialogMessage` 回传结果，再用 `push_overlay` 打开。 |
| 换一个后端实现 | 为你的类型实现 `AgentBackend`（`run_turn` 加可选的 `label` / `models`），在 `backend_factory` 里把它包成 `BackendChoice`；`Agent` 与 UI 都不用动。 |
| 新增主题 | [`solaris-tui/src/theme.rs`](../crates/solaris-tui/src/theme.rs)：加调色板 + 登记进 `Theme::NAMES`。 |
| 新增 UI 组件 | 在 [`solaris-tui/src/components`](../crates/solaris-tui/src/components) 实现 `Component`，并加进 `components/mod.rs` 的再导出。 |
| 调整快捷键 | [`keymap.rs`](../crates/solaris/src/keymap.rs) 的 `default_bindings`。 |
| 换剪贴板实现 | [`clipboard.rs`](../crates/solaris/src/clipboard.rs) 的 `Clipboard::system()`，或在构造 `AppOptions` 时替换 `clipboard` 字段（测试就是这么注入记录器的）。 |
| 加宽/加高某个区域 | 用 `layout::split` 的 `Entry` 表达意图（`grow` / `min` / `max`），不要手算坐标。 |

---

## 8. 测试策略

工作区共 **700 个测试**，分三层：

- **单元测试**贴着被测代码放在各模块内（`solaris-provider` 49、`solaris-backend` 107、`solaris-tools` 108、`solaris-agent` 102、`solaris-tui` 135、`solaris` 库 202），覆盖纯逻辑、布局、按键、渲染（含补全弹窗的列对齐与 `(n/总数)` 计数、三处列表共用的 `window_start` 及其顶 / 底 / 越界边界）、SSE 解码、三条 wire 的请求体与流解析（用录制回放，不联网）、工具声明的下发布局与调用片段的重组、截断边界（含多字节字符不被切断）、`edit` 的 BOM/CRLF/多处匹配/重叠拒绝、`Agent` 的多轮与调用顺序、按模式选工具与选择增删、提示词的组装（身份 / 模式 / 只有本轮工具贡献指南 / 技能清单及其缺省 / 平台说明 / 无指南工具不加段）、技能的发现与递归边界（含「含 `SKILL.md` 的目录不再深入」）、frontmatter 的引号 / 注释 / 块标量 / 续行 / 嵌套结构跳过、描述缺失与名字撞车的处置、steering 的入队与排空顺序、压缩的触发与安全裁切点（含摘要失败时原样发送）、缓存断点与 `prompt_cache_key` 的门控、模型清单的解析与 URL、上下文与累计口径、凭据存取与脱敏、选后端 / 选模型与状态机。需要外部程序的工具（`grep`/`find`/shell）走可注入的 runner，所以测试既不需要装了 ripgrep 与 fd，也不会真的执行命令。
- **回路测试** [`crates/solaris-backend/tests/loopback.rs`](../crates/solaris-backend/tests/loopback.rs)（10 个）：在 loopback 上起一个真的 `TcpListener`，用真的 `reqwest` 去请求它。请求头、`Content-Length` 读取、SSE 分帧、用量结算、模型清单的 GET 与 Bearer 头、401 的报错文案、429 的退避重试，这一整条链路都由真 socket 验证过 —— 仍然不碰外网。
- **工具往返的回路测试** [`crates/solaris-agent/tests/loopback.rs`](../crates/solaris-agent/tests/loopback.rs)（2 个）：同一套真 socket 手法，但服务端先回一个 `tool_call`、再回最终文本，于是「请求体带上了 `tools` → 调用被解析出来 → 真的 `read` 工具跑了 → 结果作为 `role:"tool"` 回到第二次请求」整条链路被端到端验证 —— 它住在 agent 里，因为只有那一层同时认识工具与 wire。仍然不碰外网。
- **端到端冒烟测试** [`crates/solaris/tests/tui_smoke.rs`](../crates/solaris/tests/tui_smoke.rs)（28 个）：驱动真实技术栈（`Tui` 事件循环 + `App` + 框架组件 + 真实 `Agent`），渲染到 ratatui 的 `TestBackend`，因此整条 UI 链路无需真实终端即可断言。它自带一个只回显提示词的 `FakeBackend` 顶替真实 provider，并把 `environment` 换成空表，所以既不会继承 shell 里的 key，也不会联网；另有一个先请求一个不存在工具的后端，用来验证工具步骤确实进了转录，而不会真的碰文件或执行命令。

```bash
cargo build --workspace --all-targets
cargo test --workspace
```

---

## 9. 当前边界

- **工具调用已经打通，但工具集是固定的八个**。`Agent` 会跑「模型 → 工具 → 模型」直到模型不再请求，用量与费用按轮累计；但可声明的工具就是内建那八个，没有 MCP、没有第三方扩展工具。
- **确认对话框只在 `--confirm-tools` 下出现**。默认是 `AlwaysApprove`（终端 agent 已经在替你动手，为一次 `read` 弹框只是噪音）；打开后，改动型调用会弹一个确认框，被拒绝的调用作为错误结果回到模型。
- **一次调用不可中断到一半**。`Ctrl+C` 取消一轮会让循环停止请求模型、并杀掉正在跑的命令，但已经在执行的 `edit` 不会回滚。
- **插话在两轮之间生效**。steering 的消息由循环在「构造下一轮之前」读入，所以正在跑的那条命令不会被它打断；要立刻停下只有 `Ctrl+C`。
- **follow-up 只能靠 `/queue`**。流式期间回车是 steering（默认、也最常用），想在当前这一轮之后另起一轮得打 `/queue <prompt>`——`alt+enter` 已被换行占用，而命令表顺带让 `/help` 与补全能发现它。
- **资源更新需要 `/reload`**。新增/删除技能、名称与描述修改、项目规则及追加说明修改通过 `/reload` 更新；显式调用每次读取当前正文。运行中的 stream（含后续队列）和压缩结束后才应用重载，已展开的排队消息保留入队时内容。
- **对话不落盘**。`Conversation` 只在内存里，`/quit` 之后就没了；转录同样不落盘。
- **压缩的触发靠真数，裁切靠估算**。是否超预算看 provider 上一轮报的 usage；**从哪里裁**只能按字符估算每个消息（`字符数 / 4`），与 provider 的分词器有出入——所以预留的 16384 token 同时是回复的空间和这个误差的余量。
- **摘要是有损的**。被替换掉的那些消息只剩模型写的那段摘要，原文不再发给 provider；摘要本身是一次模型调用，因此压缩只在必要时发生（或用户 `/compact` 时）。转录里会留一行说明压掉了几条。
- **overflow 的识别是猜文本**。`overflow.rs` 用 26 条固定短语匹配，每条对应一个平台的措辞，另有 3 条排除（限流不是超限）与「用 usage 看出静默超限」这条不猜的路。漏掉某家措辞的代价是少一次重试，误判的代价是白丢一段对话——所以宁可漏。
- **`grep` 与 `find` 需要外部程序**。它们 shell out 到 ripgrep 与 fd；缺失时工具会明确报错并给出安装提示，而不是退化成另一个实现。
- **工具输出是截断的**。读类保留前 2000 行或 50KB，命令类保留末尾并把完整输出写进临时文件；被截断的那部分只有通过报告出来的路径才读得到。
- **价格是内置快照**。`provider` 表里的单价是打表值，账单可能不同；未知模型按 0 计。
- **报不了用量的服务端会被估算**。OpenAI 兼容服务里有一部分不实现 `stream_options.include_usage`，那一轮退回按字符估算，并在 `/stats` 里标注 `≈`。
- **Windows 上不启用 bracketed paste**，粘贴内容以按键事件到达（`Component::handle_paste` 不会被调用）。
- **拖选依赖系统剪贴板**。`arboard` 打不开剪贴板时（例如无 X11 / Wayland 的 headless 环境），高亮仍然生效，只是 `Ctrl+C` 会发一条「取不到剪贴板」的警告。
- **剪贴板只走文本**。`Ctrl+V` 粘贴的是文本；图片、文件不在范围内 —— Claude Code 的 `Ctrl+V` 是「贴图片」，solaris 没有这条通路。终端若把 `ctrl+v` 留给自己（Windows Terminal 默认如此），按键不会到达应用，走的是终端自己的粘贴通路。
- 伙伴素材是 claurst 十八个物种的一个子集；新增物种 = 一个 `Species` 变体 + 三帧 12 格宽的精灵图。
