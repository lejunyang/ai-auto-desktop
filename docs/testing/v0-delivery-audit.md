# v0 跨平台纵向切片交付审计

> 审计日期：2026-08-25。本文把“代码已实现”“契约/fixture 已验证”和“真实平台已资格验证”分开记录。后两者不能互相替代。

## 1. 交付目标与判定标准

本阶段要求交付以下可审计能力：

1. Windows、macOS、Linux 各有独立的原生 accessibility driver，并共享版本化进程协议。
2. 描述文件支持条件、循环、DAG、只读并发、脚本、重试、超时、错误处理和 `finally`。
3. OCR 只能显式调用；允许“识别指定内容后再决定响应”，不得由 locator 或语义动作失败隐式触发。
4. 语义接口优先；键盘和鼠标输入只作为显式 `type_text` / `pointer_click` 能力，并保留 protected、焦点、会话、hit-test 和 `UNKNOWN_EFFECT` 边界。
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
| 可恢复 run 与日志 | SQLite WAL journal、owner lease、`DurableExecutor`、JSON-only `start/resume/status/list/events/pause/cancel`；action 默认 deny，CLI 可显式选择 `--durable-actions read-only` | 受限 v0 已验证 |
| 安全恢复 | 普通计划从 `between_top_level_steps` 恢复；合法 `action_intent` v2 可在重新校验绑定后重放只读 action；其他 `in_top_level_step/finalizing` 零重放并终结为 `unknown_effect` | 已验证 |
| durable 只读 action 边界 | 仅顶层隐式串行、无 `if`/pre/post/retry/handler/finally 的单次 `read_only` action；provider 与 descriptor input/output/error 均须为 `public`，provider 声明稳定 `checkpoint_fields`，descriptor 显式 `project|omit` | 已验证；写 action 与复杂控制流 reconciliation 待后续 |
| durable 最小持久化 | run 创建前验证 manifest/effect/errors/sensitivity/projection/static policy，但不预求值引用前序 step 的动态输入；dispatch 前持久化不含原始输入的 intent v2，完成后只保存批准的投影，原始 provider 响应不落 journal | 已验证 |
| durable 控制竞态 | pause/cancel 请求与 action intent、dispatch 授权、完成 checkpoint、终态提交均以 `desiredState` CAS 协调 | 已验证；不会丢失控制意图或重复派发已完成读操作 |
| durable 敏感边界 | v0 继续拒绝 script、写 action、敏感 workflow 输入/输出，以及 provider 或 descriptor 标记敏感的 action 输入/输出/错误 | 已验证；完整 taint tracking 待后续 |
| 显式 OCR | `vision.ocr.recognize@1`、JSON/YAML 示例、字面匹配和置信度分支；无截图/点击 | 已验证 |
| OCR 资源边界 | 64 MiB、20k 单边、40 MP、单帧、Pillow 完整解码；Linux Tesseract prlimit | 已验证 |
| Windows 显式输入 | UIA fresh resolve + protected/focus/前台 HWND/PID + 分批 Unicode SendInput | 契约和原生 fixture 已就绪；真实 Windows 结果待手动 CI |
| macOS 显式输入 | AX fresh resolve + Secure Event Input + focus/frontmost + CGEvent progress marker | 契约和真机套件已就绪；真实 Mac 结果待回传 |
| Linux 显式输入 | AT-SPI fresh resolve + PID/focus + 固定路径 XTest helper；fresh snapshot 验证 | GTK3/Qt5 在本机 KDE/X11 和私有 Xvfb 均通过 |
| 三端显式鼠标 | `pointer_click@1` 只接受 fresh semantic target + locator，固定中心点左键；Windows/macOS 做原生 element-at-point exact hit-test，Linux 要求 AT-SPI 命中目标子树并叠加 X11 焦点/点下窗口 PID | 三端契约已验证；Linux GTK3/Qt5 私有 Xvfb 真动作通过，Windows/macOS 真机证据待回传 |
| Linux capability probe | AT-SPI bus、根窗口有界 `xprop` X11 round-trip、portal/libei/uinput/Wayland 分项报告 | 本机 `available=3/degraded=1/unavailable=2/unknown=0`；X11 误阴性已修复 |
| KDE/QML 应用矩阵 | `tests/linux/kde_app_qualifier.py` 只启动自有进程、按精确 PID 抓取聚合树；专用 runner 仅对自有 QML fixture 调用 exact `Press` 并 fresh snapshot | Dolphin、Konsole、System Settings 与自有 Qt Quick/QML fixture 只读观察通过；QML 自有 fixture 语义 invoke 通过，真实应用未执行写动作 |
| Windows 测试门禁与证据 | `.github/workflows/ci.yml` 仅在 `workflow_dispatch` 且 `run_windows_native=true` 时运行完整测试；`tests/windows/run-native-fixture.ps1` 生成 JSON，失败时也上传并保留 30 天 | 静态契约已验证；本轮未触发，无 Windows 真机结论 |
| 虚拟机可行性 | `docs/testing/virtual-machine-capability.md` | 当前主机无 `/dev/kvm`/嵌套虚拟化；Windows 用远端 runner，macOS 用 Apple 硬件 |
| Mac 回传包 | `tests/macos/package-source.sh` 与 `run.sh`；结果包含 report、identity、SHA256 和隐私说明 | 源码包与回传格式已就绪；等待真实结果 |
| Mac 回传验真 | `tests/macos/verify-result.sh` 有界、内存解析归档，校验成员、hash、报告、identity 与规范化元数据，并区分自洽、报告通过、来源受信和最终资格 | 逻辑已验证；尚无真实 Mac 归档，不能宣称平台通过 |
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
| `1d6e7d2` | Konsole 与 System Settings 真实只读 AT-SPI 资格矩阵 |
| `4f57e5d` | 本交付审计记录 |
| `b778a7e` | Windows 原生测试 JSON 证据产物与失败留存 |
| `7421b79` | Windows 手动 job 运行完整测试套件并修正门禁 |
| `9a305a2` | macOS 回传归档本地验真与信任分层 |
| `bbf3c83` | durable 只读 action 的安全恢复与一次派发语义 |
| `414c0a5` | 本机 KDE/X11 初始资格验证 |
| `713c483` | 可搬运的 macOS 原生测试套件 |
| `f5c5a02` | durable 只读恢复边界的中文文档 |
| `3c883d9` | Dolphin、Qt Quick/QML 矩阵与原生语义动作 |
| `8258005` | KDE 资格报告的独立、失败关闭验真门禁 |
| `4bff73c` | Linux durable session 可用性校验 |
| `8f7b3a9` | durable lease heartbeat 与 finalization v3 恢复窗口加固 |
| `39990b6` | macOS 结果证据绑定源码 revision 与 package digest |
| `bf601c0` | heartbeat 启动异常清理与 lease-loss 错误分流 |
| `61e1376` | 修复共享 Linux 主机的真实 Tesseract 线程限制 |
| `73c4017` | 中文化剩余示例并门禁全部 tracked workflow |
| `189dab3` | 收敛 script/set schema 到当前可执行 v0 子集 |
| `4622e71` | Windows 显式中心点左键 `pointer_click` |
| `6d486c9` | Linux KDE/X11 显式中心点左键 `pointer_click` |
| `1e790b8` | macOS 显式中心点左键 `pointer_click` |
| `4e6df46` | macOS pointer 真机回传与严格验真 |
| `a2933c4` | 同步三端显式 pointer 边界与脚本实机沙箱证据 |
| `90aefc7` | 修正 macOS protected pointer helper 拒绝位置 |
| `fe12424` | 明确 macOS pointer 真机包的源码白名单归属 |
| `6f6dad6` | 记录三端显式 pointer 的最终资格边界 |
| `90c5c68` | 扩大自有 Qt5 fixture 的有界快照余量，消除主题/辅助功能配置导致的偶发截断 |

本表所列提交均包含且仅包含一条
`Co-authored-by: TRAE CLI <noreply@bytedance.com>` trailer。后续文档修订的 commit ID
以 `git log` 为准；这项陈述不追溯覆盖仓库早期未列入本表的历史提交。

## 4. 验证记录

- 实现冻结 revision `90c5c68` 的全量 Python 回归：`PYTHONPATH=src PYTHONDONTWRITEBYTECODE=1 python -m unittest discover -s tests -q`，共 534 项，通过，跳过 11 项非当前平台/会话用例，耗时 109.069 秒；审计文档 revision `7528df8` 使用同一命令再次运行，共 534 项通过、跳过 11 项，耗时 109.235 秒。两次实测都覆盖三端 pointer、严格 Mac 回传证据，以及 Qt5 fixture 的有界快照稳定性修复。具体对外交付 revision、源码内容摘要和源码归档摘要以每次从 clean worktree 生成测试包时的脚本输出及随包交付消息为准，不把会继续前移的 `HEAD` 写成固定事实。
- durable 定向回归：read-only、finalization、journal 与 run service 共 93 项通过；独立复审未发现 P0/P1。heartbeat 启动握手失败后线程停止、lease 不再续期且到期可接管；`0.001/0.005/0.01/0.02/0.05s` 极短 TTL 均稳定返回可重试 `RUN.LEASE_LOST`，不会误报状态冲突/存储故障，也没有 provider 重复派发或 token 落盘。
- Linux 本机：Debian 12、KDE Plasma 5.27.5、Qt 5.15.8、X11 `DISPLAY=:10.0`。
- Linux 自有 fixture：GTK3 和 Qt5 的 snapshot/find/focus/语义写动作通过；显式 XTest UTF-8 输入后由 fresh AT-SPI snapshot 观察通过。
- Linux 隔离环境：私有 Xvfb + 私有 session/AT-SPI bus 的 GTK3/Qt5 输入用例通过。
- Linux capability probe：同一 KDE/X11 会话中 `linux.at_spi`、`linux.x11` 和 `linux.remote_desktop_portal` 为 `available`；`linux.uinput=degraded`，Wayland/libei 不可用。X11 查询使用单个根窗口属性，避免完整 `xdpyinfo` 输出超过通用子进程上限。
- Linux 应用只读矩阵：Dolphin 22.12.3（358 节点）、Konsole 22.12.3（352 节点）、System Settings 5.27.5（256 节点）与自有 Qt Quick/QML fixture（5 节点）均通过精确 PID 选择和未截断快照；写动作派发数为零，Dolphin 仅打开临时空目录，详细聚合指标见 `docs/testing/kde-x11-qualification.md`。
- Windows：跨平台契约、ctypes ABI 和 CI artifact 静态契约测试通过；Windows-only fixture 在非 Windows 主机跳过，远端 CI 未触发。
- macOS：driver/testkit 静态与协议测试通过，源码包可复现并绑定 clean Git revision、manifest digest 与结果归档 hash；真机套件现将 `pointer_click_and_reread` 设为 passed 报告必需项，并严格校验 fresh target、正面积 bounds、PID/frontmost、AX hit-test、显式左键中心点和 fresh postcondition 证据。该结果只验证 Linux 上的验真逻辑，当前 Linux 主机无法编译或执行 Apple framework；最终源码包的具体 revision 和三类 SHA-256 记录在随包交付消息中。
- `python -m compileall -q src plugins tests`、`git diff --check` 和 XTest helper 本机构建通过。
- Linux 显式 pointer：私有 Xvfb + 隔离 AT-SPI bus 上，自有 GTK3 和 Qt5 fixture 均通过语义节点中心点 XTEST 左键点击，并由 fresh AT-SPI snapshot 观察独立状态变化；driver 还要求 AT-SPI element-at-point 命中目标子树，X11 helper 再核对焦点与点下窗口 PID。
- OCR 实机：本机安装 Tesseract 5.3.0 与 `eng`/`chi_sim`，真实生成中英图片后，provider 识别 `状态 READY 2026`，workflow 对字面目标 `READY` 返回 `decision=respond`。共享 UID 下固定 `RLIMIT_NPROC` 导致的误杀已改为 OpenMP 单线程约束，同时保留地址空间、CPU、文件大小、fd、deadline、输出与进程组边界。
- 最终独立只读审查未发现 P0/P1。审查指出自有 Qt5 fixture 在部分主机上可能因主题或输入法暴露额外 AT-SPI 子节点而触发 64 节点截断；`90c5c68` 将该测试专用上限提高到仍远低于 driver 5000 节点硬上限的 256，并重新实跑 GTK3/Qt5 隔离测试和全量回归。

## 5. 外部门禁与未完成资格

当前代码纵向切片可交付，但不能宣称三端产品级资格已完成：

- Windows：等待用户允许后手动触发 `workflow_dispatch(run_windows_native=true)`，还需记录 runner、系统版本、commit SHA、fixture 结果和 UIPI/secure desktop 边界。
- macOS：等待真实 Mac 回传 `macos-ax-test-result.tar.gz`；必须通过仓库验真器，并仅在 `archive_valid=true`、`report_passed=true`、`trusted_archive=true`、`source_trusted=true`、`qualified=true` 时记为通过。预期结果归档 SHA-256 必须来自与回传归档独立的可信渠道。
- Linux：自有 GTK3/Qt5 语义动作、XTest 文本输入与显式中心点左键 pointer click 已通过；Dolphin、Konsole、System Settings 和自有 Qt Quick/QML fixture 已完成初始窗口只读矩阵。真实应用写动作、第三方 QML 页面、多窗口和动态页面仍待独立资格验证。

## 6. 下一阶段

1. 收集 Mac 与 Windows 的真实平台证据。
2. 把现有 KDE/QML 只读矩阵扩展至第三方 QML 页面、多窗口与动态页面，并另行验证受控写动作；继续记录 `supported/unsupported/error`，不把未注册 AT-SPI 记作通过。
3. 在现有 durable read-only projection 与 `action_intent` v2 基础上，继续设计写 action 的显式 reconciliation、敏感值传播与 single-desktop-writer；这些能力完成前，不解除对写 action、script、敏感数据及复杂 action 控制流的保守拒绝。
4. 增加用户介入检测、确认 token、secret store、签名/发布链和真实应用 SLO；这些均不属于当前纵向切片已完成能力。
