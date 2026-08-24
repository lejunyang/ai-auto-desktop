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
| Konsole | `supported / observed_read_only` | 406.64 ms | 1066.61 ms | 352 | focus 61、invoke 30、set_text 16 |
| System Settings | `supported / observed_read_only` | 1597.05 ms | 724.27 ms | 256 | focus 27、invoke 9 |

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

## 当前边界

- 这轮只覆盖应用初始窗口的一次有界快照，没有覆盖对话框、多窗口、动态页面、虚拟列表、
  多显示器或 DPI。
- name 非空比例不应被当成所有控件均有可用 accessible name；完整 role/name/value/state
  数值以忽略的 JSON artifact 为准。
- 尚未执行任何低风险写动作；若以后增加，仍须限定自有 PID、使用 `find` 返回的 target 与
  exact locator、重新抓树验证后置条件，并在报告中逐项记录。
