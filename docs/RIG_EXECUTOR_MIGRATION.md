# 完整执行器迁移

基于官方 `rig-agent`、`rig-core`、`rig-memory` 0.46.0；使用官方依赖，不引入 vendor 或 SDK 分叉。

## 职责

| 层 | 当前实现 |
| --- | --- |
| provider | 厂商配置、目录、凭据及官方认证刷新；为每次请求解析模型配置 |
| backend | 提供原生模型连接，使用官方协议实现；检查能力、输出预算与请求选项 |
| agent 执行 | `AgentBuilder` / `AgentRunner` / `run_channel()` 完成模型和工具循环 |
| agent 消息 | 官方 MemoryStore / DirStore 为唯一消息存储；UI 读取投影 |
| agent 压缩 | 官方 CompactionPolicy、Summarizer、apply_summary、summary_state |
| agent 策略 | Solaris 钩子实现参数校验、审批、权限、预算、截断保护、事件与计费 |
| agent 子任务 | 前台和后台均运行完整 SDK；Solaris 管理模型选择、权限、生命周期和结果通知 |
| 产品持久化 | SQLite 记录转录、工具意图、用量和 Recorder 生命周期，不再另存一套模型历史 |

八个 crate 和单向依赖保持不变，应用继续负责组装。`solaris-tools`、`solaris-tui`、`solaris-mcp`、`solaris-types` 无需修改。公开接口沿用业务名称；SDK 类型与实现封装在内部。

## Provider / backend 收敛

- 每次请求准备明确的原生连接，固定模型 ID、协议、认证、能力、输出预算和价格。UI 请求投影、模型调用、摘要规划和费用结算使用连接的固定事实。
- 删除旧 Engine。原生执行和业务事件兼容入口共用官方模型与统一请求策略；重试依据 SDK 原生错误，收到输出或未知载荷后不重放。
- 标准模型发现使用官方 ModelListing 与游标驱动；仅补充严格列表 envelope 检查、稀疏网关标签和 SDK 未覆盖的 Codex 订阅 schema。
- 浏览器登录及刷新使用官方 Authenticator，删除 Solaris 自建 PKCE、回调 HTTP 服务和授权码交换。UI 异步接收 URL，凭据继续按原格式保存。
- 移除 `model_client() -> Option<_>` 和 provider trait 上的 `observe_usage`。事件源必须显式注入；预算、账本、子任务用量作为连接观察器保留。
- 官方 HTTP middleware 负责产品会话头及请求选项；只为非标准网关路径保留精确 completion URL 适配。没有 vendor、SDK 分叉或新 crate。

## 运行与持久化语义

- 模型响应直接进入 SDK，保留原生消息、reasoning、finish reason 和 usage。业务消息转换只用于显示、旧会话导入及注入式测试后端。
- SDK 任务成功后由官方 memory 保存。工具结果配对完成后的请求前边界可以主动交接：停止旧 runner，校验其原生完整历史前缀及工具配对，使用原始会话的并发写入检查提交，再启动新阶段。普通失败或取消不追加当前未提交阶段的消息；此前成功阶段和已交接阶段保留。
- 工具副作用不随消息提交回滚。工具意图、转录和已发生费用独立保留，恢复不会自动重放工具。
- steering 和已完成后台结果在下一次模型请求前交付。无工具回复后的 steering 使用官方 retry_with_feedback，保留回答与正式 user feedback；工具往返后的 steering 和后台结果通过安全阶段交接追加，不用临时 history patch 模拟持久消息。follow-up 等当前工作完成后启动。
- 自动压缩可在工具轮次之间通过安全交接执行，手动压缩使用同一路径。官方 ClearToolOutputs 先处理旧的大型成功输出，再由 plan_summary 规划摘要；清理和摘要只有在摘要有效时一起提交，失败保留原始 memory。摘要调用遵守 SDK 输出上限、取消期限和共享计费。
- 输出截断时，通过 dispatch/invalid-call hook 拒绝工具调用，由 SDK 保存配对错误结果并继续请求；不使用不支持工具调用的 ModelTurnAction::Retry。工具副作用为零，后续调用仍受共享预算限制。不猜测错误文案隐式重放上下文溢出的请求。
- 子任务沿用共享预算和计费链。后台执行是 Solaris 的生命周期服务，模型和工具执行仍由 SDK 完成。
- Recorder 只保留 effect 标识、状态、请求标识和用量等元数据，不复制完整请求、参数、原始响应或认证信息。

持久会话使用 `sessions/memory/<SESSION_ID>`；只有不存在官方 memory 时才导入旧 SQLite 消息。上下文估算使用官方 Prepared、RequestStart、Anchor 与 next_request_tokens。锚点只在进程内保存；恢复、替换、压缩或请求配置变化后重新估算，不将旧计费值当作上下文占用。UI 与预算读取同一投影；投影引用 SDK 原生消息，不写入 memory。旧版上下文 aside 只为读取兼容保留。

## 验证

`executor_contract.rs` 记录官方执行器的真实扩展点约束：审批等待与拒绝、请求补丁不修改持久消息、带工具调用的截断不能用 Retry 钩子重试。

`loopback.rs` 使用本地 HTTP 服务验证完整模型/工具循环、原生响应和 usage、唯一消息存储、失败不追加、Recorder、摘要输出上限、官方摘要状态，以及取消释放连接。其余 agent 集成测试覆盖预算、权限、steering、子任务、恢复和取消。

```sh
cargo fmt --all -- --check
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo +1.97.1 check --workspace --all-targets --all-features --locked
```

## 阶段协调与当前边界

正常模型—工具循环始终由完整 AgentRunner 驱动。on_completion_call 检查 steering、后台完成和上下文预算；没有协调需求就继续，有需求才停止并交接。内部停止有阶段状态与专用原因双重校验，不作为用户取消显示。保存失败不继续执行；取消与交接竞争时仍发送取消终态。

交接前保留官方 conversation 的保存基线，捕获 SDK 请求前的原始历史，核对它扩展保存前缀，并与 SDK 取消时返回的规范化历史等价。取消会合并相邻 user 消息，因此不直接用该规范化结果逐条比对原始前缀。保存时保留原有消息 ID 与 asides，再依赖 Store 的原子长度检查提交。模型请求预算、子任务计数、执行期限、计费和任务身份不会随 SDK 阶段重置。

这不是对同一个活跃 AgentRun 的原地修改，也不是每轮重启：只有需要正式历史变更时才交接。工具正在执行时不被 steering 打断；进程崩溃或直接丢弃 future 也不保证运行结束 hook 执行。当前不做每个工具步骤的耐崩溃消息提交，产品工具意图和费用仍独立记录。

## Provider 重构

provider 内部已拆成配置、选择、认证、内置目录配置和模型发现。
`ProviderConfig` 支持命名端点及模型，并直接持有 Rig 内置 Catalog 与用户覆盖形成的不可变目录。
Solaris 不下载、缓存或后台刷新外部元数据；已删除目录服务包装及相关启动入口。
已删除自建快照、上游解析器、价格阶梯实现及生成脚本。backend 直接接收官方
ModelFacts，编码和标准参考费用均使用官方实现。认证仍通过官方 authenticator，
共享刷新状态由 Solaris 管理。模块依赖方向不变，agent 不依赖 provider。

`models.json` 使用 `providers`、命名 `models` 和官方格式的 `overrides`，不兼容旧数组格式。
配置迁移示例见 README。模型列表发现与参数目录分离，显式元数据引用在应用用户覆盖后校验。
