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
| 步骤与控制流 | `action/set/block/fail/return/script`，以及 `if/switch/foreach/while`、`on_error/finally`；script 默认拒绝 | 可恢复的计划状态机、持久 journal、确定性 reconciliation |
| 状态 | 当前 run 结果为 `succeeded/failed/timed_out/unknown_effect` | 补全并统一 `SUCCEEDED/FAILED/TIMED_OUT/CANCELLED/UNKNOWN_EFFECT/SKIPPED` |
| 执行能力 | 长驻 NDJSON fixture/process plugin，用于 OCR mock、桌面 invoke mock、重试和超时测试 | 真实 UIA/AX/AT-SPI driver、输入后备、截图、应用专用 adapter |
| IPC | stdio NDJSON v0，便于调试和跨语言实现；已校验 manifest schema/action major，尚无完整 wire version 协商 | 保留语义兼容层，迁移到 Protobuf/CBOR 等 IDL + named pipe/Unix socket |
| 隔离 | process plugin 使用 POSIX 进程组；script 即使显式开启也因暂无强沙箱而 fail-closed | Windows Job Object/restricted token；macOS 受控 helper；Linux bubblewrap/OCI；资源与 capability 限额 |
| 安全 | 结构化错误、script fail-closed、action risk policy、manifest 与 action I/O schema 校验；确认 token/taint 等尚未实现 | 签名插件、系统 secret store、确认 token、完整 taint enforcement、审计与更新回滚 |
| 平台能力 | **尚无真实桌面 driver，不能宣称支持任一 OS 的 UI 自动化** | Windows 首先产品化；macOS 与 Ubuntu GNOME 经过 probe 后分级支持 |

v0 的价值是锁定运行语义并建立故障测试夹具，而不是以 mock 成功率代替真实桌面成功率。

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

## 4. Descriptor 编译为不可变计划

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

定位与动作的默认顺序固定为：

1. 应用原生 API / 浏览器 DOM / AppleScript、COM 等确定性接口；
2. UIA / AX / AT-SPI accessibility 强标识和语义 action；
3. accessibility 的关系、role、name、state 与可验证的弱匹配；
4. 聚焦控件后的键盘快捷键；
5. 从当前语义节点 bounds 推导的 pointer 动作；
6. descriptor 显式声明的截图 + 非 OCR 视觉定位；
7. descriptor 显式声明的 OCR step，其输出再由后续条件和动作消费。

候选不存在返回 `NOT_FOUND`，候选分数过近返回 `AMBIGUOUS`；不得默认点第一个。绝对坐标、`nth` 和 OCR 命中都必须是显式选择并接受同样的 policy、歧义检查与 postcondition。**不存在“accessibility 失败就隐式全屏 OCR”这条路径。**

每个桌面写动作执行闭环：

```text
observe → resolve → precondition → policy/confirm → dispatch
        → wait/event → re-observe → verify postcondition
```

节点 ID 只在 snapshot revision 内有效；dispatch 前必须重新解析 locator，不能跨刷新复用平台句柄。

## 6. Worker 与平台 driver

### 6.1 公共 worker 契约

capability、driver、OCR 与 script worker 都是长驻或按步启动的子进程，stdout 仅承载协议帧，诊断只写 stderr。Host 必须持续 drain 两条管道、限制单帧和总输出、检测重复/未知 request ID、处理半帧与非法 JSON，并对崩溃实施熔断，而不是无限重启。

- Capability worker：对应用业务 API、浏览器 DOM、文件/数据转换等窄能力做进程外适配，只能调用 manifest 声明且经 policy 授权的 action。
- Driver worker：窗口枚举、snapshot、语义动作、输入与截图；不负责 workflow 控制流。
- OCR worker：输入由 Host 获取且带 frame/region provenance 的图像，输出文本、bounds、language、confidence；不产生或执行点击。
- Script worker：v0 默认禁用；即使显式 `--allow-scripts`，在强 OS sandbox 尚未实现时也 fail-closed。目标实现仅从 stdin 接收结构化输入，stdout 返回一个受大小限制的结构化值，并强制文件、网络、环境变量和 secret 的 capability 控制。

### 6.2 平台拆分

| Driver | 首选接口 | 明确边界 |
|---|---|---|
| Windows | UIA/Win32，必要时补 IA2/JAB；`SendInput` 后备 | UIPI、管理员窗口、Session 0、UAC/锁屏；正式取消依赖 Job Object |
| macOS | 签名稳定的 Swift/ObjC AX helper；CGEvent 后备 | Accessibility 与 Screen Recording 分权；bundle identity/TCC 必须稳定 |
| Linux | AT-SPI2；X11 XTEST 或 Wayland portal + libei | 首版只资格验证 Ubuntu GNOME；X11、Wayland、compositor 能力分别声明 |

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

## 8. Deadline、取消、重试与效果不确定性

所有 timeout 在进入 run 时转换为单调时钟上的绝对 deadline，并按父子关系只缩短不延长：

```text
workflow deadline
  └─ step deadline
      └─ attempt / plugin RPC / OS call deadline
```

墙钟时间只用于审计展示；调度判断使用 monotonic clock，wire 中的 epoch deadline 由发送端在边界处换算并保留剩余预算。取消顺序为：停止调度和 retry timer → 发协议 cancel（协议支持后）→ 关闭能力与 stdin → 宽限期 → 终止进程树 → 释放 writer lease → 尽可能重新 observe/reconcile。

POSIX worker 使用独立 session/process group，先 `SIGTERM`、宽限期后 `SIGKILL`。当前纯标准库 Windows v0 只能创建新 process group 后对直接进程 terminate/kill，**不能保证终止任意 descendant tree**；正式桌面写能力上线前必须由 Job Object 或等价 supervisor 补齐。取消 Python future 或杀掉父 PID 都不足以证明工作已停止。

retry 需要同时满足：错误 `retryable=true`、step 被声明为 `read_only` 或 `idempotent`、deadline 尚有预算。非幂等的 click/invoke/send/delete/pay/install 在 dispatch 后超时、worker EOF 或 driver 崩溃时返回 `UNKNOWN_EFFECT`，禁止自动重放；Host 只能在重新观察并证明未生效后，由明确策略决定后续动作。

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

## 10. Secrets、Policy 与审计

- Descriptor、plan、event 和 IPC 中只出现 `secret_ref`；Host 在最后使用点向系统凭据库解引用，默认不通过命令行、环境变量、日志或 planner context 传递明文。
- Policy 绑定可信应用身份（签名、bundle ID、executable）、用户/session、capability、数据范围、effect class 与过期时间，不能只相信窗口标题。
- 发送、删除、付款、授权、安装、跨应用粘贴敏感数据等高风险动作需要绑定“目标 + 参数摘要 + 计划 revision”的一次性确认 token；UI 变化后 token 失效。
- 审计记录 descriptor/plan digest、actor、worker build/hash、capability、deadline、locator、候选决策、确认、dispatch/accepted/terminal、postcondition、状态和错误；secret 明文、受保护文本和未经许可的截图不入日志。
- 截图默认本地、短 TTL、区域最小化；上传或长期保留需独立授权。审计存储需要访问控制、完整性校验、保留策略和可追踪删除。

## 11. Rust 迁移约束

满足以下条件前不迁移核心：descriptor/plan schema 已有版本策略；NDJSON v0 契约测试稳定；真实 driver 给出了性能与可靠性瓶颈证据；取消、错误、effect 与 journal 语义已经冻结；Python 与 Rust 可运行同一批 conformance fixtures。

迁移顺序优先为协议类型/错误模型 → supervisor 与进程树治理 → deadline/scheduler/single-writer → policy/secret/audit。OCR/ML 和快速生态插件可以长期留在 Python worker。迁移期间由同一计划摘要、测试向量和 wire contract 保证行为等价，不做一次性全量重写。
