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

**改完之后又发现轮询本身是坏的**（还是靠真机探针，不是读代码）：断言「点完之后对话框出现」
时，`find` 在对话框还没出现的每一轮都报 `DRIVER.NOT_FOUND`，而这个错误**中止了整个等待**——
3 秒的窗口 0.20 秒就退出。也就是说轮询对**最常见的那类断言**完全不可用，而那正是它存在的理由。

底层其实早就是对的：driver 给 `DRIVER.NOT_FOUND` 标了 `retryable`，这个标记也一路传到引擎，
只是我的循环没看它。改为：`retryable` 的观察失败算「还没到」，继续等；非 `retryable`（未知
action、被拒的写、observe 写错）立刻上报，因为它不会自愈，重试只是把失败拖慢。

顺带修了 driver 的一处不一致：`DRIVER.NOT_FOUND`（元素没出现）是 `retryable`，
`DRIVER.WINDOW_NOT_FOUND`（窗口没出现）却不是。两者一样短暂，后者不标就没法表达「等一个
对话框弹出来」。

正反两面都在真机上验过，用一个按下按钮 1.2 秒后才开新窗口的 fixture：
- 永远不出现的东西 → 完整轮询 3.16s 后报断言失败（改之前是 0.20s 中止）；
- 迟到 1.2 秒才出现的窗口 → 1.37s 成功。**这一条不能省**：只测前者的话，一个「吞掉所有错误、
  必定超时」的坏实现也能通过。

第一次跑后者时 0.14s 就"成功"了——上一次运行留下的对话框还开着，等待根本没被触发。清干净
重跑才是真的。

**顺带补掉的两个缺口**（都是跑真机时才暴露的）：上一阶段加了按 `protected` 匹配 locator，
但 MCP 的 `find_element` schema 没暴露这个字段，而它是 `additionalProperties: false`——
AI 传了会被拒绝，等于这个能力对 AI 不存在；`aad find` 同样没有 `--protected`。两处都补了，
真机确认 `--protected true` 精确命中密码框，`--protected false` 命中 10 个时如实报
`DRIVER.AMBIGUOUS_MATCH` 而不是猜第一个。

### 2.11 断言：`absent` 之前根本写不出来

上一段修好轮询之后，本来要做录制侧的 `assertion` 语法。动手前先逐个试了规范 §7.4 列的五种
`expect.mode`，`absent`（「对话框关掉了」「转圈没了」）当场卡住。

原因是两个各自合理的决定叠在一起变成了死结：`find` 在零匹配时报 `DRIVER.NOT_FOUND`，而上一段
刚把这个错误定义成 `retryable`＝「还没到」。于是 `absent` 断言每一轮都被判定为「再等等」，
耗尽 timeout，然后失败——**永远无法满足**。这不是慢，是不可能。

Python 侧写的是 `${{ not observation.found }}`，但没有任何 driver 产出过 `found` 这个字段，
所以那边同样没跑通过。

给 `find` 加了 `expect: "optional"`：零匹配成为普通结果（`found: false`），而不是错误。
默认仍然报错——为了操作而取元素时，清楚的 `DRIVER.NOT_FOUND` 好过一个空结果在后面以
"target 不见了"的形式炸开。`found` 在两条路径上都有，所以同一个条件写法两种结果下都成立。

**`optional` 不放宽歧义**：多个匹配依然报 `DRIVER.AMBIGUOUS_MATCH`。这一条是变异测试逼出来的
——五个变异里前四个都被抓住，唯独"让 optional 也放过歧义匹配"没有任何测试拦得住。行为本身
当时就是对的（真机上 11 个匹配，两种模式都报歧义），但没有东西把它固定住。补了守卫测试后
五个变异全部被捕获。

真机验证（`expect.mode: absent` 的两个方向）：
- 断言一个确实不存在的元素不存在 → 0.16s 通过；
- 断言一个明明还在的元素不存在 → 2.15s 报 `ACTION.POSTCONDITION_FAILED`，
  `last_observation.found = true`、`match_count = 1`。

只测前者的话，一个「永远回答 found: false」的桩也能通过——这正是 `always_absent` 变异。

MCP 那一侧也补了：`find_element` 的 schema 是 `additionalProperties: false`，不显式加
`expect` 的话 agent 根本传不进来，「对话框关了吗」这个问题在 MCP 上无法表达，唯一的回答是
一个看起来像故障的错误。已通过真实 stdio 协议验过三种情况（在→found:true、
不在→found:false、默认模式缺失→`DRIVER.NOT_FOUND`）。

规范 §7.4 已补上这个机制，其中的 YAML 示例是从文档里抠出来直接跑过的。

### 2.12 顺带查清的两件事（都不必做）

**`requires.permissions`**：规范 §11.1 说编译产物必须声明它，否则 `validate` 能过但 `run`
必被策略拒绝。照 `toDescriptor` 的真实输出构造了一份（无 `requires` 块）拿去跑——**直接成功**。
这条描述的是 Python 运行时，不适用于 Rust 这套。规范该处需要标注适用范围。

**引用完整性**：规范 §8 要求录制编译器自查悬空引用，理由是 workflow 编译器不查——后者确认
属实（引用不存在的步骤、引用未声明的 input，`validate` 都放行，要到 `run` 才报
`EXPRESSION.EVALUATION_FAILED`）。但录制侧**产生不出**悬空引用：每个录制动作展开成
snapshot → find → act 三步，引用只指向同一次展开内部派生的 id，删除/禁用/重排都是整组一起动。
凭据 input 也是同一个循环里声明和引用的。四种编辑操作各试一遍，悬空引用均为空。

所以现在写这个校验就是**守着一个到不了的状态**——看起来像安全措施的死代码。等 `logic` 步骤
或跨步骤引用真的进来了再说，那时它才有对象。

### 2.13 那个偶发失败其实有三层，最后一层是测试在自欺

§2.11 做完跑全量，那个测试又红了，但**报的是另一个错**。前后一共三种症状，每次我都只看到
一层就动手改，改完再跑就换一张脸：

| 症状 | 真正的原因 |
|---|---|
| `DURABLE.INVALID_STATE` | 脚手架重试 `execute`，但失败的那次已经把运行推离 `pending` |
| `JOURNAL.CONFLICT` | **产品 bug**：`resume` 的 TOCTOU（已修，见上一条） |
| `JOURNAL.LEASE_CONFLICT` | `DurableOptions::default()` 每次生成新 owner id，重试等于换了个人来抢锁 |
| `UnknownEffect != Succeeded` | 见下 |

第三层我又想当然了：把 owner id 固定成一个，让重试以同一身份回来。结果**更糟**——固定之后
重试真的抢到了 lease，而那个运行的段是被写锁风暴从中间打断的，`reclaim` 一个段中断的运行
按设计就要判 `UNKNOWN_EFFECT`（已派发但结果未知的步骤绝不能默默重放）。于是我把「抢不到锁」
变成了「抢到了锁并宣布这个运行不可恢复」。

**第三次改错之后我停下来加了诊断输出**，让测试把 run 的 error 和最后 12 条事件打出来，而不是
再猜一次。一次就看清了：`segment_entered step36` 之后没有对应的 `exited`，紧接着 `run.reclaimed`。
产品侧完全正确（5s busy timeout、WAL、pragma 逐项校验），是**风暴把被测对象饿死了**。

真正的修法是让风暴去输：runner 的 busy timeout 给 30s，风暴那条连接给 50ms。风暴仍然在同样的
微秒级窗口里翻转 intent，被测性质一个字没改。

**最后一层最难看：这个测试有一半时间在空转。** 修好之后连跑 8 次打印 `legs`（运行被暂停的
次数），结果是 1,3,1,2,1,2,2,1——**一半的运行压根没被打断过**，没碰到任何段边界，就算 intent
处理完全坏掉也照样绿。也就是说这个测试长期有一半的通过是假的。

改成：一轮没打断就换个新 run 再来，最多 12 轮，全都打不断才算失败。现在每一次运行都真的
撞在那个窗口上。效果可测量——撤销 TOCTOU 修复后的检出率从 **1/40 升到 3/25**（约 5 倍）。
改完连跑 60 次全绿。

教训记在这：**一个偶发失败连续换三张脸，说明每次都只诊断了一层。第三次就该去加诊断输出，
而不是第三次去猜。**

### 2.14 录制侧 assertion：五种模式全部可用

执行侧上一段就齐了，这段补录制侧语法。`gui/src/recording.ts` 现在能把 assertion 编译成宿主
action 的 `postcondition`，五种模式全部在真机上验过。

**放在步骤上，而不是平级的一条**。规范用 `of_step` 按 id 指向被验证的步骤，那是一个跨步骤
引用——而这个模型此前被证明**产生不出**悬空引用（§2.12）。把 assertion 挂在步骤上保住了这个
性质：删除、禁用、重排都是整组一起动，`of_step` 悬空这件事仍然到不了。保存时照样写
`of_step`，因为文件格式是对外契约，内存里怎么摆不是。

**`value_matches` 是子串，不是正则**。求值器明确拒绝函数和方法调用，所以没有 matcher 可调
——实测正则写法连 `validate` 都过不了。把它叫做正则是一个第一次用复杂模式就会暴露的谎。

**`state_equals` 的 flag 必须校验**。引用观察里不存在的字段不是「条件为假」，而是整个运行以
`EXPRESSION.EVALUATION_FAILED` 失败。所以录制侧在编译前就拦住拼错的 flag，否则用户得到的是
一个看起来毫不相干的表达式错误。

真机验证方式是十个描述符：五种模式各造一个「说真话」和一个「说假话」。**只测前者毫无意义**
——一个恒真条件能通过全部五个。结果十项全对：说真话的通过，说假话的全部报
`ACTION.POSTCONDITION_FAILED`。

前端测试 47 → 59，六个变异全部被捕获（去掉 `expect: optional`、复用动作前的旧快照、把布尔
状态加引号、放行空比较值、静默丢弃不认识的模式、把 assertion 拆成独立步骤）。

顺带修了自己写的一个校验 bug：`locator: null` 的本意是「看这个步骤自己的元素」（也就是
「我输入的内容进去了吗」这个最常见的用法），我却当成「没有目标」，导致每一个默认 assertion
都被拒绝。是跑真机脚本时立刻暴露的——只写单元测试的话，我很可能连同错误的语义一起测进去。

### 2.15 GUI 里的 assertion 编辑，以及顺手挖出的两个 bug

模型层能表达五种断言，但界面上没有入口——用户点不出来的能力等于没做。补上
`AssertionEditor.vue`，一个勾选框加一个下拉，按模式显示对应字段。模式名用人话
（"something appears" 而不是 `exists`），保存格式仍是规范里的名字。

**编辑走 `setAssertion` 而不是直接改字段**，因为切换模式时有取舍要处理：换到不比较值
的模式必须丢掉旧的比较值（留着会写进文件，换回来时又冒出来，看着像生效其实没有），
而换到 `state_equals` 必须同时给出状态名和 true/false——只给一个的话界面上两个下拉都
显示着值，模型里却缺一半，于是表单看起来填完了而录制拒绝导出。这个是写测试时发现的。

真机验证时挖出两个更严重的问题，都不是这次改动引入的：

**一、GUI 的大纲永远是空的（从第一个提交起）**。Tauri 层读 `described["nodes"]`，但
`describe` 返回的键是 `elements`。所以列表恒空，任何元素都点不到，**GUI 从来就没能录
过一步**。而且就算键名对了，重建出来的元素也不带 `locator` 和 `protected`——前者是录制
存盘后回放的唯一依据（`ref` 随快照失效），少了它每个步骤都会存成"无法识别"。
根因是在 Tauri 层重新推导了一份驱动已经算好的大纲；locator 的合成需要完整节点表来判断
唯一性，本来就只有驱动做得了。改成直接透传。

**二、`set_value` 会撒谎**。对 WebView 里的 `<select>` 调用 UIA `SetValue` 返回 S_OK
但什么都没变，驱动照样报 `applied: true`。这正是这个项目要防的失败模式，而且比报错更
糟——报错能处理，假的成功不能。改成写完读回来确认。判定规则只把"值没动过"算失败，不
要求读回来完全相等：会规范化输入的控件（去空格、重新格式化、钳位）确实接受了写入，
要求精确相等会把它们全判成失败。另外两种情况也不算失败：控件本来就是那个值（没什么
要改的），以及值读不出来（没有证据，编造失败和编造成功一样错）。四个变异全部被捕获。

顺带记一个工具坑：变异测试用 `shutil.copy2` 还原源码会保留 mtime，cargo 因此不重新编译，
跑的是变异版的二进制——还原后测试仍然失败，看起来像还原失败。判定前需要 `touch` 一下。

### 2.16 存盘 → 重开 → 回放：走通了，但路上有两个坑

上一节修好大纲之后有个明显的疑问：locator 在那之前一直是 null，而 `fromDocument`
对 locator 为 null 的步骤拒绝重新启用——**而且是静默拒绝**，因为「步骤被禁用」本身
是个正常状态。那存盘出来的录制，是不是从来就重开不了？

先查了磁盘：`~/.ai-auto-desktop/recordings` 目录存在但**空的**，一份存档都没有。
和「GUI 从来没能录过一步」对得上，但也意味着没有历史文件可以对照，只能重新走一遍。

于是用产品自己的 CLI 驱动 GUI 走完整条链：录一步 → 存盘 → 清空 → 重开 → 跑编译产物。
结论是**链路通了**：重开后 1 步、文本还在、断言连模式一起恢复，编译产物 `run` 成功，
fixture 里确实变成了 `typed-by-roundtrip`。但路上有两个东西必须记下来。

**第一个坑：`set_value` 的读回校验会误判。** 上一节给 `set_value` 加了「写完读回来
确认」，防的是 WebView 里 `<select>` 那种「报成功但没变」。跑真机时它把一次**确实
生效**的写入判成了失败——字段里明明已经是新值。

量了一下：控件并不保证同步发布新值。本机 native Win32 edit 约 **160 ms**，WebView
input 约 **510 ms**。读一次、立刻读，等于给每个 web UI 都判了死刑。

改成轮询：值一动就返回，到点才认定拒绝。成功的写入不会因此变慢（80~180 ms 返回），
只有真被忽略的写入才付满 1 秒。三种情况实测：native 86 ms 通过、WebView input
134 ms 通过、WebView `<select>` 1136 ms 拒绝——**该抓的还抓得住**。

误判比误报成功轻，但它同样是撒谎，而且撒在这个检查唯一会触发的地方。

**第二个坑：我给大纲写的第一版回归测试是废的。** 修完大纲我补了三个测试，跑绿了。
但拿两个变异去验（把键名改回 `nodes`、把 locator 抹掉）——**一个都没抓住**。

原因是测试调的是 `shell.dispatch("describe", ...)`，那是 driver。而 bug 在
`describe_window`，是它上面那层。我测的是从来没出过问题的东西，还差点就带着「已覆盖」
的错觉提交了。

`describe_window` 是 async Tauri command，参数 `tauri::State` 在单测里造不出来，所以
把逻辑抽成普通函数 `outline_of`，command 只做适配。改完再验，两个变异都被抓住
（3 个失败 / 1 个失败）。

**顺带确认的一件事**：存盘时 workflow 写成了 `{}`，一开始以为是 bug，读界面才发现
它说得很清楚——"set_value needs text to enter"。是我的脚本录了一步没填文本，校验
拦得对，提示也给到了。

---

### 2.17 事件驱动录制：两种机制都得要，缺一种就瞎一半

开工前先量了「Rust 到底能不能收到 UIA 事件」，因为其余部分都是我会写的管道，只有这里
可能根本走不通。结果比预想的重要得多。

**第一件事：`#[implement]` 需要三处配合。** `windows` crate 要开 `implement` feature；
宏展开出的是 `windows_core::` 路径，且在 **crate root** 解析，所以 `windows-core` 必须
是一个独立依赖——`use windows::core as windows_core` 这种别名**不管用**。回调 trait 的
签名也不是 `i32`，是 `UIA_EVENT_ID` / `UIA_PROPERTY_ID` 这些 newtype，签名从 crate 源码
里读出来的，没猜。

**第二件事：回调在别的线程上。** 订阅发生在线程 916，回调到达线程 41448。所以事件缓冲
区从一开始就必须是跨线程安全的，这不是以后再加的优化。

**第三件事，也是真正要紧的：UIA 事件处理器在 WinForms 上是瞎的。**

第一版探针的结论是「按钮点击不产生任何事件」——但那是错的。读了 fixture 自己的标题才
发现 `clicks=0`，**我合成的鼠标点击根本没落地**，量的是一次失败的点击，不是 UIA。这个
坑值得记：从探针内部看，「没有事件」和「没有点击」长得一模一样。

改成「一个进程订阅、另一个进程用产品自己的 driver 去点」，并且每次都拿 fixture 的计数
器确认交互真的生效了。干净的结论：

| | WinForms fixture | Chromium WebView（本项目 GUI） |
|---|---|---|
| UIA 事件处理器 | **0 个事件** | 3 个事件（invoked / value） |
| WinEvent 钩子 | **12 个事件** | 15 个事件 |

两次点击都确认生效（clicks 0→1，checked False→True），UIA 处理器一个都没收到。而 WebView
上 UIA 处理器是好的——它原生实现 UIA，不走 MSAA 桥。

**所以两种机制都得要，而且不能二选一：**

- 只用 UIA 事件处理器 → WinForms 及一切走 MSAA 桥的老应用，**每一次按钮点击都录不到**；
- 只用 WinEvent 钩子 → 拿到的是 MSAA 层的粗事件（focus / state_changed 一片），元素身份
  和语义要另外补，WebView 里 15 个事件里绝大多数是 focus 噪声。

WinEvent 钩子还有个结构性差异：它按**线程消息队列**投递，必须有消息泵；UIA 回调走 COM
工作线程，不需要。这决定了捕获会话得自己有一个泵消息的线程。

（还有个小事实：CLI 的点击子命令叫 `click`，不叫 `pointer_click` 也不叫 `pointer-click`；
`aad do --help` 里写着。）

---

### 2.18 两种机制正好互补，所以捕获层同时用两种

上一节确定「两种都要」，还剩一个问题：WinEvent 钩子给的是 HWND + object id，**不是元素**。
录制需要元素——角色、名字，以及能合成出回放时还找得到的 locator。如果还原不出来，那钩子
看见再多事件也没用。

量的结果，两个工具包各自的答案正好互补：

| | WinForms | Chromium WebView |
|---|---|---|
| UIA 事件处理器 | 收不到点击 | **能收到，且带元素** |
| WinEvent 钩子 | **能收到** | 能收到，但全是噪声 |
| 从 hwnd 还原元素 | **精确到控件本身**（SubmitButton / SubscribeBox，带 automation_id） | 只能还原到 `Chrome Legacy Window` 容器，控件本身没有 HWND |

WinForms 的控件是真窗口，所以 `ElementFromHandle` 直接给出被点的那个控件；WebView 里 DOM
元素没有自己的 HWND，钩子只能定位到那个装着整个页面的容器，`id_child` 是 -34 / -40 / -69
这类值，还原不出「点了哪个按钮」。

**所以：WinForms 靠钩子、WebView 靠 UIA 处理器，两边各自补上对方的盲区。** 这不是「都订上
更保险」，是缺一种就真的瞎一半。

**又踩了一次同样的坑。** 这个探针第一版报告「一个事件都没有」，而同样的钩子、同样的
fixture、同样的点击，上一节明明收到 12 个。原因是我按 `id_object == 0`（OBJID_CLIENT）过滤，
而 WinForms 这些事件带的是 **`id_object == -4`（OBJID_WINDOW）**——猜了一个常量，把全部事件
丢光了。上一节的教训是「没事件」可能是「没点击」，这一节是「没事件」可能是「自己过滤掉了」。
两次都是同一个形状：**探针报告的空结果，先怀疑探针**。

---

### 2.19 捕获会话：能录到了，顺带发现「一次点击会被录成三步」

按 §2.18 的结论，`CaptureSession` 同时订阅两种机制。平台无关的部分（事件种类、归并、
有界缓冲）放在新的 `crates/aad-uia/src/capture.rs`，原生订阅在 `windows.rs`。

几个不是随手定的设计：

- **会话自己占一个线程。** WinEvent 钩子按线程消息队列投递，没有消息泵就永远收不到；
  UIA 回调不需要泵，但放在同一个线程上意味着只有一个地方需要拆订阅。
- **`start()` 等订阅装好才返回。** 否则「开始录制」之后立刻操作，会和装订阅赛跑，最前面
  几个动作静默丢失。
- **缓冲区有上限，溢出要计数。** 静默丢掉的事件和「用户什么都没做」长得一模一样。
- **两种机制只装上一种也照样开工**，但 `sources` 会如实说明装上了哪些——少一种就意味着
  某一整类交互录不到，用户有权知道。
- **回调里 `catch_unwind`。** panic 穿回 COM 调用方会把订阅整个拆掉。

**真机一跑就露出一个 bug：两次点击被录成了六步。** 单测全绿，因为它们测的是归并规则本身，
而这个现象只有真实 WinForms 窗口才产生——一次按钮点击会连着抛 **三个** state_changed
（按下、抬起、焦点框变化各算一次）。回放六步意味着 Submit 按钮被提交三次。

原来的归并只合并 value_changed，理由是「重复点击是真的重复交互」。这个理由现在依然成立，
所以不能简单地把相邻 state_changed 全合掉，得区分「一次点击报了三遍」和「用户点了三下」。

**用时间区分**：一次点击的三个通知挤在几毫秒内到达，而人不可能在这个窗口里点两下。于是
同一元素的 state_changed 只在 40ms 内合并（比最快的有意双击还小一个数量级），超出就是两次
交互。value_changed 不受时间约束——慢慢打字也还是一次编辑。

修完真机复测：两次点击 → **两步**。单测补了四个，两个方向都测（只测「一次点击合成一步」的话，
「全部合并」也能过）。

（还有一次同样形状的自我怀疑：`aad do` 的点击子命令叫 `click`，我先后猜了 `pointer_click`
和 `pointer-click` 两次都错，`--help` 里写着。）

---

### 2.20 描述性 locator：「第三个按钮」「用户名旁边的输入框」

原来的 `Locator` 只有属性合取（role/name/automation_id/class_name/...），只能描述
「这个元素是什么」。这对 AI 不够用——人给指令的方式是「第一个输入框」「用户名旁边
那个框」「文本是 xxx 的按钮」，其中只有最后一种是属性。

而且属性路线本身就不牢靠。**实测：WinForms 的 `automation_id` 和 `class_name`
跨重启都会变。**

| 元素 | 重启前 | 重启后 |
|---|---|---|
| SubmitButton | 2232590 | 4065968 |
| SubscribeBox | 15338490 | 20122088 |
| NameBox | 9178646 | 15534534 |

（`class_name` 形如 `WindowsForms10.BUTTON.app.0.8259d1_r15_ad1`，尾巴每次启动重新
生成。）拿这两个字段建 locator，会**在录制当场完美工作、第二天全部失效**——最难查
的那种。现有的 `synthesize` 是逐级收窄的，name 够用就不加，所以默认路径安全；但这
说明「靠属性描述元素」有实实在在的上限，需要别的表达方式。

**新增两种描述维度：**

- `nth`：`1`/`2`/... 或 `"first"`/`"last"`。**按屏幕阅读顺序**（先上下、后左右）数，
  不是枚举顺序——「第三个按钮」指的是人看到的第三个，而无障碍树不保证按这个顺序枚举。
- `near`：`{anchor, direction, within}`。锚点本身是个完整 locator（可以嵌套，上限 3
  层），`direction` 取 `left`/`right`/`above`/`below`/`any`，`within` 是像素上限。

**几个不是随手定的决定：**

- **`resolve(&nodes)` 而不是 `matches(&node)`。** 序数和空间关系是元素在集合中的
  位置，单节点谓词根本表达不了。driver 的 `find` 已改为走 `resolve`。
- **三段顺序固定：属性筛选 → 空间过滤 → 序数选取。** 反过来的话，「用户名旁边的第二个
  按钮」会变成「第二个按钮，且恰好在用户名旁边」——选中的元素不同，且多数时候找不到。
- **锚点找不到或有歧义时，返回空而不是退回全部候选。** 「用户名旁边的输入框」在没有
  用户名的页面上应该是查找失败；悄悄丢掉约束会导致往任意输入框里打字。
- **序数越界返回空，不退回最后一个。** 「三个里的第五个」是指令写错了，静默改指别的
  元素正是自动化造成破坏的方式。
- **`nth: 0` 在解析期就拒绝。** 所有人写指令都从 1 数起。
- **方向要求同排/同列。** 标签右边的输入框通常和它同一行，「right」是「右边且大致齐平」
  而不是单纯 x 更大。
- **距离按边缘算不按中心算。** 一个宽输入框挨着短标签时，中心距会高估它们的距离。
- **`unique_for` 也改走 `resolve`。** 否则「三个按钮里的第一个」会被判成有歧义，而
  这恰恰是序数最有用的场合。

**测试：20 个新单测 + 10 个变异全部被抓住**（把序数改成枚举顺序、把三段顺序颠倒、
锚点找不到时退回全部、忽略 within、方向失效等，每个都至少被一个测试抓到）。

**真机验证反而抓到一个单测没抓到的 bug。** `within` 传的是像素，但内部距离存的是
平方（为省开方、保持整数），直接比较等于把阈值悄悄平方了：`--within 40` 实际是 6 像素，
于是「Name 标签右边 40px 内的输入框」在真实间距只有 10px 的情况下找不到。单测漏掉是
因为我写测试时和实现共享了同一个单位假设。修完补了个用**真实窗口量到的坐标**的测试
（标签右边缘 646、字段左边缘 656），两个方向都测。

真实窗口十条验证（fixture 13 个节点，含标题栏的最小化/最大化/关闭）：

| 描述 | 结果 |
|---|---|
| 第一个按钮 | 最小化（正确——标题栏按钮 y 更小） |
| 第一个**可聚焦**的按钮 | SubmitButton |
| 第二个可聚焦的按钮 | ResetButton |
| 第一个可聚焦的输入框 | NameBox |
| Name: 右边的输入框 | NameBox |
| Name: 右边 40px 内 | NameBox |
| Name: 左边的输入框 | 找不到（正确） |
| 不存在的锚点旁边 | 找不到（正确） |
| SubscribeBox 下方的按钮 | 有歧义（正确——两个按钮同高，加 `nth` 消歧） |

第一条值得注意：**「第一个按钮」在真实窗口里是「最小化」**，因为标题栏按钮混在同一棵
树里。这不是 bug，但说明序数很少单独用，通常要配 `states.focusable` 之类一起收窄。

CLI 三个入口：`--nth`、`--near/--direction/--within`（常用捷径），`--locator <JSON>`
（完整表达，用于嵌套锚点等 flag 写不出的形状）。

---

### 2.21 两个「录制当场能用、重启就失效」的缺陷

接录制链路前先量了一下捕获元素的字段稳定性，顺带挖出两个已经存在、但一直没被
发现的真缺陷。**共同的成因是：存和放都在同一次会话里做完，没人重启过目标程序。**

#### 缺陷一：窗口选择器用了每次运行都变的 class_name

GUI 的 `selectorFor` 把 `class_name` 排在第一优先（"最像身份"）。但 WinForms 每次
启动都重新生成它：

```
WindowsForms10.Window.8.app.0.34473a7_r14_ad1   ← 重启前
WindowsForms10.Window.8.app.0.376a1c9_r8_ad1    ← 重启后
```

**已存的每一份录制在目标程序重启后都打不开。**

修法是识别出这个形状就跳过，改用 process_name/title。规则只挡实测过的
`_r<n>_ad<n>` 后缀——本机 20 个窗口里只有 WinForms 那个命中，`Notepad`、
`Chrome_WidgetWin_1`、`XLMAIN`、`CabinetWClass` 全部保留（砍掉稳定的 class_name
只会让选择器更弱）。

#### 缺陷二（更严重）：role 用的是本地化显示串

`role` 取自 `CurrentLocalizedControlType()`——**给人看的显示字符串**。实测抓到它
正在变：同一个快照里中英混杂，

```
role=窗口      name=AAD Capture Fixture     ← 中文
role=edit     name=NameBox                 ← 英文
role=button   name=SubmitButton            ← 英文
role=标题栏     name=None                    ← 中文
```

而更早的测量里这些控件还都是中文（`编辑`/`按钮`/`复选框`）。已存录制记的是
`role: "编辑"`，现在的快照报 `edit`，**那份录制已经失效了**。

而且 locator 的 role 是精确比较（`eq_ignore_ascii_case` 对中文无效），所以这不是
"匹配得宽一点"的问题，是完全失配。

修法：role 改取 `CurrentControlType`（数字常量，与语言无关），映射成固定英文名
（button/edit/check_box/title_bar/...）。未知类型保留数字（`control_type_59999`）
而不是塌缩成一个名字——塌缩会让不相关的元素互相匹配，比名字难看糟得多。修完：

```
role=window   role=text   role=edit   role=check_box   role=button   role=title_bar
```

#### 端到端验证（唯一算数的判据）

单测只能证明选择器不再输出易变字段，证明不了"重启后还能回放"。做了对照实验：
录一个把 NameBox 填成某值的工作流 → 重启 fixture → 分别用新旧选择器回放。

| | 结果 |
|---|---|
| A. 旧行为（class_name 作选择器） | `DRIVER.WINDOW_NOT_FOUND` |
| B. 新行为（跳过易变字段） | **成功**，fixture 标题变成 `text=new-way` |

两个方向都要看：都成功说明修的不是真问题，都失败说明没修好。

（中途 B 一度也失败，报 `DRIVER.NOT_FOUND`——那是**我的探针**里 locator 还硬编码着
中文 `编辑`，不是产品的问题。错误码不同（WINDOW_NOT_FOUND vs NOT_FOUND）是分辨这
两件事的关键，如果只看"成功/失败"就会得出错误结论。）

---

## 2.22 录制链路接通：捕获事件 → 可回放步骤

`CaptureSession` 此前只有 example 能用。本轮把它接成完整链路：driver 三个动作
（`watch` / `collect` / `release`）、CLI 的 `aad record`、以及事件到步骤的合成。

### 为什么 locator 合成要靠一张开始时的快照

`Locator::synthesize` 需要完整节点表判唯一性，而一个捕获事件只带一个元素。三种接法：

| 做法 | 为什么不行 |
|---|---|
| 每个事件现场拍快照 | 贵，而且有竞态：拍完 UI 已经动了，刚点掉的对话框可能已经不在 |
| 只用捕获元素自己的属性 | 无法判唯一性，会产出 `{"role":"button"}` 这种回放时 AMBIGUOUS_MATCH 的 locator |
| **开始录制时拍一张，把捕获元素匹配回去** | 一次开销，且拿到了判唯一性所需的全集 |

选第三种。代价是录制期间新出现的元素匹配不上——那种情况如实标记 `unresolved`
让人来补，而不是编一个 locator 出来。匹配用身份（role/name/automation_id/class_name）
而非值：值在打字过程中一直在变，比较值会导致**任何被编辑过的字段都匹配不上**。

### 为什么 CLI 是一条命令而不是三条

每次 CLI 调用是独立进程，捕获会话活在进程内、订阅挂在后台线程上。拆成
watch/collect/release 三条命令的话，第一条一退出会话就没了。所以 `aad record`
一条命令走完：装订阅 → 等操作 → 收步骤 → 拆掉。GUI 那边走 Tauri、进程常驻，
用的是三个动作。

`record` 内部按 200ms 轮询而非一次性睡到底：缓冲区有界，长时间录制繁忙窗口
会溢出，丢掉最早的步骤。

### 路上挖出的 synthesize 缺陷

真机录制跑通后，输出暴露一个问题：三个操作录成三步、locator 全部反查成功，但
第一步是 `{"role":"edit"}` —— `NameBox` 这个稳定的 name 明明存在却没用上。

**先量了这有多严重。** 本机 12 个窗口：

| | 数量 |
|---|---|
| 有多个 button 的窗口 | **10 / 12**（msedge 有 28 个） |
| 有多个 edit 的窗口 | 2 / 12 |
| 所有 role 中唯一的 | 66 / 141 |

所以 fixture 能通过纯属巧合（它只有一个 edit）。真实程序里这种 locator 一定
AMBIGUOUS_MATCH，而且问题要到回放时才暴露——又是「录制当场能用、换个环境失效」。

**两个原因叠加：**

1. `synthesize` 一旦发现 role 唯一就返回，而"今天唯一"不等于"明天唯一"。
   改为：元素有 name 或 automation_id 时不接受纯 role；两者都没有才退回 role
   （这个兜底必须留，很多元素确实什么身份都没有）。
2. **真 bug：加一个空字段后碰巧唯一，就停下返回了。** 元素没有 automation_id 时，
   那一步什么也没加，但循环仍然测了一次唯一性并返回——于是紧接着要试的 name
   永远试不到。修法是字段没真的加上东西就不算一次收窄机会。

顺带调整了收窄顺序：**automation_id 排到 name 之前**。理由是耐久性——id 写在
源码里、不面向用户、不会被翻译；name 是控件标签，而标签会被本地化（本 crate
刚因为 role 从 `按钮` 变成 `button` 修过一次）。

原先有个测试断言"role 唯一就该停，多余的 name 会在改名时失效"。这个顾虑是真的，
但它选的替代方案更弱。两种失败都是响亮的（不会点错东西），所以按发生频率取舍：
**多出一个按钮远比改名常见**。测试已改写并留下理由。

### 修复前后对比（同一个 fixture，同样的操作）

| | 第 1 步 locator |
|---|---|
| 修复前 | `{"role":"edit"}` |
| 修复后 | `{"role":"edit","name":"NameBox"}` |

### 端到端验证：录制能否活过目标程序重启

这是唯一算数的判据——本轮修的全是"重启才暴露"的问题，不重启等于没测。

| 阶段 | fixture 标题（真实状态，不看驱动返回） |
|---|---|
| 录制时 | `clicks=1 text=restart-proof` |
| 重启后 | `clicks=0 text=`（全新实例，window_id 从 20581954 变成 20647490） |
| 回放后 | **`clicks=1 text=restart-proof`** |

`sources = ['uia', 'win_event']`，3 个操作 → 3 步，dropped=0。

中途一次 `status = invalid` 是我凭记忆写错了 workflow 格式（`schema_version` /
`workflow_id` / `steps[].action` 全是错的，真实格式是 `apiVersion` / `kind` /
`metadata` / `steps[].{id,type,uses,with}`），不是 locator 失效——**校验错误码
和回放失败码不同，是分辨这两件事的关键**。


## 2.23 GUI 录制：边录边改

三个 Tauri 命令（`start_recording` / `collect_recording` / `stop_recording`）走的是
既有的 `Shell::dispatch`，所以捕获会话建在 driver 那个 worker 线程上——捕获订阅要求
与 driver 同一个 COM apartment，worker 线程模型天然满足（早前在主线程试过，
`RPC_E_CHANGED_MODE`）。

### 为什么 GUI 分三个命令而 CLI 只有一条

| | 形态 | 原因 |
|---|---|---|
| CLI | `aad record` 一条命令走完 | 进程会退出，会话保不住 |
| GUI | watch / collect / release 分开 | 进程常驻，前端可按自己节奏取 |

GUI 每 700ms 轮询一次 `collect`，录到的步骤**直接进同一个 recording**，所以它们在
录制过程中就出现在右侧 StepList 里，能当场改文本、加断言、禁用、删除。没有做单独的
「录制结果确认页」——那会把「操作一次、更正一次」变成「录完再统一处理」。

`stopCapture` 在 release 之前会再 collect 一次：否则最后一次轮询到按下停止之间的
操作会丢，而那恰好包含用户决定「做完了」之前刚做的那一下。

### `addCaptured`：为什么不能复用 `add`

`add` 要一个 `Element`，里面含 `ref`——那是用户在大纲里点选元素时才有的。捕获来的
元素由事件描述，不属于任何快照，只有 locator。两个不能省的点：

- **窗口选择器仍在前端算**。捕获只说「在哪个窗口」，不说「重启后怎么找到它」，而
  `selectorFor` 需要所有打开的窗口才能证明选择器无歧义，后端并不跟踪这个。
- **无法回放的步骤要留下并显示**。丢掉的话录制看起来是完整的，实际少了一次操作，
  而这要到回放时才发现——那时能解释它的会话已经结束了。人得先看见才能修。

### 真机验证：驱动 GUI 自己走完一次录制

单测和类型检查都不碰 Tauri 这条边界（`describe_window` 的 bug 当初就是这样漏过去
的），所以用 aad 自己的驱动操作 GUI：

| 检查点 | 结果 |
|---|---|
| 按钮变成 `■ Stop (2)` | ✓ 实时计数 |
| 红色录制横幅 | ✓ 右端显示 `uia + win_event` |
| RECORDING 栏 | ✓ 「2 steps」，参数框里已填着 `gui-recorded` |
| fixture 标题 | ✓ `text=gui-recorded`，操作真的落地 |

保存到磁盘后的内容：

```
1. set_value  locator={"role":"edit","name":"NameBox"}     window={"process_name":"powershell.exe"}
2. invoke     locator={"role":"button","name":"SubmitButton"}
```

大纲里能看到 `automation_id="15207336"`（纯数字、每次运行都变），**locator 没有用
它**——上一节的持久字段过滤在真实录制里生效了。

### 端到端：GUI 录的工作流活过目标程序重启

| 阶段 | fixture 标题 |
|---|---|
| 录制后 | `clicks=1 text=gui-recorded` |
| 重启后 | `clicks=0 text=` |
| 回放后 | **`clicks=1 text=gui-recorded`**，`status = succeeded` |

### 顺带发现的一个隐患

回放成功的那份录制，窗口选择器只有 `{"process_name": "powershell.exe"}`。这次能成
是因为当时恰好只剩一个 powershell 窗口——而几分钟前我确实同时开着两个 fixture。
`selectorFor` 本来就会检查唯一性，前提是调用方把所有打开的窗口传给它；已补测试守住
「候选里真有第二个同进程窗口时，必须退到 title 或禁用该步」。


## 3. 已知环境限制（非缺口，如实记录）

- 本机装有输入过滤软件（AutoHotkey / LogiBolt 一类），`SendInput` 对 ≥2 事件的批次返回
  `sent=0` 且 `GetLastError()==0`。击键因此绝不拆批，失败即报 `DRIVER.INPUT_BLOCKED`
  （拆批会导致文字错乱：目标会 latch 前一字符）。
- 受保护窗口返回 `0x80004005`，属 UIPI 正常行为。
- DPI 与完整性级别探测为 `degraded`。
- Windows script 沙箱缺网络/文件系统隔离，按 Python 原行为如实上报 `degraded` + `gaps`。
- `aad-plugin` 的 `a_requested_manifest_completes_the_handshake` 偶发失败（`cargo test --workspace`
  跑过一次失败，随后单独跑 4/4 通过、在 committed 基线上加 4 路 CPU 负载跑 3/3 通过，
  故非改动引入）。fixture 是 Python 子进程，握手默认 30s，怀疑是并行下解释器启动被拖慢。
  尚未定位，暂记录不掩盖。
- ~~`durable_exec` 的 `flipping_intent_while_a_run_advances_never_surfaces_a_conflict` 偶发失败~~
  （下面这条记录了第一次修复；后续又暴露两层，见 §2.13）
  **已定位并修复，不是环境问题，是 `resume` 的一个真 bug**。它先读 `desired_state` 看到
  `pause`，再 CAS `pause -> run`；若这中间有别人清掉了 pause，CAS 落空，整个 resume 以
  `JOURNAL.CONFLICT` 失败。但目标本来就是「让这个运行不处于暂停」，而它确实不处于暂停——
  为调用方要到的状态报错是没有道理的。改为落空即重读继续（cancel 仍在下一个段边界照常兑现）。
  同时修了测试脚手架的一个缺陷：存储失败后重试 `execute`，但失败的那次可能已经把运行推离
  `pending`，重试于是撞上 `DURABLE.INVALID_STATE`——把脚手架自己的问题报成了产品故障。
  基线 1/10 失败；两处都修好后 40/40 通过；撤销 driver 侧修复能复现（1/40），确认修复有效。

## 4. 不在计划内

- **删除 Python**：保留供查阅。
- **skills 目录**：等 CLI 能力齐全后再写。

## 2.24 Correcting a locator, with the trying next to it

A recorded locator is a guess made from one moment of one session. It stops
matching for ordinary reasons -- the label was translated, the field was
renamed, a second button appeared -- so the recording editor has to let someone
write a different one. Two things had to be true for that to be worth having.

**The correction has to be able to say more than "another attribute".** When an
element has no stable name and no author-written id, no combination of
attributes identifies it. What is left is where it sits: the third button, the
field beside `Name:`, the first focusable input. The driver already resolved
those (§2.20), but the front end could not express them -- `bridge.ts`'s
`Locator` had `role`, `name`, `automation_id`, `class_name`, `framework_id` and
`match`, and nothing else. `vue-tsc` said so plainly the first time a test
wrote `nth`: *Property 'nth' does not exist on type 'Locator'*. The type is now
the whole shape the driver accepts, including `states`, `nth` and `near`.

**The correction has to be verifiable on the spot.** Without that, editing a
locator is guessing, and the way it goes wrong is quiet. On the capture fixture:

```
{"role": "button", "nth": 3}   ->   ✓ matched   button "关闭"
```

Three buttons up from the top of that window is not the submit button; it is the
title bar's close button, because title-bar buttons live in the same tree and
sit higher on the screen. Showing only *found* would let someone keep that and
discover it when a replay closes the window. So `try_locator` reports **what it
selected**, and the editor prints it.

The same run, seen from the editor:

| locator | what the editor showed |
|---|---|
| `{"role":"button","nth":3}` | `✓ matched button "关闭"` |
| `{"role":"edit","states":{"focusable":true},"nth":1}` | `✓ matched edit "NameBox"` |
| `{"role":"edit","near":{"anchor":{"name":"Name:"},"direction":"right"}}` | `✓ matched edit "NameBox"` |
| `{"role":"button"}` | `✗ matched 5 elements` + the five candidates |
| `{"role":"button","name":"NoSuchButton"}` | `✗ matched nothing in this window` |

`expect` is `optional`, not `any`. A locator being edited matches nothing for
most of the time it is being typed, and that is a state to show rather than an
error to raise. Ambiguity is deliberately left to fail: the candidate list the
driver attaches to `DRIVER.AMBIGUOUS_MATCH` is exactly what tells someone how to
narrow the locator, and `expect: "any"` would return the first match and throw
the candidates away.

### The form, and what it refuses

Free-form JSON would have been less work, and it is still there behind
*Edit as JSON* for shapes the fields cannot hold. But the four descriptions
above all have a fixed shape, so they get fields -- and the fields can refuse
what JSON would have accepted silently:

- **Position zero.** The driver rejects `nth: 0` outright, because positions are
  1-based and reading 0 as "the first" would select a different element than
  intended. The form says so before it is tried.
- **A direction or a distance with nothing to be near.** Both are properties of
  a proximity constraint. Without an anchor they would be dropped on the way to
  the driver, and the person would believe they had applied them.
- **A locator that constrains nothing**, which matches every element.

`isBeyondForm` is the other half. A locator whose anchor is itself positional, or
which requires a state to be *false*, or which uses a field this version does not
know about, says more than the fields can show -- so it opens in the JSON view
and stays there. Round-tripping it through the form would return a locator that
matches something else while looking edited.

### What the real screen taught, again

Neither of these came from a test.

The **Edit button was off screen**. It was placed after the locator text, and
`role=button name="SubmitButton"` is long enough to push it past the edge of a
narrow panel. The capability was there and unreachable; a step with no visible
way to correct it reads as a step that cannot be corrected. The button now comes
first and does not shrink, and the locator text truncates instead.

**`describe` truncates, and the GUI describing itself is large.** The editor's
own buttons sit at `e199`-`e211`, past what `--limit 500` returns for a window
whose outline panel lists a dozen nodes each with three to five action buttons.
Two probe runs looked like "the button does not exist" when the button was
simply outside the window that `describe` returns. `find` by name is not subject
to that.

One more, from the same runs: locating the editor's own `position` field with
`{"role":"edit","near":{"anchor":{"name":"position","role":"text"},...}}`
worked, and the same thing for `name` returned `DRIVER.NOT_FOUND` -- there are
several "name" labels on that screen, and an ambiguous anchor resolves to
nothing rather than to a guess. That is the designed behaviour (§2.20) meeting
its own tooling.

**Tests**: 569 Rust, 96 front end (`locator.ts` 14, `setLocator` 4).

## 2.25 The CLI can ask the three questions, and what a browser does to counting

`find` had one behaviour: fail if nothing matched, refuse if several did. That is
right when acquiring an element to act on, and wrong for the two things a caller
working out a locator actually needs to ask. The driver had supported all three
since the beginning; the CLI exposed none of them.

| flag | question | missing | several |
|---|---|---|---|
| *(none)* | "give me this element" | `DRIVER.NOT_FOUND` | `DRIVER.AMBIGUOUS_MATCH` |
| `--optional` | "is it there?" | `found: false`, exit 0 | still refused |
| `--any` | "give me one of them" | still `NOT_FOUND` | first match **+ candidates** |

The two relaxations are deliberately not the same flag, and neither relaxes both
things:

- `--optional` must not start picking one of several. "Has the dialog closed?"
  quietly becoming "here is one of the four that matched" is how an assertion
  introduces the very wrong-element bug it exists to catch.
- `--any` must not start reporting absence as success. The caller wants
  something to act on; an empty target fails later as a puzzling missing
  element rather than here as a clear one.

They are mutually exclusive at the parser (`exit 2`), and the choice is made
once in `expectation_for` for both routes -- duplicating it per route is how
`--optional` ends up silently not applying to `--locator`.

### `--any` had nothing to be useful with

The driver returned the first match and `match_count`, and dropped the rest.
`match_count: 5` cannot be acted on. What narrows a locator is knowing that three
of those five were the window's own minimise, maximise and close buttons -- the
ambiguity *error* has carried that list all along, and a caller who chose to
proceed had no error to read it from. A successful multi-match now carries
`candidates` too; a single match does not, because a list of one implies a choice
where none existed.

### The browser measurement

Tested against a page with three buttons, in Edge, with CDP as an independent
source of truth for the DOM:

```
CDP:  3 inputs, 3 buttons
UIA:  20 buttons  --  17 of them the browser's own
      {"role":"button","nth":1}  ->  搜索标签页
      {"role":"button","nth":3}  ->  关闭标签页
      {"role":"button","nth":5}  ->  最小化
      {"role":"edit","states":{"focusable":true},"nth":1}  ->  the address bar
```

Not one of those touched the page. The WinForms lesson (§2.20: the first button
is Minimise) is mild by comparison -- a browser puts a whole toolbar in front of
the content. So counting is now documented where it is chosen: in `--nth`'s help,
and beside the GUI's `position` field, both with the fix rather than just the
warning. `countsAcrossWindow` drives the GUI hint and is asked of the locator
rather than of the form fields, so the JSON view and the form cannot disagree.

The fix is an anchor, which makes the count local:

```
{"role":"button","near":{"anchor":{"name":"Username","role":"text"},
                         "direction":"below"},"nth":1}   ->  Submit   (1 match)
```

### What the WebView got right

Worth recording, because it was better than expected and it decides where effort
goes next:

- **`automation_id` is the DOM `id`** -- `user`, `pass`, `submit`. Author-written
  names, so durable across restarts by the same test as §2.22.
- **`<label for>` becomes the accessible name.** CDP shows the `<input>` carrying
  no text of its own; UIA reports `name="Username"` on it. The label association
  is resolved for us.
- **`type="password"` becomes `protected`**, and it is usable as a locator
  (`{"role":"edit","states":{"protected":true}}` matched the one password field).
- **The redaction rule holds on a second platform.** `find` reported
  `value: None` for the password field while `set_value` wrote through it --
  DOM afterwards: `pass: "secret-123"`. Read-protected, write-usable, which is
  what automated sign-in needs (§ the value-redaction decision).

Every write in that run was confirmed through CDP rather than from the driver's
own return: `user: "cdp-verified"`, `clicks` 0 -> 1, `checked` false -> true.

CDP needed `--remote-allow-origins=*` alongside `--remote-debugging-port`; without
it the WebSocket handshake is refused with 403 and the error says exactly which
flag is missing.

**Tests**: 573 Rust (CLI 40, uia 160), 99 front end.

## 2.26 容器限定（`within`）——把「哪个面板里的」变成可写的定位

### 为什么需要它

用户的方向是让 locator 偏描述性。前面已经有序数（第三个按钮）和邻近（某文本旁边的
按钮），但真机测量指出一个两者都救不了的形状。

先量清楚，`crates/aad-uia/examples/probe_unresolved.rs`。三轮迭代都推翻了上一轮的
判据：

1. 统计全部节点：1093/3427 = 31.9% 无法合成 locator。这个数字**虚高**——样本被
   `role=group` 这类容器主导，那不是用户会点的东西。
2. 只统计可交互元素（button/edit/check_box/…）：**143/878 = 16.3%**。同时否掉了两条
   看似可行的路：同 role 兄弟**中位 66 个**（「第 66 个按钮」不是任何人能核对的描述，
   纯序数救不了）；最近具名元素距离**中位 0px**——那是包含关系（窗口标题包住整个内容
   区），毫无区分力。
3. 检查层级才找到答案：`Node` 早就有 `parent_id` 和 `depth`，只是 `Locator` 没用。
   排除包含关系后真正相邻的距离中位是 6px；而往上找**第一个自身可 synthesize 的祖先**
   （不是最近的 parent——parent 常常是无名 group，用它等于把问题往上推一层），兄弟数从
   中位 66 降到**中位 3**。

| 类别 | 数量 | 占比 |
|---|---|---|
| 祖先可识别 + 子树内唯一 | 23 | 16% |
| 祖先可识别 + 子树内 ≤10（需序数） | 83 | 58% |
| 祖先可识别 + 子树内 >10（序数不实用） | 37 | 26% |
| 没有可识别的祖先 | **0** | 0% |

原本担心的「容器也同名」不成立：7 个 `关闭 (Ctrl+F4)` 全在同名 `tool_bar "选项卡操作"`
里，但继续往上走会遇到可识别的更高层祖先。

### 三个设计决定

**`within` 在 resolve 里排在属性筛选之后、proximity 与序数之前。** 它是范围限定，
proximity 和序数都该在这个范围内工作。先计数再查包含会把「工具栏里的第二个关闭按钮」
变成「窗口里的第二个关闭按钮且恰好在工具栏里」——通常是另一个元素，或者没有。真机对照
证实了这点：`--nth 1` 不限定容器时命中的是**「最小化」**；`--in "Terminal actions"
--nth 1` 命中 e7，`--in "Explorer actions" --nth 1` 命中 e5。

**沿 `parent_id` 走，不比较矩形。** 层级包含是应用自己声明的，重叠边界是布局的副产物。
按矩形会误抓画在工具栏上的 tooltip，也会漏掉滚出视野但仍是子节点的行。

**容器不可解析或有歧义时返回空**，与锚点行为一致。悄悄丢掉范围会搜遍整个窗口，在另一个
元素上执行写操作。

### 合成端：一个可以证明不会成功的搜索

`synthesize` 的容器兜底把 unresolved 从 **16.3% 降到 3.3%**（143 → 34）。但第一版慢到
不能用：合成 254 个元素共 917ms，最慢一次 **44ms**。

分组计时定位到根因：**907ms 花在那 22 个最终失败的元素上**（平均 41ms/个），成功的 232 个
总共 9ms。失败路径走完整条祖先链，每层做一遍完整 synthesize 加多次 resolve，而单次
resolve 只要 10-18µs——是调用次数爆炸，不是单次慢。

关键观察：往上走子树只会变大，候选只会变多。一旦某祖先的子树内同类元素已多到序数不可用，
更高的祖先必然更差。这不是把上限调松或调紧的取舍，而是**砍掉可以证明不会成功的搜索**。
同一个 1000 节点窗口：**989ms → 23ms，最慢一次 47ms → 0.8ms**。

### 顺带修掉的：样式串被当成标识符

第一版合成出的 locator 里有整串 Tailwind CSS：

```json
{"role":"button","class_name":"flex shrink-0 items-center justify-center font-[400] whitespace-nowrap select-none [&_svg]:shrink-0 text-[14px] leading-..."}
```

`durable()` 只拦 WinForms 的 `_r<n>_ad<n>` 后缀，拦不住这个。测量：class_name 中位
**17 字符**（`actions-container`、`monaco-icon-label`，作者写的名字，正常可用），但
**432 个超过 60 字符，153 个超过 120，最长 937**，全是 WebView 把 class 属性原样透传。

问题不是长，是它描述的是**外观**：把字号从 14px 改成 16px，locator 就失配，而且失配得
隐蔽（元素还在，只是找不到了）。判据用形状——多个空格分隔的 token 是 class 属性的样子；
单个长 token（`NonClientVerticalScrollBar`，26 字符）仍然可用，那是控件名。

### 端到端验证（含一次假通过）

`E:\tmp\container_fixture.ps1`：两个 GroupBox，各含一对 `name="Close"` 的按钮，属性
完全相同。

- 4 个候选 → `DRIVER.AMBIGUOUS_MATCH`（属性不够）
- `within` + `nth` → 唯一命中 e8
- 重启 fixture（window_id 变、四个 automation_id 全是纯数字全部漂移）→ **仍命中 e8**

第一次跑「重启前后标题相同」，看着像通过——但 `last=` 两次都是空，点击根本没落地。
原因是 fixture 的 `GetNewClosure()` 把 `$script:` 变量捕获进了闭包自己的作用域。改成
闭包里直接写 `$this.FindForm().Text` 后才拿到真实结果：`last=Terminal actions#1` 两次
一致。**「两次相同」在这里是假通过**，只读驱动返回或只比对标题都发现不了。

### 命名撞车

`Proximity.within` 是像素距离（number），`Locator.within` 是容器（Locator）。JSON 里靠
嵌套层级区分，填错会明确报错。但**扁平的表单草稿和命令行 flag 都没有层级可用**，所以：

- 前端草稿：`nearWithin`（距离）与 `containerName`/`containerRole`（容器）
- CLI：`--within`（距离，`requires = "near"`）与 `--in` / `--in-role`（容器）

沿用一个名字会让「填 40」和「填 tool_bar」落到同一个绑定上。

### 一个测试推翻了我的预期

我断言 `synthesize` 遇到匿名 group 会跳到更高的 tool_bar。实际输出是：

```json
{"role":"button","name":"Go","within":{"role":"group","nth":1,"within":{"role":"tool_bar","name":"Actions"}}}
```

递归让匿名容器**自己也被描述**了——「Actions 工具栏里的第一个 group」。这比跳过它更好：
描述更局部，往 tool_bar 里加一个不相关的兄弟也不影响。断言改成检查真正要紧的事（唯一命中
目标、兄弟得到不同的 locator），而不是钉死用了哪一层祖先——钉死会让日后合理的改进变成失败。

### 现状

- Rust：586 passed / 0 failed（`aad-uia` 173，`aad-cli` 43）
- 前端：105 passed
- CLI：`aad find --in <NAME> [--in-role <ROLE>]`
- GUI：编辑器里可填「inside element named / of role」，`describe` 会说出 `inside "…"`，
  带容器的位置不再触发「整窗口计数」提示

### 仍然缺的

- 26%（37/143）的元素因子树内同类元素超过 `MAX_COUNTABLE_SIBLINGS = 10` 被判为
  unresolved 而非给出一个高序数 locator。这个阈值是启发式。
- WebView 里的容器效果未单独测量（浏览器的 20 个 button 中 17 个是 chrome，容器限定
  理应比序数有效得多，但没有数字）。
- 录制端还没有把合成出的容器 locator 走一遍完整的录制 → 保存 → 重启 → 回放。

## 2.27 GUI 里真的能用吗——两个只有跑起来才看得见的缺陷

`within` 在驱动和 CLI 都验证过了，GUI 侧的字段也加上了、类型检查和单测都过。但这不构成
「能用」。跑起来发现两个缺陷，测试一个都抓不到。

### 面板把字段挤出了可视区

截图证据：编辑器面板底部出现横向滚动条，`name`、`class`、`position`、`of role`、
`within px` 的输入框全在可视区之外。

这也解释了探针为什么一直报 `DRIVER.NOT_FOUND`——那些字段的 bounds 落在窗口外，被
`describe` 的 offscreen 过滤当成不存在。**「字段做出来了但摸不到」和「没做」在用户那里
是一回事**，对 AI 调用方更是如此：它只能看到 UI 报告的东西。

根因是 `.grid` 固定两列加 `label span` 固定 88px。RECORDING 栏本来就窄，两列各自还要
88px 标签加输入框，撑不下就往外溢。改成 `repeat(auto-fit, minmax(190px, 1fr))` 随宽度
回落到单列，标签宽度从固定值改成 `min-width: 72px; max-width: 96px`，并给 `.editor`
加 `min-width: 0`（没有它，grid 子项能把 flex 容器顶得比父元素还宽）。

### 表单视图永远回不去

编辑器停在 JSON 视图，点 `Use fields` 没反应。看起来像按钮坏了。

CDP 读出 textarea 里的真实内容才看清：

```json
{"role":"button","name":"Close","framework_id":"WinForm","nth":2,
 "within":{"role":"group","name":"Terminal actions"}}
```

`framework_id` 不在 `isBeyondForm` 的 known 集合里，于是整个 locator 被判「超出表单」；
一切回表单就又被判超出，所以回不去。

**这不是「超出表单」，是表单缺一个字段。** `framework_id` 和 `class_name` 一样是普通标识
字段，驱动一直会返回它，合成也会用它。少这一个字段就让整个表单不可用，而失效方式很隐蔽。
加上 `toolkit` 输入框后 `view: "form"`，11 个字段全部可见，`WinForm` 正确回填。

### 这一轮的验证通道：CDP 直连 Tauri 的 WebView

按标签名找 UIA 输入框在这里必然歧义——`of role` 在界面上出现两次（next to 组、container
组）。而 Tauri 的 WebView 就是 Chromium：用
`WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=… --remote-allow-origins=*`
启动，就能在 DOM 层精确取到是哪一个，也能直接读出编辑器当前处于哪个视图、每个输入框的
placeholder 和 value。**这是把「按钮坏了」定位成「缺一个字段」的关键**。

（端口 9333 被本机另一个进程占用，换到 9411；`Start-Process -Environment` 在本机的
PowerShell 版本上不存在，改用进程级环境变量。）

### 一次不构成证明的对比

两个容器分别 Try it，都显示 `✓ matched button "Close"`——**这什么也没证明**，四个候选
同名，看名字分不出选中的是哪一个。改为直接比较 node_id：

| 容器 | 位置 | 命中 | 实际父容器 |
|---|---|---|---|
| Explorer actions | #1 | e5 | Explorer actions |
| Explorer actions | #2 | e6 | Explorer actions |
| Terminal actions | #1 | e7 | Terminal actions |
| Terminal actions | #2 | e8 | Terminal actions |

四种组合命中 4 个不同元素，同一个位置在不同容器里命中不同元素，每个命中都落在被指定的
容器内。这才是证明。

### 现状

Rust 589 passed / 0 failed，前端 107 passed。

## 2.28 录制端的完整链路：录 → 存 → 重启 → 回放

前面验证的容器 locator 都是我手写的。这一轮走真实链路：让**录制端自己合成**，然后重启
目标程序回放。

### fixture 的选法决定了这次验证有没有意义

用两组同名按钮（四个 `name="Close"`，属性完全无法区分，只有所属 GroupBox 不同）。如果
录制端只用属性，回放必然报歧义——**失败会明确暴露，而不是碰巧成功**。

交互靠驱动的 `invoke` 触发：本机 SendInput 拒收批次事件，而 `invoke` 走控件自身的默认
动作，会产生真实的 UIA 事件。

### 录制端合成的结果

点 e8（Terminal 的第 2 个）和 e5（Explorer 的第 1 个），录到：

```json
{"action":"invoke","locator":{"role":"button","name":"Close","framework_id":"WinForm",
 "nth":2,"within":{"role":"group","name":"Terminal actions"}}}
{"action":"invoke","locator":{"role":"button","name":"Close","framework_id":"WinForm",
 "nth":1,"within":{"role":"group","name":"Explorer actions"}}}
```

两步都用上了容器，位置也对。`automation_id`（`6491306`、`4066406`，纯数字句柄）被
`durable()` 正确弃用了——留着它反而会在重启后失配。

`sources: ['uia', 'win_event']`，`dropped: 0`。

### workflow 格式：验证器比我的记忆可靠

第一版凭印象写，六个字段全错，验证器逐条指出：`budgets` 要 `max_duration` 和
`max_executed_steps`（不是 `wall_clock_seconds`），`uses` 要 `capability.action@major`
形式。真实形状是三段：`snapshot`（按进程名加标题片段找窗口，不能用 window_id——重启就
变）→ `find` → 动作。

### 回放结果

重启后 window_id 从 `hwnd:6556602` 变成 `hwnd:3214234`，四个 automation_id 全部漂移。
回放 `succeeded`，最终标题 `last=Explorer actions#0`。

### 挖出一个实质缺口：回放成功了，但看不见每步做了什么

`aad run` 的返回只有 `executed_steps: 5`；`--journal` 里的 `action.finished` 只记
`id`、`uses`、`duration_seconds`。**所以整条链路没有任何地方能回答「第一步点了哪个
元素」。**

这对录制回放很实际：一个点错元素的回放和一个正确的回放，摘要完全一样——都是五步全绿加
一个 `succeeded`。而"最终标题对"只能证明最后一步对。

（`aad events` 只对持久化 run 有效，而 durable 按用户口径暂不做；内存执行器不落 store。）

修法是让 `action.finished` 带上结果摘要。先量：snapshot 输出 6491 字符，其中 `nodes`
占 5905；裁掉大数组后剩 575，find 摘成 193。

**按值的形状裁剪，不按 action 名字做白名单。** 白名单会在新增 action 时静默漏掉，而漏掉
的表现正是这次遇到的"什么都看不到"。规则是：数组换成 `<key>_count`，嵌套对象只留识别性
字段（`node_id`/`role`/`name`/`automation_id`/`window_id`/`title`/`process_name`/
`snapshot_id`/`revision`），标量原样保留（`found`、`match_count`、`ref`、`applied` 都在
这里）。

`value` 刻意不留：它能装下整篇文档，而受保护字段本来就不给读——都不该进一份每次运行都
保存的日志。

修完之后同一次回放能看清：

```
win     snapshot@1  {"snapshot_id":"f9638a…","revision":1133,"window":{…},"nodes_count":13}
find1   find@1      {"found":true,"node":{"node_id":"e8","role":"button","name":"Close"},…}
        → 命中 e8，属于容器 'Terminal actions'
act1    invoke@1    {"applied":true,"action":"invoke","node_id":"e8",…}
find2   find@1      {"found":true,"node":{"node_id":"e5",…}}
        → 命中 e5，属于容器 'Explorer actions'
act2    invoke@1    {"applied":true,"action":"invoke","node_id":"e5",…}
```

journal 总共 6741 字符（5 步，含一次 snapshot）。之前这些信息一条都没有。

### 现状

Rust 592 passed / 0 failed（`aad-runtime` 集成测试 59），前端 107 passed。

### 仍然缺的

- 录制端合成的 locator 用了 `within` 后，仍有 26% 的元素（子树内同类超过
  `MAX_COUNTABLE_SIBLINGS = 10`）会被判 unresolved。
- WebView 里的录制回放未测（本轮只测了 WinForms）。
- `run` 的**摘要**里仍然没有每步信息，只有 journal 里有；不带 `--journal` 就仍然是盲的。

## 2.29 复杂 WebView 与内容锚点

### 复杂 fixture 才问得出问题

旧的 WebView fixture 只有 6 个控件。新造一个带真实难点的：列表行里成排的重名按钮
（`Edit`/`Delete` 各 5 个，无 id、同名同 role，只有所在行不同）、两个结构相同的地址
面板、动态增删行、模态里另一个 `Save`、iframe 表单、无 label 的输入框。

CDP 作独立事实源与 UIA 对照，测出三件事：

- **iframe 能穿透**（`Coupon`/`Redeem` 出现在快照里）——之前不确定。
- **`automation_id` 就是 DOM 的 `id`**，所以 `billing-city` 这类反而不难。
- 36 个 button 里只有 3 个属于页面，其余 33 个是浏览器 chrome。

合成质量本身不错：82 个可交互元素只有 1 个失败，10 个重名按钮全部唯一命中。

### 但"唯一命中"掩盖了一个更坏的失败

合成出的 locator 是 `within: {automation_id: "row-0"}`。fixture 的 `Add row` 会在顶部
插入一行并重新渲染——`row-0` 那时是新行。

实测（读 fixture 标题确认落地，不信驱动返回）：

```
第一次           → last=edit:Ada
插入一行后同一 locator → last=edit:New5
```

**两次都成功、都唯一命中、零报错。** 这比 unresolved 危险得多：unresolved 会明确失败，
而这个会安静地对错误的对象执行写操作。

跨全机 1134 个可交互元素量了一遍：35 个（3%）的容器靠位置识别。同时那 39 个仍然
unresolved 的元素形状是同父兄弟**中位 22 个**、`name=None`、`id=None`——本质上无法描述，
给它们编一个"第 22 个无名按钮"是把明确失败换成隐蔽错误。**所以要修的是这 35 个，不是
那 39 个。**

### 三个候选，实测选一个

行的 `name` 是 `Order for Ada`（aria-label 被 UIA 采纳）。插行前后各点一次：

```
within row-1            edit:Ada → edit:New5   漂了
within "Order for Ada"  edit:Ada → edit:Ada    稳定
near Ada, right         edit:Ada → edit:Ada    稳定
```

后两个都稳。选行的 `name`：不需要新语法，只需要改合成器的偏好。

### 真因不在容器逻辑，在字段顺序

我原以为是 `by_container` 取了不好的祖先，写了"优先找有内容身份的容器"。测试全绿、编译
通过、**真机行为完全没变**。

让探针逐层报告祖先链才看清：第 2 层祖先的 name 是 `Order for New6`，但 `synthesize`
只输出了 `{"role":"data_item","automation_id":"row-0"}`——name 被丢了。因为
`automation_id` 排在 `refinements` 第一位（上次为了"id 不被翻译"特意这么排的），
`row-0` 已经唯一就提前返回，name 永远试不到。

偏好顺序本身没错，错在它对所有 id 一视同仁。所以位置性的 id 退到 name 之后，
`billing-panel` 的行为不变。

### 判据：从"看形状"换成"在快照里数"

第一版按形状划线（单个词 + 尾号 = 位置性）。测试立刻抓到反例 **`save-1`**——stem 是
`save`、有尾号、符合规则，但它显然是"保存按钮"不是"第 1 个槽位"。

真机数据给出了可靠得多的判据：**位置性 id 都成族出现**。

```
row        7 个   row-0 … row-6
list_id_2  44 个  list_id_2_0 …
view       22 个  view_1000 …
```

而 130 个不带尾号的 id 全是 `AddButton`、`CloseButton` 这类名字，`save-1` 在族里是孤例。
所以判据是"同一 stem 下同时存在多个兄弟"——不用猜命名习惯，直接在快照里数。

### 结果

| | 改前 | 改后 |
|---|---|---|
| 容器是稳定身份 | 190 (17%) | 219 (19%) |
| 容器靠位置识别 ⚠ | **35 (3%)** | **11 (1%)** |
| unresolved | 39 (3%) | 35 (3%) |

决定性检验：让合成器自己产出 locator，插一行，再点。`node_id` 从 e122 变成 e128（页面
确实重排了），但两次都是 `edit:Ada`。

`aad-uia` 177 passed。

### 剩下的 11 个

形状是 `within` 指向一个**和自己同名同 role 的 button**（`win-osdk` 在 `win-osdk` 里）
——UIA 把可点击容器和内部按钮都报成 button，容器要加 `nth` 才唯一。占 1%，记录不追。

## 2.30 把内容锚点接进 GUI

Rust 侧能产出内容锚点了，但 GUI 能不能用是另一回事。先测，不猜：

```
isBeyondForm: true                                     ← 只能看 JSON
describe:  button named "Edit" inside "Edit Delete"    ← 区分行的那层不提
roundtrip: {... within: {name: "Edit Delete"}}         ← 里层整个丢了
```

第三条最危险：`toDraft` → `fromDraft` 会**静默降级** locator。`Edit Delete` 有四个同名
兄弟，所以往返之后「Ada 的 Edit」变成了歧义——而界面看起来像编辑成功了。

单层容器也有问题：`describe` 报 `inside "group"`，把 `billing-panel` 说成了 group，
而那个页面上每个面板都是 group。

### 三处改动

**describe 走到底，每层报真正识别它的字段。** 描述变长，但这是人判断 locator 对不对的
唯一依据——短而错不如长而准。上一轮已经吃过一次亏（`describe` 的 limit 截掉了按钮，
我两轮都以为按钮不存在）。

**表单加四个格子**：容器的 `automation_id`，以及外层容器的 name/role/id。两层是合成器
实际产出的深度；三层以上仍留在 JSON，那样的表单没人看得懂。

**容器里的 framework_id 不该产出。** 实测 11 个容器里它出现 7 次，**7 次全部与
name/automation_id 同现，0 次是唯一识别手段**——没有区分力，只是让 locator 在渲染引擎
变化时失配，还把整个 locator 推出了表单可编辑范围。

### 真机验证暴露两个单测抓不到的缺陷

**Try it 无法区分两个同名元素。** 把容器从 `Order for Ada` 改成 `Order for Brian`，
两次都显示 `✓ matched data_item "Edit Delete"`——六行的单元格全叫这个名字。用 node_id
查才知道确实换了元素（e106 → e109）。一个不能区分两次不同结果的验证按钮等于没有验证，
所以结果里现在带上 node_id。

**最大化后右栏整个在窗口外。** CDP 之前报 `horizontalScroll: false`，但那测的是
`.editor` 内部——只测局部的检查漏掉了整体溢出，是截图抓到的。量出来：视口 1536，
UI outline 栏自己撑到 1318px，总宽 1958，Recording 面板从 x=1578 开始。

原因是 grid 的 `1fr` 最小值默认为内容宽度，而每行 outline 都带一个不换行的 snapshot id。
改成 `minmax(0, 1fr)`，三个 panel 各加 `min-width: 0`。修后 `bodyScrollWidth` 1958 →
1536，三栏都在视口内。

### 结果

改容器名真的改变所选元素，用 node_id 证明（不用显示名——四个候选同名，比较它什么也不
证明）：

```
Order for Ada   → e106  所属行 Order for Ada
Order for Brian → e109  所属行 Order for Brian
Order for Chen  → e112  所属行 Order for Chen
```

Rust 596 passed，前端 116 passed。
