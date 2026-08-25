# 桌面自动化运行时架构

> 状态：目标架构草案，基线日期 2026-08-24。本文区分当前 Python v0 纵向切片与面向产品的目标架构；未完成项不构成能力承诺。

## 1. 设计目标与边界

本项目采用 **Python-first、Rust-ready** 路线：先用 Python 验证描述符、控制流、超时、错误与进程插件边界，待协议和运行语义经真实平台验证后，再把可信常驻核心逐步迁移到 Rust。Python 不是临时随手脚本，Rust 也不是为了语言而重写；二者必须共享语言无关的计划模型、错误模型和进程协议。

运行时负责把已经声明的工作流可靠地执行出来，不负责相信或延续规划器的自由推理。核心原则是：

- LLM / Planner 是不可信输入源，只能提交声明式 descriptor、locator、action 和期望结果。
- Trusted Host 负责编译、校验、策略判定、能力授权、调度、取消、结果核验和审计；规划器不能直接取得平台句柄、进程权限或 secret 明文。
- 第三方能力、平台 driver、OCR 和任意 script 都是进程外 worker，不以内嵌 Python import、动态库或 `eval/exec` 作为安全边界。
- 三端共用计划 IR、协议和策略语义，但 Windows UIA、macOS AX、Linux AT-SPI/Wayland 必须分别实现和发布。
- Accessibility 和确定性接口优先；视觉与 OCR 只能由 descriptor 显式请求，不能在失败后偷偷扫描整屏。
- 对未知应用只提供 best effort；登录、锁屏、UAC、凭据界面等安全桌面默认拒绝。

## 2. 当前 v0 与目标态

| 方面 | 当前 Python v0 子集 | 目标态 |
|---|---|---|
| 描述符 | JSON/YAML 都归一到唯一形状：`apiVersion: ai-auto-desktop.dev/v1alpha1`、`kind: Workflow`、`metadata.name`；严格校验并编译为冻结模型 | 版本化 canonical schema、迁移工具、签名与兼容策略 |
| 表达式 | 白名单 Python AST 的只读解释器，不使用 `eval/exec`，禁止所有函数和方法调用 | 保持无 I/O、有限成本、跨语言一致的表达式语义与测试向量 |
| 步骤与控制流 | `action/set/block/fail/return/script`、`if/switch/foreach/while`、`on_error/finally`；sibling DAG 已支持有界只读并发；受限串行计划支持顶层安全点恢复，显式 opt-in 后支持具备投影契约的顶层只读 action 与 `action_intent` v2 重放；script 默认拒绝 | 写 action/script reconciliation 与更通用的可恢复计划状态机 |
| 状态 | run 结果为 `succeeded/failed/timed_out/cancelled/unknown_effect`，step 另有 `skipped` | 冻结跨语言状态兼容与迁移规则 |
| 执行能力 | 已有 Windows UIA、macOS AX、Linux KDE/X11 AT-SPI 进程 driver、三端显式文本输入、受限中心点左键 `pointer_click` 和显式图片 OCR；资格范围分别记录 | 真实应用矩阵、受控截图与应用专用 adapter |
| IPC | stdio NDJSON v0，便于调试和跨语言实现；已校验 manifest schema/action major，尚无完整 wire version 协商 | 保留语义兼容层，迁移到 Protobuf/CBOR 等 IDL + named pipe/Unix socket |
| 隔离 | process plugin 使用 POSIX 进程组；Linux script 使用 bubblewrap + prlimit，其他平台 fail-closed | Windows Job Object/restricted token；macOS 受控 helper；Linux bubblewrap/OCI；资源与 capability 限额 |
| 安全 | 结构化错误、script fail-closed、action risk policy、manifest 与 action I/O schema 校验；确认 token/taint 等尚未实现 | 签名插件、系统 secret store、确认 token、完整 taint enforcement、审计与更新回滚 |
| 平台能力 | 已有只读三端 probe、Windows UIA 与 macOS AX process driver，以及 Linux KDE/X11 AT-SPI 纵向切片；Linux 自有 GTK3/Qt5 fixture 已通过，Windows/macOS 真机结果待回传 | 各平台按真实应用矩阵分级支持 |

v0 的价值是锁定运行语义并建立故障测试夹具。Windows UIA driver 已开始调用真实原生接口，但在真实 Windows runner 的 fixture app、UIPI 与权限矩阵通过前，仍不能把跨平台 contract 测试当作产品成功率。

## 3. 信任与进程边界

```text
Untrusted Planner / Workflow Author
                 │ descriptor + refs，不含 secret 明文
                 ▼
┌──────────────── Trusted Host ────────────────┐
│ parse → validate → compile immutable plan    │
│ policy → scheduler → deadline/cancel         │
│ resolver → verifier → journal/audit          │
│ secret broker → single desktop writer lease  │
└─────────┬──────────┬──────────┬──────────┬────┘
          │          │          │          │
 Capability     Driver      OCR worker  Script worker
 worker/API   UIA/AX/AT-SPI explicit only default denied
               │
          用户桌面 / OS API
```

Host 是唯一能签发本地 capability 的主体。worker 只获得完成本次请求所需的最小能力、绝对 deadline、不可复用的 request ID 和经过脱敏的参数。来自桌面树、OCR、网页、文件及 worker stderr 的内容都属于不可信 observation，不能把其中的文字解释为新的系统指令。

进程隔离首先用于故障和取消边界，并不自动等于恶意代码沙箱。若无法提供操作系统级文件、网络、凭据和资源隔离，Host 必须拒绝运行“不可信 script”，而不是把独立子进程宣传成完整沙箱。

## 4. 描述文件编译为不可变计划

执行前必须完成 `parse → schema validate → semantic validate → compile → freeze`，任何一步失败都不得产生局部执行。v0 只接受 canonical identity `ai-auto-desktop.dev/v1alpha1 / Workflow / metadata.name`，不会猜测或静默升级其他版本。编译产物是一次 run 的不可变 `ExecutionPlan`，至少固定：

- schema/API 版本、workflow identity、输入与输出契约；
- 全局唯一 step ID 和稳定 step path；唯一性覆盖分支、循环体、error handler 与 `finally`；
- 已编译的只读表达式，而不是待运行的源代码；
- step capability、effect class、retry policy、绝对 deadline 和 postcondition；
- 控制流边和最大迭代/最大步骤预算；
- policy 与 secret 仅保存引用，不保存运行时明文。

v0 的公共 step 集合为 `action`、`set`、`if`、`switch`、`foreach`、`while`、`block`、`script`、`fail`、`return`；公共控制字段包括 guard、timeout、attempt timeout、retry、`on_error` 与 `finally`。未知字段、未知 step type、非法表达式和非正数迭代上限失败关闭。当前 v0 校验 manifest、action contract major 与输入输出 schema；完整的 provider SemVer、平台和权限解析仍在后续阶段。编译后的 mapping、sequence 和 step 节点不可被运行期插件或表达式修改；每次 attempt 使用独立的可变 execution context，事件和结果另写 journal。

### 4.1 受限表达式

v0 直接解释白名单 AST：允许字面量、显式 context 的变量/属性/下标、容器、布尔/比较/算术运算和条件表达式。拒绝所有 `ast.Call`，也就是包括 `len`、`min` 等看似纯函数在内的所有函数和方法调用；同时拒绝赋值、import、lambda、推导式、await/yield、私有属性和 dunder key。

目标态仍坚持四个不变量：无 I/O、无 secret 隐式读取、无可观察副作用、可预算求值成本。未来若 Rust 采用不同实现，必须通过同一批 expression conformance vectors，不能悄悄改变 truthiness、短路或数值语义。

### 4.2 有界控制流

- `if` / `switch` 只执行一个确定分支。
- `foreach` 在编译或进入循环时冻结输入集合，并要求正数 `max_items`；超过上限结构化失败。
- `while` 必须显式提供正数 `max_iterations`，并受 step/workflow deadline 双重约束。
- retry 同时受 attempt 数、退避总时长和父 deadline 限制；它不是一种无界循环。
- `on_error` 只允许 `continue`、`rethrow`、`return` 等声明式转移；handler 本身也受预算约束。
- workflow/step 的 `finally` 在成功、失败、超时和取消路径均尝试运行，但不能突破上层 shutdown deadline。cleanup 失败附加为 `suppressed`，不得覆盖原始失败。

Host 还应维护全局 `max_steps`、`max_events`、`max_output_bytes` 等防膨胀预算。计划结构未来即使扩展 DAG/并发，也不得引入动态生成的无限新步骤。

## 5. 调度、单写者与确定性回退

一个物理 desktop session 同时只发放一个 writer lease。所有可能改变窗口、焦点、键鼠或剪贴板的步骤串行进入 writer；纯 snapshot、OCR 和无副作用计算只有在声明为 read-only 且不会读取不一致前台状态时才可并发。检测到用户键鼠介入、session 切换、锁屏或 driver generation 变化时，暂停新写入并重新观察。

当前 Python v0 调度器已经按每个 sibling scope 的 `depends_on` 拓扑调度，声明顺序只用于稳定选择和确定性合并。省略依赖的 legacy step 在编译期形成链，因此保持串行；显式 `depends_on: []` 才能建立独立分支。首版并发面刻意收窄为经 provider contract 确认为 `read_only`、没有 retry/handler/finally、也不请求 `desktop.input` 的 action。`set`、script、控制流容器、return/fail 及任何非只读 action 都是全局独占屏障，必须等已在途读取完成后才能运行。

并发 action 各自在隔离的 context/variable snapshot 上求值；事件通过线程安全出口按实际发生顺序实时发布，主调度线程在整批结束后再按 descriptor 顺序提交 `steps.<id>` 结果。只读 worker 若改写 variables、非 step context、已有 step 记录或其他 step 的结果，运行时以 `RUNTIME.CONTEXT_CONFLICT` 失败关闭。guard 为 false 的 `SKIPPED` step 只有在自身 `finally` 完成后才满足依赖；`SUCCEEDED` 和 handler `continue` 同样满足依赖。首个未处理失败、return 或取消一旦被观察到，调度器不再启动新 step：尚未派发的 step 记录为 `scope_terminated`；已派发的 peer 保留真实终态和事件，并以 `discarded_due_to_scope_termination` 标明其结果不再对后续步骤可见。运行时等待所有已派发 action 返回后再进入外层 handler/finally。`max_executed_steps` 的 attempt 预留在线程间原子执行，workflow deadline 与每个 worker 的父 deadline snapshot 共同约束并发任务；取消和 deadline unwind 时，step/workflow `finally` 使用独立的 cleanup deadline。

定位与动作的默认顺序固定为：

1. 应用原生 API / 浏览器 DOM / AppleScript、COM 等确定性接口；
2. UIA / AX / AT-SPI accessibility 强标识和语义 action；
3. accessibility 的关系、role、name、state 与可验证的弱匹配；
4. 聚焦控件后的键盘快捷键；
5. 从当前 fresh 语义节点 bounds 推导的显式 pointer 动作；v0 只允许中心点左键，并要求平台 hit-test 仍命中目标或其已证明子树；
6. descriptor 显式声明的截图 + 非 OCR 视觉定位；
7. descriptor 显式声明的 OCR step，其输出再由后续条件和动作消费。

候选不存在返回 `NOT_FOUND`，候选分数过近返回 `AMBIGUOUS`；不得默认点第一个。绝对坐标、`nth` 和 OCR 命中都必须是显式选择并接受同样的 policy、歧义检查与 postcondition。**不存在“accessibility 失败就隐式全屏 OCR”这条路径。**

每个桌面写动作执行闭环：

```text
observe → resolve → precondition → policy/confirm → dispatch
        → wait/event → re-observe → verify postcondition
```

节点 ID 只在 snapshot revision 内有效；dispatch 前必须重新解析 locator，不能跨刷新复用平台句柄。

## 6. 工作进程与平台驱动

### 6.1 公共工作进程契约

capability、driver、OCR 与 script worker 都是长驻或按步启动的子进程，stdout 仅承载协议帧，诊断只写 stderr。Host 必须持续 drain 两条管道、限制单帧和总输出、检测重复/未知 request ID、处理半帧与非法 JSON，并对崩溃实施熔断，而不是无限重启。

- Capability worker：对应用业务 API、浏览器 DOM、文件/数据转换等窄能力做进程外适配，只能调用 manifest 声明且经 policy 授权的 action。
- Driver worker：窗口枚举、snapshot、语义动作、输入与截图；不负责 workflow 控制流。
- OCR worker：输入由 Host 获取且带 frame/region provenance 的图像，输出文本、bounds、language、confidence；不产生或执行点击。
- Script worker：v0 默认禁用；显式 `--allow-scripts` 后仅在具备 bubblewrap 和 `prlimit` 的 Linux 上运行，使用私有网络/PID namespace、空环境、无 host home/`/etc` 挂载及 CPU/内存/输出限制；其他平台 fail-closed。后续增加 Windows restricted token + Job Object 和 macOS 受控 helper。

### 6.2 平台拆分

| Driver | 首选接口 | 明确边界 |
|---|---|---|
| Windows | UIA/Win32，必要时补 IA2/JAB；`SendInput` 后备 | UIPI、管理员窗口、Session 0、UAC/锁屏；正式取消依赖 Job Object |
| macOS | 签名稳定的 Swift/ObjC AX helper；CGEvent 后备 | Accessibility 与 Screen Recording 分权；bundle identity/TCC 必须稳定 |
| Linux | AT-SPI2；X11 XTEST 或 Wayland portal + libei | 当前先资格验证本机 KDE Plasma/X11；Wayland、GNOME 与其他 compositor/profile 分别声明 |

公共 Runtime 不根据 OS 猜能力，只读取 worker manifest/capability。平台特有错误在 driver 内归一化，同时保留脱敏的 native code 供诊断。

## 7. NDJSON 协议 v0 与未来 IPC

v0 使用 stdio 上每行一个 UTF-8 JSON object，协议 stdout 不得混入日志。当前调用为：

```json
{"type":"invoke","id":"01J...","action":"desktop.invoke","args":{},"deadline_ms":1787551200123}
```

成功和失败分别为：

```json
{"id":"01J...","result":{}}
{"id":"01J...","error":{"code":"DRIVER.NOT_FOUND","message":"...","retryable":false,"details":{}}}
```

v0 的 `deadline_ms` 表示 Unix epoch 毫秒绝对时间，不是“从收到消息开始再等 N 毫秒”。每个 request ID 严格对应一个含 `result` 或 `error` 的响应。Host 先短暂探测插件主动输出的 `{type: "manifest"}`；没有主动 manifest 时再发 manifest 请求。当前 v0 接受多种兼容响应形状并校验 manifest schema 与 action contract major；完整的 wire protocol major/minor 协商仍属于后续门槛。

Host 写入并 flush 请求成功后，将插件错误、timeout、EOF 和协议错误标记为 `details.dispatched=true`；写前失败则为 false 或缺省。当前每个进程只允许一个 in-flight 请求，没有 streaming、请求级 cancel、自动重启或进程池；timeout/EOF/协议错误会 fail-stop 并回收整个插件进程。stderr 只保留有界 tail，stdout 行与内部队列也必须有界。

NDJSON v0 是 bootstrap transport，不是长期 ABI。目标协议要补齐 major/minor 协商、最大帧、显式 cancel、ready/accepted 边界、capability、心跳和敏感字段标记。迁移到 Protobuf/CBOR 等语言无关 IDL 与 Unix domain socket / Windows named pipe 时，应保持 request/result/error/deadline/capability 的领域语义，并用双栈契约测试完成滚动迁移。Python、Rust、Swift 和将来的其他 worker 不共享内存布局，也不暴露语言对象序列化。

## 8. 截止时间、取消、重试与效果不确定性

所有 timeout 在进入 run 时转换为单调时钟上的绝对 deadline，并按父子关系只缩短不延长：

```text
workflow deadline
  └─ step deadline
      └─ attempt / plugin RPC / OS call deadline
```

墙钟时间只用于审计展示；调度判断使用 monotonic clock，wire 中的 epoch deadline 由发送端在边界处换算并保留剩余预算。取消顺序为：停止调度和 retry timer → 发协议 cancel（协议支持后）→ 关闭能力与 stdin → 宽限期 → 终止进程树 → 释放 writer lease → 尽可能重新 observe/reconcile。

POSIX worker 使用独立 session/process group，先 `SIGTERM`、宽限期后 `SIGKILL`。当前纯标准库 Windows v0 只能创建新 process group 后对直接进程 terminate/kill，**不能保证终止任意 descendant tree**；正式桌面写能力上线前必须由 Job Object 或等价 supervisor 补齐。取消 Python future 或杀掉父 PID 都不足以证明工作已停止。

retry 需要同时满足：错误 `retryable=true`、step 被声明为 `read_only` 或 `idempotent`、deadline 尚有预算。非幂等的 click/invoke/send/delete/pay/install 在 dispatch 后超时、worker EOF 或 driver 崩溃时返回 `UNKNOWN_EFFECT`，禁止自动重放；Host 只能在重新观察并证明未生效后，由明确策略决定后续动作。

### 8.1 持久运行控制与租约

持久 run 把“外部期望”与“runner 已生效状态”分开：控制面调用 pause、resume 或 cancel 时，只以 CAS 更新 `desiredState` 并在同一事务写入请求事件，不直接声称 run 已暂停或取消。重复请求在非终态为幂等读取；`cancel` 是吸收态，不能被 pause 或 resume 覆盖；任意终态都拒绝后续控制。

operator 不持有 runner 的 bearer token，也不应为提交控制意图而取得它。owner lease 只 fencing runner 对 status、event 与 checkpoint 的写入：runner 必须在安全点读取最新 `desiredState`，再用 owner ID、token 和未过期时间完成原子状态转换。`running → paused` 必须与 `run.paused` 事件及 lease 释放同事务提交；resume 只把期望改回 `run`，随后由 runner 重新 claim lease，才能执行 `paused → running`。取消也只在安全点落为 `CANCELLED`；若请求已 dispatch 且副作用仍无法确认，必须落为 `UNKNOWN_EFFECT`，不能伪装成已安全取消。

JournalStore 只执行调用方给出的敏感标记和存储不变量，不通过内容猜测 secret。RunService 是可信持久化入口：它必须按 descriptor 的 input/output `sensitive` 声明显式传递标记并 fail closed。当前机制不是完整 taint tracking；未实现跨变量、插件响应与派生值的自动敏感传播前，不得宣称能自动防止所有 secret 泄漏。

当前 durable executor 默认使用 `deny` 模式；CLI 只有在 `start` 与 `resume` 显式传入
`--durable-actions read-only` 时才启用受限 action 通道。两种模式都只接受
`max_concurrency=1` 且顶层未显式声明 `depends_on` 的 legacy 串行计划，其他 DAG 在创建 run 前
失败关闭。opt-in action 只能位于顶层，必须有效为 `read_only`，并且是无 `if`、
`precondition`、`postcondition`、retry、step handler 或 step `finally` 的单次 attempt。嵌套
action、写 action、script，以及声明敏感 input/output 的 workflow 仍被拒绝。

durable action 的 provider manifest 与 descriptor step 都必须把 input、output、error
sensitivity 显式声明为 `public`。manifest action 还必须给出稳定的
`durability.checkpoint_fields`，每个字段以有界 JSON Pointer、schema 和缺失策略定义；descriptor
则通过 `checkpoint.output.mode` 明确选择 `project` 的 provider 白名单字段或 `omit`。创建 run 前
会验证 canonical manifest、有效 `read_only` effect、非空且全为 `not_applied` 的错误合约、双方
sensitivity、checkpoint 投影和与输入无关的静态 policy。预检不会求值 action 的 `with`，因此
它可以引用前序顶层 step 的动态输出；实际输入只在该 action 即将执行时求值并做 schema 与
policy 校验。

普通顶层控制流节点继续复用 WorkflowRunner 的单顶层 segment API；一个节点连同其嵌套步骤、
handler 和 finally 是不可拆分的执行段。checkpoint 记录 schema/runtime 版本、canonical descriptor
SHA-256、phase、下一顶层索引、已消耗 attempt 数、首次启动时确定的绝对 deadline，以及
variables、表达式可见 step context 和诊断 step records。暂停时间仍消耗该绝对 deadline，resume
不重置预算。

普通顶层段在执行前先原子写入 `in_top_level_step` checkpoint，完成后写入
`between_top_level_steps`。符合条件的只读 action 会在 provider dispatch 前额外原子持久化
`action_intent` v2：它记录 operation、step、已预留 attempt、原始 dispatch deadline，以及
provider/contract/projection/input binding 摘要，不记录原始 action input。恢复会重新校验这些绑定，
并只对该只读 intent 执行安全重放；篡改、过期或不匹配的 intent 均在 dispatch 前失败关闭。action
成功后，表达式上下文、checkpoint 与最终输出只能看到 `project` 选中的字段；`omit` 产生空输出，
原始 provider 响应不会持久化。

没有合法 `action_intent` 的 `in_top_level_step` 或 `finalizing` 仍不得重放，必须零 dispatch 地
终结为 `UNKNOWN_EFFECT`；进入 workflow finally 前也先写 `finalizing`，避免崩溃后重复 cleanup。
action intent、dispatch 授权、完成 checkpoint 与终态提交都用期望 `desiredState` 做 CAS。若
pause/cancel 与完成并发，CAS 冲突会转入相应控制路径，不覆盖 operator 意图，也不会为已完成的
只读 observation 再次 dispatch。lease 目前只在这些持久边界同步 heartbeat；它保证旧 owner 不能
继续写 journal，但不等于能异步强杀已经进入插件或 OS 的调用。

## 9. 状态与结构化错误

Run 和 step 的终态统一使用：

- `SUCCEEDED`：动作完成且所需 postcondition 已验证。
- `FAILED`：确定没有成功，或验证明确失败。
- `TIMED_OUT`：deadline 到期，且能证明目标副作用未发生或该步骤无副作用。
- `CANCELLED`：在 dispatch 前取消，或能证明取消后无副作用。
- `UNKNOWN_EFFECT`：请求可能已到达执行点，但无法确认副作用；不可自动 retry。
- `SKIPPED`：因条件、分支或已终止的前置路径而未调度。

传输层可以使用小写 JSON 值，但必须与上述枚举一一映射，不得把 `TIMED_OUT`、`CANCELLED` 或 `UNKNOWN_EFFECT` 折叠成普通失败。恢复时遇到上次仍为 running/accepted 的写步骤，默认 reconciliation 为 `UNKNOWN_EFFECT`，不能直接重放。

错误对象至少包含稳定 `code`、`category`、`phase`、`message`、`retryable`、`effect`、step/workflow location、attempt、脱敏 details、cause 与 suppressed errors。分支匹配错误码和类别，不匹配自由文本。日志、CLI 和协议共同复用该模型，保证机器可判定。

## 10. 密钥、策略与审计

- Descriptor、plan、event 和 IPC 中只出现 `secret_ref`；Host 在最后使用点向系统凭据库解引用，默认不通过命令行、环境变量、日志或 planner context 传递明文。
- Policy 绑定可信应用身份（签名、bundle ID、executable）、用户/session、capability、数据范围、effect class 与过期时间，不能只相信窗口标题。
- 发送、删除、付款、授权、安装、跨应用粘贴敏感数据等高风险动作需要绑定“目标 + 参数摘要 + 计划 revision”的一次性确认 token；UI 变化后 token 失效。
- 审计记录 descriptor/plan digest、actor、worker build/hash、capability、deadline、locator、候选决策、确认、dispatch/accepted/terminal、postcondition、状态和错误；secret 明文、受保护文本和未经许可的截图不入日志。
- 截图默认本地、短 TTL、区域最小化；上传或长期保留需独立授权。审计存储需要访问控制、完整性校验、保留策略和可追踪删除。

## 11. Rust 迁移约束

满足以下条件前不迁移核心：descriptor/plan schema 已有版本策略；NDJSON v0 契约测试稳定；真实 driver 给出了性能与可靠性瓶颈证据；取消、错误、effect 与 journal 语义已经冻结；Python 与 Rust 可运行同一批 conformance fixtures。

迁移顺序优先为协议类型/错误模型 → supervisor 与进程树治理 → deadline/scheduler/single-writer → policy/secret/audit。OCR/ML 和快速生态插件可以长期留在 Python worker。迁移期间由同一计划摘要、测试向量和 wire contract 保证行为等价，不做一次性全量重写。
