# 键鼠补全设计（Windows P0）

> 状态：`key_chord`（组合键/功能键）**已实现并通过真机验证**；右键/双击/滚轮/悬停/拖拽为后续增量。目标是把现有 `SendInput` 通道"用满"，补齐 L0 语义动作表达不了的键鼠能力，同时**不破坏项目的安全不变量**（禁裸坐标、写操作必须锚定已校验元素）。
>
> 依据：上一轮真机实测（见 `input-synthesis-and-background-execution.md` §6）。核心结论——虚拟键/扫描码对三通道全可见，`KEYEVENTF_UNICODE` 对 Raw Input 不可见；修饰键泄漏不自愈。

## 交付进度（2026-09-25）

| 动作 | 状态 | 验证 |
|---|---|---|
| `key_chord`（组合键/功能键） | ✅ 已交付 | 单元测试 8 项 + 真机端到端：记事本 Ctrl+A 全选 23/23 字符生效；无修饰键泄漏 |
| 右键/双击/中键 | ⏳ 后续增量 | — |
| `scroll` / `hover` | ⏳ 后续增量 | — |
| `drag` | ⏳ 后续增量 | — |
| `type_text` 的 keys 模式 | ⏳ 后续增量 | — |

已改动文件：`aad-uia/src/backend.rs`（trait + 默认降级）、`aad-uia/src/windows.rs`（Chord 实现 + derive 标注 + 8 单测）、`aad-uia/src/driver.rs`（WRITE_ACTIONS/dispatch/act/契约/mock）、`aad-mcp/src/tools.rs`（key_chord 工具 + 分发 + locator enum）、`aad-cli/src/main.rs`（`do key-chord` 子命令）。验证脚本：`tests/windows/verify_key_chord_e2e.py`、`verify_key_chord_effect.py`。

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
- **不新增 `type_text` 的裸替换**，而是给它加一个 `mode` 选项（`unicode` 默认 / `keys`），修掉 Raw Input 盲区——但这属于对现有动作的增强，放在 key_chord 之后做，避免一次改太多。
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

## 4. 分层改动清单

| 层 | 文件 | 改动 |
|---|---|---|
| Backend trait | `aad-uia/src/backend.rs` | 加 `key_chord` / `scroll` / `hover` / `drag` 方法；`pointer_click` 扩展签名（button/count/fraction） |
| Windows 实现 | `aad-uia/src/windows.rs` | 用 SendInput 实现上述；键名映射表；修饰键影子状态；`derive` 里标注新动作 |
| 其他后端 | `aad-atspi`、`aad-macos-ax` | 返回 `ACTION_UNSUPPORTED` 降级（本轮不实现，保持可编译） |
| Dispatch | `aad-uia/src/driver.rs` | `WRITE_ACTIONS` 加项；`act` match 加分支 + 参数解析 + 校验；`action_contracts` 加契约 |
| MCP schema | `aad-mcp/src/tools.rs` | 加动作工具 schema（用 fraction 不用坐标）；dispatch 映射；坐标禁令测试必须仍绿 |
| CLI | `aad-cli/src/main.rs` | `DoCommand` 加子命令变体（每个都带 target）；dispatch 映射 |
| 验证 | `tests/windows/` | 复用 input_channel_probe 分通道回读；扩展 fixture |

## 5. 实现顺序

1. `key_chord`（最高价值：组合键/功能键是"无法用元素描述"的最大一类）+ 修饰键泄漏防护
2. `pointer_click` 扩展（右键/双击/中键 + 归一化位置）
3. `scroll` / `hover`
4. `drag`
5. `type_text` 的 keys 模式（修 Raw Input 盲区）

每完成一项，本地 cargo test + clippy 全绿再进行下一项。全部完成、真机回读验证通过后统一交付，不擅自 push。
