# solaris 架构

本文描述 solaris 的分层结构、运行时数据流与扩展点。内容与仓库代码一一对应，对应版本 `0.1.0`。

---

## 1. 总览

solaris 是一个用 Rust 编写的终端 AI 助手：一个全屏 TUI 聊天客户端，外带一套可复用的终端 UI 框架。仓库是一个 Cargo workspace（edition 2024，Rust 1.85+），由四个 crate 组成，依赖严格单向：

```text
solaris              应用：根组件、对话框、/connect 向导、转录渲染、按键映射、CLI 入口
├── solaris-tui      可复用终端 UI 框架（不依赖工作区其他 crate）
├── solaris-backend  后端抽象：AgentBackend / MockBackend（依赖 solaris-core）
└── solaris-core     领域类型与纯逻辑（不依赖工作区其他 crate）
```

两条硬性约束：

1. **依赖只能向下**。`solaris-core` 与 `solaris-tui` 位于最底层且互不依赖；应用层只向上组合它们。
2. **UI 不认识任何 provider**。界面把一次请求交给 `AgentBackend`，只消费它回流的 `AgentEvent` 流；换成真实模型服务只需实现这个 trait。

| Crate | 目录 | 依赖 | 职责 |
| --- | --- | --- | --- |
| `solaris-core` | [`crates/solaris-core`](../crates/solaris-core) | `serde`、`serde_json`、`thiserror` | 领域类型与纯逻辑：斜杠命令解析、配置、provider 目录、消息与事件、凭据存储、伙伴、提示轮换。不涉及终端、渲染与网络。 |
| `solaris-tui` | [`crates/solaris-tui`](../crates/solaris-tui) | `ratatui`、`crossterm`、`unicode-width`、`unicode-segmentation` | 终端 UI 框架：组件模型、类 flex 布局、浮层、主题、按键匹配、组件集、终端生命周期。 |
| `solaris-backend` | [`crates/solaris-backend`](../crates/solaris-backend) | `solaris-core`、`async-trait`、`futures`、`tokio` | 后端抽象与确定性 mock 实现。 |
| `solaris` | [`crates/solaris`](../crates/solaris) | 上述三个 + `ratatui`、`crossterm`、`futures`、`anyhow`、`clap`、`tokio` | 应用本体，同时产出库与 `solaris` 二进制。 |

---

## 2. solaris-core：领域层

模块与关键类型：

| 模块 | 内容 |
| --- | --- |
| `config` | `Mode`（`Build` / `Plan`，`label()` 给出状态栏徽章，`next()` 用于 Tab 切换）、`Config`（模型、主题、模式、上下文窗口大小）。 |
| `event` | `AgentEvent`、`TurnRequest`、`BackendError` —— 后端与 UI 之间的唯一契约。 |
| `message` | `Message` / `Role`，供转录与历史构造使用。 |
| `command` | `SlashCommandSpec` 与 `PROMPT_SLASH_COMMANDS` 命令表；`parse_slash_command`、`matching_slash_commands` 是纯函数，供编辑器补全、命令面板与帮助对话框共用。 |
| `provider` | `ProviderSpec` / `AuthKind` / `PROVIDERS`：每个 provider 的 id、展示名、认证方式、徽章与可选模型，驱动 `/connect` 向导的步骤与模型选择。 |
| `auth` | `Credential`（API key / OpenAI 兼容端点 / OAuth token）、`AuthStore`、`mask_secret`（只保留末四位）。**只做序列化与领域判断，文件读写在应用层**。 |
| `buddy` | `Companion` / `Bones` / `Soul` / `Species` / `Rarity` / `Hat`：由用户 id 经 FNV-1a 播种，用 Mulberry32 掷出「骨架」，因此稳定且不可手工篡改。 |
| `recent` | `RecentActivity` / `RecentEntry`：历史提示词，用于欢迎框与提示轮换。 |
| `tips` | `TIPS` / `select(index)`：欢迎框里的起步提示，按会话序号轮换。 |

`AgentEvent` 是流式协议的核心：

```rust
enum AgentEvent {
    ThinkingDelta(String),                    // 思考轨迹分片
    TextDelta(String),                        // 可见回复分片
    Status(String),                           // 瞬时状态行
    TurnComplete { tokens: u32, cost_usd: f64 }, // 终态：成功
    Error(String),                            // 终态：失败
}
```

`TurnRequest { history, prompt, mode }` 是输入侧：历史（含一条描述当前模式的 system 消息）、本轮提示词、当前模式。

---

## 3. solaris-backend：后端抽象

```rust
pub type AgentEventStream = Pin<Box<dyn Stream<Item = AgentEvent> + Send>>;

#[async_trait]
pub trait AgentBackend: Send + Sync {
    async fn run_turn(&self, request: TurnRequest) -> Result<AgentEventStream, BackendError>;
    fn label(&self) -> &str { "backend" }   // 状态栏显示
}
```

- 一次 `run_turn` 返回一个装箱的 `Send` 流，UI 侧通过 channel 逐块消费。
- `MockBackend` 是当前唯一实现：给定 `chunk_delay`（默认 24 ms），先按词切块发出思考轨迹，再发一条 `Status("composing reply")`，然后分块发出 Markdown 回复，最后以 `TurnComplete` 结算 token 与费用。它**完全不碰网络**，因此整套 UI 可以在离线状态被完整驱动。
- 这也是唯一的扩展点：实现 `AgentBackend` 即可接入真实 provider，UI 无需改动。

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
- **松手不会取消选区**：释放后高亮留在原地，直到下一次按下另起一个选区、尺寸变化，或应用层主动清掉（例如 `/clear` 清空对话记录时）。框架也不会自动复制任何东西。
- `render` 的最后一步在**画完的帧**上重刷选中单元格的颜色，并记下每一行此刻显示的文字，供双击取词、三击取段使用。因为跑在最后，浮层里的单元格同样可选。
- 选区范围按行主序归一化并夹到帧内，拖到屏幕外会延伸到边缘而不是取消。
- 抽文本由应用层发起：`Selection::selected_text()` 给出当前选区的文字。solaris 把它绑在 `Ctrl+C` 上（§5.4）。

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

模块划分：`app`（根组件）、`state`（会话与通知）、`dialogs`（浮层对话框）、`connect`（内联向导）、`transcript`（转录渲染与缓存）、`commands`（命令注册表）、`keymap`（全局按键与页脚提示）、`clipboard`（平台剪贴板，藏在可注入的 `ClipboardWriter` 后面）。

### 5.2 启动装配（`main.rs`）

1. `clap` 解析参数；构造 `Config`（`--plan` 映射到 `Mode::Plan`）。
2. `config_dir()` 解析状态目录，优先级：`$SOLARIS_CONFIG_DIR` → Windows 的 `%APPDATA%\solaris` → `$XDG_CONFIG_HOME/solaris`，回退 `~/.config/solaris`。
3. 读取 `auth.json`、`companion.json`、`recent.json`；**读取失败一律当作空**，绝不因为文件缺失而拒绝启动。
4. 用 `MockBackend::with_delay(--chunk-delay-ms)` 构造后端（`Arc<dyn AgentBackend>`）。
5. 若给了 `--print-config`，打印解析结果后直接返回，不进入界面。
6. 否则：建 tokio runtime 并 `enter()`，安装 panic hook，`setup_terminal()`，建 `Tui` 与 `App`（四个句柄 —— `quit`、`overlay_queue`、`overlay_flag`、`selection` —— 都取自 `Tui`，外加 `system_writer()` 作剪贴板），`tui.set_root(app)`，`tui.run(&mut terminal)`，最后无论如何都 `restore_terminal()`。

### 5.3 App 的状态组成

| 分组 | 字段 |
| --- | --- |
| 依赖与配置 | `backend: Arc<dyn AgentBackend>`、`config`、`theme` |
| 会话 | `session: SessionState`、`transcript: TranscriptView`、`notifications: NotificationQueue` |
| 输入 | `editor: Editor`、`keybindings`、`inline: Option<ConnectFlow>` |
| 与框架的句柄 | `quit`、`overlay_queue`、`overlay_flag`、`selection` |
| 跨线程通道 | `events_rx`（后端事件）、`dialog_rx` / `dialog_tx`（对话框结果）、`device_rx`（设备码授权进度） |
| 持久化 | `auth` / `auth_path`、`buddy` / `buddy_path`、`recent` / `recent_path` |
| 剪贴板 | `clipboard: ClipboardWriter`（`Rc<dyn Fn(&str) -> bool>`，生产环境是 `arboard`，测试注入记录器） |
| 展示与动画 | `version`、`greeting`、`tip`、`spinner_frame`、`buddy_step` / `buddy_started`、`queued_prompts` |

`SessionState` 持有 `turns: Vec<Turn>`（每轮含提示词、回复、思考轨迹、token、费用、是否结束）、瞬时 `status`、滚动偏移、`follow_end`（是否吸附到最新一行）与单调递增的 `version`（渲染缓存的失效键）。`NotificationQueue` 是带存活时间的临时通知队列（Info / Warning / Error）。

复制发生在 `handle_key`：`Ctrl+C` 命中 `quit` 绑定时，先看共享的 `selection` 句柄里有没有文本 —— 有就交给 [`clipboard`](../crates/solaris/src/clipboard.rs) 并通知结果，没有才真的退出。

### 5.4 输入优先级

根组件 `handle_key` 的分发顺序固定为：

```text
最顶层浮层（由框架拦截）  →  内联 /connect 向导  →  会话（编辑器 + 全局快捷键）
```

内联向导虽然不是浮层，但在此期间它拥有键盘 —— 例如 `Ctrl+C` 是「取消向导」而不是「退出」。

全局快捷键里 `Ctrl+C` 名义上是「退出」，但只要当前**有选区**，它就改成复制选区（见 §4.4）；浮层存在时按键由浮层吃下，所以要先关掉对话框才能复制。

### 5.5 渲染管线

`render_session` 用一次 `layout::split` 把整屏切成三段：

```text
┌──────────────────────────────┐
│ 转录（grow(1)，min 1）        │  ← 欢迎框或对话，可滚动
├──────────────────────────────┤
│ 输入区（px，内容自报高度）      │  ← 编辑器，或 /connect 内联向导
├──────────────────────────────┤
│ 页脚（px 1）                  │  ← 状态、模式、provider、提示
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
enum DialogMessage { Cancelled, Theme(String), Model(String), Command(String),
                     Confirm { action: ConfirmAction, accepted: bool } }
```

应用侧只暴露 `open_help` / `open_stats` / `open_buddy` / `open_palette` / `open_theme_dialog` / `open_model_dialog` / `open_clear_confirm`，内部统一走 `push_overlay`。

`/connect` 是**刻意的例外**：它不弹浮层，而是接管输入区（与 Claude Code 的 `/login` 视觉一致）。[`ConnectFlow`](../crates/solaris/src/connect.rs) 是一个状态机：

```text
ConnectStep:   Provider → ApiKey | CustomProvider | DeviceAuth → Model
ConnectOutcome: Handled | Closed | ProviderPicked | Submit | ModelPicked
DeviceAuthStatus / DeviceAuthEvent: 设备码授权进度回传
```

流程状态（第几步、输入框内容）归 `ConnectFlow`；副作用（写凭据、激活 provider、拉起授权任务）归 `App`，因为那是应用状态而非流程状态。设备码授权的网络侧尚未实现，目前是一个替身任务：延时后发出一个设备码，再发一条「已获得 token」。

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
| 接入真实 provider | 在 `solaris-backend` 实现 `AgentBackend`，在 `main.rs` 换掉 `MockBackend`。UI 零改动。 |
| 增删 provider 目录项 / 模型 | [`solaris-core/src/provider.rs`](../crates/solaris-core/src/provider.rs) 的 `PROVIDERS`（向导步骤与模型选择器都从这里读）。 |
| 新增斜杠命令 | [`solaris-core/src/command.rs`](../crates/solaris-core/src/command.rs) 的命令表 + `App::execute_command` 加一个分支；补全、面板、帮助会自动跟随。 |
| 新增浮层对话框 | 在 [`dialogs.rs`](../crates/solaris/src/dialogs.rs) 实现 `Component`，经 `DialogMessage` 回传结果，再用 `push_overlay` 打开。 |
| 新增主题 | [`solaris-tui/src/theme.rs`](../crates/solaris-tui/src/theme.rs)：加调色板 + 登记进 `Theme::NAMES`。 |
| 新增 UI 组件 | 在 [`solaris-tui/src/components`](../crates/solaris-tui/src/components) 实现 `Component`，并加进 `components/mod.rs` 的再导出。 |
| 调整快捷键 | [`keymap.rs`](../crates/solaris/src/keymap.rs) 的 `default_bindings`。 |
| 换剪贴板实现 | [`clipboard.rs`](../crates/solaris/src/clipboard.rs) 的 `system_writer`，或在构造 `AppOptions` 时替换 `clipboard` 字段（测试就是这么注入记录器的）。 |
| 加宽/加高某个区域 | 用 `layout::split` 的 `Entry` 表达意图（`grow` / `min` / `max`），不要手算坐标。 |

---

## 7. 测试策略

工作区共 **293 个测试**，分两类：

- **单元测试**贴着被测代码放在各模块内（`solaris-core` 35、`solaris-tui` 121、`solaris-backend` 7、`solaris` 库 108），覆盖纯逻辑、布局、按键、渲染与状态机。
- **端到端冒烟测试** [`crates/solaris/tests/tui_smoke.rs`](../crates/solaris/tests/tui_smoke.rs)（22 个）：驱动真实技术栈（`Tui` 事件循环 + `App` + 框架组件 + mock 后端），渲染到 ratatui 的 `TestBackend`，因此整条 UI 链路无需真实终端即可断言。

```bash
cargo build --workspace --all-targets
cargo test --workspace
```

---

## 8. 当前边界

- **只有 mock 后端**。没有任何 provider 客户端；`/connect` 会收集并保存凭据、也能记录当前 provider，但回答始终来自 mock。
- **设备码授权是替身**。`spawn_device_auth` 只按固定延时发出预置事件，不联系任何授权服务器。
- **Windows 上不启用 bracketed paste**，粘贴内容以按键事件到达（`Component::handle_paste` 不会被调用）。
- **拖选依赖系统剪贴板**。`arboard` 打不开剪贴板时（例如无 X11 / Wayland 的 headless 环境），高亮仍然生效，只是 `Ctrl+C` 会发一条「取不到剪贴板」的警告。
- 伙伴素材是 claurst 十八个物种的一个子集；新增物种 = 一个 `Species` 变体 + 三帧 12 格宽的精灵图。
