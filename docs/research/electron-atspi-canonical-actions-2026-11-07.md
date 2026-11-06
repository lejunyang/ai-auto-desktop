# Electron AT-SPI 规范动作与显式窗口视觉 surface 验证

日期：2026-11-07。环境：本机 KDE/X11，VS Code 1.131.0 / Electron 42.7.0，自建独立 profile；Qt 5 Widgets 自建 fixture。全部写操作经过 `aad control serve --allow-input`，测试结束关闭自建应用与 control session。没有修改用户的 Code 安装或 profile。

结论：Code 欢迎对话框的 Close 可以通过观察锚定的 `invoke` 关闭。此前缺失的能力来自 AT-SPI 动作名称读取错误；修复未放松指针命中、scope 或观察校验。该 Close 节点的精确指针 `click` 仍受上游命中结果限制。

## 根因与修复

AT-SPI 的规范动作名和本地化显示名不同。GNOME 的 [Action adaptor](https://github.com/GNOME/at-spi2-core/blob/main/atk-adaptor/adaptors/action-adaptor.c) 中，D-Bus `GetActions` 的名称取自 `atk_action_get_localized_name`，`GetName` 则调用 `atk_action_get_name`；[API 文档](https://gnome.pages.gitlab.gnome.org/at-spi2-core/libatspi/method.Action.get_action_name.html) 也将后者定义为非本地化名称。这里使用 D-Bus `GetName`，不涉及已重命名的 C API 别名。

Chromium 的 [AXPlatformNodeAuraLinux](https://github.com/chromium/chromium/blob/main/ui/accessibility/platform/ax_platform_node_auralinux.cc) 对 Blink 内建动作可能不提供本地化名称，但通过规范名称暴露默认动作与其他支持的动作。官方主干源码解释机制；对本机二进制的判断以独立 D-Bus 读回为准：

| 控件 | `GetActions` 显示名称 | `GetName` 规范名称 | 修复后选择 |
| --- | --- | --- | --- |
| Code Welcome / Close | 两项均为空 | `press`、`showContextMenu` | `press` |
| Qt fixture / Invoke | `Press`、`SetFocus` | `Press`、`SetFocus` | `Press` |

原实现仅检查 `GetActions`，使 Code 的可调用动作被隐藏。修复提交 `039352f` 读取规范名称，保留本地化名称作诊断；通用 `invoke` 仅接受 `click`、`press`、`activate` 中唯一的明确动作。未知或空名称不提供通用调用，多个激活动词不猜默认索引。

第一轮真实验收还暴露了属性大小写问题：`atspi-proxies 0.11` 的 `nactions()` 访问器生成 `Nactions`，协议属性实际为 `NActions`。首次修复因此仍隐藏能力，证据保存在 `artifacts/capability-completion/electron-canonical-invoke/summary.json`。改为显式读取准确属性名后，Code 和 Qt 验收均通过。新增私有真实 D-Bus 服务测试覆盖准确属性名，避免纯 fake 动作列表掩盖协议错误。

调用过程保留这些约束：

- provider 先对原目标进行 fresh 身份、指纹及 scope 校验，再将该次观察选出的 `native_invoke_name` 传入 `invoke_named`。
- 后端重新读取动作列表，要求唯一规范动作仍与 fresh 名称完全一致；在 `DoAction` 前再次核对数量和所选槽的名称，不复用观察时的数字索引。
- GTK 的 `toggle`、`expand`、`collapse` 仍按各自指定的 `click` / `activate` 唯一匹配；其他激活动词共存不会误禁用它们，大小写兼容保留。通用 invoke 的严格匹配与这些动作分开。
- `DoAction` 已开始后的 transport 错误、否定返回或过期/取消均保守标为 `unknown`，记录 `dispatch_started`，使原观察失效。同步 D-Bus 调用本身不能保证立即被取消。
- 观察阶段动作元数据读取失败仅隐藏相应语义能力；节点及遍历的 deadline/cancellation 检查仍保留。

## 真实效果与可复现入口

最终证据：`artifacts/capability-completion/electron-canonical-invoke-v2/summary.json`。Code 的 `code-native.ndjson` 记录公开输入入口，`code-before.png` / `code-after.png` 独立显示对话框从存在到消失；只读 CDP 再确认 Welcome 不存在。此次调用到独立读回耗时 587 ms，不能当作所有应用的性能指标。

Qt 的 `qt-native.ndjson` 记录公开 `invoke`；随后新的观察读到显示标签从 `Qt fixture status idle` 变为 `Qt fixture status invoked`。不是只检查 `DoAction` 返回成功。

已通过：67 个宿主单测、1 个 provider 集成测试、1 个私有 D-Bus 协议测试、clippy，以及 Code / Qt 两项真实效果验收。动作名变化、列表数量变化、同长度槽替换、未知/歧义名称都覆盖零派发负例；fresh 名称与普通 GTK 名称不同的大小写情形另有回归。

分发测试提交 `947137b`：[verify-canonical-invoke.ps1](../../tests/linux/verify-canonical-invoke.ps1)，实现 [canonical_invoke.py](../../tests/linux/canonical_invoke.py)。需要 Qt5Widgets 开发包、Python `dbus`、支持内置 WebSocket 的 Node，以及可运行的 Code。脚本针对当前 Welcome UI；不同 Code 版本若不提供该对话框，应报告前置条件不满足，不能假报动作通过。

```powershell
pwsh -NoProfile -File tests/linux/verify-canonical-invoke.ps1 `
  -Aad target/debug/aad `
  -InputHelper /absolute/path/to/x11_xtest_helper `
  -OutputDirectory /absolute/path/to/canonical-evidence

dbus-run-session -- cargo test --locked -p aad-atspi `
  canonical_action_count_uses_exact_atspi_wire_property -- --ignored
```

脚本仅对自建 profile 执行输入；CDP 用于读取状态和截图，独立 D-Bus 仅查询动作名称。没有经 CDP 或测试侧 D-Bus 执行关闭动作。

## 已有 frame surface 路径及其边界

另一条未修改输入不变量的路径已单独验证，初始证据为 `artifacts/capability-completion/electron-visual-surface/summary.json`；包含本次修复及 scoped capture 加固的最终 binary 又通过了 `electron-visual-surface-final/summary.json` 验收。实验显式观察并选择完整窗口 frame scope，使用公开 capture 获得 1200×800 PNG 并目视 Close，重新观察该 frame 后，向明确选中的 frame 发送 fraction `0.875 / 0.18125` 点击。只读 CDP 与操作后 PNG 共同确认 Welcome 消失；最终证据目录保留 `before-click.png` 和 `independent-after.png`。

该实验中系统 hit test 返回的 `/accessible/14` 是 frame 的真实 descendant，所以点击满足 frame surface 的命中约束。它却不是 Close 节点或 Close 的 descendant，因此不能称为 Close 精确指针命中已修复。`electron-pointer-hit-v4/summary.json` 中 12 次查询持续返回同一个全窗 sibling，精确 Close click 仍应拒绝。

这条路径必须由调用者明确选择完整窗口；不能将 dialog scope 的只读 frame context 自动升级为授权目标，不能自动扩大子 scope。完整 scope、二次校验、保护区域与截图检查、同用户窗口/PID 命中约束继续适用。窗口包含不可安全截图的保护区域时，应按既有规则拒绝。

Chromium [ViewAXPlatformNodeDelegate::HitTestSync](https://github.com/chromium/chromium/blob/main/ui/views/accessibility/view_ax_platform_node_delegate.cc) 的逆序子节点命中逻辑可以解释为何占满窗口的 sibling 影响访问性命中。仅直接查询目标子树无法证明没有其他 sibling 覆盖；不能按类名跳过该节点或用重复查询冒充遮挡证明。

两种恢复路径分别是明确语义动作和明确视觉 surface。它们不等于任意 Electron 控件都支持语义操作，也不保证任意自绘应用的精确节点指针命中。[待验证] 其他 Electron 版本、不同桌面环境及更多控件需要各自的真实效果验证。
