# v0 跨平台纵向切片交付审计

> 审计日期：2026-08-25。本文把“代码已实现”“契约/fixture 已验证”和“真实平台已资格验证”分开记录。后两者不能互相替代。

## 1. 交付目标与判定标准

本阶段要求交付以下可审计能力：

1. Windows、macOS、Linux 各有独立的原生 accessibility driver，并共享版本化进程协议。
2. 描述文件支持条件、循环、DAG、只读并发、脚本、重试、超时、错误处理和 `finally`。
3. OCR 只能显式调用；允许“识别指定内容后再决定响应”，不得由 locator 或语义动作失败隐式触发。
4. 语义接口优先；键盘输入只作为显式 `type_text` 能力，并保留 protected、焦点、会话和 `UNKNOWN_EFFECT` 边界。
5. 运行状态和事件持久化；仅在证明安全的边界恢复，不重放效果不明的步骤。
6. Linux 以本机 KDE Plasma/X11 为首个真实目标；Windows 不使用 Wine，原生 CI 保持手动；macOS 提供可搬运、可回传的真机套件。
7. 用户文档为中文；每个实现阶段经过测试、独立审查和原子提交。

## 2. Prompt 到产物检查表

| 要求 | 代码/文档/命令证据 | 当前结论 |
| --- | --- | --- |
| 三端语义驱动 | `plugins/windows_uia`、`plugins/macos_ax`、`plugins/linux_atspi`；三份架构文档 | 实现完成；资格按平台分别计算 |
| 程序化判断与有界控制流 | workflow schema、compiler、runtime；`if/switch/foreach/while/block/set` 测试 | 已验证 |
| DAG、并发与重试 | sibling DAG 编译和 runtime；仅并发经 manifest 证明的非桌面 read-only action | 已验证 |
| 脚本执行 | `script.py`；Linux 仅在 bubblewrap + prlimit 可用时启用，其他平台失败关闭 | v0 边界内已验证 |
| 超时、错误和清理 | 绝对 deadline、父子 deadline、结构化 `AutomationError`、`on_error/finally`、`UNKNOWN_EFFECT` | 已验证 |
| 可恢复 run 与日志 | SQLite WAL journal、owner lease、`DurableExecutor`、JSON-only `start/resume/status/list/events/pause/cancel` | 受限 v0 已验证 |
| 安全恢复 | 只从 `between_top_level_steps` 恢复；`in_top_level_step/finalizing` 零重放并终结为 `unknown_effect` | 已验证 |
| durable 敏感边界 | v0 拒绝 action/script 和声明 sensitive 的输入/输出，避免未跟踪内容进入 checkpoint | 已验证；通用 action reconciliation 待后续 |
| 显式 OCR | `vision.ocr.recognize@1`、JSON/YAML 示例、字面匹配和置信度分支；无截图/点击 | 已验证 |
| OCR 资源边界 | 64 MiB、20k 单边、40 MP、单帧、Pillow 完整解码；Linux Tesseract prlimit | 已验证 |
| Windows 显式输入 | UIA fresh resolve + protected/focus/前台 HWND/PID + 分批 Unicode SendInput | 契约和原生 fixture 已就绪；真实 Windows 结果待手动 CI |
| macOS 显式输入 | AX fresh resolve + Secure Event Input + focus/frontmost + CGEvent progress marker | 契约和真机套件已就绪；真实 Mac 结果待回传 |
| Linux 显式输入 | AT-SPI fresh resolve + PID/focus + 固定路径 XTest helper；fresh snapshot 验证 | GTK3/Qt5 在本机 KDE/X11 和私有 Xvfb 均通过 |
| KDE 真实应用矩阵 | `tests/linux/kde_app_qualifier.py` 只启动自有进程、按精确 PID 抓取聚合树；`docs/testing/kde-x11-qualification.md` 记录证据 | Konsole 与 System Settings 只读观察通过；未执行写动作 |
| Windows 测试门禁 | `.github/workflows/ci.yml` 仅在 `workflow_dispatch` 且 `run_windows_native=true` 时运行 Windows job | 已验证；本轮未触发 |
| 虚拟机可行性 | `docs/testing/virtual-machine-capability.md` | 当前主机无 `/dev/kvm`/嵌套虚拟化；Windows 用远端 runner，macOS 用 Apple 硬件 |
| Mac 回传包 | `tests/macos/package-source.sh` 与 `run.sh`；结果包含 report、identity、SHA256 和隐私说明 | 已生成并交付；等待真实结果 |
| 中文文档 | 根 README、research/spec/architecture/testing/plan 与插件 README | 已审校，技术标识保留英文 |
| 原子提交 | 见下表 | 已满足 |

## 3. 本阶段提交

| Commit | 内容 |
| --- | --- |
| `19fe284` | 显式 OCR 流程与安全加固 |
| `f797e76` | 顶层安全检查点和 durable resume |
| `4d10eab` | Windows 显式 Unicode 文本输入 |
| `8e7d90e` | macOS 显式 Unicode 文本输入与真机套件 |
| `78e398a` | Linux KDE/X11 显式 XTest 文本输入 |
| `3098015` | 当前能力、测试边界与路线图同步 |
| `9875ceb` | Konsole 与 System Settings 真实只读 AT-SPI 资格矩阵 |
| `4f57e5d` | 本交付审计记录 |

每个提交均包含且仅包含一条 `Co-authored-by: TRAE CLI <noreply@bytedance.com>` trailer。后续文档修订的 commit ID 以 `git log` 为准。

## 4. 验证记录

- 全量 Python 测试：363 项通过，8 项因平台或当前会话条件跳过。
- Linux 本机：Debian 12、KDE Plasma 5.27.5、Qt 5.15.8、X11 `DISPLAY=:10.0`。
- Linux 自有 fixture：GTK3 和 Qt5 的 snapshot/find/focus/语义写动作通过；显式 XTest UTF-8 输入后由 fresh AT-SPI snapshot 观察通过。
- Linux 隔离环境：私有 Xvfb + 私有 session/AT-SPI bus 的 GTK3/Qt5 输入用例通过。
- Linux 真实应用只读矩阵：Konsole 22.12.3 的 352 个节点、System Settings 5.27.5 的 256 个节点均通过精确 PID 选择和未截断快照；写动作派发数为零，详细聚合指标见 `docs/testing/kde-x11-qualification.md`。
- Windows：跨平台契约与 ctypes ABI 测试通过，Windows-only fixture 在非 Windows 主机跳过；远端 CI 未触发。
- macOS：driver/testkit 静态与协议测试通过，源码包可复现；当前 Linux 主机无法编译或执行 Apple framework。
- `python -m compileall -q src plugins tests`、`git diff --check` 和 XTest helper 本机构建通过。

## 5. 外部门禁与未完成资格

当前代码纵向切片可交付，但不能宣称三端产品级资格已完成：

- Windows：等待用户允许后手动触发 `workflow_dispatch(run_windows_native=true)`，还需记录 runner、系统版本、commit SHA、fixture 结果和 UIPI/secure desktop 边界。
- macOS：等待真实 Mac 回传 `macos-ax-test-result.tar.gz`，必须核验 `report.json` 为 `passed`，并检查 identity、架构与 SHA-256。
- Linux：自有 GTK3/Qt5 已通过；Konsole 与 System Settings 已完成初始窗口的只读矩阵，写动作、Dolphin、更多 QML 页面、多窗口和动态页面仍待独立资格验证。

## 6. 下一阶段

1. 收集 Mac 与 Windows 的真实平台证据。
2. 建立真实 KDE 应用只读资格矩阵并记录 `supported/unsupported/failed`，不把未注册 AT-SPI 记作通过。
3. 为 durable action 增加字段级 checkpoint projection/redaction、显式 reconciliation contract 与 single-desktop-writer 后，再解除 v0 对 action/script 的保守拒绝。
4. 增加用户介入检测、确认 token、secret store、签名/发布链和真实应用 SLO；这些均不属于当前纵向切片已完成能力。
