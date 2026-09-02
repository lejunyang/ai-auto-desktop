# Rust 移植状态跟踪

> 基线日期 2026-09-01。本文记录 Python v0 已有、但 Rust 尚未实现的能力，用于跟踪重构进度。
>
> 本文只记录**经代码核实**的状态，不记录推测。每一条"未移植"都在写入前用检索或运行验证过；
> 核实方式写在条目里，便于后续复核。核实结论会随代码变化过期，改动相关模块时应重新核对。

## 0. 项目定位

Rust 是本项目的主实现，对外提供 **CLI + GUI** 两种形态，CLI 同时通过 skills + MCP 供 AI 使用。
`src/ai_auto_desktop/` 的 Python 实现**保留供查阅，不再继续演进**；它仍是行为语义的参考来源，
因为规范文档与它一致，而 Rust 尚未覆盖全部语义。

## 1. 已移植（Rust 中可用）

| 能力 | Rust 位置 | 说明 |
|---|---|---|
| 描述符模型与严格编译器 | `crates/aad-core` | 编译为不可变计划、拓扑排序、预算 |
| 受限表达式求值 | `crates/aad-core/src/expression.rs` | 手写 tokenizer + 递归下降，刻意保留 Python 语义（`/` 恒真除法、`//` 向下取整、`%` 跟随除数符号、链式比较、`-2 ** 2 == -4`） |
| 工作流引擎与控制流 | `crates/aad-runtime/src/engine.rs` | `action`/`set`/`block`/`if`/`switch`/`foreach`/`while`/`fail`/`return`/`script`、`on_error`/`finally` |
| 内存 journal | `crates/aad-runtime/src/journal.rs` | 见 §2.1：**不持久化** |
| 脚本沙箱 | `crates/aad-runtime/src/script.rs` | 已接入引擎；Windows 缺网络/文件系统隔离，如实上报 `degraded` |
| NDJSON 插件宿主 | `crates/aad-plugin` | stdio 控制面、manifest 校验、Job Object 限额 |
| AADF 制品线格式 | `crates/aad-plugin/src/artifact.rs` | 帧编解码完整，**但未接入宿主主流程**，见 §2.5 |
| Windows UIA driver | `crates/aad-uia` | 发现、描述、locator 合成、控制、staleness 校验 |
| 能力探测 | `crates/aad-probe` | 只读 |
| MCP server | `crates/aad-mcp` | 9 个工具，见 §2.6 |
| CLI | `crates/aad-cli` | 11 个子命令，见 §2.2 |
| 录制存取（子集） | `gui/src-tauri/src/recordings.rs`、`gui/src/recording.ts` | 存 locator 而非 target，见 §2.4 |

## 2. 未移植

### 2.1 持久化执行（进行中）

**Python**：`durable.py`(65KB)、`journal.py`(51KB)、`run_service.py`(26KB)
**规范**：`docs/architecture/runtime.md` §8.1

已完成：

- SQLite 持久化 journal（`crates/aad-runtime/src/durable.rs`，49 测试）：
  `runs` / `events` 两表、5 个状态机触发器 + 表级 CHECK、WAL + `BEGIN IMMEDIATE`；
  开库时**验证**而非假定 WAL / `foreign_keys` / `synchronous=FULL` 真生效，不满足即拒绝开库。
- owner lease fencing：`claim_owner` / `heartbeat_owner` / `release_owner`，token 只存 SHA-256
  且明文不进 `Debug`；`paused` 与终态在同事务释放 lease。
- `desiredState` 与 `status` 分离的 CAS 控制面：控制面不检查 lease（操作者可请求但无法伪造写入），
  `cancel` 是吸收态，终态拒绝后续控制。
- 分段执行（`engine.rs` 的 `Segmented`）：`SegmentState` 是纯数据，可写进 journal 再被另一进程读回；
  预算用 wall-clock epoch 而非 `Instant`（暂停一小时就花掉一小时，resume 不重发额度）；
  `run_segment()` 先 advance index 再执行，崩在步骤中途的段不会被静默重跑。
- checkpoint 编解码（`durable_exec.rs`）：version 不符 → `CHECKPOINT_UNSUPPORTED`，
  planDigest 不符 → `PLAN_MISMATCH`，缺 deadline → `CHECKPOINT_INVALID`（不得当成"无限制"）。
- `in_top_level_step` / `finalizing` 被中断时**零 dispatch** 落 `UNKNOWN_EFFECT`，连 cleanup 也不跑。
  **核实方式**：`a_killed_process_leaves_a_recoverable_journal` 真的 spawn 一个
  `crash_runner` 子进程并在它提交进度后 `kill`，再从磁盘恢复；实测在第 4 步被杀，
  恢复后 `status=unknown_effect`、`run.segment_entered` 计数不增（无重放）。

仍缺：

- `action_intent` v2 受限重放：当前是 `deny` 模式，**拒绝一切 action / script**
  （无 intent 机制就无法证明某次派发可安全重复）。规范另定义 `--durable-actions read-only`，
  详见下方「§2.1.1 action_intent 待决」。
- `run_service.py` 的服务层（尚未读）。

#### 2.1.1 action_intent 待决（范围未拍板）

**规范依据**：`docs/architecture/runtime.md` §8.1 第 200–235 行（已逐字读过）。

要解决的问题：durable 现在拒绝一切 action，因为进程死在 dispatch 中途时，journal 无法区分
「请求还没发出」与「请求已到达桌面、副作用已发生」。重放可能点两次按钮，报失败可能谎称什么
都没做，所以只能落 `unknown_effect` 让人来判断。`action_intent` 是把**一部分** action 从
「不可判定」拉回「可判定」的机制——注意是一部分，不是全部。

规范定义的准入条件（全部必须满足，任一不满足即 fail-closed）：

- 只允许**只读** action（provider contract 有效 `read_only`），且必须在**顶层**；
- 必须是单次 attempt：无 `if`、`precondition`、`postcondition`、retry、step handler、
  step `finally`；
- 嵌套 action、写 action、script、声明敏感 input/output 的 workflow 一律仍拒绝；
- manifest 与 descriptor step 的 input/output/error sensitivity 都必须显式为 `public`；
- 错误合约必须非空且**全部**为 `not_applied`；
- manifest 必须给出稳定的 `durability.checkpoint_fields`（每字段：有界 JSON Pointer +
  schema + 缺失策略）；descriptor 用 `checkpoint.output.mode` 选 `project`（provider
  白名单字段）或 `omit`（空输出，原始响应不持久化）；
- **拒绝任何 `artifacts` 契约**——ArtifactRef 是本次 live execution scope 的能力，
  不能作为跨进程重启的 durable checkpoint（§7 第 168–170 行）。

intent 里记什么：operation、step、已预留 attempt、原始 dispatch deadline，以及
provider / contract / projection / input binding 的**摘要**——**不记原始 action input**。
恢复时重新校验这些绑定，只对该只读 intent 安全重放；篡改、过期、不匹配一律在 dispatch 前
失败关闭。

为什么"只读 + 投影"就够安全：只读 action 重复执行不改变世界，所以重放是安全的；输出经
`checkpoint_fields` 白名单投影后才进 checkpoint，避免把无界或敏感的 provider 响应写进
持久存储。两个条件缺一不可。

仍然不变的底线：**没有**合法 `action_intent` 的 `in_top_level_step` / `finalizing` 依旧
必须零 dispatch 终结为 `UNKNOWN_EFFECT`；进入 workflow finally 前先写 `finalizing`，
避免崩溃后重复 cleanup。所有边界（intent、dispatch 授权、完成 checkpoint、终态提交）都用
期望 `desiredState` 做 CAS，pause/cancel 与完成并发时转入控制路径而不覆盖 operator 意图。

**注意规范自己声明的局限**（第 234–235 行）：lease 只在持久边界同步 heartbeat，它保证旧
owner 不能继续写 journal，但**不等于**能异步强杀已经进入插件或 OS 的调用。

未决：是否实现。做了之后能持久化运行的仍然只是「顶层只读观察 + 纯计算」类工作流；真正的
写操作（点击、输入）**依然**落 `unknown_effect`，因为写 action 的 reconciliation 规范
本身也列为 v1 之后的工作（§2 表格「写 action/script reconciliation」）。因此收益是否
匹配工作量，需要产品判断。

CLI 七命令已补齐（`start` / `resume` / `status` / `pause` / `cancel` / `list` / `events`），
与 Python 的差异是刻意的：

- 存储参数叫 `--store` 而非 `--journal`。`run --journal` 写 NDJSON 并**截断**目标文件，
  同名会让一次打错毁掉运行库。
- 未移植 `--plugin` / `--permission` / `--allow-scripts`：durable 目前拒绝 action/script，
  这三个参数在当前模式下无处生效，接受了却忽略比不提供更糟。
- 未移植 `--durable-actions`：只有 `deny` 一种模式可选时，提供选项是误导。

**核实方式**：真实二进制跨进程验证过完整链路——`start` 落库后由**另一个进程** `status` /
`list` / `events` 读回；`pause` 时 `desiredState=pause` 而 `status=running`（不谎称已停），
runner 在 `nextTopLevelIndex=4` 的段边界停下；换 `--owner-id` 的进程 `resume` 后跑到
`total=20`，`run.segment_entered` 恰 20 条且 stepId 全不重复（零重放）；`Stop-Process`
真杀 runner 后 `status=running` + 过期 lease + `phase=in_top_level_step`，`resume` 落
`unknown_effect` 并带 remedy，再次 `resume` 得 `DURABLE.ALREADY_TERMINAL`。

### 2.2 CLI 命令差异

**核实方式**：`aad help` 实际输出 vs `cli.py` 的 `add_parser` 调用。

Rust 有：`apps`、`describe`、`snapshot`、`find`、`do`、`probe`、`validate`、`run`、`mcp`、
`tools`、`start`、`resume`、`status`、`pause`、`cancel`、`list`、`events`（共 17 个）。
Python 另有 `edit`（属 §2.4 的浏览器编辑器，已由 GUI 取代，不再需要）。
参数层面的刻意差异见 §2.1 末尾。

### 2.3 录制规范的完整语义

**Python**：`recording.py`(41KB)、`recording_editor.py`(15KB)
**规范**：`docs/spec/recording-session-v1alpha1.md`
**核实方式**：检索 `redaction|platform_binding|of_step|disambiguation`，**零匹配**。

当前 Rust/GUI 实现的是一个够用的子集：locator + window selector + enabled + 凭据外提。规范要求但未实现：

- `redaction`：**已按 2026-09-03 修订的 §5 实现凭据部分**。driver 报
  `states.protected`（`CurrentIsPassword`），outline 同时给结构化字段与可读标记，
  locator 可按 `protected` 匹配；录制到的凭据外提为 required + sensitive 的 workflow
  input，`.workflow.json` 与 `.recording.json` 两侧都不落字面量。
  规范原本要求的「默认丢弃所有 `value`」「`title_policy: drop`」**已撤销**——实测表明
  平台自己就不给密码值，而丢弃普通值会破坏可判断性。仍未实现：`screenshots` 策略声明
  （当前根本不截图，等真要截图时再做）、`disclosed` 登记。
- `assertion` 步骤：录制侧的 `of_step` + `expect.mode` 语法仍未实现，但它**依赖的执行机制
  已经具备**——`postcondition` 现在会真正重新观察并轮询（见 §2.10），所以剩下的只是把录制
  的断言语法编译成 `postcondition`，而不再需要动引擎。
- `logic` 步骤：人工插入的 `condition`/`loop`/`assign`/`group`/`fail`/`return`/`script`。
- `disambiguation.strategy` 降级链：`unique` → `scoped` → `ordinal`(必须 `fragile: true`)
  → `unresolved`(必须 `enabled:false`)；已 verified 的步骤重新校验后必须能被降级。
- `platform_binding`：产物绑定单一平台（写值 win/mac 是 `set_value`、linux 是 `set_text`）。
- 引用完整性校验：每个 `steps.<id>`、`of_step`、`${{ inputs.X }}` 必须存在且 enabled，
  否则 `RECORDING.ORDER_INVALID`。**现有 workflow 编译器不覆盖这条**。
- 一整套 `RECORDING.*` 错误码。

**已知测试盲区（变异验证发现）**：把 `windows.rs` 的
`protected: flag(unsafe { element.CurrentIsPassword() })` 改成 `protected: None`，
**91 个 aad-uia 单测全部照常通过**——因为它们直接构造 `Node`，从不经过真实 UIA 读取。
只有真机探针（相邻放一个普通输入框和一个 `UseSystemPasswordChar` 密码框，断言前者
`protected=false` 且值可读、后者 `protected=true`）能发现这个回归。改动 driver 的属性
读取后必须跑真机验证，不能只看单测。

### 2.4 录制捕获（事件驱动）

**核实方式**：`gui/src/App.vue` 的 `record()` 由用户点击 outline 触发。

当前是"从 outline 手动挑动作"，不是监听可访问性事件自动录制。规范 §5.1 要求：
**不得安装全局输入钩子**（等同键盘记录器），只能依赖可访问性事件；UIA 事件投递方式取决于
COM 套间，现有 driver 在 MTA，故须假定回调在任意 RPC 线程并发到达；已知盲区
（点击不可聚焦元素、纯 hover）必须显式提示，不得静默丢弃或退化为坐标点击。

### 2.5 制品侧信道接入

**Python**：`artifact_ipc.py`(40KB)、`_win_named_pipe.py`(19KB)
**核实方式**：检索 `ArtifactReceiver|Receiver::`，仅命中 `artifact.rs` 自身测试与一处 re-export。

`artifact.rs` 的帧编解码有 24 个测试且全部通过，但通道建立未移植：POSIX 的 socketpair、
Windows 的受保护 ACL + 单实例 + 双向 PID 校验 named pipe，以及环境变量
`AAD_ARTIFACT_CHANNEL_FD` / `AAD_ARTIFACT_PIPE_NAME` / `AAD_ARTIFACT_HOST_PID`。

### 2.6 MCP 回放工作流（已完成）

**核实方式**：用真实 `aad mcp` 子进程跑一次 stdio 会话（脚本见提交说明），
`tools/list` 返回 12 个工具；`list_workflows` → `count=1`；`describe_workflow` →
`stepCount`/`inputs`/`planDigest`/`runnable`；`run_workflow(who=agent)` →
`status=succeeded`、`outputs={"greeting":"agent"}`。

新增三个工具：`list_workflows`、`describe_workflow`、`run_workflow`。AI 现在能跑存好的
工作流，而不只是一步步临场操作。

刻意的边界（都有测试）：

- **只按名字从 store 取**，不接受内联描述文件。否则 AI 可以自己编一份工作流让本机执行，
  等于绕过「引用真实观察过的元素」这条主线约束。
- **含 `script` 的工作流按名拒绝**（`MCP.SCRIPTS_REFUSED`，`effect=not_applied`），
  并在 hint 里指明由人执行的方式。
- `describe_workflow` 回报 `runnable`，使该限制可从只读调用发现。
- `list_workflows` / `describe_workflow` **不需要 driver**（读文件夹即可），
  真正要动桌面的工具才报 `DRIVER.UNAVAILABLE`。有一对正反测试锁定这条分界。
- `initialize` 的 instructions 明确引导到这三个工具——**这是跑真机会话才发现的缺口**：
  单测全绿但 instructions 完全没提它们，AI 可能永远不会用到。

配套改动：recordings store 从 `gui/src-tauri`（它本来零 Tauri 依赖）移到
`crates/aad-runtime/src/recordings.rs`，GUI / CLI / MCP 共用同一目录，避免各有一套路径逻辑。
新增 `list_workflows()` 与既有 `list_recordings()` 分工：前者列可运行的编译产物，
后者列可重新编辑的源——把二者混同会给调用方它跑不了的东西，或藏起它能跑的东西。
新增 `workflow_path()` 复用同一套 `validate_name`，使「按名查找」与「按名保存」不可能漂移。

仍缺：`run_workflow` 走内存 `aad_runtime::run`，未接 `DurableExecutor`——AI 跑长流程时
进程中断即丢失，接上后才能用 `status` / `resume` 跟进。是否要做未定。

#### 2.6.1 已发现的缺陷：`run_workflow` 未注册桌面 provider（已修复并验证，提交 0920057）

准备接持久化时先查了一件事：durable 目前拒绝 `action` 与 `script` 步骤，而录制导出的工作流
（`gui/src/recording.ts`）**全部是** `action` 步骤（`uses` + `with`）。也就是说接上 durable 后，
每一个真实录制都会被 `DURABLE.UNSUPPORTED_PLAN` 拒绝——先接 durable 是无用功。

顺着这条线读代码，发现一个更要紧的问题：`run_workflow` 构造的是
`RunOptions::default().with_inputs(...)`，**没有 `with_providers`**。CLI 的 `run` 路径在
`crates/aad-cli/src/main.rs:646-651` 明确注册了 `aad_uia::native_driver()`，MCP 这条路径没有。
后果：AI 通过 MCP 跑任何真实录制，都会在第一个 action 步骤上因找不到 provider 而失败，
而且失败长得像「录制坏了」而不是「调用方接线错了」。

修改内容：
- `Server.driver` 改为 `Option<Arc<UiaDriver>>`（registry 按引用计数持有 provider）；
- `tools::call` 与 `run_workflow` 接收 `&Arc<UiaDriver>`，`run_workflow` 内注册
  `providers.insert(driver.clone())`，与 CLI 构造同一套 registry；
- `run_workflow` 不在 `NO_DRIVER_NEEDED` 内，仍走需要 driver 的分支（这是对的：它要动桌面）；
- 新增 `StubBackend` / `stub_driver()` 测试替身，并加
  `a_recorded_workflow_reaches_the_desktop_provider`——用 **action 步骤**而非纯计算步骤断言，
  因为纯步骤根本不碰 provider，用它做断言会漏掉这个 bug。

**验证结果**：三层都做了。变异验证——去掉 provider 注册后新测试 FAILED，报
`ACTION.UNKNOWN: no provider offers action "desktop.windows_uia.find@1"`，正是 AI 会撞上的
那个错；真机 stdio 会话跑 action 步骤工作流得 `status=succeeded`；原有 8 项 MCP 验收全部重跑通过。

### 2.7 其他平台 driver

**核实方式**：`crates/aad-uia` 只有 Windows 实现；`plugins/` 下有 `macos_ax`、`linux_atspi`
（含 X11 helper 的 C++ 源码）、`ocr_tesseract`，均为 Python。

macOS AX、Linux AT-SPI、OCR 插件都未移植。规范要求三端分别实现和发布，不按 OS 猜能力。

### 2.8 GUI 编辑能力

Python 浏览器编辑器有、GUI 尚无：插入/编辑 logic 步骤（`/api/logic`）、撤销（`/api/undo`）。

### 2.9 测试覆盖

Python 有 45 个测试文件。针对未移植能力的部分在 Rust 侧没有对应物，其中较大的有：
`test_durable_*` 4 个共 106KB、`test_recording_compiler.py`(27KB)、
`test_macos_ax_driver.py`(84KB)、`test_linux_atspi_driver.py`(115KB)、`test_ocr_plugin.py`(44KB)。

### 2.10 postcondition 真正重新观察（已完成）

**为什么先做这个**：上一步把 `run_workflow` 推给了 AI。如果一个工作流「动作派发了但什么都
没发生」也报 `succeeded`，AI 会确信任务已完成——它没有眼睛看屏幕，只有这个状态字。

**先量再改**（真机跑 `aad run`，不是读代码）：

| 断言写法 | 改动前实测 | 说明 |
|---|---|---|
| `observe` 指向一个不存在的 provider | `status=succeeded` | 说明 `observe` **根本没被派发** |
| `timeout: 3s` + 永不成立的条件 | 0.02s 就失败 | 说明 `timeout`/`poll_interval` **被忽略** |

编译器一直接受 `observe` / `timeout` / `poll_interval`（`compiler.rs` 的 `assertion()`），
引擎却只做了一次 `condition` 求值。也就是说这三个字段是**声明了但不生效**的，比不支持更糟：
写断言的人以为自己有防护。

**改动**：`engine.rs` 的一次性检查换成 `check_postcondition` 轮询循环。

- `observe` 每轮都派发，结果绑定到 `observation` 供条件读取。重读**已存的** action 输出
  只会确认已经记下来的东西，而那正是被怀疑的对象。
- 没写 `timeout` 时只求值一次。桌面 UI 是异步的，轮询才让断言可用；但没要求等待却偷偷等，
  会把快速失败变成慢速失败。
- 观察 action 必须是 `read_only`（`POLICY.DENIED`）。会改变被检查对象的断言什么也证明不了，
  而藏在 postcondition 里的写操作还会绕过真实 action 步骤的风险与确认检查。
- 失败时报 `effect=unknown` 并附 `last_observation`——动作本身成功了，只是预期结果没出现，
  桌面到底变没变确实不知道，往任何一个方向断言都是猜。

**验证**：改动后同一组真机探针，`observe` 会派发（失败暴露出来）、轮询实际等满 3.05s。
另做端到端：填一个真实输入框并断言值确实写进去了 → `succeeded`；断言一个没发生的结果 →
`unknown_effect` + `ACTION.POSTCONDITION_FAILED`，且 `last_observation` 里带着屏幕上**实际**
的值，不用重跑就能诊断。五个新测试，三个变异验证各自使对应测试失败（4 / 2 / 1 个）。

**顺带补掉的两个缺口**（都是跑真机时才暴露的）：上一阶段加了按 `protected` 匹配 locator，
但 MCP 的 `find_element` schema 没暴露这个字段，而它是 `additionalProperties: false`——
AI 传了会被拒绝，等于这个能力对 AI 不存在；`aad find` 同样没有 `--protected`。两处都补了，
真机确认 `--protected true` 精确命中密码框，`--protected false` 命中 10 个时如实报
`DRIVER.AMBIGUOUS_MATCH` 而不是猜第一个。

## 3. 已知环境限制（非缺口，如实记录）

- 本机装有输入过滤软件（AutoHotkey / LogiBolt 一类），`SendInput` 对 ≥2 事件的批次返回
  `sent=0` 且 `GetLastError()==0`。击键因此绝不拆批，失败即报 `DRIVER.INPUT_BLOCKED`
  （拆批会导致文字错乱：目标会 latch 前一字符）。
- 受保护窗口返回 `0x80004005`，属 UIPI 正常行为。
- DPI 与完整性级别探测为 `degraded`。
- Windows script 沙箱缺网络/文件系统隔离，按 Python 原行为如实上报 `degraded` + `gaps`。
- `aad-plugin` 的 `a_requested_manifest_completes_the_handshake` 偶发失败（`cargo test --workspace`
  跑过一次失败，随后单独跑 4/4 通过、在committed 基线上加 4 路 CPU 负载跑 3/3 通过，
  故非本次改动引入）。fixture 是 Python 子进程，握手默认 30s，怀疑是并行下解释器启动被拖慢。
  尚未定位，暂记录不掩盖。

## 4. 不在计划内

- **删除 Python**：保留供查阅。
- **skills 目录**：等 CLI 能力齐全后再写。
