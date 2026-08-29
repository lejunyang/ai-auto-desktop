2026-08-29，Windows 11 26200 / AMD64 / Python 3.13.13 / 250% 缩放 / medium integrity / Session 1。

本文记录录制回放设计（`docs/spec/recording-session-v1alpha1.md`、`docs/architecture/record-replay.md`）所依据的实测数据。设计中每一处「实测」表述都对应本文的一项测量。记录目的是让结论可复核、可在其他环境重测，而不是让读者相信设计文档的断言。

## 1. locator 唯一性与跨会话稳定性

方法：启动 Notepad，用 `ComtypesUIABackend.capture(max_depth=6, max_nodes=120)` 抓取窗口子树（得 46 节点，未截断）；关闭并重新启动，再抓一次。「唯一率」为该字段组合在单次快照内只匹配 1 个节点的节点占比；「稳定率」为第一次出现的组合在第二次快照中仍存在的占比。

| locator 字段组合 | 唯一率 | 跨会话稳定率 |
| --- | --- | --- |
| `role` | 10.9% | 100% |
| `role+name` | 82.6% | 100% |
| `role+automation_id` | 43.5% | 100% |
| `role+class_name` | 19.6% | 100% |
| `role+name+automation_id` | 84.8% | 100% |
| `role+name+class_name` | **87.0%** | 100% |
| `role+automation_id+class_name+framework_id` | 50.0% | 100% |
| `role+name+automation_id+class_name+framework_id` | 87.0% | 100% |

结论：`role+name+class_name` 之后继续追加字段，唯一率不再提升（87.0% → 87.0%）。这是 spec §7.2「达到唯一即停止收窄」规则的依据——多余字段只增加对 UI 变更的脆弱性。

## 2. 收窄算法原型验证

方法：对 46 个节点逐个运行 spec §7.2 的收窄算法（`role` → `+name` → `+class_name` → `+automation_id` → `+framework_id`，唯一即停），再回头验证每个判定为 `unique` 的 locator 是否真的只解析出该节点。

| 策略 | 节点数 | 占比 |
| --- | --- | --- |
| `unique` | 40 | 87.0% |
| `scoped` | 3 | 6.5% |
| `ordinal` | 3 | 6.5% |

达到唯一所需字段数：1 个字段 5 节点，2 个字段 33 节点，3 个字段 2 节点。**多数节点仅需 2 个字段**。

回验结果：40 个 `unique` locator，**0 处错配**。

## 3. 歧义节点与消歧手段的有效性

在 `role+name+class_name` 下仍有 2 组共 6 个节点无法区分：

| 组 | 节点数 | 父节点数 | 祖先能否消歧 |
| --- | --- | --- | --- |
| `pane` / `""` / `Microsoft.UI.Content.DesktopChildSiteBridge` | 3 | 1 | **否**（共享同一父节点） |
| `pane` / `""` / `InputSiteWindowClass` | 3 | 3 | 是 |

这是 spec §7.2 要求 `scoped` 策略**必须实测验证祖先确实不同**、而非假定「加上父节点就唯一」的依据。第一组只能靠序号（`ordinal`，标记 `fragile`）或人工处理。

树深度分布：`{0:1, 1:5, 2:8, 3:24, 4:5, 5:1, 6:2}`。

## 4. 捕获机制：可访问性事件是否足够

方法：注册 UIA `AddFocusChangedEventHandler`，**不安装任何键盘或鼠标钩子**，观察捕获到的事件内容。

捕获到的事件样本：

```
{'role_id': 50020, 'name': 'PowerShell 7', 'class_name': 'TermControl',
 'automation_id': '', 'process_id': 37496}
```

结论：元素身份（role / name / class_name / automation_id / process_id）可仅凭可访问性事件获得，合成 locator 所需字段齐备。因此 spec §5.1 禁止全局输入钩子是可行的，不是以功能为代价的空洞约束。

`IUIAutomation` 实际暴露的事件注册方法：`AddAutomationEventHandler`、`AddFocusChangedEventHandler`、`AddPropertyChangedEventHandler`、`AddStructureChangedEventHandler` 及对应 `Remove*`。

同时确认：三个 driver 源码中均**不存在**任何事件注册符号（`AddAutomationEventHandler` / `AXObserver` / `atspi_event` / `SetWinEventHook`），即 driver 目前是纯请求/响应的，录制需要新增能力。

## 5. UI 工具链可用性

| 选项 | 状态 |
| --- | --- |
| `tkinter` | 可 import，但 `Tk()` 运行期失败：`Tcl wasn't installed properly` |
| `PySide6` / `PyQt6` / `wx` | `ModuleNotFoundError` |
| `http.server` / `json` / `sqlite3` / `webbrowser` / `ssl` / `secrets` | 全部可用 |
| 默认浏览器 | 可用（`WindowsDefault`） |
| `node` / `npm` / `cargo` / `dotnet` | 存在；`go` 缺失 |

项目当前依赖仅 `PyYAML`、`jsonschema`、`Pillow`——无任何 GUI 或 Web 框架。这是架构 §5 选择 stdlib HTTP + 浏览器渲染的依据。

本地服务安全性实测：绑定 `127.0.0.1` + 端口 0，`X-Recorder-Token` 请求头校验。结果：带正确 token → 200；缺 token → 401；从本机非回环地址连接同一端口 → **超时不可达**。

## 6. 编译模型验证

方法：构造一份含三种步骤类型（interaction / assertion / logic-condition）的录制，按 spec §8.1 展开为 `snapshot → find → 动作`，assertion 附加为 postcondition，然后分别用 JSON Schema 与项目自带编译器校验。

- 通过未修改的 `schemas/workflow/v1alpha1/workflow.schema.json`。
- 被 `ai_auto_desktop.compiler.compile_descriptor()` **接受**。
- 依赖规范化结果（含嵌套 scope 内独立成链）：

```
focus_editor__snapshot     depends_on=()
focus_editor__find         depends_on=('focus_editor__snapshot',)
focus_editor               depends_on=('focus_editor__find',)
enter_note__snapshot       depends_on=('focus_editor',)
enter_note__find           depends_on=('enter_note__snapshot',)
enter_note                 depends_on=('enter_note__find',)   +postcondition
only_if_requested (if)     depends_on=('enter_note',)
   then: click_save__snapshot depends_on=()
   then: click_save__find     depends_on=('click_save__snapshot',)
   then: click_save           depends_on=('click_save__find',)
```

产物见 `examples/recordings/save-note.recording.yaml` 与 `examples/recordings/save-note.compiled.json`。

## 7. 重排与删除的静态可捕获性

这组测量**推翻了设计初稿的假设**，据此修改了架构 §6 与 spec §8。

| 场景 | 现有编译器行为 |
| --- | --- |
| 把引用 `steps.X.output` 的步骤移到 X 之前 | **拒绝**：`uncovered_step_reference: steps reference 'enter_note__snapshot' is not covered by depends_on` |
| 删除被引用的步骤，保留引用方 | **接受**（依赖被规范化为前一 sibling，悬空引用未被发现） |
| 引用完全不存在的 `steps.no_such_step` | **接受** |

原因：现有校验语义是「引用必须被 `depends_on` 覆盖」，而缺失的 step 不产生依赖，因此不触发该检查。

结论：顺序倒置可复用现有校验；**引用完整性必须由录制编译器自行校验**（spec §8 已据此改为规范要求），这也是 UI 默认「禁用」而非「删除」的直接理由。

## 8. 脱敏必要性

一次 `max_nodes=120` 的 Notepad 快照中，`document` 节点的 `value` 字段包含了当前打开文件的**全部正文**；窗口标题包含文件名（`favorite.txt - Notepad`）。语义树带出用户数据是正常行为而非边缘情况，这是 spec §5 要求 `value_policy` 默认 `drop`、`title_policy` 默认 `drop`、`screenshots` 默认 `none` 的依据。

## 9. 复现方式

各项测量均可用 `ComtypesUIABackend` 直接复现：抓取快照后按 §1/§2 的定义计算字段组合的唯一率与稳定率；§4 注册 focus handler 并观察事件字段；§6/§7 将编译产物交给 `compile_descriptor()`。注意 §1 与 §3 的具体数字与 Windows 版本、Notepad 版本相关，重测时应关注**结论方向**（收窄到 2–3 字段即饱和、祖先并非总能消歧）而非精确百分比。

## 10. 脚本执行与 Windows 隔离原语

支撑 `docs/architecture/script-execution.md` 的实测。

### 10.1 Windows 脚本沙箱：实测强制与未强制

Windows 脚本沙箱已实现（`_execute_windows`，复用 `_win_job.WindowsJob` 的资源上限）。以下每项都通过「尝试违反 → 确认被拦」验证：

| 边界 | 实测结果 |
| --- | --- |
| 内存上限（512 MiB） | 分配 2 GiB → `SCRIPT.EXIT_NONZERO` |
| 挂钟超时 | `sleep(60)` 在 3s 超时下 → `SCRIPT.TIMEOUT` |
| 进程树回收 | 脚本 spawn 的孙进程在步骤返回后不再存活 |
| 空环境 | `len(os.environ)` == 0 |
| 隔离 cwd | `os.listdir('.')` == `[]` |
| 隔离解释器 | `sys.flags.isolated` 与 `no_user_site` 为真 |
| 非 JSON stdout | `SCRIPT.OUTPUT_INVALID` |
| 超出 `max_output_bytes` | `SCRIPT.OUTPUT_INVALID` |
| 非零退出 | `SCRIPT.EXIT_NONZERO`，`details.returncode` == 3 且含 stderr |

端到端（经真实 CLI，`examples/workflows/windows-script-sandbox.yaml`）：不带 `--allow-scripts` → `SCRIPT.SANDBOX_DENIED`；带该参数 → `succeeded`，输出
`{"count": 8, "total": 31, "mean": 3.875, "median": 3.5, "sorted": [1,1,2,3,4,5,6,9]}`，数值经独立核对正确。

**未强制的边界，以及一处容易误判的现象。** Windows 无 per-process 网络/mount 命名空间，`network` 与 `filesystem` 未隔离。需要特别说明：在 `env={}` 下脚本的 socket 调用确实会失败（`WinError 10106`，winsock 无法初始化；DNS 报 `gaierror`），但这**不是**沙箱在拦网络——把 `SystemRoot` 加回环境后，同一脚本成功连上本地监听端口并收到数据。故网络不可用属空环境的副作用，`sandbox_availability()` 必须继续把 `network` 列为 `gaps`。

探针以 `script.sandbox` 报告该状态（本机 `degraded`，`not_enforced=["network","filesystem"]`）。探针**不**输出解释器路径：用户目录下的路径含账号名，会触发既有的「报告不得泄漏环境标识值」契约——该契约在实现过程中确实捕获了这一泄漏（`'ljy' unexpectedly found`），已改为只报告布尔事实 `interpreter_resolved`。

空环境未牺牲纯计算能力：19 个常用计算/数据模块全部可导入，`tempfile` 在无 `TEMP`/`TMP` 时可用；仅 `zoneinfo` 因 Windows 缺系统 tzdata 抛 `ZoneInfoNotFoundError`，与沙箱无关。

### 10.2 Windows 隔离原语可用性

| 目标 | Windows 手段 | 实测结果 |
| --- | --- | --- |
| 内存上限 | Job Object `ProcessMemoryLimit` | **有效**：256 MiB 上限下分配 512 MiB → rc=1，未输出 `ALLOCATED` |
| 进程数上限 | Job Object `ActiveProcessLimit` | `SetInformationJobObject` 返回 True |
| 降权 | `CreateAppContainerProfile` | **成功**：`HRESULT 0x00000000`，无需提权 |
| 清空环境 | `subprocess(env={})` | **有效**：脚本内 `len(os.environ)` == 0 |
| 隔离工作目录 | `cwd=` 临时目录 | **有效** |
| 进程树回收 | Job Object `KILL_ON_JOB_CLOSE` | 项目已实现（`_win_job.WindowsJob`） |

`_win_job._JOBOBJECT_EXTENDED_LIMIT_INFORMATION` 已包含
`ProcessMemoryLimit`、`JobMemoryLimit`、`PerProcessUserTimeLimit`、`ActiveProcessLimit`；当前仅设置
`KILL_ON_JOB_CLOSE` 一个 flag。

**尚未验证**（不得据此声称与 Linux 等价）：AppContainer 是否真的拒绝网络（Windows 无 per-process 网络命名空间），以及是否真的限制文件系统访问（无 per-process mount 命名空间，无法 `--ro-bind`）。这两项必须真机验证后才可把 Windows 脚本沙箱标为 `available`。

另：Linux 路径硬编码 `/usr/bin/python3`；Windows 无等价固定路径，实测 `sys.executable` 指向嵌入式 runtime，故 Windows 实现必须显式发现并固定解释器路径。
