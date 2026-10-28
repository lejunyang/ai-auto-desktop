# macOS AX 原生能力

`desktop.macos_ax` 的 manifest、参数校验、snapshot/locator、stale target 和 effect
语义由 [`aad-macos-ax`](../../crates/aad-macos-ax) 以 Rust 实现，并由 CLI/MCP 直接注册。
仓库不再包含 Python adapter。

Swift helper 仍然保留：macOS TCC 权限和 `AXUIElement` 对象必须归属于固定、可签名的
`.app` 进程。它只负责 AppKit/ApplicationServices/CGEvent 边界，私有 NDJSON 中使用短期
opaque token；Rust adapter 管理 helper 生命周期和公开 provider 契约。

## 构建

在 macOS 13+ 与 Xcode Command Line Tools 环境中运行：

```sh
plugins/macos_ax/build.sh
cargo build --locked -p aad-cli
```

helper 默认生成到：

```text
plugins/macos_ax/.build/MacOSAXHelper.app/Contents/MacOS/MacOSAXHelper
```

`build.sh` 默认使用 ad-hoc 签名；稳定部署应设置
`MACOS_AX_CODESIGN_IDENTITY`。可用 `AI_AUTO_DESKTOP_MACOS_AX_BUILD_DIR` 改变构建目录，
或用 `AI_AUTO_DESKTOP_MACOS_AX_HELPER` 指定可信 `.app` 内的 helper。Rust adapter 会执行
`codesign --verify --strict`，并校验固定 bundle ID、可执行文件名与 package type；这证明
bundle 完整性，不等于认证发布者，自定义 helper 会明确标为 `custom_untrusted`。

首次使用需要在“系统设置 → 隐私与安全性 → 辅助功能”中授权 `MacOSAXHelper.app`。
provider 只静默检查权限，不主动弹出 TCC 请求。

## 能力与边界

公开动作是 `list_apps`、`snapshot`、`find`、`focus`、`invoke`、`set_value`、显式
`type_text`、`pointer_click`、`pointer_gesture`（右键、中键、双击、滚轮、悬停，位置是元素内比例）、
`drag`（同一应用内，起止两点都做 AX 命中检查）和 `key_chord`（ctrl、shift、alt、cmd 加一个主键，
按美式键位的虚拟键码，名称按字面映射：`ctrl` 就是 Control，全选要写 `cmd,a`）。写动作必须带当前快照的 target 与精确 locator；Rust
adapter 重新抓取 AX 树，helper 用 `CFEqual` 核对原生对象。protected、截断、多义、身份
漂移与未授权状态均失败关闭。

录制用 `watch` / `collect` / `release`（helper 的 `record_start` / `record_collect` / `record_stop`）：
AXObserver 报告值变化和焦点，只监听的 CGEvent tap 报告点击、滚轮和组合键，需要“输入监控”权限，
没有时只记录值变化并在结果里给出 `pointer_watch: false`。

指针动作的落点必须经 `AXUIElementCopyElementAtPosition` 命中目标或其子元素，事件只投递给
目标进程（`postToPid`），不做全局注入；跨应用拖放因此不支持。

键鼠输入不会作为其他动作的自动 fallback。helper 在首个 CGEvent 前发送独立 progress
marker；越过该边界后的 timeout、EOF 或协议失败归一为不可重试的
`DRIVER.UNKNOWN_EFFECT`。成功只表示事件已提交，调用方仍须以 fresh snapshot 验证业务
后置条件。

CI 在 macOS runner 上构建并签名 helper，随后由 Rust adapter 完成握手与应用枚举 smoke。
仓库还保留独立 Swift 真机套件 [`tests/macos`](../../tests/macos)，用于显式 TCC、AX 写动作
与 CGEvent 回读验证。完整边界见
[`macos-ax-driver.md`](../../docs/architecture/macos-ax-driver.md)。
