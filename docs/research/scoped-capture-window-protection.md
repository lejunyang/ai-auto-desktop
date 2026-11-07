# AT-SPI scope 截图的完整窗口复核

日期：2026-11-07。验证环境：本机 Linux/KDE/X11、Qt 5 Widgets。其他桌面/平台不由本次结果推断。

## 问题与修复

原 `checked_capture` 会刷新目标 scope，但只在该树中查找 protected 后代和重叠 sibling。缩到一个小控件后，同窗口另一分支的覆盖控件可能完全不在 scope 中。中心点命中也不能证明整个截图矩形安全：覆盖角落的控件不会影响中心命中。新增 fake 回归在修复前确实返回了 PNG，且像素 backend 已被调用。

修复后，若 scope 小于包含窗口，先私下抓完整窗口，再验证目标 NativeRef、指纹、目标 bounds 及窗口 NativeRef/bounds 未变。完整窗口只用于否定保护/遮挡条件，不发布内部节点，不扩大成员资格，不把 context 祖先变成可操作目标。截图不要求目标具备 drag 或其他输入 action。

- 目标后代受保护，或完整窗口中的 protected sibling 与截图矩形重叠，拒绝截图。protected sibling 的 visible/showing=false 不能解除保护；Qt 存在真实可见控件误报这些状态的情况。
- protected sibling 没有可用正面积 bounds 时，无法证明其在截图区域外，保守拒绝。明确在矩形之外的 protected sibling 不阻止目标截图。
- 普通 visible sibling 与目标矩形重叠时，拒绝截图；仍不假设 AT-SPI 树顺序代表绘制层级。
- 完整窗口超过节点/深度限制或枚举不完整时，拒绝截图，即使小 scope 本身完整。无 scope 或 scope 已等于窗口的路径，也由 `fresh_target` 的原始及 fresh truncated 门禁保护。
- 私有读取使用独立 Record store，保留当次取消 token；取消后的下一次普通调用不继承旧 token。原 scoped observation 在 capture 后仍可用。

`private_window_record` 与有路径拖拽共用动作无关的窗口身份/几何检查；拖拽自己的 action、enabled/sensitive、protected 检查仍在其调用路径中。本次 AT-SPI 单测 62/62、all-targets clippy 通过，包括原 scoped drag 回归。

## 诊断与兼容性影响

| 拒绝条件 | error code | details.check |
|---|---|---|
| 完整窗口无法枚举完整 | DRIVER.SNAPSHOT_TRUNCATED | containing_window_completeness |
| 截图矩形无法证明无 protected 内容 | DRIVER.PROTECTED_ELEMENT | capture_window_protection |
| 与 visible sibling 重叠 | DRIVER.ACTION_UNSUPPORTED | capture_window_occlusion |
| 私有读取期间目标/窗口身份或 bounds 改变 | DRIVER.STALE_SNAPSHOT | 无内部节点内容 |

错误不返回私有窗口中的 node ID、名称、值或路径。成功 provenance 的 `private_window_checked` 只表示本次是否进行了额外的私有完整窗口读取。

小 scope 不再能绕开大窗口的截图完整性要求。因此深层 Electron 树或超过 5000 节点的窗口可能仍能用 scope 查找/操作元素，却因无法证明全窗口的遮挡和保护条件而拒绝截图。这是明确的能力限制；本次没有为这些窗口增加截图兜底，也没有把已有 KDE 正例外推到所有应用。

## 真实 Qt 验收

fixture 的目标是 240×140 绿色 surface；覆盖 sibling 仅覆盖左上角，中心点仍落在原 surface 内。所有操作只涉及本次创建的临时窗口，脚本退出时关闭 fixture/control 进程。

| case | 实际结果 |
|---|---|
| clear | 截图成功，QImage 独立解码确认尺寸和 98.699% 绿色像素 |
| overlay | scope 中没有该 disabled sibling；完整窗口复核以 ACTION_UNSUPPORTED 拒绝，无 PNG |
| protected | scope 中没有该密码框；完整窗口复核以 PROTECTED_ELEMENT 拒绝，无 PNG |
| protected-outside | 密码框位于矩形外；目标截图成功，尺寸和绿色比例与 clear 一致 |

每例 capture 后均用原 observation 查找目标成功。两张成功 PNG 还经人工视觉检查，仅显示目标 surface。

本次证据目录：`artifacts/capability-completion/scoped-capture/`，含 `summary.json`、各 case 的 `.ndjson`、`clear.png`、`protected-outside.png`。受测 CLI SHA256 为 `efdace802d5189739597138dab03c841dde5801e28c6d9ad28d98d13deecc74e`；capture helper SHA256 为 `badb8fd71ae13de03ce835abda711d4ece4d85ec7a5c4b9160f4d6ee8202c78c`。

复现（与其他 GUI 测试串行）：

```powershell
osdk exec -- cargo build -p aad-cli
pwsh -NoProfile -File tests/linux/verify-scoped-capture.ps1 `
  -Aad target/debug/aad `
  -InputHelper /absolute/path/to/x11_xtest_helper `
  -CaptureHelper /absolute/path/to/x11_capture_helper `
  -BuildDirectory /absolute/path/to/results
```

构建 fixture 可只加 `-BuildOnly`。脚本不依赖 Pillow；PNG 像素回读由 fixture 的只读 `--verify-png` 模式完成，该模式使用 QCoreApplication，不创建 GUI。
