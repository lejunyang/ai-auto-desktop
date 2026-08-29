# 脚本执行现状与启用路径

状态：Draft，2026-08-29。本文回答：脚本能执行什么、会传入什么、各平台沙箱强制了哪些边界、又有哪些边界**没有**强制。Linux 使用 bubblewrap，状态为 `available`；Windows 使用 Job Object，状态为 `degraded`（网络与文件系统未隔离）；macOS 仍 fail-closed。所有结论均为本机实测，并有回归测试覆盖，不是读码推断。

## 1. 能执行什么

**只有 Python，且只有一个文件。** schema 中 `scriptStep.runtime` 是 `{"const": "python"}`——`runtime` 不是枚举，而是常量，其他运行时在 schema 层即不可表达。`validate_script_policy()` 再次检查 `runtime != "python"` 并抛 `DESCRIPTOR.UNSUPPORTED_FEATURE`。

脚本来源二选一，schema 用 `oneOf` 强制互斥：

| 字段 | 含义 | 限制 |
| --- | --- | --- |
| `source` | 内联脚本正文 | `maxLength: 1048576`（1 MiB） |
| `entrypoint` | 脚本文件路径 | `maxLength: 4096`，相对路径按 descriptor 所在目录解析 |

`entrypoint` 经 `resolve_entrypoint()` 处理：`resolve(strict=True)` 后必须是**常规文件**，否则 `SCRIPT.START_FAILED`。`source` 则写入临时目录的 `script.py`。

两种来源最终都以**只读绑定**挂进沙箱的 `/workflow/script.py`，并用 `python3 -I` 执行（`-I` = isolated 模式：忽略 `PYTHONPATH`、用户 site-packages 与环境变量）。

`output_schema` 是**必填**字段（schema `required: ["runtime", "output_schema"]`）。

## 2. 会传入什么

**输入通过 stdin 传入，是一个 JSON 值；不是命令行参数，也不是环境变量。**

`runtime.py` 的调用点：

```python
result = execute_python_script(
    self.descriptor, step,
    self._evaluate(thaw(step.params.get("inputs", {}))),   # 先求值表达式
    timeout,
)
self._validate_schema(result, step.params["output_schema"], "SCRIPT.OUTPUT_INVALID", step.id)
```

因此 `inputs` 里的 `${{ }}` 表达式在**宿主侧**求值，脚本收到的是求值后的字面数据。传递方式为
`process.communicate(json.dumps(inputs, ensure_ascii=False, allow_nan=False))`——注意
`allow_nan=False`，即 `NaN`/`Infinity` 会导致序列化失败而非静默传入。

脚本侧的契约：

```python
import json, sys
data = json.load(sys.stdin)          # 读取输入
print(json.dumps({"result": ...}))   # stdout 必须是唯一一个 UTF-8 JSON 值
```

stdout 必须是**恰好一个** UTF-8 JSON 值，否则 `SCRIPT.OUTPUT_INVALID`。返回值随后被 `output_schema` 校验，不符也报 `SCRIPT.OUTPUT_INVALID`。

环境变量只有两个：`PATH=/usr/bin:/bin` 与 `PYTHONIOENCODING=utf-8`（`--clearenv` 先清空一切）。工作目录是 tmpfs 上的 `/tmp`。

已实测验证：一个 `{"n": 21}` → `{"doubled": 42}` 的脚本步骤能通过 `compile_descriptor()`，且
`validate_script_policy()` 通过。

## 3. 沙箱给了什么

`--unshare-all`（网络/PID/IPC/UTS/mount 全部隔离）、`--die-with-parent`、`--new-session`、`--cap-drop ALL`、`--clearenv`；只读绑定 `/usr` 与 `/lib`、`/lib64`；`/proc`、`/dev`、tmpfs `/tmp`；脚本文件只读绑定。

`prlimit` 施加：`--fsize=max_output_bytes`、`--as=536870912`（512 MiB 地址空间）、
`--cpu=ceil(timeout)+1`、`--nofile=64`、`--core=0`。

超时走 `process.communicate(timeout=...)`，超时后 `_kill_process_group()` 先 `SIGTERM` 再 `SIGKILL`（默认 30s）。

## 4. 平台支持：Linux 完整，Windows degraded

脚本执行有两道独立的门，两者都满足才会运行：

1. `runtime._script()` 检查 `--allow-scripts`，否则 `SCRIPT.SANDBOX_DENIED`；
2. 当前平台必须有可用沙箱，否则 `SCRIPT.SANDBOX_UNAVAILABLE`。

`sandbox_availability()` 用能力探针的词表报告第二道门的状态：

| 平台 | state | mechanism | 未强制的边界 |
| --- | --- | --- | --- |
| Linux（`bwrap` + `prlimit` + `/usr/bin/python3` 齐备） | `available` | `linux.bubblewrap` | 无 |
| Linux（缺任一前置） | `unavailable` | `linux.bubblewrap` | — |
| Windows | `degraded` | `windows.job_object` | `network`、`filesystem` |
| macOS 及其他 | `unavailable` | — | — |

该状态同时出现在 `probe` 的 `script.sandbox` 检查里，因此操作者在决定是否传 `--allow-scripts` 之前，可以先看到本机哪些边界是真的。

macOS 仍然 fail-closed：没有实现等价隔离之前，宁可拒绝执行，也不跑不受约束的脚本。

### 4.1 Windows 沙箱强制了什么（实测）

复用已有的 `_win_job.WindowsJob`，为其加上可选的资源上限；进程先以 `CREATE_SUSPENDED` 创建、**assign 进 job 之后才 resume**，因此脚本不存在「先于 job 成员身份运行」的窗口，也无法把子进程放到 job 之外。

以下每一项都由「尝试违反 → 确认被拦」验证，而不是只跑通顺利路径：

| 边界 | 手段 | 实测结果 |
| --- | --- | --- |
| 内存上限 | Job `ProcessMemoryLimit`（512 MiB，对齐 Linux `--as`） | 脚本分配 2 GiB → `SCRIPT.EXIT_NONZERO`，内核拒绝提交 |
| CPU 时间上限 | Job `PerProcessUserTimeLimit` | 随 timeout 计算（`ceil(timeout)+1` 秒） |
| 进程数上限 | Job `ActiveProcessLimit` = 8 | 设置成功 |
| 挂钟超时 | `communicate(timeout=...)` + `TerminateJobObject` | `sleep(60)` 在 3s 超时下 → `SCRIPT.TIMEOUT` |
| 进程树回收 | Job `KILL_ON_JOB_CLOSE` + `TerminateJobObject` | 脚本 spawn 的孙进程在步骤返回后已消失 |
| 空环境 | `env={}` | 脚本内 `len(os.environ)` == 0 |
| 隔离工作目录 | 专用临时目录作 `cwd` | 脚本内 `os.listdir('.')` == `[]` |
| 隔离解释器 | `-I -B -E -s -S` | `sys.flags.isolated` 与 `no_user_site` 均为真 |
| 输出契约 | 与 Linux 共用 `_decode_script_result()` | 超限 / 非 JSON / 非零退出分别报对应错误码 |

解释器路径必须显式解析并固定（Linux 硬编码 `/usr/bin/python3`，Windows 无等价固定路径），且拒绝 `py.exe` 启动器——沙箱必须确切知道自己执行的是哪个二进制。**探针只报告「已解析到解释器」这一布尔事实，不报告路径**：用户目录下的解释器路径含账号名，而探针报告不得携带环境标识值。

### 4.2 Windows 沙箱没有强制什么（必须如实说明）

Windows 没有 per-process 网络命名空间，也没有 mount 命名空间，因此**无法**做到 Linux 的 `--unshare-net` 与 `--ro-bind`。脚本仍然保有运行账号的网络与文件系统可达性。

一个必须澄清的实测细节，避免把偶发现象当成安全边界：在当前 `env={}` 配置下，脚本里的 socket 调用实际会失败（`WinError 10106`，winsock 无法初始化服务提供程序），DNS 解析报 `gaierror`。但这**不是**沙箱在拦网络——把 `SystemRoot` 加回环境变量后，同样的脚本可以成功连接本地监听端口并收到数据。也就是说，网络不可用是空环境的**副作用**，不是强制边界，随时可能因实现调整而消失。

因此 `sandbox_availability()` 坚持把 `network` 与 `filesystem` 列在 `gaps` 中，探针 evidence 也把它们放在 `not_enforced`。**不得**因为「实测连不上网」就宣称网络已隔离。

要把 Windows 提升到 `available`，需要 AppContainer 降权并真机验证网络与文件系统确实被拒。已做过一轮实测，结论是**当前被一个结构性障碍挡住**，详见 §4.3。在此之前状态保持 `degraded`。

顺带确认空环境没有牺牲纯计算能力：`math`、`statistics`、`decimal`、`fractions`、`datetime`、`re`、`itertools`、`functools`、`collections`、`random`、`hashlib`、`base64`、`csv`、`textwrap`、`unicodedata`、`tempfile`、`uuid`、`secrets` 共 19 个模块全部可导入，`tempfile` 在无 `TEMP`/`TMP` 时仍可用。唯一例外是 `zoneinfo` 查询时区抛 `ZoneInfoNotFoundError`——这是 Windows 缺少系统 tzdata 的既有特性，与沙箱无关。

### 4.3 AppContainer 实测结论：被解释器可达性挡住

尝试用 AppContainer 补上 §4.2 的两个缺口，实测结果如下。

**能做到的部分。** `CreateAppContainerProfile` 在无提权下成功（`HRESULT 0x00000000`），可派生容器 SID，`DeleteAppContainerProfile` 可清理。用 `STARTUPINFOEX` + `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` 创建容器内进程也能成功返回 pid。

**挡住的地方：容器进程读不到解释器。** AppContainer 令牌里**没有用户 SID**，因此继承自 `Users` 的授权对它一律无效；它只能访问 ACL 中显式授予其容器 SID、或授予 `ALL APPLICATION PACKAGES` 的对象。实测本机三个 Python 解释器：

| 解释器 | 位于用户目录 | 有 `ALL APPLICATION PACKAGES` 授权 |
| --- | --- | --- |
| 当前运行的解释器 | 是 | 否 |
| `C:\Python311\python.exe` | 否 | 否 |
| `WindowsApps\python.exe` | 是 | 否 |

对比之下 `System32` 下的 `cmd.exe`、`curl.exe`、`PING.EXE` **都有**该授权——这也说明缺失并非偶然，而是 Windows 只给系统组件开放。

于是要让容器跑起来，只有两条路，都不可接受：

1. **给真实解释器路径加 ACL**：这会持久修改用户文件的权限，属于沙箱不该有的副作用；
2. **把整个 runtime 复制到沙箱自有目录**：实测该 runtime 为 79.9 MiB，每次执行脚本都复制不可行。

**一个必须记录的教训：三次测量都差点得出错误结论。** 前两次都报告「网络已被拒绝」，但都是假象：

- 第 1 次用 `&` 串接命令并读 `%errorlevel%`，而 cmd 在**解析期**就展开该变量，所有返回码都是初始的 0，数字毫无意义；
- 第 2 次改用 `^&` 转义，结果容器只是把命令行**原样 echo 出来**，什么都没执行——「拒绝」其实是「从未运行」；
- 第 3 次改用 .bat 文件并加入 `START` 执行标记与容器外**对照组**，才暴露真相：容器进程根本没启动（无 `START` 标记），结论只能是 **INCONCLUSIVE**。

因此本文档对 AppContainer 的网络/文件系统隔离效果**不做任何断言**。可靠的测量必须同时具备：容器外对照组证明目标本可达、以及容器内执行标记证明进程真的跑了。缺少任何一项，「拒绝」都可能只是「没跑起来」。

**后续可行方向**（均未验证，不得据此改状态）：改用带 `ALL APPLICATION PACKAGES` 授权的系统级解释器位置；或让宿主提前把脚本编译为不依赖用户目录 runtime 的形式；或改用 Windows Sandbox / 容器等更重的隔离，代价是启动开销。

## 5. 当前 sandbox 参数的真实自由度

值得指出：`sandbox` 的三个边界在 schema 中都是 `{"mode": {"const": "deny"}}`——**deny 是唯一可表达的值**。因此
`validate_script_policy()` 里对 `mode != "deny"` 的检查在 schema 校验后其实不可能触发，属于防御性冗余（不是缺陷，但说明当前没有「授予网络/文件系统」的表达能力）。

同理 `capabilities` 在 `scriptStep.properties` 中**不存在**，且 `unevaluatedProperties: false`，所以
`validate_script_policy()` 中 `if step.params.get("capabilities")` 这一分支同样不可达。

唯一真正可调的参数是 `max_output_bytes`（1 到 1 GiB）。

结论：当前脚本能力是「**纯计算**」——读 JSON、算、写 JSON。它不能联网、不能读写宿主文件、不能读环境变量、不能调用桌面 driver。**这正是把它作为录制回放「自定义逻辑」载体时的能力边界**：脚本适合做数据变换、条件计算、字符串处理，不适合做需要外部 IO 的事。

## 6. 后续工作

Windows 沙箱已落地并有回归测试（`tests/test_windows_script_sandbox.py`，22 项，其中 2 项在非 Windows 上跳过），并已纳入 CI：`windows-contracts` job 每次 push 都在真机 Windows 上运行沙箱、Job Object 与探针契约，`tests/test_ci_contract.py` 反过来断言该 gate 不会被悄悄移除。

剩余工作及其真实状态：

- **AppContainer 降权：已尝试，被结构性障碍阻塞**（详见 §4.3）。容器令牌没有用户 SID，而本机所有 Python 解释器都缺少 `ALL APPLICATION PACKAGES` 授权，因此容器进程无法启动解释器；绕过它要么持久改动用户文件 ACL，要么每次复制 79.9 MiB runtime，两者都不可接受。**在找到不产生副作用的解释器可达方案之前，状态必须保持 `degraded`。**
- macOS 仍无实现，保持 `unavailable`。

一条流程约束：任何声称「网络/文件系统已被隔离」的测量，都必须包含容器外对照组与容器内执行标记。§4.3 记录的三次失败测量说明，缺少任一项时，「被拒绝」与「根本没运行」在观测上无法区分。

## 7. 与录制回放的关系

若把 `script` 作为录制回放的自定义逻辑载体（`docs/spec/recording-session-v1alpha1.md` §7.5），当前边界是：

- 需要 `--allow-scripts` **且**平台沙箱可用；两者缺一即 fail-closed。
- 脚本是纯计算：适合数据变换与条件判断，**不能**用来做「顺便下载个文件」或「读一下配置」。
- 脚本不能调用桌面 driver。所有 UI 交互必须走 interaction 步骤，这保证了 policy/risk/confirmation 检查不被绕过。

因此建议：录制 UI 中的「自定义逻辑」默认提供 `condition`/`loop`/`assign`/`group`（无需沙箱、跨平台一致），`script` 作为进阶选项，并在 UI 中明示上述两道门与纯计算边界。
