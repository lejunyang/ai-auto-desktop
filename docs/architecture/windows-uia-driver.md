# Windows UI Automation（UIA）Rust 驱动

> 当前实现：`crates/aad-uia`。Windows 上直接调用 UIAutomationCore/Win32；非 Windows
> 构建保留平台无关模型和 fake-backend 测试，但 `native_driver()` 明确返回 unavailable。

## 能力与边界

provider 名为 `desktop.windows_uia`，提供：

- 观察：`list_windows`、`snapshot`、`describe`、`overview`、`find`；
- 写动作：`focus`、`invoke`、`set_value`、显式 `type_text`、显式 `pointer_click`；
- 录制：`watch`、`collect`、`release`。

CLI 与 MCP 在 Windows 上直接注册 Rust driver，不存在外部 Python worker 或 `comtypes`
运行依赖。UIA 同步调用无法在调用线程内安全抢占，因此 Host 的绝对 deadline 与进程生命周期
仍是最终硬边界。

## 观察、定位与 snapshot discipline

snapshot 记录窗口元数据、扁平节点树、revision、digest 和截断状态。节点包含 role/name/value、
automation/class/framework identity、bounds、states、actions 及父子关系。密码值不读取。

locator 只做声明式匹配；零匹配返回 `DRIVER.NOT_FOUND`，多匹配默认返回
`DRIVER.AMBIGUOUS_MATCH`，不会选择第一个。写动作必须引用已观察 target：
`snapshot_id + revision + node_id`。driver 从 SnapshotStore 恢复原节点，并由 native backend
重新定位、比较 role/name/automation_id/class_name；目标变化时返回 `DRIVER.STALE_HANDLE`。

快照可在单进程内使用，也可由 CLI 的持久 SnapshotStore 跨相邻命令使用；它们不是可长期
重放的原生 COM handle。达到深度或节点上限时 `truncated=true`，调用方不能将有限观察误作
完整 UI 证明。

## 原生动作

`focus` 使用 `IUIAutomationElement::SetFocus`。`invoke` 依次尝试 InvokePattern、
TogglePattern 和 LegacyIAccessible 默认动作。`set_value` 使用 ValuePattern，并轮询回读；如果
API 返回成功但值保持原状，会报告 `DRIVER.ACTION_FAILED`，不会伪装成成功。

`type_text` 与 `pointer_click` 仅在调用方显式选择时使用 `SendInput`：

- `type_text` 先聚焦目标，再按 UTF-16 code unit 提交 Unicode key-down/key-up；最多 1024
  个字符。
- `pointer_click` 只接受目标 bounds 的中心点和左键，按虚拟桌面坐标提交 move/down/up。

二者都不会成为 `set_value`/`invoke` 的隐式 fallback。成功只表示 Windows 接受输入事件，
业务结果仍必须由 fresh snapshot/postcondition 验证。部分派发或派发后无法确认的情况属于
不可自动重放的 unknown effect。UIPI、管理员窗口、Session 0、UAC secure desktop、登录/锁屏
界面和跨用户 session 不在支持范围内。

## 录制

`watch` 在指定窗口上建立 UIA event handler 与 WinEvent hook，`collect` 将有界事件转换为
可编辑的 locator step，`release` 显式拆除 session。buffer 溢出数量会返回给调用方；无法证明
唯一的 locator 标记为 unresolved，不会乐观生成可回放动作。

## 验证

平台无关逻辑由 crate 单元测试覆盖。Windows CI 还运行
`crates/aad-uia/tests/windows_native.rs`：测试在 Rust 中创建真实 Win32 控件，并用正式 driver
验证窗口枚举、snapshot/find、歧义拒绝、ValuePattern、InvokePattern、Unicode SendInput 和
中心点鼠标输入后的 fresh snapshot。

```powershell
cargo test --locked -p aad-uia --test windows_native
```

自有 fixture 通过不代表任意第三方 UI、提权边界、多显示器/DPI、IME 或焦点竞争均已完成
资格验证；这些仍需独立平台矩阵。
