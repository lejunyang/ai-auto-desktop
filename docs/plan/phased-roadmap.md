# Python 优先、Rust 就绪的分阶段路线图

> 基线日期：2026-08-24。路线图以当前 Python v0 纵向切片为起点，不把 fixture、mock OCR 或研究结论记作真实平台能力。里程碑按退出门槛推进，不按日期自动宣告完成。

## 1. 交付原则

- 每阶段都交付可运行、可故障注入、可审计的纵向切片；安全、取消和 `UNKNOWN_EFFECT` 不是发布后的补丁。
- Accessibility-first 的确定性路径先于像素路径；OCR 只作为 descriptor 中显式的 perception step。
- Windows、macOS、Linux 共用 contract suite，但平台资格分别统计和发布。
- 一个 desktop session 只有一个 writer；规划器、OCR 和 script worker 无权绕开 Trusted Host。
- 只有证据证明 Python 常驻核心成为瓶颈、且语义已稳定后，才进入 Rust 迁移。

## 2. 当前基线：Python v0

当前 v0 已形成可执行的运行语义原型，并已加入显式 Tesseract OCR provider 与只读三端能力探针：

- JSON/YAML 归一到 `apiVersion: ai-auto-desktop.dev/v1alpha1`、`kind: Workflow` 与 `metadata.name`，再严格编译和冻结；step ID 在全部分支、handler、`finally` 中全局唯一。
- 只读白名单 AST 表达式，不使用 `eval/exec`，禁止所有函数和方法调用。
- `action/set/block/fail/return/script`、`if/switch/foreach/while`、声明式 error handler 与 `finally`；循环有显式上限。
- 结构化 `AutomationError`；当前 run 状态是 `succeeded/failed/timed_out/unknown_effect`，`cancelled/skipped` 仍需在 M0 补齐。
- stdio NDJSON 长驻 process plugin 和确定性 fixture，用于成功、retryable error、永久失败、睡眠、mock OCR 与 mock desktop invoke；另有真实 Tesseract process provider，只接受调用方显式提供的图片。
- `probe` 命令只读检查 Windows UIA、macOS Accessibility/Screen Capture 和 Linux AT-SPI/X11/Wayland/portal/libei/uinput 前置条件，不请求权限、不截图、不注入输入。
- script 默认拒绝；`--allow-scripts` 显式启用后仅在具备 bubblewrap + `prlimit` 的 Linux 上运行，其他平台返回 `SCRIPT.SANDBOX_UNAVAILABLE`。

尚未实现或尚未证明：Windows UIA 真机执行结果、Linux KDE/Qt 应用资格、macOS AX driver；可靠的 Windows 后代进程树终止；完整 wire 协议版本协商；single-writer session manager；持久 journal；系统 secret store；签名插件；真实截图；taint tracking、确认 token 与完整 policy enforcement；安装器和权限引导。当前已有 Windows Win32 fixture 和真实 Windows CI 测试入口，也已有 Linux KDE/X11 AT-SPI driver；本机默认 Gio/D-Bus 后端通过 registry、进程协议和只读 snapshot smoke，另用显式测试 typelib overlay 让自有 GTK3 fixture 验证 focus 的原生接受结果，以及 set_text/invoke 的动作后重新观察。当前 OCR 已能处理显式图片，但尚未接入受控截图/frame provenance，因此不能视为桌面视觉闭环。M0 已具备 manifest/action contract、`postcondition.observe` 和基础 action risk policy 校验，其任务是继续把 v0 收敛成可验证基线，而不是扩大产品宣称。

## 3. M0：冻结 Python 运行时 v0 合约

### 范围

- 固化 canonical descriptor identity `ai-auto-desktop.dev/v1alpha1 / Workflow`；YAML 仅作为输入适配，编译结果与 JSON 一致。
- 完成 parse/schema/semantic validation，未知字段失败关闭，编译为不可变 plan。
- 完成受限表达式、`if/switch/foreach/while`、`on_error/finally` 和全局预算。
- 定义绝对 deadline、父子取消、retry eligibility 和 `SUCCEEDED/FAILED/TIMED_OUT/CANCELLED/UNKNOWN_EFFECT/SKIPPED`。
- 固化 NDJSON v0：manifest、invoke、request ID、绝对 `deadline_ms`、structured result/error；stdout 只输出协议/CLI JSON，日志走 stderr。
- capability/driver/script 都遵守进程外 worker 边界；process plugin 支持长驻复用、输出上限、非法帧、EOF、超时、崩溃和 transient error 故障注入。
- 建立 CLI `validate` / `run` 与 Python API 的同一错误模型。
- 写明 OCR 只能作为显式 step；用 fixture 测试 `ocr → condition/switch → desktop action`，并证明普通 resolve 失败不会自动触发 OCR。

### 退出门槛

- 全部 compiler/expression/runtime/plugin/CLI 测试通过，且在至少 Windows、macOS、Linux CI 上跑 process contract。
- 两个同名候选时不 dispatch；非幂等请求只 dispatch 一次，dispatch 后 timeout/EOF 得到 `UNKNOWN_EFFECT`。
- retry 只发生在 `retryable=true` 且 read-only/idempotent 的步骤；attempt、退避和总 deadline 均有上限。
- `finally` 在成功、失败、超时、取消路径运行，cleanup 错误进入 `suppressed`，不覆盖根因。
- 文档、样例和测试明确标注所有桌面/OCR 结果均为 fixture，不是 OS 支持证明。

### 非目标

真实桌面操作、GUI、云 planner、任意脚本沙箱、第三方插件市场、对外稳定 v1 schema。

## 4. M1：平台能力探针与 Windows UIA 纵向打通

### 范围

先为三端制作只读 capability/permission probe，再把 Windows 作为第一个真实 driver：

- 公共 `list_windows`、`snapshot_tree`、`find`、`focus`、`invoke`、`set_value`、`select`、`toggle`、`scroll` contract。
- Windows UIA/Win32 独立 worker；标准化 node、snapshot revision、bounds/坐标变换、supported actions 与 native provenance。
- dispatch 前重新 resolve，完整执行 `observe → resolve → precondition → policy → execute → re-observe → postcondition`。
- Windows Job Object、kill-on-close、driver watchdog、crash restart generation 和 stale node 拒绝。
- macOS probe 验证 AX trust/TCC；Linux probe 分别记录 AT-SPI、X11、Wayland portal/libei 可用性。Windows 已有 UIA 进程驱动和 Win32 fixture，覆盖窗口枚举、树快照、精确定位、focus/invoke/set_value，以及 Runtime 的动作后重新观察；仍需在真实 Windows runner 上产出执行结果和权限边界资格证据。
- 建立 15–30 个目标应用/页面的 ground truth 和 element recall、semantic completeness、action coverage、latency、hang/crash 指标。

### 退出门槛

- Windows fixture app 与至少一组真实系统/标准应用通过同一 contract suite；歧义、刷新、提权和 secure desktop 边界均有负向测试。
- driver 卡死不会卡死 Host，deadline 后整个 worker 进程树被回收，writer lease 可恢复。
- 真实动作必须由 postcondition 验证；无法判断 effect 的路径准确返回 `UNKNOWN_EFFECT`。
- 形成带环境版本的 macOS/Linux probe 报告，据证据决定 M2 的支持矩阵，不用“Linux/macOS 已支持”概括只读探针。

### 非目标

任意 Windows 应用、无人值守 secure desktop、OCR、视觉自动 fallback、同时控制多个物理桌面。

## 5. M2：macOS / Linux 驱动、OCR 与脚本隔离

### 范围

- macOS：签名稳定的 Swift/ObjC AX helper，分离 Accessibility 与 Screen Recording 权限；验证 AX action、可写属性、通知与 CGEvent 后备。
- Linux：先限定当前已验证基线 KDE Plasma 5.27/X11；继续补齐 AT-SPI typelib 与自有 Qt fixture 的真实写动作测试。GNOME、X11 输入后备与 Wayland portal/libei 均作为不同 capability/profile，必须独立资格验证。
- 三端加入 single-desktop-writer session manager、用户介入检测、多显示器/DPI/坐标 provenance。
- screenshot 由 Host/driver 获取并受 policy/audit 控制；OCR 独立进程仅接受显式 frame/region，输出带 confidence 和 bounds 的 perception layer。
- 固定回退顺序并实现显式 gate：native/app API → accessibility → keyboard → semantic bounds pointer → declared vision → declared OCR。
- script worker 默认关闭；启用时使用独立工作目录、最小环境、输入/输出/CPU/内存/文件/网络限制和整棵进程树取消。无法提供所需隔离的平台拒绝不可信 script。
- 增加 SQLite 或等价 journal，支持 crash 后 reconciliation；dispatch 后状态不明的写步骤默认 `UNKNOWN_EFFECT`。

### 退出门槛

- 三端各有真实 fixture app，运行同一组语义场景；每个 OS/桌面环境的通过率独立报告。
- OCR golden 覆盖中英文、DPI、裁剪、低置信度和多候选；测试证明 OCR 不自行获取整屏权限、不直接 dispatch、不会被隐式调用。
- script timeout/cancel 后没有残留孙进程；stdout 污染、超大输出、非零退出、资源超限均产生稳定错误。
- 用户输入介入会暂停 writer；两个 workflow 无法同时取得同一 desktop lease。
- secret 明文不进入 descriptor、IPC、命令行、环境、journal 或错误；高风险动作确认 token 绑定目标与参数。

### 非目标

所有 Linux 发行版/compositor、游戏/Canvas/VDI 的高可靠覆盖、无确认付款/删除/发送、开放任意原生插件。

## 6. M3：产品化、打包与运营安全

### 范围

- 版本锁定、SBOM、依赖/模型许可证、artifact hash/signature、可回滚更新通道。
- Windows x64 首发，按证据增加 ARM64；签名安装器、权限/提权诊断、Job Object 回归和杀软兼容测试。
- macOS `.app` 内嵌固定 bundle ID 的 helper，完成 hardened runtime、codesign、notarization、staple 与升级后 TCC 回归。
- Linux 先发布经资格验证的 KDE Plasma/X11 `.deb` 与诊断包；GNOME 与 Wayland 达到各自门槛后再增加对应 capability/profile，不宣称一包覆盖任意发行版。
- Python 使用锁定 wheelhouse/哈希与可复现构建，优先 onedir/内嵌 runtime；原生 helper、OCR 模型和浏览器依赖分层打包。
- 系统 secret store、确认 UI、policy 管理、审计完整性/保留/导出、截图 TTL 与隐私擦除。
- 崩溃报告和 telemetry 默认脱敏、可关闭；建立真实应用 qualification matrix、SLO 和回归实验室。

### 退出门槛

- 每个平台在对应真实 runner/VM 完成安装、升级、回滚、卸载、权限授权与最小真实动作 smoke；不能只以交叉编译成功验收。
- 更新和插件包签名校验失败时拒绝安装，并能恢复上一可用版本。
- 断电/Host crash/driver crash 后 journal 能区分可重放 read-only、可 reconcile idempotent 与 `UNKNOWN_EFFECT` 写步骤。
- 发布说明列出经过资格验证的应用、OS 版本、桌面环境、权限和已知边界；未知应用明确标为 best effort。
- 完成安全评审：prompt injection、plugin supply chain、secret exfiltration、symlink/path traversal、IPC spoofing、日志泄漏与 DoS。

### 非目标

用“单文件”掩盖模型/helper/native dependency，或在没有资格测试时承诺任意三端应用。

## 7. M4：按门槛迁移 Rust 可信核心

### 启动门槛

只有以下条件同时满足才开始迁移：

1. descriptor/plan、NDJSON、错误、状态、effect、deadline 和 journal 已有稳定版本与 conformance suite；
2. M1–M3 数据证明 Python Host 在常驻内存、启动/延迟、进程治理、安全加固或分发上存在值得迁移的瓶颈；
3. Python 与 Rust 能消费同一 fixture、golden plan 和协议测试，且有逐组件回滚方案；
4. 团队具备三平台 Rust CI、签名发布和 incident debugging 能力。

### 迁移顺序

1. 生成或手写语言无关 IDL，Rust 实现 plan digest、错误/状态与 NDJSON v0 compatibility adapter。
2. 迁移 worker supervisor、IPC framing、输出限额、process-tree cancellation 和 health/circuit breaker。
3. 迁移 absolute deadline scheduler、bounded control flow、retry gate 和 single-writer lease。
4. 迁移 policy、secret broker、journal/audit；对每步做 shadow/replay 对比。
5. 评估将 Windows/Linux driver 移入 Rust sidecar；macOS 可长期保留签名 Swift helper，Python 可长期承载 OCR/ML 与快速插件。
6. NDJSON 与新 Protobuf/CBOR transport 双栈一段版本窗口，确认兼容后再退役 v0 transport。

### 退出门槛

- Python/Rust 对同一 plan 产生相同的 step 顺序、终态、结构化错误、deadline 传播和 audit digest；差异均有显式版本说明。
- fault suite 覆盖 worker hang/crash/partial frame、dispatch 后 EOF、取消竞态、Host restart 和 secret redaction。
- Rust Host 在真实 qualification matrix 上不降低任务成功率或安全拒绝率，且性能/资源或部署指标达到预设收益。
- 支持组件级回滚；不要求一次性删除 Python，也不把 OCR/ML 重写作为完成条件。

## 8. 跨阶段质量门

每个里程碑都必须持续满足：

- **协议门**：版本协商、未知消息、超大帧、重复 ID、stdout 污染和敏感字段有测试。
- **可靠性门**：所有循环/重试/等待有上限；所有子进程有绝对 deadline 和整树回收；cleanup 不覆盖根因。
- **效果门**：dispatch 前/后边界可观测，非幂等不盲目 retry，`UNKNOWN_EFFECT` 不伪装成失败或成功。
- **安全门**：最小 capability、secret ref、应用身份校验、高风险确认、审计脱敏与插件来源验证。
- **桌面门**：单 writer、用户介入暂停、stale snapshot 拒绝、歧义不执行、动作后验证。
- **感知门**：OCR/视觉均显式、区域最小化、provenance 完整、低置信度拒绝，没有全屏隐式 fallback。
- **诚实度门**：mock、probe、fixture、qualified support 和 best effort 在文档与 CLI 中清楚区分。

## 9. 推荐的里程碑记录格式

每个阶段完成时记录：commit/release、schema/protocol version、验证 OS/应用矩阵、通过与失败用例、关键指标、安全评审结果、已知限制和下一阶段决策。若退出门槛未满足，状态应保持“进行中”或“阻塞”，不能因代码目录已经存在就标记完成。
