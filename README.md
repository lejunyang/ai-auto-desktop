# ai-auto-desktop 桌面自动化运行时

ai-auto-desktop 让人和 AI 用同一套能力观察并操作桌面：**发现正在运行的程序 → 读出界面结构 → 在被观察到的元素上执行动作**。

核心实现是 Rust，对外提供三个入口：

| 入口 | 形态 | 用途 |
| --- | --- | --- |
| **CLI** | `aad` 命令 | 人工使用、脚本编排、CI |
| **MCP** | `aad mcp`（stdio 上的 JSON-RPC） | 给 AI agent 调用 |
| **GUI** | Tauri + Vue 桌面窗口 | 可视化查看界面、录制并编排步骤 |

AI 只有一套入口：`aad control serve`（NDJSON）和 MCP 的 `control_*` 工具。Windows UIA、Linux AT-SPI、
macOS AX 与显式启用的 Chromium CDP、Firefox BiDi 共用会话、资源、观察引用和类型化动作。
`aad mcp` 与 `aad control serve` 默认都只读，输入由启动参数 `--allow-input` 授予；
`aad mcp --connect` 可通过已有的认证 SSH 连接控制远程环境，授权由远端服务的启动参数决定。
详见[统一控制与远程会话](docs/control-sessions.md)。

给人用的 `apps/describe/overview/snapshot/find/do` 命令也走同一个会话层，三个平台都能用。
GUI 也走同一个会话，三个平台都能查看界面、搭建、录制和回放步骤。录制人的操作（GUI 的“录制”、`aad record`）
在 Windows（UIA 事件）、Linux（AT-SPI 事件 + XRecord，KDE/X11）和 macOS（AXObserver + 只监听的事件 tap）上都可用，
产出同一种平台无关的步骤；`aad watch` 仍只在 Windows 上。
录制、回放和事件观察的限制见[控制能力审视](docs/research/control-audit-2026-10-17.md)。

浏览器任务观测使用 `aad browser-record`：记录操作、关键元素变化、网络请求和截图，生成给 AI 编写 skill 的步骤说明。`aad browser-record start --browser edge --url 'https://example.com' --out ./recording` 自动启动并录制，可用 `--profile-dir` 复用工作 profile，或用 `--existing-profile [ID]` 选择日常已有 profile（只有一个时无需填 ID）。占用时默认提示，显式 `--kill-browser` 才结束占用浏览器并启动。**网络默认保留原值，`--redact-network` 才开启脱敏**。录完可以用 `aad browser-record steps --out ./recording --save 名字` 转成可回放的步骤，再 `aad run 名字`。详见[浏览器任务录制](docs/browser-recording.md)。

## 安全模型：先观察，才能操作

这是整个项目的主轴，也是与“坐标点击”类工具的根本区别。

**任何写操作都必须引用一个真实观察过的元素**。引用是 `session_id + resource_id + observation_id + node_id`，
只在本会话、本次观察内有效；给人用的 `aad do` 在同一个进程里先观察、再定位、再操作，不跨进程传引用。执行前 driver 会用捕获时记录的 role / name / automation_id / class_name 与当前活动界面二次比对：

- 界面变了 → 返回 `DRIVER.STALE_HANDLE` 并**拒绝执行**，而不是打到别的元素上；
- 定位命中多个 → 返回 `DRIVER.AMBIGUOUS_MATCH` 并列出候选，绝不“取第一个”；
- 元素不支持该动作 → 返回 `DRIVER.ACTION_UNSUPPORTED` 并列出它支持什么。

MCP 工具 schema **不接受任何** `x` / `y` / `point` / `coordinates` / `position` 字段，并有测试守住这一点：AI 无法绕过观察直接点坐标。

## 快速开始

CLI 发布流程提供以下 8 种归档，每种附带 SHA256 校验文件：

| 系统 | x64 target | ARM64 target |
| --- | --- | --- |
| Linux GNU，glibc 2.35+ 构建基线 | `x86_64-unknown-linux-gnu` | `aarch64-unknown-linux-gnu` |
| Linux musl，静态程序 | `x86_64-unknown-linux-musl` | `aarch64-unknown-linux-musl` |
| Windows | `x86_64-pc-windows-msvc` | `aarch64-pc-windows-msvc` |
| macOS | `x86_64-apple-darwin` | `aarch64-apple-darwin` |

Windows 使用 `.zip`，其他平台使用 `.tar.gz`。归档包含 `aad`、`aad-browser` 和平台 helper，
解压后保持文件在一起。musl 归档中的四个可执行文件均静态链接；OCR、脚本和浏览器功能仍需要
相应的 Tesseract、Python/bubblewrap 或 Chromium 安装，以及桌面会话权限。

[CLI packages 工作流](.github/workflows/cli-packages.yml) 可手动构建全部归档而不发布版本；
正式 release 复用同一流程。各目标使用本架构 runner，打包后解压执行，并检查 ELF/PE/Mach-O 架构；
Linux musl 额外检查无动态加载器、共享库依赖和 GLIBC 符号，并在空文件系统中启动。

osdk 0.0.7 同时遇到 GNU/musl 资产时不会可靠地按 libc 选包。安装时可指定模板，例如发布相应版本后：

```powershell
osdk use --global github:lejunyang/ai-auto-desktop@0.0.2 -o 'asset-template=aad-{version}-x86_64-unknown-linux-musl.tar.gz'
```

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
aad record 99e160f2 --until-stdin-closes --save 报销填写   # 由宿主决定何时停：关掉标准输入就停（最长 1 小时）
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

`--store` 可省略：默认是每用户一份的 `~/.ai-auto-desktop/runs.sqlite3`（`AAD_RUN_STORE` 可覆盖），
MCP 的 `run_workflow` 持久模式也写这里，所以 AI 起的运行能直接用 `aad status` / `aad resume` 跟进。

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

**看与做**：`control_capabilities`、`control_targets`、`control_observe`、`control_find`、`control_wait`、`control_act`、
`control_capture`、`control_cancel`、`control_close`，另有不需要桌面的 `probe_environment`。

- `control_targets` 列出窗口、应用和浏览器页面；`control_observe` 默认返回有上限的 outline（可操作或有名字的元素，
  按 `limit` 与 `max_characters` 截断，`stopped_by` 说明是哪个上限）。窗口很大时先用 `view.mode=overview`
  看区域划分，再用 `view.region` 读其中一块。区域在 Windows 与 Chromium/CDP 上计算；其他 provider 用 outline，
  支持局部观察时可用 `scope_target` 从已有元素缩小范围。
- `control_find` 只返回唯一一个节点；匹配多个时报错并列出候选。定位器支持 role、name、node_id，
  Windows 上还支持 automation_id、class_name、value、states、`match: contains`、`within`、`near`、`nth`；
  浏览器支持 value、states、`match`、`within`、`nth`；各 provider 对未支持字段明确拒绝，不会忽略。
  `expect: optional` 用来问"这个东西没了吗"。
- `control_wait` 反复观察一个目标，直到定位到的元素出现（`appears`）、消失（`disappears`）或达到给定 `states`（`state`），
  或 `timeout_ms` 用完（`CONTROL.WAIT_TIMEOUT`）；出现时直接返回可以 act 的 target。所有 provider 都有，多个匹配不算找到。
- `control_act` 的动作：focus、invoke、click（可选 button、count、元素内 fraction）、fill、type_text（unicode/keys）、
  key、scroll、hover、drag、select（列表框选项）、check（复选框、开关、单选）。drag 默认锚定同一次观察里的两端，
  支持的 surface 可用同一节点的不同 fraction；Linux AT-SPI 另支持声明了跨资源能力的双端观察。
  浏览器页面还有 navigate（默认 http/https；本地文件目录与内部来源需服务器显式授权）、back、forward、reload，标签页的 open_tab、activate_page、close_page，
  JavaScript 对话框的 accept（prompt 可带 text）、dismiss，文件框的 upload（只能读启动时 `--allow-upload-dir` 给出的目录）。
  页面打开的弹窗在 `control_targets` 里带 `metadata.opener_page_id`。位置只能是元素内 0..1 的比例。
  以观察结果里每个节点的 `capabilities` 为准。Chromium/CDP 普通 DOM 节点的无选项 click 是 DOM 激活；带位置、按键或双击时改发可信的指针事件，
  落点被别的元素盖住时拒绝（`CONTROL.OCCLUDED`），不会点到盖在上面的东西。
- `control_capture` 由支持 capture 的桌面或浏览器 provider 给已观察元素截 PNG；保护字段及截图范围按 provider 规则检查，同时返回 `digest`：
  隔一会儿再截一次比对 digest，就知道显示的内容变没变（例如登录二维码刷新了），不用每次看图。

**Firefox BiDi**：连接操作者已启动的本机 BiDi 端口，使用独立 provider：

```powershell
aad control serve --allow-input --provider browser.firefox_bidi --browser-bidi-port 9223
aad mcp --allow-input --provider browser.firefox_bidi --browser-bidi-port 9223
```

它与 `--browser-port` 互斥。首版支持主文档 DOM 编辑、可信单击/按键/滚轮、有界长按、安全局部截图、导航与标签页，
历史仅限可证明目标的连续同源 HTTP(S) 条目；子 frame 只读。双击、指针修饰键、drag、保密输入、上传与对话框应答尚未实现。
完整参数与实测边界见[Firefox BiDi 说明](docs/control-sessions.md#firefox-webdriver-bidi首版)；不能套用下面的 Chromium 工具范围。

**Chromium 浏览器**（需要 `--allow-input`，`browser_profiles` 只读）：`browser_open` 启动一个浏览器并把会话接到它上面，
之后 `control_targets` 就能列出它的页面。用户目录三选一：不给（临时目录，关掉即删）、`profile_dir`（工作目录，
登录一次之后一直保留）、`copy_profile`（复制已有 profile 的登录文件，要启动服务时加 `--allow-profile-copy`；
`browser_profiles` 列出可选的 profile）。占用中的浏览器只有传 `close_running` 才会被关掉。
`browser_record_start` / `browser_record_stop` 记录一个页面：页面上的动作和引起的变化、网络请求与响应正文
（默认保留原值，`redact_network` 才抹掉敏感字段）、WebSocket 消息和关键时刻的截图，停止时给出写入的目录和文件。
`browser_open` 带 `record` 时在打开网址之前就开始记录。这些是过程数据，据此写技能、筛接口由 Agent 自己做。
`browser_downloads`（只读）列出浏览器下载到本会话专用目录的文件：网址、建议文件名、状态、字节数和保存路径。
`browser_storage`（只读）列出一个页面/frame 资源的 cookie 名称与属性及 localStorage 键（cookie 值需服务端 `--allow-cookie-values`）；
`browser_network`（只读）列出页面最近的请求（状态、MIME、耗时，可选带打码后的正文）。控制动作另有 `emulate`（desktop/tablet/mobile/reset 与配色）和 `clear_site_data`，
`control_observe` 的 `view.mode` 另有 `text`（页面或区域的可见文字与表格）。

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

**持久运行**（长流程、要跨进程跟进的）：`run_workflow` 传 `durable: true`（显式开启，默认仍是内存运行），
经 `DurableExecutor` 写入与 `aad start` 相同的每用户运行库；随后用 `workflow_run_status`（状态、当前步骤、
输出或错误、`unknown_effect` 详情与下一步建议）、`workflow_run_list`、`workflow_run_events`（`after_seq` +
`limit` 分页）跟进，用 `workflow_run_pause` / `workflow_run_cancel` / `workflow_run_resume` 控制
（这三个是写操作，只读服务器上被拒）。

- **持久模式的限制原样保留**：拒绝 `script`、写 action、敏感输入输出；顶层只读 action 需要显式
  `durable_actions: "read_only"`。被拒时返回 `DURABLE.*` 错误（`run_created=false`，什么都没执行），
  **不会悄悄改成内存运行**——只有调用方传 `fallback_in_memory: true` 才回退，并在 `durable_refusal` 里说明原因。
- **长流程不阻塞调用**：执行器在服务器后台线程里跑，调用最多等 `wait_ms`（默认 20 秒，上限 120 秒），
  超时就返回 `run_id` 与 `status=running`。
- **服务器退出时**：客户端断开会对正在跑的运行请求 pause 并等待约 2 秒，处于步骤边界的运行干净地停成 `paused`；
  进程直接死掉时运行保持 `running`，租约（默认 60 秒）过期后 `workflow_run_resume` 或 `aad resume` 才能接管。
  停在步骤之间的从检查点继续；死在步骤中途的落 `unknown_effect`，**不会自动重放**。
- `workflow_run_resume` 需要原工作流名；工作流改过（plan digest 不同）会被拒。输入沿用创建时持久化的值。

该模式下 stdout 由协议独占，任何诊断信息都走 stderr，客户端解析不会错位。失败会以结构化
错误返回，并附带可执行的恢复建议（例如界面已变化时提示"重新读取窗口后重试"）。

## 请人协助（aad assist）

Agent 遇到自己做不了的事（扫码之外的登录、验证码、需要人拿主意的地方）时，把屏幕交给人：

```powershell
aad assist --browser-port 9222            # 分享一个浏览器页面（CDP screencast）
aad assist --desktop                      # 分享整个桌面（Windows；Linux 的 X11；macOS 的主显示器）
aad assist --desktop --view-only          # 只给看，不接收鼠标键盘
aad assist --list-screens                 # 列出可分享的显示器和窗口
aad assist --desktop --monitor 1          # 只分享第 1 块显示器
aad assist --desktop --window 0x3077a     # 只分享一个窗口（句柄，或标题的一部分）
```

`--monitor` 和 `--window` 只分享桌面的一部分：截屏照常用 DXGI，再裁出这块区域（裁剪缓冲同样两份轮换，只补变化的区域），
鼠标位置和光标都换算到这块区域里，区域外的光标不显示。窗口按 DWM 的可见边框跟随：移动后输入跟着走，大小改变后画面
按新尺寸从关键帧重新开始，最小化时停在最后一帧，窗口关闭时报错。分享的是窗口在屏幕上占的那块区域，盖在它上面的
东西也会被看到。

可以同时有多个查看者（`--max-viewers`，默认 4）：各自选自己的编码，截一次屏供所有人用；暂时跟不上的查看者把漏掉的
变化区域合并起来，等它能接收时一起补上，tiles 不会漏块。同一时刻只有一个查看者的鼠标键盘会被转发：最早连上的那个；
其他人只看，查看页上有“Take control”按钮（发 `{"type":"take"}`）可以接管，接管时先把上一位还按着的键和鼠标键松开；
控制者离开后交给下一位。谁在控制变化时每个查看者收到 `{"type":"control","you":..,"holder":..,"viewers":..}`。
画面静止时新连上的查看者也会立刻收到当前画面。

启动后在 stdout 打印一行 JSON：`url`（自带的查看页，可选）、`websocket`、`token`、`codecs`（提供的编码）
和 `video`（找到的 ffmpeg 与每种视频编码选中的编码器）。只监听本机回环地址，每个连接都要带 token。
自带的查看页在自己所在的路径下连 `ws`（https 下用 wss），宿主可以把页面和 WebSocket 一起放在自己的路径前缀下转发
（例如 `/remote/<会话>/?token=…` 与 `/remote/<会话>/ws?token=…`）。主要产物是 WebSocket 上的帧流，由 omem 这类宿主自己渲染：

- 画面是二进制消息，一条一帧：24 字节大端头（版本、编码、标志位 bit0 = 关键帧 / bit1 = 后面带变化区域、
  宽、高、头长度、从开始算起的毫秒时间戳），之后是负载。读的一方按“头长度”跳过头，以后加字段不影响旧的读法；
  不认识的编码跳过不画。编码值：1 = JPEG、2 = WebP（各是一整张图），3 = H.264（Annex B，
  关键帧前带 SPS/PPS）、4 = VP9、5 = AV1（OBU），各是一帧视频；6 = tiles（只有变化的部分，见下）。
  bit1 置位时头后面跟着 u16 个数和若干 x、y、w、h（u16）：这一帧只有这些区域变了。
- 画面没变化就什么都不发。图片编码每次发整张图；tiles 只发变了的 32 像素块；视频编码里没变的块被编成跳过块。
- 查看端可以先发 `{"type":"hello","accept":["tiles","h264","webp","jpeg"]}` 按自己的偏好选编码；不发 hello 的
  查看端收到 `--codecs` 里的第一个（默认 WebP，旧宿主不受影响）。hello 之后服务端发
  `{"type":"mode","mode":{"codec":"tiles","video":"h264"}}` 说明实际模式。每段视频流开始前服务端发
  `{"type":"stream","codec":"h264","webcodecs":"avc1.64001f","encoder":"h264_nvenc","width":..,"height":..}`，
  紧接着是关键帧；`webcodecs` 可直接作 WebCodecs `VideoDecoder.configure` 的 `codec`。解码出错时查看端发
  `{"type":"keyframe"}`，服务端重开编码器，从当前画面的关键帧重新开始。
- 分享桌面时服务端另发 `{"type":"cursor","x":..,"y":..,"visible":true,"shape":{"png":"<base64>","width":..,
  "height":..,"hotspot":[x,y],"screen":[w,h]}}`：鼠标指针的位置（画面内比例）和样子（变了才带 `shape`，
  `screen` 是 `width`/`height` 所在的屏幕像素尺寸），指针不画进画面，由查看端叠加；`--hide-cursor` 关掉。
- 控制消息是 JSON 文本：服务端发 `ready`（含 `frames.codecs`、`mode`、`video`）、`mode`、`stream`、`cursor`、
  `clipboard` 和 `error`，查看端发
  `hello`、`keyframe`、`mouse`（move/down/up/wheel，位置是画面内 0..1 的比例，`count` 是第几次连击）、
  `key`（不出字的按键和快捷键，`code` 是物理键名，见下）、`text`（输入的文字）、`release` 和 `clipboard`；
  分享浏览器页面时还有 `viewport`（`{"type":"viewport","width":..,"height":..}`，CSS 像素，页面按这个大小排版，
  不是缩放进查看端）。`ready` 里的 `os` 是分享端的系统（`windows` / `linux` / `macos`）。
- `key`、`mouse`、`text` 可以带 `"modifiers":["Control","Shift"]`：查看端此刻按着的修饰键。分享端还没按下的先补按、
  已经松开的先松开，再处理这条消息；所以修饰键自己的按下没送到时（按住 Ctrl 再把焦点切进查看页、工具只发一个带
  Ctrl 的 A 键），快捷键照样成立。自带的查看页发 `text` 时带 `"modifiers":[]`，按着 Ctrl 粘贴出的是字而不是快捷键。
  不带这个字段的消息照旧处理。

### 远程操作

| 操作 | 浏览器页面（CDP `Input.*`） | Windows 桌面（`SendInput`） |
|---|---|---|
| 移动、按下、松开、拖拽，左/中/右键，横竖滚轮 | ✅ | ✅ |
| 双击、三击 | ✅ 查看端发 `count`（`e.detail`） | ✅ 系统按间隔合成 |
| 文字输入、粘贴（单次 ≤4096 字节） | ✅ `insertText` | ✅ Unicode 输入，和键盘布局无关 |
| 中文等输入法 | ✅ 查看页里组字，只发组好的字 | ✅ 同左 |
| 快捷键 | ✅ 按物理键（`code`）映射：字母、数字、标点（Ctrl+/、Ctrl+=、Ctrl+[）、Ctrl+Space、F1–F24、小键盘、CapsLock、PrintScreen、菜单键等 | ✅ 同左，带扫描码，方向键/Home/End/Insert/Delete/右侧修饰键带扩展键标志 |
| 断开或查看页失去焦点时松开所有按住的键和鼠标键 | ✅ | ✅ |
| 把复制的文字带回查看端 | ✅ 查看端按 Ctrl+C/Ctrl+X 后取回页面选中的文字（输入框里的选区或页面选区，同源 iframe 里也算） | ✅ 查看端按 Ctrl+C/Ctrl+X 后自动取回（≤10 万字），密码管理器标记为不供读取的内容拒绝；`--no-clipboard` 关闭，`--view-only` 时不提供 |
| 粘贴查看端的剪贴板 | ✅ 查看页让 Ctrl+V 走本机的粘贴，把文字作为 `text` 发出 | ✅ 同左 |
| 页面按查看端的大小排版 | ✅ `viewport` → `Emulation.setDeviceMetricsOverride`（控制者的查看页打开和改变大小时发） | — |

`key` 消息优先按 `code`（物理键）找键，没有 `code` 时按 `key` 的名字找；所以查看端是什么键盘布局，快捷键都落在
同一个物理键上。出字的键不走 `key`：查看页把焦点放在点击处的一个隐藏输入框上，按键先经过本机输入法，组好的字和普通
字符作为 `text` 发出（死键、输入法组字中的按键不发），Ctrl/Alt/Meta 组合和不出字的键作为 `key` 发出。在 Mac 上看
不是 macOS 的分享端时，⌘ 当 Ctrl 发出，⌘C、⌘V、⌘A 和那边的 Ctrl 组合一样用。查看页里右键不弹本机浏览器的菜单。
查看页的文字按 `?lang=zh|en` 或浏览器语言显示中文或英文。按下鼠标后
用指针捕获，拖到画面外再松开也会送达；查看页失去焦点或切到后台时发 `release`，对端把还按着的键和鼠标键全部松开，
服务端在连接断开时也会这么做，不会留下按住的 Shift 或鼠标左键。

### 只传变化的部分（tiles）与自适应

`tiles` 不依赖 ffmpeg：画面切成 32×32 的块，只把变了的块合并成矩形、各编成一张 WebP 补丁发出去，查看端在
已有画面上原位画这些补丁；整块移动的区域（滚动）只发一条“从 (sx, sy) 复制过来”。一帧的负载是 u16 补丁数，
每个补丁：类型 u8（1 = 图片，2 = 复制）、格式 u8（图片为 2 = WebP）、x、y、w、h（u16），复制再跟来源 x、y
（u16），图片再跟 u32 长度和恰好 w×h 的图片。关键帧是一块盖满全画面的补丁；`--quality 100` 时补丁用无损 WebP。

变化从哪里来：Windows 桌面用 DXGI 桌面复制（Desktop Duplication），系统直接给出每帧重绘和移动的区域，
画面不变时根本没有新帧，只把变化的行从显存读回；再在这些区域里逐块比较，把“重画了但像素没变”的块去掉。
DXGI 不可用（部分远程会话、旋转的显示器）或丢失（分辨率变化、安全桌面）时退回 GDI 整屏拷贝加逐块比较，
一秒后再试 DXGI；`source.capture` 报告当前是 `dxgi` 还是 `gdi`。浏览器页面用 screencast 的 PNG 帧逐块比较。
分享桌面的进程按每显示器 DPI 感知运行，拿到的是物理像素（以前在 250% 缩放下拿到的是系统缩小过的 1536×960
模糊画面，现在是 3840×2400 再按 `--max-width` 缩小）。

缩小也只做变化的部分：服务端保留一份缩小后的画面，每次只把变化区域从原图重新取平均；抓屏缓冲区两份轮换，
新一帧只补上一帧变过的区域，不再整屏复制。

查看端 hello 里 tiles 排在某种视频编码前面（自带查看页就是这样）时进入**自适应**：小改动发 tiles；
变化面积超过 `--video-area`（默认屏幕的 30%）或每秒超过 `--video-rate` 次（默认 8 次，只算超过屏幕 1% 的变化，
如滚动、播放）时发视频。视频帧在头里带变化区域，查看端照常解码每一帧，但**只把变化区域画到画布上**，别处保留
tiles 画的清晰内容；画面静止 0.4 秒后，视频画过的区域再用 tiles 补发一遍清晰的。两路帧按送入顺序发出，
不会出现旧画面盖住新补丁。

本机实测（release 构建，3840×2400 桌面缩到 1600×1000，屏幕上有一处持续动画约 26 次/秒，10 秒平均，
CPU 为 aad 加 ffmpeg 占单核的比例）：

| 模式 | CPU | 带宽 |
|---|---|---|
| WebP 整图（5 帧/秒） | 50% | 约 540 KiB/s |
| tiles | 8% | 约 14–34 KiB/s |
| 自适应 tiles + h264 | 12% | 约 14–36 KiB/s |
| h264（NVENC） | 23% | 约 4–10 KiB/s |
| h264（x264） | 70% | 约 14–52 KiB/s |

优化前同样场景下 NVENC 是 159%、x264 186%（每帧把 37 MB 的 4K 原图送进 ffmpeg 再由它缩小），tiles 26%（每帧整屏复制）。
`tests/windows/verify_assist_tiles.py`（加 `--desktop` 再测桌面）检查：tiles 首帧是整画面、页面静止 6 秒 0 帧、
点击只发按钮附近的补丁（1.8 KB，关键帧 35 KB）、按所有补丁拼出的画面和同一时刻的整张 WebP 只差 0.56/255、
点击打字真实生效；自适应在大面积动画时切到视频、视频帧只报告动画区域、动画停止后该区域用 tiles 补发；
自带查看页（另起一个 Chrome，避免被分享的页面变成后台标签而停止绘制）最终画面和页面一致；桌面报告 `dxgi`、
补丁拼出的画面和服务端此刻的整帧逐块相比最差的块平均差 12/255（只有文字边缘的有损差异，没有错位或漏发的块）、
打字只在编辑框附近产生约 1.2–6 KB、指针形状的 PNG 能解码且指针移到了点击处。

### 视频流（需要宿主自备 ffmpeg）

本项目不带编码器。按 `--ffmpeg PATH` → 环境变量 `AAD_FFMPEG` → PATH 上的 `ffmpeg` 的顺序找；找到后启动时
对每种视频编码依次试编码器（先硬件后软件），**每一帧都必须在下一帧送进去之前编出来**才算可用，所以列在
`ffmpeg -encoders` 里却用不了的（没有 NVIDIA 卡的 NVENC、没有 AMD 卡的 AMF）以及会攒帧的（Media Foundation
的 H.264 要等输入结束才吐帧）都不会被选中。没有 ffmpeg 时只提供 WebP/JPEG，行为和以前一样。

```powershell
aad assist --probe-video                                 # 只报告这台机器能用哪些视频编码器，然后退出
aad assist --desktop --codecs h264,webp                  # 优先 H.264，不支持视频的查看端退回 WebP
aad assist --desktop --encoder libx264 --preset veryfast --bitrate 4000
aad assist --browser-port 9222 --codecs vp9 --chroma 444 # 4:4:4，彩色小字更清楚
aad assist --desktop --codecs av1 --ffmpeg-arg=-svtav1-params --ffmpeg-arg=pred-struct=1:lp=4
```

| 参数 | 作用 |
|---|---|
| `--codecs` | 提供哪些编码及顺序：`webp,jpeg,tiles,h264,vp9,av1`；`auto`（默认）= WebP、JPEG、tiles 加上能用的视频编码 |
| `--ffmpeg` | ffmpeg 路径（否则 `AAD_FFMPEG`，再否则 PATH） |
| `--encoder` | 指定 ffmpeg 编码器，可重复：作后备，或每种编码各指定一个；不指定时先试硬件 |
| `--fps` | 最高帧率 1..60；不填时整图 5、tiles 和视频 30。只是上限，画面不变就不出帧 |
| `--quality` | 1..100，图片的质量；视频不设 `--bitrate` 时换算成各编码器自己的恒定量化参数 |
| `--bitrate` | 视频恒定码率（kbit/s） |
| `--gop` | 关键帧间隔（帧数）；默认 0 = 只在开始或查看端要求时出关键帧 |
| `--preset` | 编码器速度档（x264 `veryfast`、NVENC `p4`、SVT-AV1 `8`…）；默认各自最快的一档 |
| `--chroma` | `420`（默认）或 `444`；不支持 4:4:4 的编码器会在试编时落选 |
| `--ffmpeg-arg` | 原样追加一个 ffmpeg 输出参数，可重复（兜底，用于本项目没有包装的选项） |
| `--max-width` / `--max-height` | 发出的画面最大宽高（默认 1600 宽、高不限），更大的屏幕按比例缩小；视频再取偶数 |
| `--video-area` / `--video-rate` | 自适应时改用视频的门槛：变化占屏幕的百分比（默认 30）/ 每秒次数（默认 8） |
| `--hide-cursor` | 不发桌面鼠标指针的位置和样子 |

内置的编码器参数都是低延迟配置：x264 `ultrafast` + `zerolatency`、NVENC `p1` + `ull` + `zerolatency`、
QSV `async_depth 1`、libvpx `realtime`、SVT-AV1 低延迟预测结构、libaom `realtime`，都不带 B 帧。
H.264 用 FLV、VP9/AV1 用 IVF 从 ffmpeg 取出（两者都在帧前写长度，一帧编完立刻转发，不用等下一帧开头）。
抓屏和编码在各自线程上，网络慢时不再抓新画面（而不是在视频流中间丢帧），所以查看端拿到的总是最新画面。

自带查看页用 WebCodecs 解码，WebCodecs 只在安全上下文里有：`localhost`/`127.0.0.1` 直接打开可以，宿主通过
https 代理也可以；不支持某种编码时查看页会重新 hello 把它去掉，退回 WebP。

本机实测（Windows 11，RTX 4080 Laptop + Intel 核显，ffmpeg 9.0.1 full build，Chrome headless=new，
`tests/windows/verify_assist_video.py --desktop`）：自动选中 h264_nvenc / vp9_qsv / av1_nvenc，试编各约
250 ms、vp9_qsv 约 1.5 s（并行进行）；768×496 的图文页面关键帧 72 KB（H.264）/ 104 KB（VP9）/ 86 KB（AV1），
页面静止 6 秒发出 0 帧，点击和打字每帧最大 0.8–1.7 KB；收到的每一帧都能被独立的 ffmpeg 解码（帧数一致），
经视频流的点击和输入真实生效；自带查看页在 Chrome 里用 WebCodecs 解出并画出画面（三种编码、软件编码器、
4:4:4 都试过）；整桌面 1536×960 H.264 解码和输入生效。macOS 的 h264_videotoolbox、AMD 的 AMF 只按文档配置，
`[待验证: 对应硬件]`；Linux 的 VAAPI 等需要上传滤镜的编码器没有内置配置，可用 `--encoder` 加 `--ffmpeg-arg` 自配。

浏览器画面的输入用 CDP `Input.*` 发给页面，桌面用 `SendInput`。这是给人用的通道，按屏幕位置操作，
所以不通过 MCP 提供给 Agent。
`tests/windows/verify_assist.py` 用真实 Chrome（加 `--desktop` 再测 Windows 桌面）检查默认收到 WebP、
声明只要 JPEG 时收到 JPEG、点击和输入生效、错误 token 被拒绝；`tests/windows/verify_assist_video.py`
和 `verify_assist_tiles.py` 检查上面的视频与 tiles 行为；`tests/windows/verify_assist_area.py` 检查 `--list-screens` 的显示器和窗口、`--monitor` 的画面就是该显示器、不存在的编号被拒绝；
加 `--window` 时用隔离的探针窗口检查：首帧就是窗口大小、在画面里点哪里就落在窗口的哪里（窗口移动后也一样）、
改变大小后按新尺寸重新开始、光标按窗口内的位置报告、补丁拼出的画面和此刻的关键帧逐块一致（最差的块平均差 2.5/255）；
`tests/windows/verify_assist_viewers.py` 检查多个查看者：两人用不同编码同时看、只有控制者的点击生效、接管后上一位按着的
Shift 被松开、控制者离开后控制权交回、画面静止时后来者和全部离开后重连的人都拿到首帧、超过 `--max-viewers` 的被拒绝；
`tests/windows/verify_assist_input.py` 按页面上的
实际效果检查远程操作：双击选中单词、Ctrl+A 后输入替换文字、Ctrl+/ 和 Ctrl+Space 以对应物理键到达页面、
`release` 松开按住的 Ctrl、查看端按着 Shift 和鼠标键断开后两者都被松开；在自带查看页里用输入法组出“中文”只送达一次、
在画面里按下拖到画面外松开仍然送达；加 `--desktop` 时比对桌面剪贴板的回答和本机独立读到的内容（不改剪贴板），
并在隔离的探针窗口里检查 Home、Shift+End、Delete、方向键的编辑效果、扫描码和扩展键标志，以及断开后 Shift 被松开。
`tests/linux/verify_assist_desktop.py` 在自起的 Xvfb 里检查 Linux（CI 的 Linux 任务每次都跑）。

### macOS

截屏经 ScreenCaptureKit（macOS 12.3 起）：每一帧都带着自上一帧以来变化的区域（像素坐标），只把这些区域读进保留的画面，
和 Windows 的 DXGI、X11 的 DAMAGE 一样，不再每帧整屏拷贝和比较；画面按显示器的像素尺寸取，不含指针（指针单独发送，
由查看端画）。窗口移动在 macOS 上只作为变化区域报告，没有 Windows 那样的“移动”指令。没有 ScreenCaptureKit
（更早的系统）、它启动失败或中途停止时，退回 `CGDisplayCreateImage` 截整屏再逐块比较；`source.capture` 报告用的是
`screencapturekit` 还是 `coregraphics`，退回时 `source.screencapturekit` 写明原因。ScreenCaptureKit 是弱链接的，
旧系统上照样能启动。设置环境变量 `AAD_ASSIST_TRACE` 时，每一帧带来的变化区域个数打印到 stderr。
`--desktop` 分享主显示器，`--monitor N` 分享其他显示器，`--list-screens` 列出显示器（单位是点）。输入作为 CGEvent 发出：
鼠标事件带点击次数（macOS 靠它判断双击），按键用物理键对应的 Mac 虚拟键码并带上按住的修饰键标志，文字以 Unicode
字符串输入，与键盘布局无关；快捷键按物理键原样转发，Windows 查看端的 Ctrl+C 在 Mac 上就是 Control+C，不会换成 Command。
指针只报位置，形状用内置箭头（取系统光标图像需要 AppKit）；剪贴板取 `pbpaste` 的输出。截屏需要“屏幕录制”权限，
输入需要“辅助功能”权限，`source.permissions` 报告两者；没有屏幕录制权限时会明确报错。`--window` 在 macOS 上不支持
（`CGWindowListCreateImage` 在新系统上已废弃）。

本机没有 Mac，macOS 在 CI 的 macOS runner 上验证（runner 授予了屏幕录制和辅助功能权限）：列出显示器、截屏走的是
ScreenCaptureKit、查看端拿到第一帧（1024×768）；在 TextEdit 里经 assist 打六行字，发出的补丁约占屏幕的五分之一，
补丁拼出的画面和系统 `screencapture` 独立截的屏逐块比对没有一块不同（和 assist 自己的关键帧比对查不出漏报，
因为关键帧也是同样读出来的），二进制里 ScreenCaptureKit 是弱链接；输入按效果检查——指针落在查看端指的位置（由系统读回）、在 TextEdit 里输入的文字经 Cmd+S 存进文件、
Cmd+A 后输入替换掉全文、Cmd+A Cmd+C 后 `pbpaste` 读到同样的文字。观察到的一点：组合键的四个事件毫无间隔地连发时，
Cmd+C 有时不生效，查看端按真人节奏（键间约 50 ms）发送时稳定生效。双击、滚轮、多显示器、Retina 屏，
以及 ScreenCaptureKit 在 4K/Retina 上相对整屏比较省下多少 CPU，在 Mac 上还没有检查 `[待验证: macOS 真机]`。

### Linux（X11）

不经过 helper 进程，进程内直接说 X11 协议（x11rb，纯 Rust）：DAMAGE 扩展报出每次截屏以来重画的区域，只用 GetImage
读回这些区域，写进和 Windows 一样两份轮换的缓冲区，画面不变时一次往返都不多；没有 DAMAGE 的 X server 退回整屏读取加
逐块比较（`source.capture` 为 `x11` 而不是 `x11-damage`）。输入走 XTest：按键按物理键对应的 keysym 在服务器键盘映射里
找键码，映射里没有的字符（如中文）临时映射到一个空闲键码再按，用完恢复，和 xdotool 的做法相同。XFixes 给出指针位置和
图像（预乘 ARGB 转成 PNG），RandR 给出显示器，`_NET_CLIENT_LIST` 给出窗口，`--monitor`、`--window`、`--list-screens`
和 Windows 一样可用；剪贴板向 CLIPBOARD 的所有者要 UTF8_STRING。只支持 24/32 位色深的屏幕；Wayland 会话里的
原生窗口看不到（XWayland 的可以）。

`tests/linux/verify_assist_desktop.py`（需要 Xvfb、openbox、xev、xdotool、xsetroot、xclip，自起一个 X server，
不碰用户桌面）检查：画面像素就是 xsetroot 设置的颜色、画面静止 3 秒 0 帧、移动一个窗口只发出屏幕 22.8% 的补丁、
补丁拼出的画面和关键帧逐块一致、指针精确落在查看端指的位置（xdotool 读回）、点击、Ctrl、滚轮和输入的“aZ;中”
（“中”走临时键码）都作为 X 事件到达 xev 窗口、查看端按着 Shift 断开后 Shift 被松开、光标消息带可解码的形状、
剪贴板取回 xclip 放进去的文字、`--list-screens` 列出显示器和窗口、`--window` 的画面正好是该窗口大小。

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

- **观察不返回密码值。** `value` 为 `null`，并单独标记 `states.protected`；截图也执行保护区域检查。
- **写入使用专门授权。** 服务启动时授予 `--allow-input --allow-secret-input`，再用
  `--secret-env login=APP_PASSWORD` 绑定服务端环境变量。客户端只发送
  `{"type":"secret_input","secret_ref":"login"}`，替换已观察且再次验证的密码框内容；
  普通 `fill` / `type_text` / `paste` / `key` 不能代替这个授权。
- **本次专用输入不进入录制或工作流。** 保存入口拒绝 `secret_input`，运行时也拒绝直接调用
  原生 secret action；只有即时会话可解引用。服务端结果不返回值或长度。完整条件与平台验证范围见
  [会话的保密输入说明](docs/control-sessions.md#保密输入)。

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
  aad-platform  各系统共同的词汇：共享屏幕与输入（Desktop）、画面与变化区域、按键表、探测结果、结构化错误
  aad-windows   Windows 的系统能力：DXGI/GDI 截屏、PrintWindow、SendInput、剪贴板、OCR、探测（只在 Windows 上编译）
  aad-linux     Linux 的系统能力：X11 截屏（DAMAGE）、XTest 输入、剪贴板、探测（只在 Linux 上编译）
  aad-macos     macOS 的系统能力：ScreenCaptureKit 截屏、CGEvent 输入、剪贴板、探测（只在 macOS 上编译）
  aad-desktop   与系统无关的桌面驱动：UI 模型、Backend 边界、动作分发、录制与观察
  aad-uia       Windows UI Automation 后端（只在 Windows 上编译）
  aad-atspi     Linux AT-SPI / X11 驱动（只在 Linux 上依赖）
  aad-macos-ax  macOS AX 的 Rust adapter 与 Swift helper 协议（只在 macOS 上依赖）
  aad-probe     只读能力探测（汇总报告；各系统的检查在对应平台 crate 里）
  aad-browser   本地浏览器协议：Chromium CDP 与 Firefox WebDriver BiDi、浏览器启动与资料租约、页面录制
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
