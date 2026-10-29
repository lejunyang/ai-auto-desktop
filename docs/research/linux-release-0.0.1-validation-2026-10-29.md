# Linux 0.0.1 全局安装与实测

日期：2026-10-29。环境：veLinux 2 / Debian 12 系、x86_64、glibc 2.36、KDE/X11、Qt 5.15.8、Fcitx5 双拼、Microsoft Edge。工具链由 osdk 0.0.7 管理，Rust 1.98.0。

## 结论与安装状态

GitHub 的 `aad-0.0.1-x86_64-unknown-linux-gnu.tar.gz` 可以被 osdk 下载、解包和全局选择，但原包要求 `GLIBC_2.39`，在本机连 `aad --version` 都无法启动。

本次修复了发布兼容性、Fcitx5 误输入和拖拽快照过期三个问题。最终全局入口仍为 osdk 的 `aad` shim，运行的是基于 `cf20682` 本地重建的 **0.0.1 修复版**，不是原 GitHub release 二进制。安装后的 `aad` 与验证过的 release 构建 SHA256 一致，两个 helper 保持同目录，动态依赖均可解析。

- `aad --version`：`aad 0.0.1`。
- `osdk current github:lejunyang/ai-auto-desktop`：0.0.1，global config。
- 获取实际安装位置：`osdk where github:lejunyang/ai-auto-desktop`。
- 最终归档 SHA256：`7df61cb8b254d7fce4219f15e40ab96948e418cd6f147f1ff5689d6b9a6d77e3`。
- 本地包、osdk catalogue、安装路径和测试日志保存在忽略目录 `artifacts/linux-0.0.1-validation/`。

本地包通过带 SHA256 的 osdk static catalogue 安装，具有独立安装身份，未覆盖原发布包。下载时使用临时回环 HTTP 服务；安装完成后运行 `aad` 不需要该服务。catalogue 记录的是本地重建来源，重新下载该包需重新提供本地归档服务。GitHub release 未替换，修复提交未推送。

另外在用户 `~/.profile` 中补充了 osdk shim 目录的 PATH 去重逻辑。原配置只在交互式 `.bashrc` 中激活 osdk，非交互登录 shell 在仓库外找不到 `aad`；补齐后，从 `/tmp` 新建登录 shell 直接运行 `aad --version` 和 `aad probe` 均通过。没有手改 osdk 的 data/config 文件。

## 修复

| 提交 | 问题与修复 | 验证 |
| --- | --- | --- |
| `e843087` | Linux release 使用 `ubuntu-latest`，引入过高 glibc 依赖。固定为 Ubuntu 22.04，并检查解包后的三个 ELF、执行位、版本和 CLI 帮助。 | 新检查能拒绝原来的 GLIBC_2.39 包；本地重建的 aad、XTest 和 capture helper 均最高要求 GLIBC_2.34，通过 GLIBC_2.35 符号基线检查。 |
| `79c6e3b` | 中文输入法会把 `type-text` 的 `replaced 你好` 改成 `热拍案测定你好`，命令仍报成功。聚焦目标后、发送任何按键前查询 Fcitx5，仅允许直接键盘输入法。 | 双拼开启时拒绝注入，原文本不变；英文模式下 Ctrl+A 后输入得到预期文本；`fill` 在中文输入法下仍可用。 |
| `cf20682` | 拖拽验证起点时刷新快照，随后用旧版本验证终点，合法拖拽必然失败。改为对照同一份原观察和同一份新快照校验两端。 | 真实 Qt 输入框之间拖动选中文字，终点文本独立回读正确；CI fixture 增加同一实效断言。原生身份、语义 fingerprint、两端命中和同节点拒绝仍保留。 |

输入法拒绝的原因是 `DRIVER.INPUT_METHOD_ACTIVE`，包含 `input_dispatched=false`。由于聚焦可能已经发生，外层仍遵守现有 `DRIVER.UNKNOWN_EFFECT` 合约，并在 `cause` 中保留明确原因。检查只覆盖 Fcitx5；没有在 IBus、其他输入法或 Wayland 上宣称等价验证。

## 验证覆盖

| 功能 | 结果与证据 |
| --- | --- |
| 源码回归 | `osdk run test`：772 passed；原本忽略的 Chromium 测试另行显式执行，1 passed；`osdk run plugin-test`：14 passed。 |
| 构建与静态检查 | `cargo build --release --locked -p aad-cli`、格式检查、`osdk run clippy` 通过。 |
| 原生桌面 | 全局入口发现专用测试应用，describe / snapshot / find 正常。fill、focus、Ctrl+A、Unicode 输入、invoke、双击和跨元素拖拽均检查实际 UI 结果。hover 完成派发；没有把它当作可见 hover 样式变化的证明。 |
| 输入边界 | 多义定位、未支持的定位参数、伪造节点和同节点拖拽明确拒绝。Fcitx5 误输入回归在 debug 和 release 上均通过。 |
| MCP 与录制回放 | 真实 stdio 握手、schema 无裸坐标、只读输入拒绝、观察后操作、save/list/describe/run workflow、CLI 读取共享 store、record --save 后回放均通过。输入租约阻止另一个会话抢占资源，关闭原会话后可继续。 |
| 原生截图与 OCR | 最终安装目录中的 capture helper 产出 PNG ArtifactRef，经 OCR 准确识别界面上的 `HELLO AAD 123`。另测显式图片 OCR 和工作流决策成功。 |
| 浏览器录制 | 7 个 Node 测试通过：启动、profile 复用/复制、占用保护、清理、语义变化、网络默认保留与显式脱敏、断连、不相关页面隔离和退出。 |
| MCP 浏览器 | 临时 Edge profile、本地测试站点：二维码截图和 digest 变化、点击跳转、页面数据回读、API 请求与响应正文录制、关闭清理通过。 |
| 浏览器 assist | 默认 WebP、协商 JPEG、帧尺寸/关键帧、两次点击与输入真实生效、错误 token 拒绝通过。 |
| 工作流 | Linux/共享示例校验、普通执行、类型化返回、非法字段拒绝通过。 |
| 持久化 | 不同进程 start/status/list/events、pause/resume/cancel 均通过；内置 Linux inspect_session 以 read-only durable action 执行并持久化 KDE/X11 信息。 |
| Python 沙箱 | 默认拒绝脚本；显式 allow-scripts 后返回预期值，用户 home 不可见，自定义环境变量没有传入沙箱。 |
| 私有桌面 fixture | 完整 `tests/linux/run-rust-atspi-fixture.sh` 通过，包括输入、截图、统一控制、录制和新增拖拽断言。 |

原生截图曾返回 `target_occluded`。通过 X11 窗口 ID 查明遮挡物是 Fcitx5 输入法切换提示窗，而非误判；提示消失后截图成功。没有放宽遮挡检查。

## 可复现的关键回归

发布包校验：

```powershell
./tests/linux/verify-release.ps1 -Archive artifacts/linux-0.0.1-validation/aad-0.0.1-x86_64-unknown-linux-gnu.tar.gz -Version 0.0.1
```

Fcitx5 实效回归（只操作新建的测试窗口，结束时恢复该窗口原输入法）：

```powershell
$flags = (pkg-config --cflags --libs Qt5Widgets).Trim() -split '\s+'
g++ -std=c++17 -O2 -fPIC -no-pie tests/linux/qt_atspi_fixture.cpp -o /tmp/aad-qt-fixture @flags
$install = osdk where github:lejunyang/ai-auto-desktop
./tests/linux/verify-fcitx-input.ps1 -Aad "$install/aad" -Fixture /tmp/aad-qt-fixture -ComposingMethod shuangpin
```

在其他机器应将 `ComposingMethod` 改为实际安装并启用的组合输入法名称。真实 GUI 验证必须串行。

## 范围边界

- 本次交付是 Linux CLI 归档；Windows/macOS 原生后端和 Tauri GUI 未进行本机验证。
- Linux 的 region overview、`watch` 和整桌面 `assist --desktop` 尚不支持，已确认会拒绝；浏览器 assist 可用。
- 没有声称验证了 Wayland、锁屏、提权窗口、任意应用或所有输入法。
- **[待验证]** 修改后的 GitHub Ubuntu 22.04 release job 尚未在线执行；需要推送后在 GitHub Actions 实际构建。当前证据为本机重建包的 ELF 符号检查和 glibc 2.36 上的实际执行。
