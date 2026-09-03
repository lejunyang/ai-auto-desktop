# ai-auto-desktop 桌面自动化运行时

ai-auto-desktop 让人和 AI 用同一套能力观察并操作桌面：**发现正在运行的程序 → 读出界面结构 → 在被观察到的元素上执行动作**。

核心实现是 Rust，对外提供三个入口：

| 入口 | 形态 | 用途 |
| --- | --- | --- |
| **CLI** | `aad` 命令 | 人工使用、脚本编排、CI |
| **MCP** | `aad mcp`（stdio 上的 JSON-RPC） | 给 AI agent 调用 |
| **GUI** | Tauri + Vue 桌面窗口 | 可视化查看界面、录制并编排步骤 |

三者驱动的是同一个 driver，因此安全模型完全一致，不存在“GUI 能点、CLI 不能点”这类差异。

## 安全模型：先观察，才能操作

这是整个项目的主轴，也是与“坐标点击”类工具的根本区别。

**任何写操作都必须引用一个真实观察过的元素**，引用形如 `snapshot:revision:node`（例如 `a2466628:10:e68`）。执行前 driver 会用捕获时记录的 role / name / automation_id / class_name 与当前活动界面二次比对：

- 界面变了 → 返回 `DRIVER.STALE_HANDLE` 并**拒绝执行**，而不是打到别的元素上；
- 定位命中多个 → 返回 `DRIVER.AMBIGUOUS_MATCH` 并列出候选，绝不“取第一个”；
- 元素不支持该动作 → 返回 `DRIVER.ACTION_UNSUPPORTED` 并列出它支持什么。

MCP 工具 schema **不接受任何** `x` / `y` / `point` / `coordinates` / `position` 字段，并有测试守住这一点：AI 无法绕过观察直接点坐标。

## 快速开始

```powershell
cargo build --release
```

产物是单个 `aad` 可执行文件（Windows 上为 `target\release\aad.exe`）。

```powershell
aad probe                      # 只读检查环境是否具备自动化前置条件
aad apps                       # 列出当前所有可见窗口
aad describe hwnd:12454104     # 读出某个窗口的界面结构
```

`describe` 的每个元素都带一个 `ref` 字段，直接拿它操作：

```powershell
aad find hwnd:12454104 --name "保存"      # 精确定位，返回 ref
aad do invoke    --target abc123:4:e9      # 触发默认动作
aad do click     --target abc123:4:e9      # 点击元素中心
aad do set-value --target abc123:4:e9 --value "写入的内容"
aad do type-text --target abc123:4:e9 --text "逐字输入"
aad do focus     --target abc123:4:e9
```

引用可跨进程使用：一个进程 `find` 得到的 `ref`，另一个进程可以直接 `do`。快照默认落在
`%TEMP%/ai-auto-desktop/snapshots`，可用 `AAD_SNAPSHOT_DIR` 覆盖。

> PowerShell 会吞掉命令行参数里的内层引号，因此 `--target` 优先使用紧凑引用
> `snapshot:revision:node`；它也接受等价的 JSON 对象形式。

## 运行工作流

```powershell
aad validate workflow.yaml
aad run workflow.yaml --inputs '{\"n\":9}'
```

描述文件只接受规范标识：

```yaml
apiVersion: ai-auto-desktop.dev/v1alpha1
kind: Workflow
metadata:
  name: hello
budgets:
  max_duration: 30s
  max_executed_steps: 20
steps:
  - id: done
    type: return
    value: hello
```

支持 `action`、`set`、`if`、`switch`、`foreach`、`while`、`block`、`script`、`fail`、`return`
十种步骤。核心对象拒绝未知字段，步骤 ID 在分支、错误处理器和清理步骤中全局唯一。编译是
fail-closed 的，并且会一次性收集**全部**问题而不是遇到第一个就返回。

其中 `script` 步骤**默认不执行**，需要显式 `aad run <file> --allow-scripts`，否则以
`SCRIPT.SANDBOX_DENIED`（`effect=not_applied`）拒绝。理由是它以本进程权限运行描述文件里
的任意代码——「有人让你跑这个文件」不应该等于「有人决定信任这段代码」。

`${{ ... }}` 是只读、确定性、无副作用的表达式，只能访问 `inputs`、`vars`、`steps`、当前控制流
绑定和错误处理器绑定，**禁止一切函数与方法调用**，也不能访问文件、网络、环境变量、时钟或
随机源。整串单模板保留结果类型，嵌在文本中的表达式转为字符串。

## 确认动作真的生效了

派发成功不等于生效。点击落在了空处、表单静默拒绝了输入、对话框始终没弹出——这些在自动化里
都会报"成功"。给步骤加 `postcondition` 就能让运行时**重新去看一眼**：

```yaml
steps:
  - id: fill
    type: action
    uses: desktop.windows_uia.set_value@1
    with:
      target: ${{ steps.locate.output.target }}
      value: hello
    postcondition:
      condition: ${{ observation.node.value == 'hello' }}
      observe:                      # 重新读一次，而不是复述已记下的输出
        uses: desktop.windows_uia.find@1
        with:
          window: {title: Notepad}
          locator: {name: SearchBox}
      timeout: 3s                   # 界面异步，给它时间
      poll_interval: 200ms
      message: 值没有写进去
```

几点值得说明：

- **`observe` 每轮都真的派发。** 重读已经存下来的 action 输出只会确认已经记下的东西，而那
  正是被怀疑的对象。观察结果绑定到 `observation` 供条件读取。
- **不写 `timeout` 就只判一次。** 桌面 UI 是异步的，轮询才让断言可用；但没要求等待却偷偷
  等，会把一次快速失败变成慢速失败。
- **观察动作必须是只读的**，否则 `POLICY.DENIED`。会改变被检查对象的断言什么也证明不了。
- **失败报 `effect=unknown`**，并在 `last_observation` 里附上当时**实际**看到的内容。动作
  本身成功了、只是预期结果没出现，桌面到底变没变确实不知道；往任何一个方向断言都是猜。
- **等待期间观察失败是正常的。** 断言「点完之后对话框出现」时，对话框还没出现的那几轮
  `find` 会报 `DRIVER.NOT_FOUND`——这被当作「还没到」而继续等，不是失败。反过来，不会
  自愈的错误（未知 action、被拒的写、写错的 `observe`）立即上报，不会拖到超时才说。
  一轮都没观察成功时，失败里会带 `last_observation_error` 说明它一直在失败什么。

反过来问「东西没了吗」——对话框关掉了、转圈消失了——要给 `find` 加 `expect: optional`：

```yaml
postcondition:
  condition: ${{ not observation.found }}
  observe:
    uses: desktop.windows_uia.find@1
    with:
      window: { title: "保存" }
      locator: { name: "正在保存…" }
      expect: optional      # 少了它，这个断言永远不成立
  timeout: 5s
```

没有 `expect: optional` 时，找不到＝报错＝「还没到」，于是断言会一直等到超时再失败。加上之后
找不到就是一个普通结果（`found: false`），断言才成立。`found` 两种结果下都有，所以条件不用
写两遍。默认仍然报错：为了操作而取元素时，清楚的「没找到」好过一个空结果在后面炸开。

`optional` 只放宽「没有」，不放宽「有好几个」——匹配到多个仍然报错，因为「在不在」不该被
答成「给你其中一个」。

录制的时候不用手写这些。给某一步加一个检查，导出时会自动编译成上面的 `postcondition`：

| 想确认的事 | mode | 还要填 |
|---|---|---|
| 点完之后某个东西出现了 | `exists` | 看哪个元素（不填就是这步自己的元素） |
| 某个东西消失了 | `absent` | 同上 |
| 输入框里就是这个值 | `value_equals` | 期望的值 |
| 输入框里包含这段文字 | `value_matches` | 期望的子串 |
| 按钮变成可用 / 只读 / 有焦点 | `state_equals` | 哪个状态 + true 或 false |

`value_matches` 是**子串包含，不是正则**——条件表达式里禁止函数调用，没有 matcher 可用。
`state_equals` 只能取观察里真实存在的六个状态（`enabled`、`offscreen`、`focusable`、
`focused`、`read_only`、`protected`）；写错的状态名在导出前就会被拦下，因为引用一个不存在
的字段会让整个运行以表达式错误失败，那个报错和真正的问题看起来毫不相干。

检查跟着步骤走：删掉或禁用这一步，它的检查也一起走，不会留下指向空处的引用。

在界面上，每个步骤下面有一个 **check it worked** 勾选框，勾上之后选想确认的事、填期望值、
需要等待的话再填一个时限（比如 `5s`，留空就只查一次）。

对通过 MCP 使用的 AI 来说这一条尤其要紧：它看不见屏幕，只能拿到那个状态字。

## 持久化运行（可暂停、可恢复、进程崩溃也不丢）

`aad run` 跑在内存里，进程退出即结束。需要一个能跨进程存活的运行时，用 `aad start`：
状态落在 SQLite 里，另一个进程随时可以查、可以接管。

```powershell
aad start workflow.yaml --store runs.sqlite3 --run-id job-1
aad status job-1  --store runs.sqlite3     # 进度、当前状态、检查点
aad list          --store runs.sqlite3 --status running
aad events job-1  --store runs.sqlite3 --after-seq 12   # 事件历史，可增量拉取
aad pause  job-1  --store runs.sqlite3     # 记录意图，runner 到安全点才停
aad cancel job-1  --store runs.sqlite3     # 粘性，不可被 pause 降级
aad resume job-1 workflow.yaml --store runs.sqlite3
```

几个刻意的设计：

- **`--store` 不叫 `--journal`**。`run --journal` 写 NDJSON 日志并且会**截断**给它的文件，
  两者同名的话打错一个词就会毁掉运行库。
- **`pause` 只记意图，不谎称已停**。返回里 `desiredState=pause` 而 `status` 可能还是
  `running`——步骤还在飞就说"已暂停"，是会被操作者当真的谎话。runner 在下一个段边界兑现。
- **`resume` 不接受 `--inputs`**。输入在创建时就持久化了；允许中途替换，等于让一次运行的
  后半段跑在与前半段不同的值上。
- **`cancel` 是吸收态**，之后 `pause` 会被 `RUN.CANCEL_PENDING` 拒绝，不会把已决定停掉的
  运行悄悄救回来。
- **崩溃恢复不重放**。进程死在步骤中途时，journal 无法知道副作用是否已经到达桌面，因此落
  `unknown_effect` 并**零派发**（连 cleanup 也不跑），在 error 里给出 remedy 交给人判断。
- 只有**不含 `action` / `script`** 的工作流可以持久化运行，否则以 `DURABLE.UNSUPPORTED_PLAN`
  拒绝并点名具体步骤。原因见 `docs/plan/rust-port-status.md` §2.1：没有 `action_intent`
  机制就无法证明某次派发可以安全重复。

## 给 AI 使用（MCP）

```powershell
aad mcp
```

在 stdio 上说 JSON-RPC 2.0，protocolVersion `2024-11-05`。提供 12 个工具，分两类：

**看与做**（临场操作）：`list_apps`、`describe_window`、`find_element`、`focus`、`invoke`、
`set_value`、`type_text`、`pointer_click`、`probe_environment`。

**跑存好的流程**（重复劳动）：`list_workflows`、`describe_workflow`、`run_workflow`。
录一次、存进 store，之后 AI 按名字跑，比每次重新拼一遍点击序列可靠得多。

```
list_workflows              → {"count":1,"workflows":[{"name":"greeter"}],"directory":"..."}
describe_workflow(greeter)  → 步骤数、声明的 inputs/outputs、planDigest、runnable
run_workflow(greeter, ...)  → {"status":"succeeded","outputs":{...}}
```

几个刻意的边界：

- **工作流只能按名字从 store 取**，不接受 AI 内联提交描述文件——否则 AI 就能自己编一份
  工作流让本机执行，等于绕过整套「先观察后行动」的约束。
- **含 `script` 的工作流按名拒绝**（`MCP.SCRIPTS_REFUSED`），并指明「要跑请人来跑：
  `aad run <file> --allow-scripts`」。AI 决定运行某个流程，不等于有人决定信任其中的代码。
- `describe_workflow` 会回报 `runnable`，所以这条限制**从只读调用就能发现**，不必靠一次
  被拒的运行去踩。
- `list_workflows` / `describe_workflow` **不需要桌面可用**：它们读的是文件夹。桌面坏了
  也能回答「有哪些流程」，而真正要动桌面的工具才报 `DRIVER.UNAVAILABLE`。
- store 与 GUI 共用同一个目录（`AAD_RECORDINGS_DIR` 可覆盖），GUI 里录完存下的东西
  AI 立刻能看到。

该模式下 stdout 由协议独占，任何诊断信息都走 stderr，客户端解析不会错位。失败会以结构化
错误返回，并附带可执行的恢复建议（例如界面已变化时提示"重新读取窗口后重试"）。

## 桌面窗口（GUI）

```powershell
cd gui
npm install
npm run tauri dev      # 开发模式
npm run tauri build    # 打包
```

三栏布局：左侧是正在运行的程序，中间是所选窗口的界面结构（可过滤、显示每个元素支持的
动作），右侧是录制出的步骤序列，可启用/禁用、上下调序、补填文本、导出。

导出结果就是 `aad validate` 能直接接受的工作流描述文件——GUI 产出的和 CLI 运行的是同一
份契约，且有测试锁定二者不会漂移。

### 密码框：可写不可读

界面结构里的普通输入框会照常显示它的值——判断表单填对没有、检索词对不对，都要看得见
这个值，所以这里不做无谓的遮盖。密码框不一样，标记为 `protected`：

- **读不到，而且不是我们挡的。** Windows 在 `ValuePattern` 层面就不给密码框的值（实测：
  相邻的普通输入框返回 `"quarterly-report-2026"`，密码框返回 `null`）。之所以显式标记
  出来，是为了把「这里没有值」和「这里的值不可读」分开——否则它看起来就像一次读取失败，
  让人去查一个不存在的故障。
- **仍然能写，自动登录照常可用。** 实测 `set_value` 能成功写入密码框（写入 19 字符后由
  被测程序回显长度与校验和验证），因此不需要任何「解除保护」的开关。
- **录进去的密码不落盘。** 密码框的输入会被提成 workflow input（`required` +
  `sensitive`），运行时再传入。`.workflow.json` 和可再编辑的 `.recording.json`
  **两个文件都不留字面量**——只清一个，等于把密码从一个文件挪到它旁边的另一个文件。

> UIA 需要多线程 COM，而窗口框架要求主线程是单线程 COM，两者不可兼得。因此 driver 运行在
> 独立线程上并通过 channel 通信；命令为 `async`，避免主线程等待自身窗口枚举而死锁。

## 脚本步骤

`script` 步骤在沙箱中执行 Python。stdout 必须是**单个 UTF-8 JSON 值**，否则返回
`SCRIPT.OUTPUT_INVALID`；退出码非 0 返回 `SCRIPT.EXIT_NONZERO`；超时返回 `SCRIPT.TIMEOUT`。
默认 stdout 上限 1 MiB、stderr 保留尾部 64 KiB。

Windows 上使用 Job Object 施加内核级上限（内存、CPU 时间、进程数），并保证进程树被回收，
同时清空环境变量、使用隔离工作目录、以 `-I` 隔离模式启动解释器。**但 Windows 不提供网络与
文件系统隔离**（无 per-process network/mount namespace），这一缺口由 `availability()` 以
`degraded` 状态和显式 `gaps` 列表如实上报，不会伪装成 available。Linux 使用 bubblewrap 提供
完整 namespace 隔离；其他平台 fail closed 返回 `SCRIPT.SANDBOX_UNAVAILABLE`。

解释器**不从 PATH 解析**（PATH 条目可被攻击者控制），只在固定位置查找。`entrypoint` 必须是
相对路径、不含 `..`，且 canonicalize 后仍须位于描述文件目录内，防止符号链接逃逸。

## 已知环境限制

这些是在真实机器上实测到的，不是推测：

- **合成输入可能被拦截。** 若本机运行按键宏、远程控制或外设驱动等安装了低级输入钩子的软件，
  `SendInput` 会拒绝整批事件（返回 0 且 `GetLastError()` 为 0）。此时 `type_text` 会以
  `DRIVER.INPUT_BLOCKED` 失败并且**什么都不写**——拆批投递会导致文字错乱（实测
  "one event at a time" 变成 "one eeeeeeeeeeeeeee"），写错内容比不写更糟。需要写文本时请改用
  `set_value`，它直接赋值、不经过合成输入。点击可以拆批，因为三个事件只在乎顺序。
- **并非所有窗口都能被解析。** 受保护或更高权限的进程会返回 `0x80004005`。
- **DPI 虚拟化与未提权进程**会让 `aad probe` 报 `degraded`，这是诚实上报而非故障。

## 仓库结构

```
crates/
  aad-core      描述文件模型、严格编译器、表达式求值器
  aad-plugin    NDJSON over stdio 插件宿主、AADF 制品侧信道
  aad-runtime   工作流引擎、模板、journal、沙箱脚本
  aad-uia       原生 UI Automation：发现、描述、控制
  aad-probe     只读能力探测
  aad-mcp       stdio 上的 MCP server
  aad-cli       aad 可执行文件
gui/            Tauri + Vue 桌面窗口
src/            Python 原型（保留备查，不再演进）
```

## 测试

```powershell
cargo test --workspace     # 557 个测试
cd gui; npm run test       # 65 个测试
```

## Python 原型

`src/ai_auto_desktop/` 是先前的 Python 实现，**保留供查阅，不再继续演进**；新功能一律进
Rust。历史文档见 `docs/`。
