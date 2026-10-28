# ai-auto-desktop 桌面自动化运行时

ai-auto-desktop 让人和 AI 用同一套能力观察并操作桌面：**发现正在运行的程序 → 读出界面结构 → 在被观察到的元素上执行动作**。

核心实现是 Rust，对外提供三个入口：

| 入口 | 形态 | 用途 |
| --- | --- | --- |
| **CLI** | `aad` 命令 | 人工使用、脚本编排、CI |
| **MCP** | `aad mcp`（stdio 上的 JSON-RPC） | 给 AI agent 调用 |
| **GUI** | Tauri + Vue 桌面窗口 | 可视化查看界面、录制并编排步骤 |

AI 只有一套入口：`aad control serve`（NDJSON）和 MCP 的 `control_*` 工具。Windows UIA、Linux AT-SPI、
macOS AX 与显式启用的 Chromium CDP 共用会话、资源、观察引用和类型化动作。
`aad mcp` 与 `aad control serve` 默认都只读，输入由启动参数 `--allow-input` 授予；
`aad mcp --connect` 可通过已有的认证 SSH 连接控制远程环境，授权由远端服务的启动参数决定。
详见[统一控制与远程会话](docs/control-sessions.md)。

给人用的 `apps/describe/overview/snapshot/find/do` 命令也走同一个会话层，三个平台都能用。
GUI 也走同一个会话，三个平台都能查看界面、搭建、录制和回放步骤。录制人的操作（GUI 的“录制”、`aad record`）
在 Windows（UIA 事件）、Linux（AT-SPI 事件 + XRecord，KDE/X11）和 macOS（AXObserver + 只监听的事件 tap）上都可用，
产出同一种平台无关的步骤；`aad watch` 仍只在 Windows 上。
录制、回放和事件观察的限制见[控制能力审视](docs/research/control-audit-2026-10-17.md)。

浏览器任务观测使用 `aad browser-record`：记录操作、关键元素变化、网络请求和截图，生成给 AI 编写 skill 的步骤说明。`aad browser-record start --browser edge --url 'https://example.com' --out ./recording` 自动启动并录制，可用 `--profile-dir` 复用工作 profile，或用 `--existing-profile [ID]` 选择日常已有 profile（只有一个时无需填 ID）。占用时默认提示，显式 `--kill-browser` 才结束占用浏览器并启动。**网络默认保留原值，`--redact-network` 才开启脱敏**。详见[浏览器任务录制](docs/browser-recording.md)。

## 安全模型：先观察，才能操作

这是整个项目的主轴，也是与“坐标点击”类工具的根本区别。

**任何写操作都必须引用一个真实观察过的元素**。引用是 `session_id + resource_id + observation_id + node_id`，
只在本会话、本次观察内有效；给人用的 `aad do` 在同一个进程里先观察、再定位、再操作，不跨进程传引用。执行前 driver 会用捕获时记录的 role / name / automation_id / class_name 与当前活动界面二次比对：

- 界面变了 → 返回 `DRIVER.STALE_HANDLE` 并**拒绝执行**，而不是打到别的元素上；
- 定位命中多个 → 返回 `DRIVER.AMBIGUOUS_MATCH` 并列出候选，绝不“取第一个”；
- 元素不支持该动作 → 返回 `DRIVER.ACTION_UNSUPPORTED` 并列出它支持什么。

MCP 工具 schema **不接受任何** `x` / `y` / `point` / `coordinates` / `position` 字段，并有测试守住这一点：AI 无法绕过观察直接点坐标。

## 快速开始

开发环境使用正式发布的 one-sdk（无需依赖草稿 PR）；CI 使用官方
`lejunyang/one-sdk@main` Action，显式设置 `version: latest`，Rust 仍由项目配置固定：

```powershell
osdk install                    # Rust 1.98.0 + rustfmt/clippy
osdk task list
osdk run fmt-check
osdk run test                   # 不包含需要 Tauri 系统依赖的 GUI
osdk run clippy
osdk run plugin-test
osdk run control-test           # 统一控制、传输与 MCP/CLI 回归
osdk run ci --dry-run           # 查看聚合检查；原生 fixture 仍需各平台环境
```

`osdk.lock` 只记录实际解析过的平台，首次在其他平台执行 `osdk lock` 后应提交该平台
产生的锁条目。Linux 脚本测试还要求可用的 bubblewrap 用户命名空间、prlimit 与
`/usr/bin/python3`；仅安装命令并不代表容器允许创建沙箱。GUI 开发另需 Node/npm
和平台的 Tauri 系统库，不由这些 Rust 任务隐式安装。

```powershell
cargo build --release
```

产物是单个 `aad` 可执行文件（Windows 上为 `target\release\aad.exe`）。

```powershell
aad probe                      # 只读检查环境是否具备自动化前置条件
aad apps                       # 列出可读、可操作的窗口和应用，每个带 resource_id
aad describe 99e160f2          # 读出界面结构；resource_id、其哈希的唯一前缀或 Windows 的 hwnd:… 都行
aad overview 99e160f2          # 只看区域划分（Windows）；再用 describe --region 读其中一块
aad find 99e160f2 --name "保存"  # 确认定位器恰好选中一个元素
```

`aad do` 接受与 `find` 相同的定位参数，在一个进程里观察、定位、操作：

```powershell
aad do invoke    99e160f2 --name "保存"            # 触发默认动作
aad do click     99e160f2 --name "保存"            # 点击元素中心
aad do click     99e160f2 --name "文件" --button right
aad do fill      99e160f2 --role Edit --value "写入的内容"
aad do type-text 99e160f2 --role Edit --text "逐字输入"            # 默认 Unicode
# Raw Input 应用使用 keys；字符必须能由目标键盘布局表达，IME 开启时会明确拒绝
aad do type-text 99e160f2 --role Edit --text "aA1!" --mode keys
aad do key       99e160f2 --role Edit --keys ctrl,a
# 滚轮/悬停/元素内位置：位置只能是元素内的 0..1 比例，不接受屏幕坐标
aad do scroll    99e160f2 --role List --direction down --notches 3
aad do hover     99e160f2 --name "帮助" --ms 400
# 拖拽的起点与终点都必须是同一次观察里的元素
aad do drag      99e160f2 --name "a.txt" --to-name "回收站"
aad do focus     99e160f2 --role Edit
```

定位参数：`--role`、`--name` 在所有平台精确匹配；`--automation-id`、`--contains`、`--protected`、
`--nth`、`--near`（配 `--side`、`--within`）、`--in`（配 `--in-role`）和 `--locator <JSON>` 由 Windows driver 求值，
其他平台会明确拒绝。匹配多个时报错并列出候选，不会“取第一个”；`find --optional` 用来问“它还在吗”。
Linux（KDE/X11）上 key、scroll、hover、drag 和带选项的 click 经 XTest helper 执行；macOS 上经签名的 AX helper 用 CGEvent 执行，要求目标应用在前台（macOS 部分尚未在真机验证）。

## 录制操作

```powershell
aad record 99e160f2 --seconds 20 --save 报销填写   # 在这段时间里操作那个窗口
aad run <输出里的 workflow_path> --permission desktop.input --permission desktop.observe
```

记录点击（含右键、双击）、滚轮、带 Ctrl/Alt/Cmd 的组合键和 Enter/Tab/Esc/F 键、文本修改和勾选。
连续输入合并成一次“填写”，点进输入框或点勾选框的那一下并入它引起的修改；普通打字记成最终文本，
密码框的内容不保存，改成运行时输入。无法识别元素的动作不会被猜成步骤，而是列在 `skipped` 里。
macOS 记点击和按键需要“输入监控”权限；没有时只记录文本和勾选的变化。

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
- **默认不重放 action**。进程死在普通步骤或 cleanup 中途时，journal 无法证明副作用是否已经
  到达桌面，因此落 `unknown_effect` 并**零派发**，在 error 里给出 remedy 交给人判断。
- 默认的 `--durable-actions deny` 只接受不含 `action` / `script` 的工作流。需要持久化只读观察时，
  显式传 `--durable-actions read-only`，并用 `--plugin NAME=COMMAND` 注册 provider、用
  `--permission` 授权；运行时只接受具备 public sensitivity、`not_applied` 错误合约和输出白名单
  投影的顶层单次只读 action。它会在派发前写入 `action_intent`，崩溃后重新校验绑定再安全重放。
  写 action、嵌套 action 和 `script` 仍然拒绝。完整边界见 `docs/plan/rust-port-status.md` §2.1。
- workflow cleanup 使用三阶段 checkpoint：`finalization intent` 之前崩溃可安全执行 cleanup，
  `started` 之后崩溃保守落 `unknown_effect`，cleanup 完成后的 `result` 可只补交终态而不重放。

## 给 AI 使用（MCP）

```powershell
aad mcp                  # 只读：能看，不能动
aad mcp --allow-input    # 由启动它的人授予输入
```

在 stdio 上说 JSON-RPC 2.0，protocolVersion `2024-11-05`。工具分两类：

**看与做**：`control_capabilities`、`control_targets`、`control_observe`、`control_find`、`control_act`、
`control_capture`、`control_cancel`、`control_close`，另有不需要桌面的 `probe_environment`。

- `control_targets` 列出窗口、应用和浏览器页面；`control_observe` 默认返回有上限的 outline（可操作或有名字的元素，
  按 `limit` 与 `max_characters` 截断，`stopped_by` 说明是哪个上限）。窗口很大时先用 `view.mode=overview`
  看区域划分，再用 `view.region` 读其中一块。区域只在 Windows 上计算，其他平台用 outline。
- `control_find` 只返回唯一一个节点；匹配多个时报错并列出候选。定位器支持 role、name、node_id，
  Windows 上还支持 automation_id、class_name、value、states、`match: contains`、`within`、`near`、`nth`；
  其他平台遇到这些字段会明确拒绝，不会忽略。`expect: optional` 用来问"这个东西没了吗"。
- `control_act` 的动作：focus、invoke、click（可选 button、count、元素内 fraction）、fill、type_text（unicode/keys）、
  key、scroll、hover、drag（终点必须是同一次观察里的另一个节点）、select（列表框选项）、check（复选框、开关、单选），
  以及浏览器页面的 navigate（只限 http/https）、back、forward、reload。位置只能是元素内 0..1 的比例。
  以观察结果里每个节点的 `capabilities` 为准。浏览器里普通 click 是 DOM 激活；带位置、按键或双击时改发可信的指针事件，
  落点被别的元素盖住时拒绝（`CONTROL.OCCLUDED`），不会点到盖在上面的东西。
- `control_capture` 给一个观察到的元素截 PNG（目前限浏览器页面，密码框拒绝），同时返回 `digest`：
  隔一会儿再截一次比对 digest，就知道显示的内容变没变（例如登录二维码刷新了），不用每次看图。

**浏览器**（需要 `--allow-input`，`browser_profiles` 只读）：`browser_open` 启动一个浏览器并把会话接到它上面，
之后 `control_targets` 就能列出它的页面。用户目录三选一：不给（临时目录，关掉即删）、`profile_dir`（工作目录，
登录一次之后一直保留）、`copy_profile`（复制已有 profile 的登录文件，要启动服务时加 `--allow-profile-copy`；
`browser_profiles` 列出可选的 profile）。占用中的浏览器只有传 `close_running` 才会被关掉。
`browser_record_start` / `browser_record_stop` 记录一个页面：页面上的动作和引起的变化、网络请求与响应正文
（默认保留原值，`redact_network` 才抹掉敏感字段）、WebSocket 消息和关键时刻的截图，停止时给出写入的目录和文件。
`browser_open` 带 `record` 时在打开网址之前就开始记录。这些是过程数据，据此写技能、筛接口由 Agent 自己做。

一个例子：打开浏览器到登录页 → `control_capture` 二维码发给人扫 → 隔几秒比对 digest，变了就重发 →
登录后在页面上操作 → `browser_record_stop` → 读 `evidence/network.jsonl` 找到返回订单数据的接口。
`tests/windows/verify_mcp_browser_session.py` 用本机的假站点在真实 Chrome 里走完这一遍。

命令行同样能操作浏览器：设置 `AAD_CDP_PORT=<端口>` 后，`aad apps`、`aad describe`、`aad do`、`aad capture`
都会把这个调试端口上的浏览器页面算进来，例如 `aad browser-record start --browser chrome` 启动时打印的端口。
`aad capture <页面> --role img --name 登录二维码 --out code.png` 把元素截图写到文件。
- 失败以结构化错误返回，并附 `hint` 说明怎样恢复（例如界面变了要重新 observe，匹配多个要用 `within` 收窄）。

**保存与运行流程**（重复劳动）：`save_workflow`、`list_workflows`、`describe_workflow`、`run_workflow`。
`save_workflow` 收的是和 `aad record`、GUI 相同的平台无关步骤（`control-steps/1`）：每步写应用（provider、进程名、
标题里不变的部分或网址前缀）、`locator`（照抄 outline 里每个元素带的 `locator`）和 `control_act` 用过的动作（去掉 target，
drag 的终点写在 `to`），也可以是等元素出现或消失的 `check`。Windows、Linux、macOS 和浏览器页面都能存、都能回放；
不收 `node_id` 这类只在一次会话里有效的引用。密码等用 `input` 声明，文字不落盘，运行时传入。
同名还会存一份录制文档，GUI 能打开继续编辑。录一次、存进 store，之后 AI 按名字跑，比每次重新拼一遍操作序列可靠得多。

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
  也能回答「有哪些流程」；`control_targets` 则在 `unavailable` 里说明桌面为什么不可用。
- **`run_workflow` 与 `control_act` 走同一个会话**：同一个原生 driver、同一把串行锁、同一份输入租约，
  只读服务器上它和 `save_workflow` 一样被拒（`POLICY.DENIED`）。限定了 `--provider` 或 `--resource` 时
  不提供工作流工具：工作流按窗口名自己找窗口，这类授权管不住它。
- store 与 GUI 共用同一个目录（`AAD_RECORDINGS_DIR` 可覆盖），GUI 里录完存下的东西
  AI 立刻能看到。

该模式下 stdout 由协议独占，任何诊断信息都走 stderr，客户端解析不会错位。失败会以结构化
错误返回，并附带可执行的恢复建议（例如界面已变化时提示"重新读取窗口后重试"）。

## 请人协助（aad assist）

Agent 遇到自己做不了的事（扫码之外的登录、验证码、需要人拿主意的地方）时，把屏幕交给人：

```powershell
aad assist --browser-port 9222            # 分享一个浏览器页面（CDP screencast）
aad assist --desktop                      # 分享整个桌面（目前只有 Windows）
aad assist --desktop --view-only          # 只给看，不接收鼠标键盘
```

启动后在 stdout 打印一行 JSON：`url`（自带的查看页，可选）、`websocket`、`token`。只监听本机回环地址，
每个连接都要带 token。主要产物是 WebSocket 上的帧流，由 omem 这类宿主自己渲染：

- 画面是二进制消息，一帧一张图：24 字节大端头（版本、编码、标志位 bit0 = 关键帧、宽、高、头长度、
  从开始算起的毫秒时间戳），之后是图片本身。读的一方按“头长度”跳过头，以后加字段不影响旧的读法；
  不认识的编码跳过不画。
- 编码默认 WebP（有损，`--quality` 默认 70），比同质量 JPEG 小得多；查看端可以先发
  `{"type":"hello","accept":["webp","jpeg"]}` 按偏好选编码，只认 JPEG 的就只收 JPEG。
  浏览器的 WebP 用 `Page.captureScreenshot` 按 `--fps` 取图（screencast 只给 JPEG），没变化不发；
  桌面用 libwebp 编码。现在每帧都是完整的图（都是关键帧）；以后加 H.264 等视频编码只需新增一种编码值，
  头里已有关键帧标志和时间戳。
- 控制消息是 JSON 文本：服务端发 `ready`（含 `frames.codecs`）和 `error`，查看端发 `hello`、
  `mouse`（move/down/up/wheel，位置是画面内 0..1 的比例）、`key`（Enter、方向键、F1–F12、修饰键等非文字按键）
  和 `text`（输入的文字）。

浏览器画面的输入用 CDP `Input.*` 发给页面，桌面用 `SendInput`。这是给人用的通道，按屏幕位置操作，
所以不通过 MCP 提供给 Agent。
`tests/windows/verify_assist.py` 用真实 Chrome（加 `--desktop` 再测 Windows 桌面）检查默认收到 WebP、
声明只要 JPEG 时收到 JPEG、点击和输入生效、错误 token 被拒绝。Linux 和 macOS 的整桌面分享还没有做。

## 桌面窗口（GUI）

```powershell
cd gui
npm install
npm run tauri dev      # 开发模式
npm run tauri build    # 打包
```

三栏布局：左侧是窗口和浏览器页面（底部可填浏览器调试端口），中间是所选窗口的元素
（可筛选；Windows 上可按区域查看），右侧是步骤。

- **搭建步骤**：在中间点一个元素，选它支持的操作（触发、点击、填写、按键、滚动、拖到……，
  或“等它出现 / 消失”），“添加”或“添加并执行”。三个平台都可用。
- **录制**：点“录制”后直接操作窗口，步骤实时出现在右侧；无法回放的动作会被略过并提示。Windows、Linux（KDE/X11）、macOS 都可用。
- **编辑与回放**：每一步可单独执行、改定位条件和应用条件（“测试定位”立即看匹配结果）、调序、删除；
  “运行全部”经工作流引擎逐步回放，失败的步骤标红并给出恢复建议。
- **保存**：录制文件和工作流都存进录制目录。步骤是平台无关的格式（`control-steps/1`）：
  应用按进程/应用名、标题片段或网址前缀命名，元素用定位器，动作用会话的类型化动作，
  不保存任何会话内引用，所以同一份录制可以在 `aad run`、MCP 的 `run_workflow` 和 GUI 里回放。
  回放要授予工作流声明的权限：`aad run x.workflow.json --permission desktop.input --permission desktop.observe`。

`npm run dev` 在普通浏览器里打开界面时没有后台，会用虚构的演示数据，便于调界面。

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
  aad-ocr       原生 Tesseract OCR 与图片输入校验
  aad-atspi     Linux AT-SPI / X11 驱动
  aad-macos-ax  macOS AX 的 Rust adapter 与 Swift helper 协议
  aad-uia       原生 UI Automation：发现、描述、控制
  aad-probe     只读能力探测
  aad-mcp       stdio 上的 MCP server
  aad-cli       aad 可执行文件
gui/            Tauri + Vue 桌面窗口
plugins/        Linux C++ 与 macOS Swift 原生 helper 源码
```

## 测试

```powershell
cargo test --workspace --exclude aad-gui
cd gui; npm run test       # 130 个测试
```
