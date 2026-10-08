# solaris-tools 与 pi 的差异

本文件对照 [earendil-works/pi](https://github.com/earendil-works/pi) 的**工具层与 agent loop**，逐项记录 solaris 的实现与它的区别。核对时间：2026-10-08，对照 pi 的 `main` 分支。

**怎么读**：第一节是**有意为之的取舍**（已确认的边界，不是缺陷）；第二、三、四节是**实现差异**，其中带 ⚠️ 的是会实际影响行为的缺口，第五节把它们按风险排了序。每节末尾给出 pi 侧的文件路径，便于回查。

> pi 是一个快速演进的项目，文件与常量都可能变动；本文件描述的是核对当时的状态。

---

## 1. 有意为之的取舍

这些是 solaris 主动选择与 pi 不同，而不是遗漏。

| 方面 | pi | solaris | 为什么 |
| --- | --- | --- | --- |
| Windows 上的 shell | 找 Git Bash（`%ProgramFiles%\Git\bin\bash.exe` → PATH 上的 `bash.exe` → 否则报错），**默认工具集里就是 `bash`** | 没有 `bash` 工具；Windows 上注册的是 `powershell` | 不假设用户装了 Git for Windows。给 Windows 声明 `bash` 是在声明一个可能跑不起来的东西 |
| `powershell` 参数 | `-NoProfile -NonInteractive -ExecutionPolicy Bypass -Command` | 只有 `-Command` | 保持最小；需要时按需加回 |
| `rg` / `fd` 缺失 | `ensureTool` **自动下载**二进制到 bin 目录 | 明确报错并给出安装提示 | 不引入网络下载与供应链成本 |
| 工具往返的轮数 | 没有上限（`while (hasMoreToolCalls)`） | `DEFAULT_MAX_ROUNDS = 24` | 模型可能卡在同一个请求上；没有上限的一轮既不会结束也不会停止花钱 |
| 工具执行的确认 | project trust + `tool_call` 处理器可拦截 | `Approver` 钩子，默认全部批准 | 交互式确认还没做；钩子先就位，当前行为是所有调用直接执行 |

---

## 2. 工具契约

pi 的 `ToolDefinition` 比 solaris 的 `Tool` trait 厚一层。

| 能力 | pi | solaris |
| --- | --- | --- |
| 字段 | `name` / `label` / `description` / `parameters` / `outputSchema` / `constrainedSampling` / `prepareArguments` / `executionMode` / `execute` + 渲染器 | `name` / `description` / `parameters` / `run` |
| 返回值 | `{ content: [{type:"text"｜"image"}], details }`，`details` 是**结构化**数据（`edit` 的 `diff`/`patch`/`firstChangedLine`、`read`/`grep` 的截断信息），专门喂给渲染器 | `ToolOutput { text, is_error }`，只有一段字符串 |
| 失败 | 工具可以 throw，循环把它转成 error tool result | 工具永不失败，自己返回 `is_error`（约定见 [`tool.rs`](../crates/solaris-tools/src/tool.rs) 的文档） |
| 参数准备 | `prepareArguments`，例如 `edit` 兼容顶层的旧写法 `oldText`/`newText` | 无 |
| 参数校验 | 跑之前 `validateToolArguments` 按 JSON Schema 校验 | 各工具用 `string_arg` / `optional_count_arg` 等助手自查 |
| 采样约束 | `read`/`edit`/`write` 带 `constrainedSampling: { type: "json_schema", strict: "prefer" }` | 没有这个概念，请求体里不带任何约束采样的字段 |
| 执行模式 | 每个工具可声明 `executionMode: "sequential"`，强制该批调用串行 | 无（一律串行，见 §3） |
| 渲染 | `renderCall` / `renderResult`，`pi.registerToolRenderer` 可覆盖任意工具 | 应用层在 [`transcript.rs`](../crates/solaris/src/transcript.rs) 里硬编码成一行 |

**系统提示词**：pi 把每个工具的 `promptSnippet`（一行说明）与 `promptGuidelines`（规则条目）注入系统提示词的 tools / rules 段，并列出可用的 skills；solaris 的系统提示词一个字没改，工具说明只出现在请求体的 `tools` 数组里。差别是模型少了一层「什么时候该用哪个工具」的引导。

> pi 侧：`packages/coding-agent/src/core/tools/tool-definition-wrapper.ts`、`core/system-prompt.ts`

---

## 3. agent loop

| 行为 | pi | solaris |
| --- | --- | --- |
| ⚠️ 执行顺序 | **默认并行**（`Promise.all`）；只有配置成 `sequential` 或该批里任一工具声明了 `executionMode: "sequential"` 才串行 | 始终串行 |
| 轮数上限 | 无 | 24（`ToolLoopOptions::max_rounds`） |
| ⚠️ 输出被截断的调用 | 检查 `stopReason === "length"`，**拒绝执行**并告诉模型「响应撞到输出上限、参数可能残缺，请重新发起」 | 不检查：残缺参数会被解析成 `null`，模型只收到「参数不是 JSON 对象」 |
| 工具事件 | `tool_execution_start` / `tool_execution_update`（流式部分结果）/ `tool_execution_end` | 只有 `ToolCall` + `ToolResult`，没有中间过程 |
| 嵌套调用 | `ctx.executeTool()`（codemode 脚本内调工具），事件带 `parentToolCallId` | 无 |
| 等待期间的用户输入 | steering / pending messages 可在循环内插入 | 应用层排队提示词（`queued_prompts`），在整轮结束后才跑 |
| 上下文压缩 | 有 compaction | 无 |
| 用量结算 | 每轮各自记账 | `ToolLoop` 拦下每轮的 `TurnComplete`，用 `Usage::merge` 累计后只发一个终态事件 |

solaris 的取舍：一轮对外仍是「一次提问到一次回答」，内部可能是多次模型调用；因为 `ToolLoop` 实现的是 `AgentBackend`，应用层不需要知道这件事。

> pi 侧：`packages/agent/src/agent-loop.ts`（`executeToolCalls` / `executeToolCallsSequential` / `executeToolCallsParallel` / `runToolCall`）

---

## 4. 单个工具的行为差异

### `read`

| | pi | solaris |
| --- | --- | --- |
| 图片 | 支持 jpg/png/gif/webp/bmp，作为附件回给模型；模型不支持图像时降级成一句文字说明 | 纯文本；非 UTF-8 文件报错 |
| offset 越界 | **错误**：`Offset N is beyond end of file (M lines total)` | 普通信息：`(file has M line(s); line N is past the end)` |
| 截断 | 2000 行 / 50KB，续读用 `offset` | 相同 |

### `grep`

| | pi | solaris |
| --- | --- | --- |
| ⚠️ 单行长度 | `GREP_MAX_LINE_LENGTH = 500`，超长行**截断**并提示「用 read 看完整行」 | 不截断单行（只受 50KB 总预算约束，一行仍可能占掉大半预算） |
| 匹配上限 | 100 | 100（相同） |
| 参数 | `pattern` / `path` / `glob` / `ignoreCase` / `literal` / `context` / `limit` | 相同 |

### `find`

| | pi | solaris |
| --- | --- | --- |
| 结果上限 | 1000 | 1000（相同） |
| `--no-require-git` | **有条件**加：非 git 仓库里才加，仓库内用 fd 默认的 git 感知行为 | 无条件加（普通目录里的 `.gitignore` 也会生效） |

### `ls`

默认 500 条 + 50KB 字节截断，两侧一致。

### shell（`bash` / `powershell`）

| | pi | solaris |
| --- | --- | --- |
| ⚠️ 进程树 | 超时与取消都 `killProcessTree`：Windows 走 `System32\taskkill.exe /F /T /PID`，Unix 走 `process.kill(-pid, SIGKILL)` 杀进程组 | Windows 走 `taskkill /T /F`；**Unix 上只 `child.kill()`，孙进程会泄漏** |
| ⚠️ 内存 | `OutputAccumulator` 边收边处理，内存有界；超预算即开临时文件并**把原始字节流进去** | 全读进 `Vec<u8>` → 截断 → 再写临时文件，**内存无界** |
| ⚠️ 输出净化 | `sanitizeBinaryOutput` 剔除控制字符与 U+FFF9–FFFB，避免打乱 TUI | `from_utf8_lossy` 之后原样返回 |
| timeout | 校验有限、为正、有上限（`MAX_TIMEOUT_MS`），超时即杀进程树 | 只校验「正整数」，没有上限 |
| 环境变量 | 默认把 `PI_SESSION_ID` / `PI_SESSION_FILE` / `PI_PROVIDER` / `PI_MODEL` / `PI_REASONING_LEVEL` 交给命令 | 不注入任何会话信息 |
| 完整输出 | 截断时把完整输出写临时文件，路径写在结果的 `full_output_path` 里 | 同样写临时文件并把路径写进文本通知 |

### `edit` / `write`

替换语义（`edits[{oldText,newText}]`、BOM 与行尾保留、`oldText` 必须唯一且互不重叠）、按文件串行化（`FileMutationQueue`）两侧一致。差别在返回值：pi 回结构化的 `diff` 与统一 `patch`，solaris 只回一句「Replaced N block(s)」。

### 截断

常量完全一致（`DEFAULT_MAX_LINES = 2000`、`DEFAULT_MAX_BYTES = 50 * 1024`，都是「先到者为准，不返回半行」）。差别在返回值的丰富度：pi 的 `TruncationResult` 记录 `truncatedBy`（`lines` / `bytes`）、总行数、总字节数、输出行数/字节数等，solaris 只记录「从哪一端丢了多少行」。

> pi 侧：`core/tools/{read,grep,find,ls,bash,powershell,edit,write,truncate,output-accumulator}.ts`、`utils/shell.ts`

---

## 5. pi 有、solaris 完全没有

| 能力 | pi | solaris |
| --- | --- | --- |
| MCP | 服务器工具 `mcp__<server>__<tool>` + 资源工具 `list_mcp_resources` / `read_mcp_resource` 等，四种 exposure | 无 |
| `codemode` | 模型写 JS，在 QuickJS 沙箱里通过 `tools.<name>()` 调其他工具，可并行 | 无 |
| `tool_search` | 对未声明的工具做 BM25 检索并声明给下一次调用 | 无 |
| 扩展注册工具 | `pi.registerTool()`，`prepareLoadout` / `setActiveTools` 动态启停 | 无（注册表在编译期固定为八个） |
| skills | 扫描 SKILL.md，把名字与描述注入系统提示词，按需读取 | 无 |
| 会话落盘 | 会话文件、`/tree`、resume、fork | 无（转录只在内存里） |
| 上下文压缩 | compaction | 无 |
| 权限 | project trust、`tool_call` 拦截、容器化 | 只有 `Approver` 钩子 |

---

## 6. 值得补的缺口（按风险排序）

前四项是**缺陷级**的——会打乱界面、会泄漏进程、可能执行残缺调用、会浪费输出预算——而不是单纯的「少个功能」。

| # | 缺口 | 症状 | 修法 |
| --- | --- | --- | --- |
| 1 | 命令输出未净化 | 命令打印的 `\x1b` 等控制字符会打乱 TUI 渲染 | 在 `shell.rs` 输出处剔除控制字符与 U+FFF9–FFFB（可参照 pi 的 `sanitizeBinaryOutput`） |
| 2 | Unix 不杀进程树 | 超时/取消后，命令派生的孙进程继续运行 | `kill()` 在 Unix 上改为杀进程组（`setsid` + 杀 `-pid`，或记录进程组） |
| 3 | 无 `stop_reason = length` 检查 | 参数被输出上限截断的调用仍会被执行，模型只收到一句含糊的「参数不是 JSON 对象」 | 让协议层把停止原因带出来，循环据此拒绝执行并给出可操作的说明 |
| 4 | `grep` 单行不截断 | 一个 minified 文件的一行就能吃掉大半个输出预算 | 加 `GREP_MAX_LINE_LENGTH = 500` 并在结果里说明 |
| 5 | shell 输出内存无界 | 打印海量输出的命令会把整段输出读进内存 | 改成边收边截断、超预算即开临时文件（pi 的 `OutputAccumulator`） |
| 6 | 参数不做 schema 前置校验 | 错误信息不如按 schema 校验来得准；每个工具都要自己查字段 | 循环里按 `parameters` 校验一次，失败直接作为错误结果回给模型 |
| 7 | 一轮内不并行 | 一轮里多个独立调用现在串行，慢 | 无依赖的调用并行执行（写类工具已有按文件锁，安全性现成） |
| 8 | 无 `constrainedSampling` | 对支持严格 JSON Schema 的服务端少了一层保障 | 在 `ToolSpec` 上加一个可选字段，三条 wire 各自下发 |

第 1–4 项可以直接开工，各配一个测试；第 5–8 项是能力补强，可以单独排期。
