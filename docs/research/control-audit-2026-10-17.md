# 统一控制与远程执行审视（2026-10-17）

范围：云端隔离 checkout，main `255070a`；未连接用户 Mac、Windows 桌面或移动设备。
本次修复故障与现有安全边界，不实现新的公网控制服务，不更改工作流版本。

## 优先级与现状

| 优先级 | 结论 / 证据 | 本次处理 / 后续 |
| --- | --- | --- |
| P0 | 最新 CI [39054828847](https://github.com/lejunyang/ai-auto-desktop/actions/runs/39054828847) 三平台均在 `Enforce the single Rust implementation` 失败。命中的是 `tests/windows/*.py` 硬件探针，不是发布的 Python 实现。 | CI 与 release 保留探针、校验语法，继续禁止其他路径中的 Python 实现；没有删除测试。 |
| P0 | 较早 [34699693104](https://github.com/lejunyang/ai-auto-desktop/actions/runs/34699693104) 仅 macOS helper smoke 失败，结构化错误为 `DRIVER.TIMEOUT`、`dispatch_started=false`。Swift `serve` 用 `readData(ofLength: 65_536)` 读取持久 stdin 管道，短请求可能等待填满或 EOF。 | 使用 `availableData`；新增 `helper_protocol` 原生回归，连续两次请求并保持输入管道打开。由 macOS CI 验证，不修改超时或绕过 smoke。 |
| P0 | [`aad-cdp/src/ws.rs`](../../crates/aad-cdp/src/ws.rs) 直接连接 discovery 返回的任意主机，与只连接回环的文档约束矛盾；路径未防 HTTP 请求注入。 | 只接受显式回环 IP / `localhost`，不进行 DNS 解析，拒绝控制字符；nonce 使用随机 16 字节、每帧随机 mask。mock 回归拒绝远端/伪装域名/注入。 |
| P1 | 同文件：重组分片时错误使用最后一帧 continuation opcode，导致 `Client::send` 丢弃合法文本响应并超时；此前仅限制单帧大小。 | 保留首帧 opcode，校验 continuation 顺序，限制完整消息总长度；回归覆盖分片响应和非法顺序。 |
| P1 | [`aad-mcp/src/lib.rs`](../../crates/aad-mcp/src/lib.rs) 过去使用 `native_driver()` 的共享磁盘快照，连接之间能读取对方引用。 | MCP 使用连接私有内存快照；CLI 保留跨进程引用。mock 验证另一 session 的引用被拒且零写入。此隔离不是同一 OS 账号内恶意进程的安全边界。 |
| P1 | [`aad-uia/src/lib.rs`](../../crates/aad-uia/src/lib.rs) 的 `native_driver` 只有 Windows；[`aad-mcp/src/lib.rs`](../../crates/aad-mcp/src/lib.rs)、CLI `apps/describe/find/do` 都绑定 UiaDriver。Linux AT-SPI 与 macOS AX provider 是单独运行时能力。 | 统一交互 facade 尚未实现，README 所述“三入口同 driver”只适用于 Windows UIA。下一步先定义 provider 无关合约，再接入现有两种 provider。 |
| P1 | [`aad-cdp/src/lib.rs`](../../crates/aad-cdp/src/lib.rs) 是页面文本与活动观察客户端；未公开 observe/find/act 浏览器工具，没有 DOM 元素引用、导航/点击/输入动作或浏览器 provider manifest。 | 目前不能称为完整 browser-use。应新增受观察引用约束的 browser provider，复用合约，而非把任意 `Runtime.evaluate` 暴露为 AI 工具。 |
| P1 | MCP 只有 stdio，无远程监听、认证/授权或远端 session API。CDP 只有本机明文调试端口。 | 当前可用 SSH 承载 stdio，见下例。托管的多租户远程控制需要单独设计与审批，不能把裸 CDP 或 MCP stdio 直接映射成公网服务。 |
| P2 | [`aad-cli/src/browser_launch.rs`](../../crates/aad-cli/src/browser_launch.rs) 请求的浏览器在非 login 模式下没有传给 `aad-browser`；helper 自动选第一个浏览器。login 模式还会关闭浏览器并复制原有登录资料。 | 不在本次云端执行这些路径；后续需要显式选择浏览器、独立临时 profile 生命周期与登录副本权限/清理审查。 |
| P2 | [`aad-cli/src/cdp_watch.rs`](../../crates/aad-cli/src/cdp_watch.rs) 用标题/第一个 tab 猜测前台页面；断开的缓存 client 没有移除重连；活动读取错误被当作空队列。 | 观察不能作为动作授权依据。后续显式绑定 target id，失连重建并报告缺口；不扩大录制/观察产品范围。 |
| P2 | [`aad-cdp/src/lib.rs`](../../crates/aad-cdp/src/lib.rs) 的 JS 队列无上限，按 label 合并不同输入，click 与 input 的相对时序会变化；Runtime exception / malformed drain 可变成空活动。 | 暂不保证无损事件记录或可重放语义。密码 value 已遮蔽，但页面文本、URL、普通表单仍属敏感数据，不能默认远端上传。 |
| P2 | [`aad-runtime/src/recordings.rs`](../../crates/aad-runtime/src/recordings.rs)、[`aad-runtime/src/durable_exec.rs`](../../crates/aad-runtime/src/durable_exec.rs) 保留 Recording 源文件和编译 Workflow，持久写 action 默认拒绝，unknown effect 不自动重放。 | 保留这些保守规则。桌面录制主要围绕 Windows，不能宣传浏览器/跨平台统一录制已可用。 |
| P2 | Rust pin 已是 1.98.0，但 osdk 未声明组件或任务，锁只有 windows-x64。 | 使用发布版 osdk 0.0.6 的 `use/lock` 生成 Linux 锁与组件声明，加入与 CI 一致的 Rust tasks。Windows 历史锁条目保留，其他平台应在本机重新 lock。 |

## 跨 desktop / Android 合约提案（未实现，供父任务对齐）

统一的是外层请求与能力发现，不强制 UIA、AT-SPI、AX、CDP、Android 节点模型相同。
保留现有 provider 动作 ID 与工作流兼容性，通过 adapter 接入，避免一次破坏性重写。

* Session：`session_id`、执行端实例 / 用户会话、协议版本、允许的 provider 与权限、
  有界生命周期；本地/SSH/将来 TLS transport 共用同一请求语义。重连生成新引用域。
* Target：`session_id + provider + resource_id + snapshot_id + revision + node_id`。
  浏览器另绑定 page/frame/document generation；Android 绑定 device / display / app / window。
  引用不可跨执行端或 session 使用，导航、设备断连、权限撤销时失效。旧紧凑引用可仅在
  单 session adapter 内继续使用，不能被远程路由解析成全局句柄。
* Action：请求 ID、能力 ID/版本、target、参数、deadline、幂等性声明、授权范围。
  先验证 capability 支持，再观察/重校验目标；禁用 AI 裸坐标和任意 JS/脚本旁路。
* Result：请求/会话 ID、provider、执行时间、输出、证据/快照引用；错误沿用
  `code/category/phase/retryable/effect`，区分 `not_applied/contextual/applied/unknown`。
  输入已提交不等于业务成功，后置条件单独回读；失连后未知写入不得自动重试。
* 远端权限：观察、捕获、输入、应用启动、文件/工作流读写分别授权；拒绝客户端任意选择
  本地可执行路径、profile 或设备。服务端将 principal / session / resource 绑定，不能仅凭
  客户端传来的设备或窗口标识放行。原生桌面焦点写入按资源串行化。
* Event：后续再增加 session 内单调 `seq`、source、采集时间与接收时间、快照关联、
  dropped/gap/backpressure。录制引用重定位与动作执行日志分离，不能把事件“看起来发生”
  当成成功执行证明。

## 现阶段远程示例

在已授权的专用 Windows 测试会话上安装 aad，由现有 SSH 做账号认证、主机验证与加密。
支持 stdio command 配置的 MCP 客户端可使用：

```json
{
  "mcpServers": {
    "desktop-test": {
      "command": "ssh",
      "args": ["-T", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes",
               "automation@test-host", "C:/tools/aad.exe mcp"]
    }
  }
}
```

示例不会自动创建账号、密钥、授权或桌面会话。本次未实际建立 SSH / 控制真实设备。
每个连接运行独立进程；`AAD_RECORDINGS_DIR`、工作流目录和工件目录仍需由管理员按账号/
用途隔离。MCP 的私有快照不能隔离同一账号的共享工作流文件，也不能阻止不同连接竞争
同一个前台窗口。先使用专用单操作者测试会话，不能据此宣称多租户远程服务已完成。
Linux/macOS 当前只能通过各自 provider 工作流运行，不能借 SSH 自动获得未接入的 MCP GUI 能力。

## 验证边界

云端 Linux 无真实桌面、macOS AX 或 Windows UIA；新增协议/driver 测试用 loopback mock、
临时目录、fake backend。Windows/macOS 原生结果以本分支 CI 为准，真机键鼠、TCC、
Wayland、Electron、登录 profile 复制与跨机 SSH 均 `[待验证]`，需要独立授权测试环境。
本环境安装了 bwrap/prlimit，但容器禁止用户命名空间，完整测试中 6 个 Python 脚本沙箱
效果用例报 `bwrap: setting up uid map: Read-only file system`，未放宽隔离或修改测试。

本次的精确测试结果和最终 CI 链接记录在 PR 描述中；本报告中的能力缺口不应被解读为已实现功能。
