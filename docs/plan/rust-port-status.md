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
  （无 intent 机制就无法证明某次派发可安全重复）。规范另定义 `--durable-actions read-only`。
- CLI `start` / `resume` / `status` / `list` / `events` / `pause` / `cancel` 七个命令（见 §2.2）。
- `run_service.py` 的服务层（尚未读）。

### 2.2 CLI 命令差异

**核实方式**：`aad help` 实际输出 vs `cli.py` 的 `add_parser` 调用。

Rust 有：`apps`、`describe`、`snapshot`、`find`、`do`、`probe`、`validate`、`run`、`mcp`、`tools`。
Python 另有：`start`、`resume`、`status`、`pause`、`cancel`、`list`、`events`（均属 §2.1）、
以及 `edit`（属 §2.4 的浏览器编辑器，已由 GUI 取代，不再需要）。

### 2.3 录制规范的完整语义

**Python**：`recording.py`(41KB)、`recording_editor.py`(15KB)
**规范**：`docs/spec/recording-session-v1alpha1.md`
**核实方式**：检索 `redaction|platform_binding|of_step|disambiguation`，**零匹配**。

当前 Rust/GUI 实现的是一个够用的子集：locator + window selector + enabled。规范要求但未实现：

- `redaction`：**默认行为而非可选项**。默认丢弃节点 `value` 只留 `observed.had_value`；
  `states.protected` / 密码类不可解除；`type_text` 默认必须外提为 `inputs` 引用；
  窗口标题默认 `title_policy: drop`；必须声明 `screenshots` 策略。
- `assertion` 步骤：`of_step` + `observe`(仅 `find`/`snapshot`) + `expect.mode`，
  且**不得**编译为独立步骤，必须附加为 `postcondition`。
- `logic` 步骤：人工插入的 `condition`/`loop`/`assign`/`group`/`fail`/`return`/`script`。
- `disambiguation.strategy` 降级链：`unique` → `scoped` → `ordinal`(必须 `fragile: true`)
  → `unresolved`(必须 `enabled:false`)；已 verified 的步骤重新校验后必须能被降级。
- `platform_binding`：产物绑定单一平台（写值 win/mac 是 `set_value`、linux 是 `set_text`）。
- 引用完整性校验：每个 `steps.<id>`、`of_step`、`${{ inputs.X }}` 必须存在且 enabled，
  否则 `RECORDING.ORDER_INVALID`。**现有 workflow 编译器不覆盖这条**。
- 一整套 `RECORDING.*` 错误码。

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

### 2.6 MCP 无法回放工作流

**核实方式**：`aad tools` 输出 9 个工具，无 `run_workflow`；
在 `crates/aad-mcp` 检索 `aad_runtime|run_workflow|validate`，**零匹配**。

AI 目前能一步步观察和操作，但不能执行一个存好的录制或工作流。这是 CLI 与 MCP 之间的能力落差。

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
