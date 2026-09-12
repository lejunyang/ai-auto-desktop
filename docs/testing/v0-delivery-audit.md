# Rust 迁移交付审计

> 更新日期：2026-09-12。代码可用、fixture 通过与第三方应用平台资格是三件不同的事。

## 当前实现

| 能力 | 实现 | 自动验证 | 结论 |
| --- | --- | --- | --- |
| compiler / expression / runtime | `aad-core`、`aad-runtime` | Rust workspace tests | 已迁移 |
| durable journal / resume | `aad-runtime` + SQLite | intent/finalization/lease/failure tests | read-only 安全重放；写 action/script 保守拒绝 |
| ArtifactStore / process artifact protocol | `aad-runtime`、`aad-plugin` | Rust 单元与集成测试 | run-scoped、不可变、有配额 |
| OCR | `aad-ocr` | fake Tesseract、解析/资源/超时测试 | Rust provider；引擎仍是外部 Tesseract |
| Windows UIA | `aad-uia` | fake backend + Rust Win32 fixture | 每次 Windows CI 编译；交互式执行显式 opt-in |
| Linux AT-SPI | `aad-atspi` + C++ X11 helpers | fake backend + 私有 Xvfb/Qt fixture | 当前 KDE/X11 profile 通过受控链路 |
| macOS AX | `aad-macos-ax` + Swift helper | fake helper + macOS build/smoke + Swift testkit | adapter 已迁移；完整 TCC 写动作仍需单独真机资格 |
| CLI / MCP / GUI | `aad-cli`、`aad-mcp`、Tauri/Vue | Rust tests + front-end tests | 内置 provider 已接入 |
| macOS 回传验真 | Rust `aad-macos-result-verifier` | synthetic archive tests + unsupported archive smoke | 保留来源/hash/结构 fail-closed 语义 |

旧 Python runtime、provider、测试 runner 和重复 schema 已在对应 Rust 能力与测试建立后删除。
仓库仍允许 workflow 的显式 `runtime: python` script；解释器是用户运行环境中的受限工具，
不是项目自身维护的 Python 产品代码。

## 持久执行边界

durable 默认拒绝 action。显式选择 `--durable-actions read-only` 时，只允许满足以下全部条件
的顶层单次只读 action：provider/descriptor sensitivity 为 public、错误均为 not-applied、
checkpoint output 有稳定字段投影、没有 artifact、retry、handler、finally 或嵌套控制流。

dispatch 前写入 `action_intent` 并绑定 provider/contract/projection/input digest；恢复时重新验证
绑定后才可重放只读 action。finalization 使用 intent/started/result 三阶段 checkpoint：只有
尚未开始的 cleanup 可安全执行；可能已开始但无结果的阶段落 `UNKNOWN_EFFECT`；已有结果只提交
持久化终态。

写 action 与 script 没有足够的业务 reconciliation 规范，崩溃后不能证明副作用是否发生，
因此 durable resume 仍不自动重放它们。这不影响普通非 durable workflow 执行。

## 当前自动门禁

CI 在 Ubuntu、Windows、macOS 上执行：

- `cargo fmt --all --check`；
- `cargo test --workspace --exclude aad-gui --locked`；
- `cargo clippy --workspace --exclude aad-gui --locked --all-targets -- -D warnings`；
- 全部 tracked workflow 通过 Rust CLI `validate`，并执行 `aad probe` smoke；
- Linux 构建 XTest/capture helper，并跑私有 Xvfb + Qt AT-SPI fixture；
- Windows 编译研究 examples，并跑 Rust Win32/UIA fixture；
- macOS 构建/签名 Swift helper，并由 Rust adapter 做握手与应用枚举 smoke；
- Linux 还构造一个 macOS unsupported 结果归档，交给 Rust verifier 验证。

GUI 测试由 `cd gui && npm test -- --run` 执行；release workflow 在 Windows 构建 Tauri
安装器，并在各平台打包 Rust CLI 和必要 native helper。

## 资格声明边界

- Windows 自有 Win32 fixture 由手动 input 或 `[windows-native]` push 显式执行，且不能代表 Office、浏览器、提权窗口、secure desktop、RDP、
  IME 与多显示器组合。
- Linux fixture 只证明受控 Qt5/KDE/X11 路径；Wayland/GNOME 与第三方应用需独立 profile。
- macOS CI smoke 不会主动请求 TCC，也不执行写动作；完整验证使用 `tests/macos/` Swift kit，
  回传包必须同时通过归档 hash 和 source revision/package digest 独立 pin 才能标记 qualified。

## 后续优先级

1. 获取 Windows/macOS CI 与真实应用证据并按环境记录。
2. 扩展 Linux QML、多窗口、动态页面及更多真实应用矩阵。
3. 设计写 action/script 的 durable reconciliation、single-desktop-writer、用户介入检测。
4. 补齐 secret store、确认 token、签名发布链、taint policy 与真实应用 SLO。
