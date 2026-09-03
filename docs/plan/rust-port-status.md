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
