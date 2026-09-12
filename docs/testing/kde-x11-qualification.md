# KDE/X11 真实应用资格矩阵

> 实测日期：2026-08-25。本矩阵只声明当前固定环境中的只读 AT-SPI 观察能力，
> 不等同于任意 KDE、Qt、发行版、Wayland 或写动作已获支持。

## 结论

本机 `veLinux 2`、KDE Plasma `5.27.5`、Qt `5.15.8`、X.Org `1.21.1.7`、
`DISPLAY=:10.0` 上，Dolphin `22.12.3`、Konsole `22.12.3`、System Settings
`5.27.5` 与仓库自有 Qt Quick/QML fixture 均由本任务新启动，并通过生产
`desktop.linux_atspi` driver 的精确 PID selector 注册和读取。四份有界快照均未截断。

| 应用 | 结果 | 注册延迟 | 快照延迟 | 元素数 | 暴露的语义动作 |
| --- | --- | ---: | ---: | ---: | --- |
| Dolphin | `supported / observed_read_only` | 611.59 ms | 1188.59 ms | 358 | focus 56、invoke 15、set_text 3 |
| Konsole | `supported / observed_read_only` | 411.30 ms | 974.04 ms | 352 | focus 61、invoke 30、set_text 16 |
| Qt Quick/QML fixture | `supported / observed_read_only` | 506.72 ms | 19.42 ms | 5 | focus 2、invoke 1、type_text 1 |
| System Settings | `supported / observed_read_only` | 1432.18 ms | 770.02 ms | 256 | focus 27、invoke 9 |

这里的“暴露动作”只表示 snapshot 中 driver 根据 AT-SPI 信息声明了相应语义能力；
本次 qualifier 没有调用它们，不能据此宣称写动作通过。Dolphin 只打开 qualifier 临时空目录，
不会枚举用户真实 home；QML 项是仓库自有 fixture，用于验证 Qt Quick 语义树而非第三方应用。
System Settings 若在其他环境
15 秒内没有以本次启动 PID 注册，将记录为 `unsupported`，不会作为 skip 或 pass。
另一个专用原生测试已对自有 QML 按钮执行 exact `Press`，并从 fresh snapshot 观察状态 label
变化；该测试只使用 AT-SPI `Action.do_action`，不使用 XTEST、OCR 或坐标输入。
独立的真实应用写动作测试还会启动发行版 KCalc 22.12.3，在禁用 TCP 并使用一次性
Xauthority 的私有 Xvfb/KWin、私有 session/AT-SPI bus 与临时 HOME/XDG 中依次精确定位
`1`、`+`、`2`、`=` 按钮。每次动作前都重新抓取未截断快照并重新定位：语义路径只接受
`Action.do_action` 的 exact `Press`；显式 pointer 路径只使用 fresh bounds 中心点，并经过
AT-SPI subtree hit-test 与 X11 PID/focus 复核后提交 XTEST 左键事件。两条路径最后都从
fresh snapshot 读取同一显示控件的 `3`。这不改变上表四应用只读 qualifier 的零写动作判定，
也不使用 OCR 或截图。

## 安全和隔离

> 本节及后续复测记录描述迁移前的历史 qualification harness。对应 Python runner 已在
> Rust provider 与非 Python Qt fixture 建立后删除，不再是当前 CI 或发布入口。当前自动回归
> 使用 `tests/linux/run-rust-atspi-fixture.sh`；如需恢复旧真实应用矩阵，应以 Rust 编写新的
> qualifier，而不是恢复第二套 Python provider/runtime。

历史入口曾由 Python qualifier 提供。它在外层恢复当前用户的 KDE/X11 display 后，
通过 `dbus-run-session` 创建私有 session bus；AT-SPI bus 也由该私有会话按需启动。
`HOME`、XDG config/cache/data/state/runtime 均为临时目录，因此不会加载或修改用户配置。

每个应用单独以 `start_new_session=True` 启动。qualifier 只接受同时满足以下条件的应用：

1. AT-SPI `process_id` 与 `Popen.pid` 完全一致；
2. 该 PID 仍存活、属于当前用户；
3. 该 PID 是本次创建进程组的组长；
4. snapshot 再使用 PID，并叠加已观测到的 bus name 与 toolkit 精确选择。

清理只向上述自有进程组发送 TERM，三秒后仍存活才发 KILL。不会选择、聚焦或关闭用户
已有窗口。本轮不截图、不 OCR、不调用 focus/invoke/set_text/type_text/toggle/expand/
collapse，也不保留节点名称和值；机器报告只保留完整度计数、role 分布和动作计数。

## 机器结果和判定

运行产物写入 `artifacts/kde-x11-qualification.json`（该目录被 `.gitignore` 排除）。
机器格式版本为 `ai-auto-desktop.kde-x11-qualification/v1`。每个应用有三种结果：

- `supported`：精确自有 PID 已注册，且有界 snapshot 成功；
- `unsupported`：程序缺失、提前退出，或 15 秒内未注册到私有 AT-SPI registry；
- `error`：driver、协议、超时或快照调用发生错误。

报告包括主机/Plasma/Qt/X11/应用版本、backend、注册和 snapshot 延迟、编码大小、
`truncated`、元素数、role/name/description/value/state 完整度、driver 语义动作、错误和
清理结果。`unsupported` 是有效的资格结论，不会伪装成成功。qualifier 的退出码也代表
资格结论：只有默认 `APP_SPECS` 中每个必选应用均为
`supported / observed_read_only`、snapshot 未截断、自有进程组清理成功且
`writes_dispatched` 为空时才返回 0；`unsupported`、`error` 或任一安全条件不成立均返回非 0，
但报告仍会原子写入指定路径。外层私有 D-Bus 运行达到 75 秒上限时，会依据启动时记录的
PID、进程组和 Linux `/proc` starttime 有界清理已观测到的自有进程树，再写入 `error` 报告。
`--app` 子集运行可用于定向诊断并照常写 JSON，但不是完整矩阵，因此 qualifier 和严格
验真器都会返回非 0，不能将子集误当成整套资格通过。

上述历史 qualifier 与验真器已随 Python 副本删除。当前可重复的自动测试命令是：

```bash
tests/linux/run-rust-atspi-fixture.sh
```

它生成私有 Xvfb/AT-SPI 会话并运行 Rust driver；若要重新建立 KDE 真实应用矩阵，需新增
Rust qualifier 和结果验真器。

验真器只读取不超过 1 MiB 的普通 JSON 文件，拒绝 symlink、非普通文件、重复 JSON key、
非有限数值与不支持的 schema。历史实现从同版本的应用清单常量读取
必选应用集合（因此 Dolphin、QML fixture 或后续新增项不会被静态名单漏掉），并核对顶层与
summary 一致性、应用唯一性和完整性、`support_level`、snapshot bounds/`truncated`、
launch/AT-SPI/snapshot 的 exact PID、聚合且无 UI 文本的 content retention、安全隔离字段、
cleanup 证明以及零写动作。通过时输出 verifier schema
`ai-auto-desktop.kde-x11-result-verifier/v1`、`qualified=true` 并返回 0；格式有效但资格不通过
以及格式不可信时均 fail closed、输出机器可读错误并返回非 0。

当前 Rust provider 的确定性契约测试不启动 GUI：

```bash
cargo test --locked -p aad-atspi
```

## 本机原生复测记录

2026-08-25 的复测宿主是 `veLinux GNU/Linux 2 (lyra)`、内核
`5.15.120.bsk.3-amd64`、x86_64。活动图形会话由 `xrdp-sesman` 启动，
`loginctl` 报告 session `c2` 为 active X11，实际进程为 X.Org `:10.0`、
`kwin_x11` 和 Plasma `5.27.5`。`xdpyinfo` 确认 X.Org `1.21.1.7`，扩展列表
包含 `XTEST`；`org.a11y.Bus.GetAddress` 返回当前用户的 AT-SPI bus 地址。

测试开始时该会话的 `LockedHint=yes`，KDE 屏保也报告 active。因此结果严格
区分以下三类：

- **真实 KDE display 通过**：自有 GTK3 fixture 在 `:10.0` 上通过
  `snapshot/find/focus/set_text/invoke/toggle/expand/collapse`；自有 Qt 5 Widgets
  fixture 通过 `snapshot/find/focus/set_text/invoke`。两者使用生产 driver 和真实
  AT-SPI bridge，不使用 OCR 或坐标点击。
- **本机私有 X11 通过**：私有 Xvfb、私有 AT-SPI bus 上，GTK3 与 Qt5 fixture
  均通过 XTest helper 输入 UTF-8 文本并由 fresh snapshot 验证后置条件；负向测试
  同时证明 helper 在 Wayland profile 或焦点 PID 不匹配时会在派发前拒绝。
- **真实 KDE display 输入跳过**：因 `LockedHint=yes`，测试没有尝试向锁屏会话
  注入按键。`XTEST` 扩展可见不等于锁屏后的应用能够接收事件，因此本轮不能声明
  已解锁 KDE 桌面的 `type_text` 端到端通过。

当时的定向命令和计数属于历史证据，不再作为当前仓库可执行说明。Rust 迁移后的复测记录
以 `cargo test --workspace --exclude aad-gui --locked` 和上面的 native fixture 为准。

native suite 的五个 skip 均有明确边界：Atspi typelib 已安装而无需测试 Gio
fallback；长期桌面 registry 当时没有应用，两个基础设施 smoke 因此跳过；System
Settings 没有注册到长期 registry；真实 KDE display 的 GTK XTEST 用例因锁屏跳过。
System Settings 随后在 qualifier 的私有 bus 中以精确自有 PID 成功注册并完成快照，
所以长期 registry 的 skip 不影响上表只读资格结论。

同一环境运行只读 capability probe 时，最初使用完整 `xdpyinfo` 输出触发了 65,536 bytes
通用上限，产生 `linux.x11=unknown` 误阴性。probe 已改为读取根窗口单个属性的有界
`xprop` 查询，并在同一 `:10.0` 会话复测为 `linux.x11=available/query=ok`。最终汇总为
3 项 available、1 项 degraded、2 项 unavailable、0 项 unknown；其中 AT-SPI、X11 与
RemoteDesktop portal 可用，uinput 因当前进程不可写而 degraded，Wayland 与 libei 不可用。

## 当前边界

- 这轮只覆盖应用初始窗口的一次有界快照，没有覆盖对话框、多窗口、动态页面、虚拟列表、
  多显示器或 DPI。
- name 非空比例不应被当成所有控件均有可用 accessible name；完整 role/name/value/state
  数值以忽略的 JSON artifact 为准。
- 只读 qualifier 本身仍不派发写动作；真实 KCalc 的 `1+2=3` 是独立、隔离的定向测试，
  不能替代对 Dolphin、Konsole、System Settings 或任意第三方页面的写资格。
- 自有 fixture（含 Qt Quick/QML）已覆盖更多语义动作；真实 KDE 应用目前只额外覆盖 KCalc
  的四次 exact `Press`、四次显式中心点 pointer click 与结果回读。该 pointer case 在私有
  Xvfb + 私有 KWin 中运行；活动 KDE display 仍处于锁屏状态，因此未覆盖用户真实桌面上的
  XTEST 后置条件。
