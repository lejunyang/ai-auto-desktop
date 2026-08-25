# KDE/X11 真实应用资格矩阵

> 实测日期：2026-08-25。本矩阵只声明当前固定环境中的只读 AT-SPI 观察能力，
> 不等同于任意 KDE、Qt、发行版、Wayland 或写动作已获支持。

## 结论

本机 `veLinux 2`、KDE Plasma `5.27.5`、Qt `5.15.8`、X.Org `1.21.1.7`、
`DISPLAY=:10.0` 上，Konsole `22.12.3` 与 System Settings `5.27.5` 均由本任务
新启动，并通过生产 `desktop.linux_atspi` driver 的精确 PID selector 注册和读取。
两份有界快照均未截断。

| 应用 | 结果 | 注册延迟 | 快照延迟 | 元素数 | 暴露的语义动作 |
| --- | --- | ---: | ---: | ---: | --- |
| Konsole | `supported / observed_read_only` | 406.26 ms | 1089.88 ms | 352 | focus 61、invoke 30、set_text 16 |
| System Settings | `supported / observed_read_only` | 1614.83 ms | 696.78 ms | 256 | focus 27、invoke 9 |

这里的“暴露动作”只表示 snapshot 中 driver 根据 AT-SPI 信息声明了相应语义能力；
本次 qualifier 没有调用它们，不能据此宣称写动作通过。System Settings 若在其他环境
15 秒内没有以本次启动 PID 注册，将记录为 `unsupported`，不会作为 skip 或 pass。

## 安全和隔离

入口是 `tests/linux/kde_app_qualifier.py`。外层恢复当前用户的 KDE/X11 display 后，
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
清理结果。`unsupported` 是有效的资格结论，不会伪装成成功。

运行：

```bash
PYTHONPATH=src python tests/linux/kde_app_qualifier.py \
  --output artifacts/kde-x11-qualification.json
```

确定性契约测试不启动 GUI：

```bash
PYTHONPATH=src python -m unittest tests.test_linux_kde_qualification -v
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

定向命令与输出摘要：

```bash
# 安装本轮唯一缺少的测试入口；其余 GTK/Qt/AT-SPI/X11 开发依赖均已存在
sudo -n apt-get install -y --no-install-recommends python3-pytest
# 结果：0 upgraded, 7 newly installed；python3-pytest 7.2.1-2

PYTHONPATH=src /usr/bin/python3 -m pytest -q \
  tests/test_linux_atspi_driver.py tests/test_linux_kde_qualification.py
# 结果：36 passed in 0.70s

sh plugins/linux_atspi/build_x11_xtest_helper.sh
# 结果：生成 .build/x11_xtest_helper；链接 libX11.so.6 与 libXtst.so.6

PYTHONPATH=src /usr/bin/python3 -m pytest -vv -rs \
  tests/test_linux_atspi_native.py
# 结果：5 passed, 5 skipped in 25.63s

PYTHONPATH=src /usr/bin/python3 tests/linux/kde_app_qualifier.py \
  --output artifacts/kde-x11-qualification.json
# 结果：2 supported, 0 unsupported, 0 error；总耗时 4890.09 ms
```

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
- 尚未执行任何低风险写动作；若以后增加，仍须限定自有 PID、使用 `find` 返回的 target 与
  exact locator、重新抓树验证后置条件，并在报告中逐项记录。
- 本轮只在自有 fixture 上执行语义写动作；真实 KDE 应用资格矩阵保持只读。活动 KDE
  display 处于锁屏状态，因此未覆盖已解锁桌面上的 XTEST 后置条件。
