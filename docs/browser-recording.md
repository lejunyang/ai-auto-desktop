# 浏览器任务录制：给 AI 编写 skill 的操作证据

`aad browser-record` 记录选定页面中的操作、关键元素与状态变化、网络请求和关键截图，结束后生成简短的 `walkthrough.md`。它独立于 Windows UIA，不需要先启动桌面 `watch`，也不会替用户操作页面、重启浏览器或复制日常登录凭据。

**网络默认不脱敏。** 默认保留采集到的 URL 参数、请求头（包括 Cookie/Authorization）、请求体及响应体原值。只有显式传入 `--redact-network` 才过滤常见敏感字段。正文大小限制、静态资源过滤和重复请求归并是体积策略，与脱敏开关独立。

## 开始录制

先用独立、可复用的 profile 启动支持 CDP 的浏览器，在其中打开要操作的网站。只需首次在该 profile 登录；录制登录流程时从未登录状态开始即可。以下为 PowerShell 示例，浏览器路径按本机调整：

```powershell
# Windows Edge；使用独立目录，不关闭或修改日常浏览器 profile。
& 'C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe' `
  --remote-debugging-port=9222 --remote-debugging-address=127.0.0.1 `
  "--user-data-dir=$env:LOCALAPPDATA\aad\recording-profile"

# Linux（在 PowerShell 7 中执行）
microsoft-edge --remote-debugging-port=9222 --remote-debugging-address=127.0.0.1 `
  "--user-data-dir=$env:HOME/.local/share/aad/recording-profile"
```

用当前仓库构建 CLI；以下命令中的 `aad` 指该构建产物或已安装的同版本可执行文件。

```powershell
osdk exec --tool rust -- cargo build --locked -p aad-cli

# 先列出标签。复制目标页面的 id；不按标题猜测，也不会取第一个标签。
aad browser-record targets --cdp-port 9222

# login-recording 必须是一个尚不存在的目录。
aad browser-record start --cdp-port 9222 --page '<page-id>' --out ./login-recording
```

开始后正常操作网站。可以在另一个终端标记关键结果或停止：

```powershell
aad browser-record mark --out ./login-recording --note '点击这里出现二维码；扫码后个人菜单可见'
aad browser-record stop --out ./login-recording
```

原终端按 Ctrl+C 同样会正常收尾、移除监听并生成步骤说明。`stop`/`mark` 返回的是请求已写入；以录制进程退出及 `manifest.json` 的最终状态为准。也可以使用 `--seconds 120` 定时结束。

## 参数与行为

| 参数 | 默认 | 含义 |
| --- | --- | --- |
| `--cdp-port` | `9222` | 连接已经显式开启的本机调试端口 |
| `--page` | 必填 | 精确 page ID |
| `--out` | 必填 | 新目录，不覆盖旧记录 |
| `--seconds` | `0` | 0 表示持续到 Ctrl+C 或 stop |
| `--redact-network` | **关闭** | 开启后按字段名过滤 Cookie/Authorization、密码、token/ticket 等；JSON 保留业务字段，无法解析的正文省略为占位符 |
| `--body-limit` | `32768` | 每条文本请求/响应正文上限，单位字节；0 跳过正文，最大 1MiB |
| `--max-bytes` | `104857600` | 100MiB 会话证据预算；到达后明确标记 incomplete，不静默继续 |
| `--no-screenshots` | 关闭 | 不生成截图 |
| `--no-follow-popups` | 关闭 | 只记录选中页及其 frame，不跟随 opener 关联页面 |

例如需要网络脱敏时：

```powershell
aad browser-record start --page '<page-id>' --out ./login-filtered --redact-network
```

脱敏是按常见字段名进行的可选过滤，不是任意站点的通用秘密识别器；它只影响网络证据。截图、页面文字、用户备注采用各自的记录行为，截图当前没有自动遮蔽。密码输入框值不通过页面事件读取，此规则不改变网络默认保留原值的行为。

响应正文以浏览器 CDP 实际提供的内容为准：文本/JSON 等可读取类型才取 body；二进制图片不作为响应 body 导出。超限截断、浏览器缓存淘汰导致正文不可取、重定向、尚未完成等状态显式记录。不会声称得到完整的线上字节流。

## 输出与降噪

```text
login-recording/
  manifest.json                # 配置、范围、complete/incomplete、覆盖盲区、丢失统计
  walkthrough.md               # 先给 AI 阅读的简短步骤说明
  steps.jsonl                  # 每步完整动作、目标候选、关键变化、截图及请求引用
  evidence/
    actions.jsonl              # 动作检查点；连续输入用 edit_update 更新同一步
    ui.jsonl                   # 界面变化详情；按 step_id 查询
    network.jsonl              # 请求/响应及轮询分组；按 id 查询
    network-extra.jsonl        # CDP ExtraInfo 里的真实请求/响应头
    network-streams.jsonl      # WebSocket/SSE 消息（有采集到时生成）
    health.jsonl               # 事件溢出、附加空窗、截图缺失等（发生时生成）
  images/<sha256>.png          # 关键检查点截图，按内容去重
  commands.jsonl               # mark/stop 控制请求（使用时生成）
```

- **操作顺序**：保留每次点击；只合并同一文档、同一元素的连续输入，跨点击/提交不会归并。记录输入目标 locator 候选与来源 `trusted` 标记。
- **界面**：读取可见区域的命名控件、弹窗、文本、图片/Canvas 等语义信息，比较 value/enabled/checked/expanded 等状态。class/style 变化本身不会逐条进入日志。每个快照最多 100 个关键元素，截断有标记；每步摘要最多 12 项，更多变化保存在 UI 证据文件。
- **网络**：Document/XHR/Fetch 等保留详情；普通静态资源主要计数，URL 含 qr 的资源及错误请求保留详情。相同请求、响应体、业务状态的连续轮询归并次数；waiting→confirmed→waiting 保留三段变化。补充头按原始 wire_key 及接收时间另存，避免将不同重定向 hop 的 ExtraInfo 错配。
- **截图**：基线、操作后的界面变化、导航和 mark 请求触发；每步最多 3 张，节流及内容去重。截图前后校验主文档 loader，跨导航的图不冒充同一状态。操作太快时不保证每个中间画面都被拍到。
- **业务信息**：请求摘要会列出常见返回字段（如 status、qr_id、order_id、data.id），完整采集值仍在网络证据中。字段摘要不是脱敏。

对当前默认分组策略，时间戳每次都变化的 URL/body 不会强行合并，以免错误丢掉业务变化；站点专用的埋点/动态字段过滤尚未提供。默认分类也无法准确识别所有非命名二维码图片接口，可通过页面 src 和截图补充识别。

`walkthrough.md` 示例：

```text
点击「扫码登录」
目标：button「扫码登录」，id=login。
- 出现 dialog「使用手机扫码」
- 出现 canvas「code」
- 请求：POST /auth/qr/create → 200，1 次；证据 req-1。
  返回信息：status="created"，qr_id="fixture-qr"。
- 请求：GET /auth/qr/status → 200，3 次；证据 req-2。
  返回信息：status="waiting"。
- 请求：GET /auth/qr/status → 200，1 次；证据 req-5。
  返回信息：status="confirmed"。
- 出现 button「个人菜单」
```

给 AI 的输入以该说明为主；需要判断参数或提取字段时，再按 req-id/step-id 阅读证据。等待条件是从观察结果推导的候选，不代表 HTTP 200 就是业务成功。生成的 skill 应保留手机扫码等人工步骤、参数及成功检查，执行时重新观察并校验目标；录制内的 node_id 不是可长期复用的操作句柄。

需要重新生成说明时：

```powershell
aad browser-record export --out ./login-recording
```

## 完整性与当前边界

- CDP 断连、写入失败、超预算会失败退出并尽力写入 `incomplete`，不要把缺失当作用户没有操作。网络和步骤分组在正常停止时最终落盘；崩溃时可读动作/UI 检查点，但不能保证所有内存中的请求分组已保存。当前不自动重连恢复录制。
- 同进程 iframe 与跨进程 iframe 有独立上下文归属。跨进程 frame 附加前的短暂空窗会写 health；登录 popup 每秒按 opener 关系发现，初始请求可能早于附加，记录明确的 gap。无 opener 关系的新标签、其他已有标签不在范围内。
- 不独立附加 service worker；不是“全浏览器所有网络”。Canvas 内部独立重绘靠检查点截图，不具备持续像素变化检测。
- UI 证据按采样时状态记录，多个快速操作之间的短暂界面可能未被拍到。网络与动作的对应是时间/frame 关联推断，不是因果证明。
- 标准录制是只读采集，不安装 fetch 拦截，不禁用缓存、不重发请求，不增加接受裸坐标或任意 JS 的 MCP 工具。

## 验证

2026-10-24 在 Linux 的真实 Edge headless 进程、独立 profile 和 loopback fixture 上验证；输入是测试夹具的合成事件，不等同于真人物理输入：

- 输入→输入→点击、重复点击、不同无标签输入框、队列溢出计数。
- 二维码弹窗/Canvas、按钮状态变化、关键截图（已人工回读截图），最终个人菜单。
- 网络默认保留 query/header/Cookie/postData/response 中的测试原值；显式开启脱敏后对应测试敏感值消失。
- 四次状态轮询形成两组（3 waiting + 1 confirmed），服务端独立请求计数核对通过。
- 同源 iframe、跨域 OOPIF、整页导航前点击、关联 popup 的操作与关闭、无关标签隔离。
- mark、stop、Ctrl+C、停止后移除监听、旧产物拒绝覆盖、断连标记 incomplete。

一次该测试夹具的默认原值录制：7 步、4 张去重截图，`walkthrough.md` 为 3138 字节，全部产物约 80KiB。这是本地 fixture 的实测体积，不是实际网站的压缩率或容量保证。

复现命令（按本机路径设置浏览器；GUI/浏览器实测串行）：

```powershell
osdk exec --tool rust -- cargo build --locked -p aad-cli
osdk exec --tool rust -- cargo test --locked -p aad-cdp -p aad-cli
osdk exec --tool rust -- cargo clippy --locked -p aad-cdp -p aad-cli --all-targets -- -D warnings
$env:AAD_TEST_CHROME = '/usr/bin/microsoft-edge'
# Windows 需另外设置 AAD_TEST_CLI 指向 target/debug/aad.exe；当前 fixture 清理主要针对 Linux。
node --test --test-concurrency=1 tests/browser/observer.test.mjs tests/browser/recording.test.mjs
```

`[待验证]` Windows/macOS 的目标浏览器、真人中文 IME/物理点击、真实站点 SSO/扫码、企业策略，以及至少 30 分钟的性能与容量基准。当前结果不代表这些环境均已通过。
