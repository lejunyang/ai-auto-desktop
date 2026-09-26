# 键鼠补全设计（Windows P0）

> 状态：`key_chord`、Windows 指针手势与 `type_text` 的 layout-aware keys 模式均**已实现并通过隔离真机验证**。Windows P0 键鼠表达力补全完成。目标是把现有 `SendInput` 通道"用满"，补齐 L0 语义动作表达不了的键鼠能力，同时**不破坏项目的安全不变量**（禁裸坐标、写操作必须锚定已校验元素）。
>
> 依据：上一轮真机实测（见 `input-synthesis-and-background-execution.md` §6）。核心结论——虚拟键/扫描码对三通道全可见，`KEYEVENTF_UNICODE` 对 Raw Input 不可见；修饰键泄漏不自愈。

## 交付进度（2026-09-26）

| 动作 | 状态 | 验证 |
|---|---|---|
| `key_chord`（组合键/功能键） | ✅ 已交付 | 单元测试 8 项 + 真机端到端：记事本 Ctrl+A 全选 23/23 字符生效；无修饰键泄漏 |
| 右键/双击/中键 | ✅ 已交付 | 隔离 Win32 探针经 `find → target → do` 回读 Raw Input，完整观察到对应 down/up 序列 |
| 纵向/横向 `scroll` | ✅ 已交付 | 隔离探针分别观察到 3 个竖向、2 个横向 wheel detent |
| `hover` | ✅ 已交付 | 光标落在被观察元素内部，且 350 ms 停留已兑现 |
| `drag` | ✅ 已交付 | 起终点均为已观察 target；探针观察到 left-down → move → left-up 完整序列 |
| `type_text` 的 keys 模式 | ✅ 已交付 | 英文目标布局下 `aA1!` 精确落入 Edit；Raw Input 收到 12 个非零 VK 事件；emoji 整段零输入拒绝；IME 打开时 `DRIVER.INPUT_METHOD_ACTIVE` 且 0 事件 |

指针改动文件：`aad-uia/src/backend.rs`（手势模型 + 非 Windows 默认拒绝）、`aad-uia/src/windows.rs`（SendInput 手势实现、边界内缩、失败恢复）、`aad-uia/src/driver.rs`（双 target 校验、参数互斥与范围校验）、`aad-mcp/src/tools.rs`（`pointer_gesture` / `drag` schema）、`aad-cli/src/main.rs`（`do gesture` / `do drag`）。隔离真机脚本：`tests/windows/verify_pointer_gesture_effect.py`；共享 Raw Input 探针的 `RAWMOUSE` 联合体对齐也已修正。

> 注：`key_chord` 暂不进 `save_workflow` 可保存动作——workflow 的 `assemble.rs` 用单个 `argument` 字符串表达参数，而 keys 是数组，接入需改动 workflow 序列化格式，留待后续。即时调用（CLI `do` / MCP 工具）已完全可用。

## 1. 不变量（实现全程不得违反）

1. **禁裸坐标**：MCP schema 递归禁止字段名 `x`/`y`/`point`/`coordinates`/`position`（`tools.rs` 两个测试守着）。元素内位置一律用**归一化比例** `fraction: {fx, fy}`，取值 0.0–1.0，语义是"元素内的相对位置"而非屏幕坐标。
2. **写操作锚定已校验元素**：每个新动作都必须带 `target`（snapshot:revision:node），经 `resolve` + `verify` 校验。唯一例外是全局快捷键（不属于任何元素），单独处理、默认关闭。
3. **动作能力门控**：`act` 用 `node.actions` 拦截元素不支持的动作。因此新动作必须在 `derive`（windows.rs:782）里按能力标注，否则永远报 `ACTION_UNSUPPORTED`。
4. **原子投递**：修饰键序列必须整批 `SendInput`（复用 `deliver_atomically`）。批次不可分割，分割会导致文字错乱（已实测）。

## 2. 新增动作一览

| 动作 | 锚定 | actions 标注门槛 | 参数 |
|---|---|---|---|
| `key_chord` | 焦点元素（先 focus 再发键） | `focusable == Some(true)` | `keys`: 如 `["ctrl","c"]` / `["f5"]` / `["shift","end"]` |
| `pointer_click`（扩展） | 元素中心，可加归一化位置 | 现有：`bounds && !offscreen` | `button`: left/right/middle；`count`: 1/2；`fraction`: {fx,fy} 可选 |
| `scroll` | 可滚动容器元素 | `bounds && !offscreen`（发送时校验落点） | `direction`: up/down/left/right；`amount`: 档数（默认 3） |
| `hover` | 元素中心 | `bounds && !offscreen` | `dwell_ms`: 停留毫秒（默认 300） |
| `drag` | 起点元素 + 终点元素 | 两端都需 `bounds && !offscreen` | `to`: 终点 target；两端可各带 `fraction` |

设计取舍：
- `type_text` 新增 `mode`：`unicode` 保持默认，支持任意 Unicode；`keys` 按目标窗口线程的键盘布局用 `VkKeyScanExW` 映射为虚拟键，Raw Input 可见。整段先映射再发送；任何字符不可表达则零输入拒绝。目标输入法处于打开状态时同样拒绝，避免 IME 把按键改写成另一段文本。
- **拖拽的中间路径**用插值（起点→终点若干 move 事件），只有起点和终点锚定元素；中间点是派生的，不是调用方给的，符合不变量。
- **`scroll` 的方向**用语义值而非 delta 数字，避免"滚动量"变成变相坐标。

## 3. 键盘编码（key_chord 的核心）

- 默认走**虚拟键 `wVk`**（`MapVirtualKeyW` 补 scancode），因为对普通应用最稳，且不随键盘布局变。
- 键名→VK 映射表：字母 `a`-`z`、数字、功能键 `f1`-`f24`、方向键、`enter`/`tab`/`esc`/`space`/`backspace`/`delete`/`home`/`end`/`pageup`/`pagedown`、修饰键 `ctrl`/`shift`/`alt`/`win`。未知键名 → `INVALID_REQUEST`（不静默忽略）。
- 组合键序列：`[mod... down] + [key down] + [key up] + [mod... up 逆序]`，整批提交。
- **修饰键泄漏防护**（实测确认不自愈，必做）：
  - driver 侧维护"本次动作按下的修饰键"集合；
  - 批次投递后无论成功失败，若检测到异常，**主动补发所有已按下修饰键的 up 事件**；
  - `SendInput` 部分失败时，错误 detail 里加 `residual_state` 字段说明可能残留，`effect` 保持保守（`unknown`）。
- **扫描码模式**作为显式可选项（`encoding: scancode`），不设默认——扫描码在非 US 布局下按键位而非字符，对普通应用是错的。

## 3b. 指针动作的接入方式（第二档）

现有 pointer_click(window_id, node) 是无参的“左键单击中心”，已被 mock/driver/mcp/cli 多处引用。为向后兼容，不改它的签名，而是新增带默认降级的 trait 方法，参数用一个结构体承载：PointerGesture { button(Left|Right|Middle，默认 Left)、count(1|2 双击，默认 1)、fraction(元素内归一化位置 0..1，默认中心)、scroll(Some 时为滚轮：方向 up/down/left/right + 档数)、hover_ms(Some 时为悬停：移动到点并停留) }。

- pointer_gesture(window_id, node, &PointerGesture)：右键/双击/中键/滚轮/悬停/元素内定位，锚定单个元素。
- drag(window_id, from, to, &PointerGesture)：拖拽，锚定起点+终点两个元素；中间路径由插值派生。

两个方法都 default 返回 ACTION_UNSUPPORTED，非 Windows 后端不必立刻实现。坐标一律由 node.bounds + fraction 在 backend 内部算出，调用方永远给不了屏幕坐标——这是禁裸坐标不变量在指针动作上的落点。滚轮方向用枚举 + 档数（正整数），不暴露 delta 数字。

## 4. 分层改动清单

| 层 | 文件 | 改动 |
|---|---|---|
| Backend trait | `aad-uia/src/backend.rs` | `key_chord`、`pointer_gesture`、`drag`；指针参数由 `PointerGesture` 承载 |
| Windows 实现 | `aad-uia/src/windows.rs` | SendInput 实现组合键与全部指针手势；derive 标注动作；部分投递时释放修饰键/鼠标键 |
| 其他后端 | `aad-atspi`、`aad-macos-ax` | trait 默认返回 `ACTION_UNSUPPORTED`，保持可编译 |
| Dispatch | `aad-uia/src/driver.rs` | `WRITE_ACTIONS`、dispatch、双 target resolve/verify、参数互斥与范围校验、动作契约 |
| MCP schema | `aad-mcp/src/tools.rs` | `key_chord` / `pointer_gesture` / `drag` 工具；仅暴露 fraction，不暴露坐标；递归禁坐标测试保持全绿 |
| CLI | `aad-cli/src/main.rs` | `do key-chord` / `do gesture` / `do drag`，成对参数与互斥参数由 clap 提前拒绝 |
| 验证 | `tests/windows/` | 隔离 Win32 探针分通道回读；不操作用户已有窗口 |

## 5. 实现顺序

1. `key_chord`（✅ 已完成）
2. `pointer_gesture`：右键/中键/双击 + 元素内归一化位置（✅ 已完成）
3. 纵向/横向滚轮与悬停（✅ 已完成）
4. 双 target 拖拽与失败释放恢复（✅ 已完成）
5. `type_text` 的 keys 模式（✅ 已完成：目标布局映射、Caps Lock 修正、IME fail-closed、Raw Input 真机验证）

每完成一项，本地 cargo test + clippy 全绿再进行下一项。真机验证必须使用脚本自行创建和销毁的隔离窗口，不得选择桌面上既有的用户窗口。全部完成后本地提交，不擅自 push。
