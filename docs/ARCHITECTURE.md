# solaris 架构

本文描述 solaris 的分层结构、运行时数据流与扩展点。内容与仓库代码一一对应，对应版本 `0.1.0`。

---

## 1. 总览

solaris 是一个用 Rust 编写的终端 AI 助手：一个全屏 TUI 聊天客户端，外带一套可复用的终端 UI 框架。仓库是一个 Cargo workspace（edition 2024，Rust 1.85+），由五个 crate 组成，依赖严格单向：

```text
solaris              应用：根组件、对话框、/connect 向导、转录渲染、按键映射、CLI 入口
├── solaris-tui      可复用终端 UI 框架（不依赖工作区其他 crate）
├── solaris-provider 平台层：provider 目录、鉴权与模型信息，把凭据解析成一个后端
├── solaris-backend  传输层：一个客户端讲三条 wire，不认识平台、也不认识模型
└── solaris-core     领域类型与纯逻辑（不依赖工作区其他 crate）
```

两条硬性约束：

1. **依赖只能向下**。`solaris-core` 与 `solaris-tui` 互不依赖；`solaris-provider` 建在 `solaris-backend` 之上——平台层必须知道「谁讲哪种协议」，而传输层反过来什么平台都不需要知道。应用层只向上组合它们。
2. **UI 不认识任何 provider**。界面把一次请求交给 `AgentBackend`，只消费它回流的 `AgentEvent` 流；换成真实模型服务只需实现这个 trait。

| Crate | 目录 | 依赖 | 职责 |
| --- | --- | --- | --- |
| `solaris-provider` | [`crates/solaris-provider`](../crates/solaris-provider) | `solaris-backend`、`solaris-core`、`async-trait`、`serde`、`serde_json`、`thiserror` | 平台层：`providers`（provider 目录——认证方式、wire、端点、环境变量、每个模型的上下文窗口/最大输出/单价）、`choose`（`Credential` 解析成后端，`AuthStore`、`mask_secret`）。向导步骤、模型选择器与价格表都从这一张表读。 |
| `solaris-core` | [`crates/solaris-core`](../crates/solaris-core) | `serde`、`serde_json`、`thiserror` | 领域类型与纯逻辑：斜杠命令解析、配置、provider 目录、消息与事件、token 计费、伙伴、提示轮换。不涉及终端、渲染、凭据存储与网络。 |
| `solaris-tui` | [`crates/solaris-tui`](../crates/solaris-tui) | `ratatui`、`crossterm`、`unicode-width`、`unicode-segmentation` | 终端 UI 框架：组件模型、类 flex 布局、浮层、主题、按键匹配、组件集、终端生命周期。 |
| `solaris-backend` | [`crates/solaris-backend`](../crates/solaris-backend) | `solaris-core`、`async-trait`、`futures`、`tokio`、`reqwest`（rustls）、`bytes`、`serde`、`serde_json`、`thiserror` | 传输层：`AgentBackend` 统一接口、`HttpBackend`（三条协议的请求体与流解析）、SSE 解码、重试与计费。它只接收一个已经解析好的请求，因此既不认识平台也不认识模型。 |
| `solaris` | [`crates/solaris`](../crates/solaris) | 上述四个 + `ratatui`、`crossterm`、`futures`、`anyhow`、`clap`、`tokio` | 应用本体，同时产出库与 `solaris` 二进制。 |

---

## 2. solaris-core：领域层

模块与关键类型：

| 模块 | 内容 |
| --- | --- |
| `config` | `Mode`（`Build` / `Plan`，`label()` 给出状态栏徽章，`next()` 用于 Tab 切换）、`Config`（模型、主题、模式、上下文窗口大小）。 |
| `event` | `AgentEvent`、`TurnRequest`、`BackendError` —— 后端与 UI 之间的唯一契约。 |
| `usage` | `Usage` / `Price`：四个互不重叠的 token 桶（普通输入、输出、缓存读、缓存写）外加「这是估算」标记；`Price` 是 USD / 百万 token 的单价。 |
| `message` | `Message` / `Role`，供转录与历史构造使用。 |
| `command` | `SlashCommandSpec` 与 `PROMPT_SLASH_COMMANDS` 命令表；`parse_slash_command`、`matching_slash_commands` 是纯函数，供编辑器补全、命令面板与帮助对话框共用。 |
| `provider` / `auth` | 已迁至 [`solaris-provider`](../crates/solaris-provider)：`ProviderSpec` / `ModelSpec` / `PROVIDERS` / `AuthKind` 在它的 `providers`，`Credential` / `AuthStore` / `mask_secret` 在它的 `choose`。核心层不再认识任何平台。 |
| `buddy` | `Companion` / `Bones` / `Soul` / `Species` / `Rarity` / `Hat`：由用户 id 经 FNV-1a 播种，用 Mulberry32 掷出「骨架」，因此稳定且不可手工篡改。 |
| `recent` | `RecentActivity` / `RecentEntry`：历史提示词，用于欢迎框与提示轮换。 |
| `tips` | `TIPS` / `select(index)`：欢迎框里的起步提示，按会话序号轮换。 |

`AgentEvent` 是流式协议的核心：

```rust
enum AgentEvent {
    ThinkingDelta(String),                    // 思考轨迹分片
    TextDelta(String),                        // 可见回复分片
    Status(String),                           // 瞬时状态行
    TurnComplete { usage: Usage, cost_usd: f64 }, // 终态：成功
    Error(String),                            // 终态：失败
}
```

`TurnRequest { history, prompt, mode }` 是输入侧：历史（含一条描述当前模式的 system 消息）、本轮提示词、当前模式。

---

## 3. solaris-backend：传输层

```rust
pub type AgentEventStream = Pin<Box<dyn Stream<Item = AgentEvent> + Send>>;

#[async_trait]
pub trait AgentBackend: Send + Sync {
    async fn run_turn(&self, request: TurnRequest) -> Result<AgentEventStream, BackendError>;
    fn label(&self) -> &str { "backend" }   // 状态栏显示
}
```

- 一次 `run_turn` 返回一个装箱的 `Send` 流，UI 侧通过 channel 逐块消费。
- **`ProviderBackend`** 是真实客户端。它只认识 §2 的 `Wire`，于是三条协议各一个模块，统一放在 [`protocols/`](../crates/solaris-backend/src/protocols) 下：
  - [`anthropic`](../crates/solaris-backend/src/protocols/anthropic.rs) —— Messages API（`POST /v1/messages`，`x-api-key` + `anthropic-version`）。注意 system 提示词必须提到顶层 `system` 字段，不能留在 `messages` 里，所以历史里的 system 条目会被摘出去。另外最后一条消息会带一个 `cache_control: {type:"ephemeral"}` 断点：缓存是逐块开关的，断点会把它之前的一切（系统提示词 + 已发出的轮次）缓存下来，于是第二轮起不变的前缀按缓存读计价。低于 provider 的最小可缓存长度时它会被忽略，短对话不多花钱。
  - [`openai`](../crates/solaris-backend/src/protocols/openai.rs) —— Chat Completions（`POST /chat/completions`，Bearer），兼容性下限。New API、OpenRouter、Google 的兼容端点、本机 Ollama 与 llama.cpp 走的都是这条。因为「OpenAI 兼容」描述的是**响应**形状而请求字段各家不同，这个模块还带一张**修补表**：服务端用 400 指名它不认识的字段（`stream_options`、`max_tokens`）时，改一次请求体重发，而不是预先猜——每条修补每轮最多应用一次。
  - [`responses`](../crates/solaris-backend/src/protocols/responses.rs) —— Responses API（`POST /responses`，Bearer），OpenAI 自己走这条。system 提示词与 Messages API 一样提到顶层，但字段名是 `instructions`，转录是 `input` 条目列表，长度上限叫 `max_output_tokens`（不是 `max_tokens`）。请求带 `store: false`：终端会话不该把转录留在服务端。它比 Chat Completions 宽进严出——每条事件既在 `event:` 上命名、又在负载的 `type` 里重述一遍，所以网关丢掉前者也读得出来。
  - 顺带：`reasoning_content` / `reasoning`（Chat Completions）与 `response.reasoning_text.delta` / `response.reasoning_summary_text.delta`（Responses）若出现就映射成思考分片，不出现就跳过；`thinking` 参数本身不下发，因为不支持的模型会直接 400。
- 三条协议共用的部分：
  - [`sse`](../crates/solaris-backend/src/sse.rs) —— 跨 chunk 的 SSE 帧解码，**按字节**缓冲，所以帧切在多字节字符中间也不会损坏。
  - [`http`](../crates/solaris-backend/src/http.rs) —— 共享的 `reqwest` 客户端（10s 连接超时，无整请求超时，因为一轮本来就要流几分钟）、状态码 → 可操作的错误文案、退避重试。
  - [`wire`](../crates/solaris-backend/src/wire.rs) —— 按 `Wire` 分发请求体构造与流解析。跨协议但**只有 OpenAI 需要**的字段在这里补上：`prompt_cache_key` 只在 `Wire::is_openai()` 且端点主机就是 `api.openai.com` 时发送（OpenAI 的缓存本来就是自动的，这个键只影响路由，而兼容网关可能因为不认识它而拒掉整个请求，不值得冒险）。
- **模型发现**：`AgentBackend::models()` 是可选能力（默认实现直接报「本后端不会列模型」），`ProviderBackend` 用 `Endpoint::models_url()`（Messages API 走 `/v1/models?limit=1000`，OpenAI 兼容走 `/models`）发一次 GET，复用同一套认证；`parse_models` 从 `{"data":[{"id":…}]}` 里取 id——两条 wire 的清单形状相同。**不重试**：它只喂选择器，慢或不可达的代价应当是退回目录，而不是让用户等。**网关的 `/v1` 在解析层补齐**：OpenAI 兼容网关的 API 挂在 `/v1` 下，而 `/connect` 收集来的 URL 常常只写到主机，于是请求打到的是网关的网页前端——它用 200 + HTML 回答，`parse_models` 读不出任何 id，于是「没有模型」而不是一个看得见的错误。所以 `resolve()` 会给没有路径的 base URL 补上 `/v1`（带路径的一律按原样使用：只有部署者知道 API 挂在哪），Messages API 不在其列——它的 base 就是主机本身。**200 但不成清单的回复是一种要报出来的失败**：`parse_models` 返回 `Option`，`None` 表示这压根不是清单（代理的 HTML 页、被包进 200 的错误），此时错误信息指名 URL 并转述网关自己的说法，而不是当成一个没有模型的 provider。真正空的清单（`{"data":[]}`）则是答案——网关在说这个凭据够不到任何模型；没有目录可退时这一条也会告知用户。这两者必须分得开，否则「URL 打错地方」和「网关不给模型」看起来一模一样。
- **重试只在第一个事件发出之前**（429 / 5xx / 传输错误；指数退避并尊重 `Retry-After`，最多 3 次）。已经吐过字就绝不再试，否则会重复输出。中断靠丢掉接收端：app 放弃 `rx` 后 `send` 失败、任务结束、`reqwest` 的流随之被 drop，请求被取消。
- **计费**：`Usage` 把各家的报告归一成四个互不重叠的桶（两家 OpenAI 协议都把缓存读计入 `input_tokens` / `prompt_tokens`，会被减掉）；提供商什么都没报时退回按字符估算并置位 `estimated`，`/stats` 会据此显示 `≈`。价格来自 `provider` 表的内置快照，未知模型不收费。生成的 token 还是 0 的 `usage` 块不算「报告过」。
- **上下文 vs 会话总量**：`SessionState::total_tokens()` 是全会话累计（帧脚把它和累计费用并排显示），`SessionState::context_tokens()` 才是「窗口有多满」——取最后一轮有上报的 usage，加上其后新提交内容的估算；没有任何上报时退化为对提示词取估算。把累计量拿去比窗口是错的，因为窗口是每请求的，而累计会无限增长。
- **没有可用凭据时**，`lib.rs` 里的 `UnconnectedBackend` 接管：它不发任何请求，而是把每一轮直接变成一条可操作的错误（「run /connect」，或指名该 provider 的环境变量），页脚也会写 `unconnected`。这样既不会假装有回复，也不会发一个必然 401 的请求。
- [`lib.rs`](../crates/solaris-backend/src/lib.rs) 的 `choose_backend(auth, model, options)` 是应用唯一需要知道的入口：按「环境变量 > `auth.json` > 本机运行时无需凭据」解析出一个 `BackendChoice`（后端 + provider id + 端点 + 凭据来源）。解析不出可用凭据就交给 `UnconnectedBackend`（`CredentialSource::Missing`）；OAuth 类 provider 在签名流程落地前明确报告 `NotImplemented`，同样不发请求。模型名为空时（用户还没选过模型）换成该 provider 提供的第一个模型；用户明确给过的名字则原样送出。**目录里也没有名字时**（网关与自定义端点就是这样）凭据一样解析出端点，只是包成 `NamelessBackend`：turn 照旧拒绝——空模型名只会换来 400——但 `models()` 直通真正的客户端，因为那份清单正是名字的唯一来源，否则 `/model` 会永远空着（`CredentialSource::NoModel`，provider id 仍然给出来）。`BackendOptions` 里的 `environment` 可注入，测试因此既不继承 shell 里的 key、也碰不到网络。
- `AgentBackend` 是唯一扩展点：再实现一个即可接入新协议，UI 零改动。

---

## 4. solaris-tui：终端 UI 框架

### 4.1 组件模型

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

### 4.2 布局

[`layout`](../crates/solaris-tui/src/layout.rs) 是一组纯几何函数，语义对齐 flex（对应 pi-tui 的 `VStack` / `HStack`）：

- `Axis::Vertical` / `Axis::Horizontal`
- `Basis::Cells | Percent | Auto`
- `Entry { basis, grow, shrink, min, max }`，配 `px` / `percent` / `auto` / `grow` / `shrink` / `min` / `max` 构造
- `split(area, axis, entries) -> Vec<Rect>`

框架与应用共用同一套布局，不存在两套排版逻辑。

### 4.3 浮层

[`overlay`](../crates/solaris-tui/src/overlay.rs) 负责几何：`Anchor`（默认居中，另有八向锚点）、`SizeValue::Cells | Percent`、`OverlayOptions`（宽高、`min_width`、最大宽高、锚点、偏移、外边距；默认宽 70%、`min_width` 30、最大宽 90%、最大高 80%、外边距 1），`resolve(area, options) -> Rect` 给出最终矩形。

绘制时浮层是**不透明**的：先 `Clear` 掉矩形区域，再绘制组件 —— 因此不存在半透明叠加的排版问题。

### 4.4 事件循环与输入路由

[`Tui`](../crates/solaris-tui/src/tui.rs) 持有根组件、浮层栈与两个句柄：

- `OverlayQueue = Rc<RefCell<Vec<(Box<dyn Component>, OverlayOptions)>>>` —— 根组件用它**请求**打开浮层，不在渲染中途直接改动栈。
- `QuitFlag = Rc<Cell<bool>>` —— 根组件用它请求退出。
- `overlay_flag: Rc<Cell<bool>>` —— 报告「当前有浮层」，根组件据此让编辑器失焦（即便浮层是被点击外部关闭的）。
- `SelectionHandle = Rc<RefCell<Selection>>` —— 全屏拖选状态，见 §4.5。

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
- 抽文本由应用层发起：`Selection::selected_text()` 给出当前选区的文字（按行去掉尾随空白；整块都是空白的选区则原样保留，所以缩进也能复制）。solaris 把它绑在 `Ctrl+C` 上，而且**只要有选区就复制** —— 空白选区不会掉进「清空输入框 / 退出」那一档（§5.4）。

### 4.5 组件集、主题与终端

- 组件集（[`components`](../crates/solaris-tui/src/components)）：`Editor`、`SelectList`（带模糊过滤）、`Markdown` 渲染、`ScrollView`、`Panel`、`Text`、`Spacer`、`Loader`、`Welcome`（欢迎框）以及 `fuzzy` 匹配。
- 拖选（[`selection`](../crates/solaris-tui/src/selection.rs)）：`Selection` 记锚点 / 焦点、按 `selection_bg` / `selection_fg` 给单元格上色、抽取文本；`Tui::selection()` 把句柄交给应用层。
- 主题（[`theme`](../crates/solaris-tui/src/theme.rs)）：`Theme` 含约二十个颜色槽位（含 build / plan 两种强调色），`Theme::NAMES = ["dark", "light"]`，未知名称回退到 dark。
- 按键（[`keys`](../crates/solaris-tui/src/keys.rs)）：`ctrl` / `alt` / `plain_char` / `is_submit` / `is_newline` 等匹配助手、`parse_key_spec`（支持 `ctrl+k`、`alt+enter`、`f1`…）、以及 action → 按键的 `Keybindings` 表。
- 终端生命周期（[`terminal`](../crates/solaris-tui/src/terminal.rs)）：进入 raw mode + 备用屏幕 + 鼠标捕获（Windows 上**不**开启 bracketed paste，因为 Windows Terminal 的转义序列不会被控制台后端解码为粘贴事件），并安装 panic hook —— 先恢复终端再交给原 hook，保证 panic 不会把用户留在 raw mode。`restore_terminal` 通过原子标志保证幂等。

---

## 5. solaris：应用层

### 5.1 形态

同一个 crate 同时提供**库**与**二进制**：库（`lib.rs`）承载全部 UI 与状态逻辑，`main.rs` 只做参数解析与装配。这样集成测试可以 `use solaris::{App, AppOptions}` 直接驱动真实技术栈。

模块划分：`app`（根组件）、`state`（会话与通知）、`dialogs`（浮层对话框）、`connect`（内联向导，以及复用它渲染的 `/model` 选择器）、`transcript`（转录渲染与缓存）、`commands`（命令注册表）、`keymap`（全局按键与页脚提示）、`clipboard`（平台剪贴板，藏在可注入的 `Clipboard` 后面）。

### 5.2 启动装配（`main.rs`）

1. `clap` 解析参数；构造 `Config`（`--plan` 映射到 `Mode::Plan`，`--model` 未给时模型名留空，由 provider 决定）。
2. `config_dir()` 解析状态目录，优先级：`$SOLARIS_CONFIG_DIR` → Windows 的 `%APPDATA%\solaris` → `$XDG_CONFIG_HOME/solaris`，回退 `~/.config/solaris`。
3. 读取 `auth.json`、`companion.json`、`recent.json`；**读取失败一律当作空**，绝不因为文件缺失而拒绝启动。
4. `--provider`（可选）覆盖本次运行的 provider，只改内存里的 `auth`，不重写 `auth.json`；未知 id 直接报错并列出可选值。
5. 用 `choose_backend(&auth, &model, BackendOptions::default())` 解析出初始后端；显式给了 `--provider` 却解析不出凭据时，向 stderr 说明「先运行 /connect 或设置环境变量」，以免静默降级。
6. 若给了 `--print-config`，打印解析结果（含 backend / endpoint / 凭据来源）后直接返回，不进入界面。
7. 否则：建 tokio runtime 并 `enter()`，安装 panic hook，`setup_terminal()`，建 `Tui` 与 `App`。`AppOptions.backend_factory` 是一个闭包（生产环境就是 `choose_backend`），四个句柄 —— `quit`、`overlay_queue`、`overlay_flag`、`selection` —— 都取自 `Tui`，外加 `system_writer()` 作剪贴板；`tui.set_root(app)`、`tui.run(&mut terminal)`，最后无论如何都 `restore_terminal()`。

### 5.3 App 的状态组成

| 分组 | 字段 |
| --- | --- |
| 依赖与配置 | `backend: Arc<dyn AgentBackend>`、`backend_factory`（按凭据与模型重新解析后端）、`provider_id`（解析到的 provider，`/model` 据此列模型）、`config`、`theme` |
| 会话 | `session: SessionState`、`transcript: TranscriptView`、`notifications: NotificationQueue` |
| 输入 | `editor: Editor`、`keybindings`、`inline: Option<ConnectFlow>` |
| 与框架的句柄 | `quit`、`overlay_queue`、`overlay_flag`、`selection` |
| 跨线程通道 | `events_rx`（后端事件）、`dialog_rx` / `dialog_tx`（对话框结果）、`device_rx`（设备码授权进度） |
| 持久化 | `auth` / `auth_path`、`buddy` / `buddy_path`、`recent` / `recent_path` |
| 剪贴板 | `clipboard: Clipboard`（`write` / `read` 两个可注入的闭包，生产环境是 `arboard`） |
| 展示与动画 | `version`、`greeting`、`tip`、`spinner_frame`、`buddy_step` / `buddy_started`、`queued_prompts` |

`SessionState` 持有 `turns: Vec<Turn>`（每轮含提示词、回复、思考轨迹、`Usage`、费用、是否结束）、瞬时 `status`、滚动偏移、`follow_end`（是否吸附到最新一行）与单调递增的 `version`（渲染缓存的失效键）。`NotificationQueue` 是带存活时间的临时通知队列（Info / Warning / Error）。

**后端在会话中途会被重新解析**：`/connect` 写入凭据、`/model` 改模型时都调 `App::rebuild_backend()` → `resolve_backend()`。它做三件事，而不只是换一个后端：把凭据与模型交给 `backend_factory`；**采纳真正会被询问的模型**（模型名为空时换成该 provider 提供的第一个模型，因为 provider 不会接受空模型名）；按模型目录更新 `config.context_window`，footer 的占比条据此才诚实。`AppOptions::new()` 会把传进来的那个后端**钉死**成一个固定工厂（并把模型原样回传），所以测试驱动的永远是它自己交给 app 的后端；生产环境则用真实的 `choose_backend`。

**模型发现在后台跑**：`App::new` 与 `/connect` 之后各调一次 `App::refresh_models()`，它把 `backend.models()` 丢进 tokio 任务，答案经 channel 在 `tick()` 里收。发现只做加法——没落地时选择器就用内置目录，永远不落地就一直用它。清单到手后：若模型还没命名，取其中的第一个并重建后端（与目录提供首个模型是同一套替换）；选择器改用它，并从目录里补上已知模型的描述与单价。`AppOptions::discover_models` 控制是否发起这次请求：**二进制里开着，`AppOptions::new()` 里关着**，所以测试即使驱动真实 `choose_backend` 也不会自己联网。**`/connect` 的模型步不等这份清单**：网关与自定义端点的目录里本来就没有名字，所以凭据一落地就进入 Model 步（列表还空着时由选择器的说明行交代正在向 provider 要），答案到手后 `App::fill_in_models()` 把它填进同一个选择器——否则这一步只会在有内置目录的 provider 上出现，网关用户存完 key 就被直接弹回会话。清单是空的、而目录也没得退时这一步没有东西可选，于是收掉向导，只留下那条「name one with /model <name>」的通知。

复制发生在 `handle_key`：`Ctrl+C` 命中 `quit` 绑定时，先看共享的 `selection` 句柄里有没有文本 —— 有就交给 [`clipboard`](../crates/solaris/src/clipboard.rs) 并通知结果，没有才真的退出。

### 5.4 输入优先级

根组件 `handle_key` 的分发顺序固定为：

```text
最顶层浮层（由框架拦截）  →  内联向导 / 选择器  →  会话（编辑器 + 全局快捷键）
```

内联向导虽然不是浮层，但在此期间它拥有键盘 —— 例如 `Ctrl+C` 是「取消向导」而不是「退出」。

全局快捷键里 `Ctrl+C` 走的是三家（claurst / Claude Code / pi）同一套阶梯：

```text
有选区            → 复制选区（pi-tui 把 ctrl+c 绑给 copy）
正在流式生成      → 中断本次生成（丢掉事件接收端，后端任务随之结束；
                     已生成的内容留在屏幕上，队列里的下一条接着跑）
否则              → 清空输入框，并提示 "press ctrl+c again to quit"，
                     2 秒内第二次按下才真的退出
```

`Ctrl+D` 是同一个手势的另一半，只在输入框为空时生效 —— 否则按键留给编辑器。浮层存在时按键由浮层吃下，所以要先关掉对话框才能复制。

`Ctrl+V` 走反方向：从剪贴板取文本，插进当前接受输入的地方 —— 内联向导的字段在场就给它，否则给输入框；剪贴板为空时提示 `clipboard is empty`。claurst 就是在同一个键（它还接受 `Cmd+V`）上读剪贴板、并对空剪贴板报警的。

### 5.5 渲染管线

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

### 5.6 一次对话的完整数据流

```text
编辑器提交
   │
   ▼
App::start_turn
   ├─ TurnRequest { history: session.history(mode), prompt, mode }
   ├─ 追加一个未完成的 Turn，follow_end = true，session.bump()
   ├─ 建 tokio unbounded channel，events_rx = Some(rx)
   └─ tokio::spawn：消费 backend 流，逐条 tx.send(event)
                     （run_turn 失败则发一条 AgentEvent::Error）
   │
   ▼  （后台任务，主线程不阻塞）
每帧 App::tick
   ├─ try_recv 排空 events_rx → apply_event 写入当前 Turn / status
   ├─ 收到终态事件或通道断开 → 结束该轮、清 status、
   │                          取出 queued_prompts 中的下一条提示词开跑
   ├─ 排空 dialog_rx → on_dialog_message（主题/模型/命令/确认）
   ├─ 排空 device_rx → 驱动内联向导的设备码步骤
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
- **按需重绘**。渲染只由 `dirty` 触发；`ratatui` 的双缓冲差分只把变化的单元写进终端，所以空闲帧是零成本的。
- **流式期间仍可输入**。此时提交的提示词进入 `queued_prompts`，在当前轮结束后按序执行。
- **终态必达**。即使后端流意外结束且没有终态事件，`tick` 也会把该轮标记为完成并停掉 spinner。

### 5.7 对话框与内联向导

对话框（[`dialogs`](../crates/solaris/src/dialogs.rs)）有一条**唯一**的模态路径：每个对话框都是普通 `Component`，被压入框架的浮层栈，结果通过 `mpsc` 回传，而不是反向持有应用引用。

```rust
enum DialogMessage { Cancelled, Theme(String), Command(String),
                     Confirm { action: ConfirmAction, accepted: bool } }
```

应用侧只暴露 `open_help` / `open_stats` / `open_buddy` / `open_palette` / `open_theme_dialog` / `open_clear_confirm`，内部统一走 `push_overlay`。

**列表类选择器走内联**：`/connect` 与 `/model` 都不弹浮层，而是接管输入区（与 Claude Code 的 `/login` 视觉一致）。[`ConnectFlow`](../crates/solaris/src/connect.rs) 是一个状态机：

```text
ConnectStep:   Provider → ApiKey | CustomProvider | DeviceAuth → Model
ConnectOutcome: Handled | Closed | ProviderPicked | Submit | ModelPicked
DeviceAuthStatus / DeviceAuthEvent: 设备码授权进度回传
```

`/model` 由 [`model.rs`](../crates/solaris/src/model.rs) 实现：它只写下这个命令的策略（列表说什么、选中意味着什么），列表本身是框架组件 [`InlineSelect`](../crates/solaris-tui/src/components/inline_select.rs) —— 标题、说明、问题行、`❯` 行、底部提示、按键、滚轮与命中测试都在那里，`/connect` 的 provider / model 两步用的也是它，所以两者样式不会漂移。`App` 用一个 `Inline` 枚举持有两者并转发事件。

Model 步可以先于清单打开：[`ConnectFlow::enter_models`](../crates/solaris/src/connect.rs) 接受空列表（此时给选择器加一行「正在问 provider」的说明），provider 的答案到达后同一个方法再调一次，把列表填上并把当前模型设为高亮行，回车即沿用——所以网关用户看到的第三步和内置目录的 provider 完全一样，只是晚半秒。

流程状态（第几步、输入框内容）归 `ConnectFlow`；副作用（写凭据、激活 provider、重新解析后端）归 `App`，因为那是应用状态而非流程状态。文本步骤会从 `AuthStore` 回填该 provider 已存的 URL 与 key（key 照常掩码显示），所以重新 `/connect` 看到的是已保存的内容而不是空字段，直接回车即沿用；`ctrl+u` 清空当前字段，用来换成另一个 key。设备码授权的网络侧尚未实现，所以向导的这一步会直接报「not implemented」并让用户改用 API key —— 编一个占位 token 更糟：它会连上一个回答不了的 provider，然后第一轮以一个解释不了任何事的认证错误失败。

### 5.8 持久化

三个 JSON 文件都写在同一个状态目录下，全部由应用层负责读写（领域层只负责序列化）：

| 文件 | 内容 | 写入时机 |
| --- | --- | --- |
| `auth.json` | `AuthStore`：凭据（按 provider id）+ 当前 provider 与模型 | `/connect` 产生变更后 |
| `companion.json` | 伙伴的「灵魂」：名字、性格、孵化时间 | `/buddy name <name>` 之后 |
| `recent.json` | 最近提示词 | 每次提交提示词后 |

写入走 `write_json`：先 `create_dir_all` 建父目录，再写文件。三个文件彼此独立，删掉任意一个只会重置对应那部分状态。

---

## 6. 扩展点

| 想做的事 | 改哪里 |
| --- | --- |
| 接入新的 wire 协议 | 在 [`solaris-core/src/provider.rs`](../crates/solaris-core/src/provider.rs) 的 `Wire` 加一个变体，在 [`wire.rs`](../crates/solaris-backend/src/wire.rs) 的 `request_body` / `WireStream` 各加一个分支，照 [`protocols/`](../crates/solaris-backend/src/protocols) 里既有的三个模块写一个只做「请求体 + 流解析」的模块，最后让 `PROVIDERS` 里对应的 provider 指向它。UI 与 `choose_backend` 都不用动。 |
| 增删 provider 目录项 / 模型 | [`solaris-core/src/provider.rs`](../crates/solaris-core/src/provider.rs) 的 `PROVIDERS`（向导步骤、模型选择器、端点、环境变量与价格表都从这里读）。 |
| 调整重试 / 超时策略 | [`solaris-backend/src/http.rs`](../crates/solaris-backend/src/http.rs)：`BACKOFF`、`IDLE_TIMEOUT`、`CONNECT_TIMEOUT`。 |
| 新增斜杠命令 | [`solaris-core/src/command.rs`](../crates/solaris-core/src/command.rs) 的命令表 + `App::execute_command` 加一个分支；补全、面板、帮助会自动跟随。 |
| 新增浮层对话框 | 在 [`dialogs.rs`](../crates/solaris/src/dialogs.rs) 实现 `Component`，经 `DialogMessage` 回传结果，再用 `push_overlay` 打开。 |
| 新增主题 | [`solaris-tui/src/theme.rs`](../crates/solaris-tui/src/theme.rs)：加调色板 + 登记进 `Theme::NAMES`。 |
| 新增 UI 组件 | 在 [`solaris-tui/src/components`](../crates/solaris-tui/src/components) 实现 `Component`，并加进 `components/mod.rs` 的再导出。 |
| 调整快捷键 | [`keymap.rs`](../crates/solaris/src/keymap.rs) 的 `default_bindings`。 |
| 换剪贴板实现 | [`clipboard.rs`](../crates/solaris/src/clipboard.rs) 的 `Clipboard::system()`，或在构造 `AppOptions` 时替换 `clipboard` 字段（测试就是这么注入记录器的）。 |
| 加宽/加高某个区域 | 用 `layout::split` 的 `Entry` 表达意图（`grow` / `min` / `max`），不要手算坐标。 |

---

## 7. 测试策略

工作区共 **444 个测试**，分三层：

- **单元测试**贴着被测代码放在各模块内（`solaris-provider` 40、`solaris-core` 31、`solaris-tui` 130、`solaris-backend` 67、`solaris` 库 142），覆盖纯逻辑、布局、按键、渲染、SSE 解码、三条 wire 的请求体与流解析（用录制回放，不联网）、缓存断点与 `prompt_cache_key` 的门控、模型清单的解析与 URL、上下文与累计口径、凭据存取与脱敏、选后端 / 选模型与状态机。
- **回路测试** [`crates/solaris-backend/tests/loopback.rs`](../crates/solaris-backend/tests/loopback.rs)（7 个）：在 loopback 上起一个真的 `TcpListener`，用真的 `reqwest` 去请求它。请求头、`Content-Length` 读取、SSE 分帧、用量结算、模型清单的 GET 与 Bearer 头、401 的报错文案、429 的退避重试，这一整条链路都由真 socket 验证过 —— 仍然不碰外网。
- **端到端冒烟测试** [`crates/solaris/tests/tui_smoke.rs`](../crates/solaris/tests/tui_smoke.rs)（27 个）：驱动真实技术栈（`Tui` 事件循环 + `App` + 框架组件），渲染到 ratatui 的 `TestBackend`，因此整条 UI 链路无需真实终端即可断言。它自带一个只回显提示词的 `FakeBackend` 顶替真实 provider，并把 `environment` 换成空表，所以既不会继承 shell 里的 key，也不会联网。

```bash
cargo build --workspace --all-targets
cargo test --workspace
```

---

## 8. 当前边界

- **没有工具调用**。`AgentEvent` 里没有工具调用/结果，也就没有多轮 agent loop、权限确认、MCP 与会话落盘；一轮就是一次请求。
- **设备码授权仍是替身**。`spawn_device_auth` 不联系任何授权服务器，只回报「尚未实现」，所以 `claude-subscription` 目前必须手工往 `auth.json` 里放 token 才会真的发请求。
- **价格是内置快照**。`provider` 表里的单价是打表值，账单可能不同；未知模型按 0 计。
- **报不了用量的服务端会被估算**。OpenAI 兼容服务里有一部分不实现 `stream_options.include_usage`，那一轮退回按字符估算，并在 `/stats` 里标注 `≈`。
- **Windows 上不启用 bracketed paste**，粘贴内容以按键事件到达（`Component::handle_paste` 不会被调用）。
- **拖选依赖系统剪贴板**。`arboard` 打不开剪贴板时（例如无 X11 / Wayland 的 headless 环境），高亮仍然生效，只是 `Ctrl+C` 会发一条「取不到剪贴板」的警告。
- **剪贴板只走文本**。`Ctrl+V` 粘贴的是文本；图片、文件不在范围内 —— Claude Code 的 `Ctrl+V` 是「贴图片」，solaris 没有这条通路。终端若把 `ctrl+v` 留给自己（Windows Terminal 默认如此），按键不会到达应用，走的是终端自己的粘贴通路。
- 伙伴素材是 claurst 十八个物种的一个子集；新增物种 = 一个 `Species` 变体 + 三帧 12 格宽的精灵图。
