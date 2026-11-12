# solaris 架构

本文描述 solaris 的分层结构、运行时数据流与扩展点。内容与仓库代码一一对应，对应版本 `0.1.0`。

---

## 1. 总览

solaris 是一个用 Rust 编写的终端 AI 助手。Cargo workspace 使用 edition 2024，最低支持 Rust 1.97.1，由八个 crate 组成。应用负责组装各层，依赖保持单向：

```text
solaris                  应用组装、配置、认证、TUI 与 CLI
├── solaris-tui          可复用终端 UI 框架
├── solaris-provider     provider 目录、鉴权、模型与价格、后端解析
│   ├── solaris-backend
│   └── solaris-types
├── solaris-agent        会话、多轮工具循环、审批与子代理
│   ├── solaris-backend  后端接口与传输协议 → solaris-types
│   └── solaris-tools    工具接口、内建工具与注册表 → solaris-types
├── solaris-mcp          MCP 连接、发现与工具适配 → solaris-tools
└── solaris-tools        应用显式注册 MCP 返回的工具
```

三条硬性约束：

1. **应用负责组装**。`solaris` 调用 provider 解析后端，转换成 `AgentBackendConfig` 后注入 agent；同时发起 MCP 连接并显式注册返回的工具。agent 不依赖 provider，MCP 不接收或修改注册表。
2. **认证与执行分离**。provider 目录、凭据来源、端点选择和认证刷新由应用与 provider 层处理。agent 只接收后端接口、模型、会话记录标识、上下文窗口回退值与已解析的附加指令。
3. **协议与策略分离**。MCP 返回的适配器实现 `Tool`，与本地工具共用审批、预算和执行路径。适配器持有连接引用；应用注册它们后，连接持续到最后一个持有者释放。

| Crate | 目录 | 依赖 | 职责 |
| --- | --- | --- | --- |
| `solaris-backend` | [`crates/solaris-backend`](../crates/solaris-backend) | `async-trait`、`futures`、`tokio`、`rig-core`、`rig-reqwest`、`reqwest`（rustls）、`serde`、`serde_json`、`thiserror` | 传输层，外加一轮对话的词汇：`AgentBackend` 连接准备接口、`Backend`（SDK 模型连接）、事件适配与请求策略；`message`（`Message`/`Content`/`Role`）、`event`（`AgentEvent`/`TurnRequest`/`BackendError`）、`tool`（`ToolSpec`/`ToolCall`/`ToolResult`）、`usage`（`Usage`/`Price`）。它只接收一个已经解析好的请求，因此既不认识平台也不认识工具。 |
| `solaris-tools` | [`crates/solaris-tools`](../crates/solaris-tools) | `async-trait`、`futures`、`tokio`（`process`/`io-util`/`fs`）、`serde_json`、`thiserror`、`libc`（仅 Unix） | 工具层：八个内建工具（`read`/`write`/`edit`/`ls`/`grep`/`find` 与所在平台的 shell）、`ToolRegistry`、命令 runner、截断与文件写入串行化、`edit` 的 diff。不依赖工作区里任何其他 crate。 |
| `solaris-provider` | [`crates/solaris-provider`](../crates/solaris-provider) | `solaris-backend`、`async-trait`、`serde`、`serde_json`、`thiserror` | 平台层：`providers`（provider 目录——认证方式、wire、端点、环境变量、每个模型的上下文窗口/最大输出/单价）、`choose`（`Credential` 解析成后端，`AuthStore`、`mask_secret`）。向导步骤、模型选择器与价格表都从这一张表读。 |
| `solaris-agent` | [`crates/solaris-agent`](../crates/solaris-agent) | `solaris-backend`、`solaris-tools`、`rig-agent`、`rig-core`、`async-trait`、`futures`、`serde`、`tokio` | agent 会话：`Agent`（持有对话并驱动 SDK 多轮状态机）、`Mode` 与它声明给一轮的默认工具集、`Approver` 审批钩子，并把应用需要的词汇（消息、事件、工具、用量、注册表与选择）再导出。 |
| `solaris-tui` | [`crates/solaris-tui`](../crates/solaris-tui) | `ratatui`、`crossterm`、`unicode-width`、`unicode-segmentation` | 终端 UI 框架：组件模型、类 flex 布局、浮层、主题、按键匹配、组件集、终端生命周期。 |
| `solaris-mcp` | [`crates/solaris-mcp`](../crates/solaris-mcp) | `solaris-tools`、`rmcp`、`tokio`、`reqwest` | 连接 MCP 服务器、发现工具与资源/提示词操作，返回工具适配器和状态；不负责注册。 |
| `solaris` | [`crates/solaris`](../crates/solaris) | `solaris-agent`、`solaris-provider`、`solaris-mcp`、`solaris-tools`、`solaris-tui` + `ratatui`、`crossterm`、`futures`、`anyhow`、`clap`、`tokio`、`serde`、`serde_json`、`thiserror` | 应用本体，同时产出库与 `solaris` 二进制；斜杠命令表、偏好设置、伙伴与提示语也住在这里。 |

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

### 2.2 模型连接与官方实现

`AgentBackend::model_client(self: Arc<Self>)` 必须返回明确的 `ModelClient` 或错误，不再通过 `None` 隐式选择事件执行路径。`Backend` 是唯一的生产模型实现；内部使用官方 `rig-core` / `rig-reqwest` 的厂商配置、Model、ModelAdapter 和 ErasedHandler。

- [`client.rs`](../crates/solaris-backend/src/client.rs) 从已解析配置建立官方模型和厂商方言。`ModelClient` 固定本次模型、协议、认证、能力、输出上限及价格；目录刷新不修改在途连接。SDK 的较小输出预算会被保留。
- [`request.rs`](../crates/solaris-backend/src/request.rs) 在官方 handler 上集中实现有限重试和网关兼容修正，使用 Rig 的 ErrorReport 分类和 Retry-After。任何已解码内容或未知载荷到达后都不重放。不存在独立的旧 Engine。
- [`transport.rs`](../crates/solaris-backend/src/transport.rs) 使用官方 HTTP middleware 添加产品会话头及必要字段策略。唯一的传输适配是保留网关精确 completion URL；HTTP、分帧和协议解码全部由 SDK 实现。
- [`discovery.rs`](../crates/solaris-backend/src/discovery.rs) 使用官方 ModelListing 和游标驱动读取模型，Anthropic 分页全部跟随。薄适配保留 HTTP 200 错误/HTML 校验和仅含 ID 的网关条目；Codex 的订阅专用 slug、可见性及优先级 schema 通过 SDK Wire 扩展点处理。
- `run_turn()` 只保留业务事件兼容入口，复用相同连接和请求策略。[`events.rs`](../crates/solaris-backend/src/events.rs) 只转换显示事件，不发 HTTP 请求、不独立重试。正常 agent 与摘要直接消费原生响应。
- 测试或应用注入的事件源必须显式调用 `ModelClient::from_events`；适配集中在 [`injected.rs`](../crates/solaris-backend/src/injected.rs)，agent 不再重建响应或维护第二套摘要调用路径。
- 计费观察器属于请求连接上的产品策略，不属于 provider trait。agent 附着预算、账本和子任务观察器，收到结果后统一结算。价格使用连接快照，context 测量与计费独立。

### 2.3 Provider 配置与认证

`solaris-provider` 保留厂商选择、凭据存储、外部模型目录、用户覆盖、价格和列表缓存。每次请求从目录解析实际模型 ID，同时选择对应协议；不只更换模型名称而沿用旧协议。模型能力映射为官方 ModelFacts / ModelSpec，agent 只消费连接提供的固定能力。

浏览器登录和刷新使用官方 ChatGPT `Authenticator`：SDK 负责 PKCE、state、回调监听及固定 issuer 的令牌交换；Solaris 显示登录 URL、限制五分钟期限并保存既有凭据格式。SDK 尝试本机 1455、1457 端口；取消会停止监听。后台刷新禁用交互式设备登录。

父子连接共享刷新协调。轮换凭据保存失败时保留新令牌，后续重试落盘，不重复使用已失效的旧刷新令牌。HTTP 连接池继续复用。外部模型目录和价格属于 Solaris 产品配置，不被 SDK 内置目录替换。

## 3. solaris-agent：agent 会话

`solaris/src/assembly.rs::agent_backend` 是 provider 到 agent 的唯一转换入口，TUI、headless、认证刷新和子代理模型覆盖共用它。`BackendChoice` 与 `CredentialSource` 留在应用/provider 侧；agent 的 `AgentBackendConfig` 不包含凭据或端点。后端报告的上下文窗口优先，否则使用应用注入的 `fallback_context_window`。更换后端时，模型、附加指令与回退窗口一起更新。

`McpManager::connect(config, cwd)` 只连接与发现；应用遍历 `manager.tools()` 并调用 `registry.add(...)`。资源、资源模板和提示词对应的协议工具也遵循同一路径。失败服务器只产生状态诊断，不阻止已连接工具和本地工具使用。

模型只能「请求」，真正把「请求 → 执行 → 再请求」串起来的是这一层。它接收应用组装好的执行参数：`Agent` 包住一个已经解析好的后端，持有整段对话，按模式决定一轮声明哪些工具，跑完本轮的调用，把结果写回对话，再问一次。

```rust
pub struct Agent {
    choice: AgentBackendConfig, registry: Arc<ToolRegistry>, options: AgentOptions,
    cancel: Cancel, conversation: Arc<Conversation>, steering: Queue, follow_up: Queue,
}

impl Agent {
    pub fn new(choice: AgentBackendConfig, registry: Arc<ToolRegistry>, options: AgentOptions) -> Self;
    pub fn cancel_handle(&self) -> Cancel;
    pub fn steering_handle(&self) -> Queue;      // 纠正正在跑的这一轮
    pub fn follow_up_handle(&self) -> Queue;     // 这一轮之后另起一轮
    pub fn clear(&self);                         // /clear
    pub fn conversation_len(&self) -> usize;
    pub async fn compact(&self) -> Option<usize>;  // /compact
    pub fn run_turn(&self, prompt: impl Into<String>, mode: Mode) -> AgentEventStream;
}
```

### 3.0 执行器与对话所有权

[`driver.rs`](../crates/solaris-agent/src/driver.rs) 使用官方 `AgentBuilder`、`AgentRunner` 和 `run_channel()`。SDK 负责模型—工具—模型循环、工具结果配对及成功任务的 memory 保存；Solaris 不再手动驱动 `AgentRun`，也不维护第二份模型历史。

[`conversation.rs`](../crates/solaris-agent/src/conversation.rs) 包装官方 `rig-memory` Store：临时会话使用 MemoryStore，持久会话使用配置目录下 `sessions/memory/<session-id>` 的 DirStore。SQLite journal 保存转录、计费、工具意图及执行元数据；旧版消息仅在没有官方 memory 时导入一次。界面消息是官方消息的投影，不用于重新构造 SDK 工作历史。

每次模型调用通过后端提供的原生连接直接进入 SDK，保留原始响应、reasoning、finish reason 和 usage。钩子负责能力与预算检查、UI 投影、参数校验、权限、审批及用量结算。工具通过官方 ToolServer 注册，仍复用现有工具和子代理业务策略。

SDK 任务成功后保存消息；请求前安全交接也会保存已经配对完成的 SDK 原生历史。普通失败或取消不追加当前未提交阶段的消息；此前成功或已交接阶段仍保留。已经产生的工具副作用不会回滚，其意图、转录和费用记录独立保留。恢复时不会自动重放工具。

系统提示词来自当前资源，不存入待摘要的消息。上下文查询读取内存投影；模型请求使用 SDK 自己的工作历史。

### 3.1 提示词系统

系统提示词每轮从当前资源快照现写一次（[`prompt.rs`](../crates/solaris-agent/src/prompt.rs)）：

```text
You are solaris, a terminal assistant. …
# Working directory   ← AgentOptions.cwd：实际工作目录
# Working mode        ← Mode::instructions()：这一轮能做什么、不能做什么
# Using the tools     ← 每个工具自己的 Tool::guideline()，只列这一轮声明的
# Skills              ← 每个技能的名字、描述与 SKILL.md 路径，说明留在文件里
# How to work         ← 调查先于修改、最小改动、不重排无关代码、失败要读原因
# This endpoint       ← AgentBackendConfig.instructions，由应用解析后传入
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

1. Solaris 检查取消、存储状态和共享预算，在阶段开始前按需压缩官方 memory。
2. 创建官方 runner，注入模型连接、工具服务、策略钩子、memory 和 Recorder。
3. SDK 接收新 prompt，自动执行模型与工具循环。Solaris 排空 `run_channel()` 的事件，并同时监听取消和消费端关闭。请求前 hook 检查 steering、后台结果和上下文预算；需要变更正式历史时才停止旧阶段，校验并保存其原生历史后交接。
4. 成功后 SDK 保存本任务消息，Solaris 使用进程内官方请求锚点发布上下文投影并累计用量。无工具回复后的 steering 可用 retry_with_feedback 在同一 runner 内继续。
5. 处理已完成的后台任务、steering 和 follow-up；存在新输入时启动下一 SDK 任务，共享预算与计费继续累计。

任务内不设置固定模型轮数上限；取消、请求预算、费用预算、执行期限和子代理限制仍有效。SDK 或厂商错误直接返回，不靠错误文本猜测上下文溢出并隐式重放请求。

### 3.4 一轮内的并行与取消

**调用默认并行**：上限是 `max_parallel`（`DEFAULT_MAX_PARALLEL = 4`），结果按模型请求的顺序回填给模型；只要该批里任一工具声明了 `execution_mode() == Sequential`（`write`、`edit` 与 shell 都是），整批退回逐个执行——写入类工具的按文件锁保证的是不互相踩，串行保证的是「同一批里能读到刚写下的东西」。

**被输出上限截断的调用不执行**：协议层把 `stop_reason` 为 `length` / `max_tokens` / `incomplete` 变成 `AgentEvent::OutputTruncated`，执行钩子据此拒绝整批调用，返回配对错误结果，由 SDK 继续请求完整参数；所有调用仍受共享预算约束；残缺参数被解析成 `null` 时那句含糊的「参数不是 JSON 对象」不再是唯一线索。

**取消按轮次隔离**：`Cancel` 在每次 `run_turn` 开始时 `reset()`，上一轮的取消不会波及下一轮；应用层持有 `Agent::cancel_handle()`，`Ctrl+C` 因此能杀掉正在跑的命令，而不只是不再读事件。循环在独立任务里跑，返回的流被丢弃（用户取消）就等于取消这一轮——任务发现发送端已关闭便不再继续。

### 3.5 两个队列：steering 与 follow-up

一轮可能跑很久，而用户想说的话有两种，它们不是同一件事：

| | steering | follow-up |
| --- | --- | --- |
| 意思 | 「不对，是另一个文件」 | 「这轮弄完再跑测试」 |
| 何时被读 | 下一次模型请求前、无工具回复完成后，或等待后台结果时 | 当前工作、steering 和后台结果处理完后 |
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
- **steering 在安全边界交付**：工具结果配对完成后交接；无工具回复可通过官方反馈重试继续，否则「别结束了，改成 X」会被丢在刚结束的那一轮里。
- **follow-up 让循环继续跑**：它不是「等这一轮结束应用再发一次请求」，而是同一个循环里接着下一轮，所以对话是接着长的。循环为此报 `AgentEvent::FollowUp { prompt }`，应用据此把上一轮标记完成、并为这个提示词新开一个 Turn——从这一刻起流出来的东西都属于它。
- **记录由循环回报**：`AgentEvent::Steered { text }` 在消息**真正被读到**时发出，应用据此写进当前轮的 `Turn.steering`（附 `after_steps`，即当时已完成的调用数）。如果按「用户按下回车时」记录，那么刚好在模型收尾时到达的插话——读到它的可能是**下一轮**——就会在转录里挂错轮次。
- **显示位置**：[`state.rs`](../crates/solaris/src/state.rs) 用 `after_steps` 把消息画在它当时的位置（`↳ steered …`），因为「纠正落在哪里」才是这件事的重点。

应用侧的用法：流式期间直接回车 = steering（默认，也是最常用的）；`/queue <prompt>` = follow-up。后者用命令而不是快捷键，因为 `alt+enter` 已经被换行占用，而命令表顺带让 `/help` 与补全都能发现它。

### 3.6 上下文压缩

Solaris 保留 `Compaction` 的启用、预留回复空间和近期上下文配置；实现委托给官方 `rig-memory` 的 `CompactionPolicy`、`Summarizer`、`apply_summary` 和 `summary_state`。合法切点、工具调用/结果配对及增量摘要状态由 SDK 管理，不再维护自定义历史序列化和两次摘要算法。

自动压缩可在工具轮次之间通过 SDK 阶段交接执行，手动 `/compact` 使用同一路径。摘要调用通过当前原生模型连接，遵守 SDK 摘要输出上限、模型输出上限、共享计费和取消期限。Solaris 提供面向编程任务的结构化摘要提示词。摘要为空、被截断、包含工具调用或请求失败时，不替换原始 memory。

只有摘要成功并应用后才保存新的官方 conversation、使旧测量失效并发布 `Compacted`。系统提示词始终在摘要范围之外。清理旧工具输出与摘要规划使用官方 ClearToolOutputs 和 plan_summary；只有有效摘要产生后才一起保存。上下文溢出不靠猜测错误文案自动重发已可能产生副作用的操作。

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
| `bash` | 合并 stdout+stderr，超时可杀，输出尾部截断 |

所有平台统一使用 `environment::ExecutionEnvironment` 构造 Bash 子进程。Windows
只定位随包 Bash，禁用个人启动文件，显式加载 `/etc/profile`；PATH 限定为 Cygwin
目录，HOME/TMPDIR 位于用户可写位置。`grep/find` 通过固定 `exec "$@"` 脚本传递
程序名与参数，不再直接启动原生程序。Linux/macOS 使用系统 Bash、系统工具和 PATH。

`filesystem.rs` 通过固定脚本调用系统命令，实现用户文件的读取、目录枚举、原子替换
和撤销；Rust 不直接访问这些目标文件。二进制内容通过 stdin/stdout 传输，目录响应
采用 NUL 分隔字段，不解析人工目录列表。Rust 在内存中保留精确编辑、BOM/CRLF、
补丁与哈希检查。写入限制 16 MiB，读取限制 32 MiB，图片限制 8 MiB；固定文件操作
有 60 秒期限、输出上限和取消控制。应用内部数据库、变更记录目录、输出捕获等状态
仍由 Rust 管理。

Windows 工具路径统一使用 `/cygdrive/<盘符>/...`；应用原生工作目录与技能路径通过
`cygpath` 转为模型可见路径。文件工具与权限检查共用 Bash 路径规范化：`cd -P` 解析
目录链接，`readlink` 解析文件链接，并处理尚不存在的后缀。Windows 路径规则兼容
原生路径，并保守地忽略路径大小写；shell 规则始终按命令原文匹配。更改队列按规范
路径加锁，原子替换使用同目录临时文件、保留权限，变更前后仍做乐观冲突检查。
替换前同步临时文件，替换后同步父目录；Linux 使用 GNU `sync` 的逐文件屏障，
macOS/Cygwin 使用系统级 `sync`，因此写操作可能产生额外同步延迟。
这不是文件系统沙箱，也不宣称消除外部进程修改文件的竞争窗口。

Windows 启动校验包含随包命令集；缺失时明确提示发行包不完整，不回退到宿主工具。
`edit` 与 `write` 共用一张按路径的锁（`FileMutationQueue`）：一轮里模型完全可能同时要求改同一个文件的两处，两次「读—改—写」重叠就会丢掉一次改动。

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

名字在注册表里不存在时只是不被声明，不会报错；`unmatched()` 则专门报出选择里打错的名字。

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

Markdown 直接使用 crates.io 官方 `tui-markdown`，无本地副本、分支或 Cargo 补丁。通过公开的 `StyleSheet` 配置主题颜色，并隐藏标题的 `#` 前缀。无序列表、引用、代码围栏和分隔线使用官方默认标记（`-`、`>`、代码围栏、`---`）；不通过字符串替换修改正文。不启用代码语法高亮、代码块控件或独立查看器。

通用布局层仅按终端宽度换行，保留样式、空格和完整字素簇，Tab 按四列制表位展开。显示行同时构建 `TextDocument`，供滚动、选择和复制使用，最终绘制不二次换行；不推断列表结构来重建悬挂缩进。助手回复、子代理报告和 Markdown 弹窗使用同一个渲染入口，继续沿用现有 Transcript 缓存。

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
| 接入新的 wire 协议 | 在 backend 私有模型工厂选择官方 SDK 实现，更新业务 `Wire` 与 provider 配置，并补齐 HTTP 契约测试；不新增自绘协议编码器。 |
| 增删 provider 目录项 / 模型 | [`solaris-provider/src/providers.rs`](../crates/solaris-provider/src/providers.rs) 的 `PROVIDERS`（向导步骤、模型选择器、端点、环境变量、价格表，以及每个模型自己的 wire 都从这里读）。 |
| 调整重试 / 超时策略 | [`solaris-backend/src/http.rs`](../crates/solaris-backend/src/http.rs)：`BACKOFF`、`IDLE_TIMEOUT`、`CONNECT_TIMEOUT`。 |
| 新增斜杠命令 | [`crates/solaris/src/commands.rs`](../crates/solaris/src/commands.rs) 的命令表 + `App::execute_command` 加一个分支；补全、面板、帮助会自动跟随。 |
| 新增浮层对话框 | 在 [`dialogs.rs`](../crates/solaris/src/dialogs.rs) 实现 `Component`，经 `DialogMessage` 回传结果，再用 `push_overlay` 打开。 |
| 调整模型接入 | 更新 backend 私有 SDK 模型工厂与 provider 配置。生产只保留一个模型执行实现；`AgentBackend` 用于层间契约、策略包装及测试注入。 |
| 新增主题 | [`solaris-tui/src/theme.rs`](../crates/solaris-tui/src/theme.rs)：加调色板 + 登记进 `Theme::NAMES`。 |
| 新增 UI 组件 | 在 [`solaris-tui/src/components`](../crates/solaris-tui/src/components) 实现 `Component`，并加进 `components/mod.rs` 的再导出。 |
| 调整快捷键 | [`keymap.rs`](../crates/solaris/src/keymap.rs) 的 `default_bindings`。 |
| 换剪贴板实现 | [`clipboard.rs`](../crates/solaris/src/clipboard.rs) 的 `Clipboard::system()`，或在构造 `AppOptions` 时替换 `clipboard` 字段（测试就是这么注入记录器的）。 |
| 加宽/加高某个区域 | 用 `layout::split` 的 `Entry` 表达意图（`grow` / `min` / `max`），不要手算坐标。 |

---

## 8. 测试策略

测试分为三层，测试数量以 `cargo test --workspace --all-features --locked` 的结果为准：

- **单元与策略测试**：消息转换、缓存用量拆分、价格、请求策略、SDK 状态转换、工具配对、审批、预算、取消、压缩、steering、follow-up 和日志恢复。专门验证压缩释放 SDK 工作历史，以及宽松 schema 下的损坏参数不产生副作用。
- **真实 HTTP 回环测试**：在本地启动 `TcpListener`，通过官方 SDK 发送请求，检查三种协议的请求、图片与工具结果、认证头、模型别名、输出预算、流事件、截断、重试边界和取消释放连接。模型发现区分空清单、HTML 及 HTTP 200 错误包装。
- **集成与交互测试**：headless 入口、子代理并发与权限继承、运行时控制、会话持久化，以及 TUI 的布局、事件、渲染和输入行为。OAuth 刷新使用 SDK 的记录型 HTTP 客户端验证固定 issuer、令牌轮换、字段保留与错误脱敏，不依赖真实凭据。

验证命令：`cargo fmt --all -- --check`、`cargo test --workspace --all-features --locked`、`cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`、`cargo +1.97.1 check --workspace --all-targets --all-features --locked`。

---

## 9. 当前边界

- **工具调用已经打通，但工具集是固定的八个**。`Agent` 会跑「模型 → 工具 → 模型」直到模型不再请求，用量与费用按轮累计；应用也可注册 MCP 工具及其资源/提示词操作，沿用同一执行策略。
- **确认对话框只在 `--confirm-tools` 下出现**。默认是 `AlwaysApprove`（终端 agent 已经在替你动手，为一次 `read` 弹框只是噪音）；打开后，改动型调用会弹一个确认框，被拒绝的调用作为错误结果回到模型。
- **一次调用不可中断到一半**。`Ctrl+C` 取消一轮会让循环停止请求模型、并杀掉正在跑的命令，但已经在执行的 `edit` 不会回滚。
- **插话在请求安全边界生效**。steering 在下一次模型请求前交付；正在执行的工具不会被它打断，要立即停止仍使用 `Ctrl+C`。
- **follow-up 只能靠 `/queue`**。流式期间回车是 steering（默认、也最常用），想在当前这一轮之后另起一轮得打 `/queue <prompt>`——`alt+enter` 已被换行占用，而命令表顺带让 `/help` 与补全能发现它。
- **资源更新需要 `/reload`**。新增/删除技能、名称与描述修改、项目规则及追加说明修改通过 `/reload` 更新；显式调用每次读取当前正文。运行中的 stream（含后续队列）和压缩结束后才应用重载，已展开的排队消息保留入队时内容。
- **失败阶段的未提交消息不保存**。官方 memory 提交成功任务及安全交接的历史；普通失败保留此前已提交阶段。产品转录、工具意图和已发生费用独立持久化。
- **上下文使用官方估算与请求锚点**。可兼容的请求使用已报告用量加新增消息估算；模型、提示词、工具、历史变化和恢复会话后回退完整估算。合法切点由官方压缩策略管理。
- **摘要是有损的**。被替换掉的那些消息只剩模型写的那段摘要，原文不再发给 provider；摘要本身是一次模型调用，因此压缩只在必要时发生（或用户 `/compact` 时）。转录里会留一行说明压掉了几条。
- **`grep` 与 `find` 需要外部程序**。它们 shell out 到 ripgrep 与 fd；缺失时工具会明确报错并给出安装提示，而不是退化成另一个实现。
- **工具输出是截断的**。读类保留前 2000 行或 50KB，命令类保留末尾并把完整输出写进临时文件；被截断的那部分只有通过报告出来的路径才读得到。
- **价格是内置快照**。`provider` 表里的单价是打表值，账单可能不同；未知模型按 0 计。
- **报不了用量的服务端会被估算**。OpenAI 兼容服务里有一部分不实现 `stream_options.include_usage`，那一轮退回按字符估算，并在 `/stats` 里标注 `≈`。
- **Windows 上不启用 bracketed paste**，粘贴内容以按键事件到达（`Component::handle_paste` 不会被调用）。
- **拖选依赖系统剪贴板**。`arboard` 打不开剪贴板时（例如无 X11 / Wayland 的 headless 环境），高亮仍然生效，只是 `Ctrl+C` 会发一条「取不到剪贴板」的警告。
- **剪贴板只走文本**。`Ctrl+V` 粘贴的是文本；图片、文件不在范围内 —— Claude Code 的 `Ctrl+V` 是「贴图片」，solaris 没有这条通路。终端若把 `ctrl+v` 留给自己（Windows Terminal 默认如此），按键不会到达应用，走的是终端自己的粘贴通路。
- 伙伴素材是 claurst 十八个物种的一个子集；新增物种 = 一个 `Species` 变体 + 三帧 12 格宽的精灵图。

### Provider model metadata

`solaris-provider::ModelCatalog` normalizes models.dev v2 and OpenRouter catalogs
into provider-scoped snapshots. Startup merges independent validated caches over a
bundled snapshot and refreshes them in the background. Exact model lookup and user
overrides produce one `ResolvedModel` containing request aliases, capabilities,
limits, provider prices, context tiers and field provenance. Explicit capability references across providers never import prices or protocols.
Providers without catalogs can fall back to a unique exact model ID in the bundled
manufacturer allowlist, including official estimated prices; user overrides win. `choose_backend` captures an
immutable resolution for each HTTP request; later requests can observe refreshed
metadata while reusing the HTTP pool. Agent code consumes backend capabilities and
input budgets without depending on provider catalog types. Provider credentials and
wire selection remain in the application/provider boundary.

### Resource preparation, context projection, and billing

Resources own prepared model-path strings. Agent caches the in-memory prompt and
tool estimation inputs for each mode when resources or backend selection change.
Each request derives its actual tool loadout from those inputs. Context queries do
not invoke resource loaders or prompt builders. Reload workers run serially and
publish resources only when the session is idle; session navigation cannot overtake
a pending loader.

`rig-memory::Prepared`, `RequestStart`, `Anchor` and `next_request_tokens`
provide the common estimator for budgets and published UI snapshots. The projection
keeps shared native messages from the active SDK history, never authors memory,
and preserves process-local identities while that history grows. Tool changes
reestimate that working view, rather than the last saved conversation. Request
anchors are not restored from persisted usage records; restored sessions estimate.

Compaction uses the official clearing policy, summary planner and persisted summary
state. Cleaning and summarizing are committed together after a valid summary.
Automatic compaction can hand off at a request boundary after a completed tool batch.
There is no hidden overflow replay or restart of every model round.

`AccountingBackend` attaches a ledger observer to each prepared connection and
journals usage when its result is settled.
Child requests write to the parent ledger before forwarding their completion, so
parent failure or cancellation cannot discard already consumed usage. Absorbing
a child report updates turn totals without charging the ledger again.
Model and child charges are separate from context annotations. Billing restore
supports legacy cumulative completions and manual summary charges without counting
new request records twice. Successful task completion remains a cumulative API
event, while the session ledger also retains charges before failures/cancellation.
