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

## 运行与持久化语义

- 模型响应直接进入 SDK，保留原生消息、reasoning、finish reason 和 usage。业务消息转换只用于显示、旧会话导入及注入式测试后端。
- 每个 SDK 任务成功后由官方 memory 保存。失败或取消不追加当前任务未提交的消息；之前成功的阶段不受影响。
- 工具副作用不随消息提交回滚。工具意图、转录和已发生费用独立保留，恢复不会自动重放工具。
- steering、follow-up、后台结果在任务边界作为正式输入交付；不使用仅影响一次请求的历史补丁。长任务内不即时插入 steering。
- 自动压缩在任务之间运行；手动压缩使用同一官方摘要实现。摘要调用遵守 SDK 输出上限、取消期限和共享计费；失败不替换原始 memory。
- 带工具调用的输出截断直接停止任务，整批不执行。不猜测错误文案并自动重试上下文溢出，也不隐式重放工具。
- 子任务沿用共享预算和计费链。后台执行是 Solaris 的生命周期服务，模型和工具执行仍由 SDK 完成。
- Recorder 只保留 effect 标识、状态、请求标识和用量等元数据，不复制完整请求、参数、原始响应或认证信息。

持久会话使用 `sessions/memory/<SESSION_ID>`；只有不存在官方 memory 时才导入旧 SQLite 消息。上下文测量以官方 message ID 对应的 aside 保存，不进入模型消息。

## 验证

`executor_contract.rs` 记录官方执行器的真实扩展点约束：审批等待与拒绝、请求补丁不修改持久消息、带工具调用的截断不能用 Retry 钩子重试。

`loopback.rs` 使用本地 HTTP 服务验证完整模型/工具循环、原生响应和 usage、唯一消息存储、失败不追加、Recorder、摘要输出上限、官方摘要状态，以及取消释放连接。其余 agent 集成测试覆盖预算、权限、steering、子任务、恢复和取消。

```sh
cargo fmt --all -- --check
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo +1.97.1 check --workspace --all-targets --all-features --locked
```

## 当前边界

完整 runner 不提供可原地修改的活跃会话。因此长时间单个 SDK 任务内的即时 steering、压缩和部分失败消息提交没有被模拟成与旧循环相同的行为；它们采用上述任务边界和成功提交语义。若未来需要工具轮次之间安全暂停并交接，需要单独验证 SDK 的暂停与提交协议，不能依靠请求历史补丁实现。
