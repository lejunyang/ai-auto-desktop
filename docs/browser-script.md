# 用 JavaScript 操作浏览器

`aad browser script`（兼容旧名 `aad browser-script`）和 MCP 的 `browser_script` 执行同一种 JavaScript。每次调用从一个已经打开的页面资源开始。脚本可以写函数、循环、分支，处理数组和字符串，读取页面后决定下一步，并把结果作为 JSON 返回。脚本使用同步的 `page` 方法调用 AAD 的浏览器驱动，运行时随 `aad` 一起分发。

例如把下面的内容存为 `drafts.js`。表单有“标题”输入框、“保存草稿”按钮和“已保存标题”回执字段。

```javascript
const title = {name: "标题"};
const save = {role: "button", name: "保存草稿"};
const receipt = {name: "已保存标题"};
const saved = [];
for (const draft of inputs.drafts) {
    page.fill(title, draft);
    page.click(save);
    const actual = page.read(receipt);
    if (actual !== draft) throw new Error("回执标题与草稿不同");
    saved.push(actual);
}
return {saved};
```

```powershell
aad browser script ./drafts.js --url 'http://localhost:8080/drafts' --browser-port 9222 --allow-input --inputs '{"drafts":["周一","周二"]}'
```

MCP 使用同名参数。在一次 `browser_script` 调用中传 `source`（上面 JS 的源码）、`url` 和 `inputs` 即可。两个标签页地址完全相同时，用 `control_targets` 返回的 `resource_id` 代替 `url`。MCP 的写权限由启动时的 `aad mcp --allow-input` 提供。Firefox 会话使用 `--browser-bidi-port`，MCP 调用另传 `provider: "browser.firefox_bidi"`，可用动作随驱动而定。

| 方法 | 返回或用途 |
|---|---|
| `page.observe("outline")` | 当前界面的摘要。也可选择 `nodes`、`text` 或 `overview`，读取相应的观察结果。 |
| `page.present(locator)`、`page.read(locator, property)` | 判断元素是否存在，或读取 `value`（默认）、`name`、`checked`、`enabled`、`selected`。定位有歧义会报错。 |
| `page.fields([{as, locator, property}, …])` | 在一次观察中读取多个字段，返回按名字取值的对象。 |
| `page.table(locator, columns)` | 从 Chromium 已渲染的表格里按列取记录，返回完整性提示。 |
| `page.collect({table, columns, row_key, traversal, expected_rows})` | 跨页或滚动收集同一张表。用 `traversal: {mode:"pagination", next:{name:"下一页"}}` 指定翻页按钮；虚拟列表使用 `mode:"virtual_scroll"` 和 `container` 定位器。结果包含 `records`、`complete` 和 `stop_reason`。 |
| `page.wait(locator, condition, states)` | 等待 `appears`、`disappears` 或 `state`。最后一种传期望状态，例如 `{enabled:true}`。 |
| `page.fill(locator, text)`、`page.click(locator)`、`page.key(locator, keys)`、`page.scroll(locator, direction, notches)` | 在脚本绑定的页面上重新观察、定位，然后通过驱动执行。 |
| `page.openPopup(locator, exactUrl?)` | Chromium 和 Firefox 都支持。点击当前资源中观察到的元素，等待该 frame 新开的唯一匹配标签页，再将后续操作绑定到新页。可指定完整 URL 等待重定向到达；不传时，不按 URL 筛选。 |
| `page.returnToOpener()` | 将后续操作绑回原来的资源（包括具体 frame）；不激活系统窗口，也不关闭新页。 |
| `page.chooseFiles(locator, files)` | 给 Chromium 的上传按钮选文件。传入一个或多个绝对路径；单选控件传一个。脚本负责点击按钮并处理它打开的文件选择器。 |
| `page.upload(locator, files)` | 已经知道网页文件输入框的位置时，可直接给这个输入框选文件。 |
| `page.act(locator, action)` | 同样的定位方式，可执行 `select`、`check`、`hover`、`navigate`、`back` 等动作。页面导航使用驱动提供的页面元素作为目标。 |

脚本在独立的 JS 环境中运行，通过 `page` 访问浏览器。它没有网页的 `window`、`document`、原始 CDP 会话、Node 模块或宿主文件接口。文件内容是函数体，使用 `return` 返回 JSON。输入和源码各限 64 KiB，每次最多 500 次 `page` 调用，默认 30 秒，可用 `timeout_ms` 或 CLI 的 `--timeout-ms` 调至 120 秒。

成功时返回 `result`、`page_calls`、`completed_actions` 和 `elapsed_ms`。页面调用出错后整段停止，报告保留之前已完成的动作数，以及驱动报告的错误和动作效果。读状态失败时，可以先用 `page.present` 判断可选字段是否存在。

## 给报销单添加附件

把以下代码存为 `attach.js`。“添加附件”是网页上的上传按钮，点下后由网页打开文件选择器。脚本一次完成点击和选文件。网站若另外要求点“提交报销”，可在收到附件回显后继续写该步骤。

```javascript
return page.chooseFiles({role: "button", name: "添加附件"}, inputs.files);
```

```powershell
$dir = (Resolve-Path ./invoices).Path
$files = @{files = @((Join-Path $dir '本月发票.pdf'))} | ConvertTo-Json -Compress
aad browser script ./attach.js --url 'http://localhost:8080/expenses' --allow-input --allow-upload-dir $dir --inputs $files
```

文件放在运行浏览器的机器上。使用 MCP 时，在启动 `aad mcp` 时传 `--allow-input --allow-upload-dir <目录>`，调用 `browser_script` 时只传脚本、页面和业务输入。需要从多个目录选文件，可以重复传 `--allow-upload-dir`。

### 选择整个目录（Chromium，Linux）

对普通 HTML `<input type="file" webkitdirectory>`，仍用 `page.upload`，但列表只能包含**一个明确授权的目录本身**。例如把下面的脚本存为 `folder.js`：

```javascript
return page.upload({name: "项目目录"}, [inputs.tree]);
```

```powershell
$tree = '/home/alice/projects/invoice-bundle'
$stage = '/home/alice/private/invoice-snapshot-001'   # 父目录已存在，此路径不能已存在
$inputs = @{tree = $tree} | ConvertTo-Json -Compress
aad browser script ./folder.js --url 'http://localhost:8080/import' --allow-input --allow-upload-tree $tree --upload-tree-stage $stage --inputs $inputs
```

- `--allow-upload-dir` **不授予递归读取权限**；`--allow-upload-tree` 也不授权选择任意单文件、子目录或相邻目录。当前目录授权只在 `aad browser script` 的 Chromium 模式提供，不用于 Firefox、MCP 或已打开的系统目录选择器。
- 源目录和快照位置都用无软链接的绝对路径。快照要放在源树外、仅由自己和受信任管理员管理的目录中。源树包含软链接（即使指向树内）、多硬链接文件、特殊文件或下级挂载点时，整次选择拒绝，不跳过后继续。只选择可公开给该页面的目录，包括其中的隐藏文件。
- 需要 Linux `openat2` 和 `/proc/self/fd`。授权时固定源根目录；复制时，每个条目都从该根禁止跟随链接、禁止跨挂载地打开，再从已固定的普通文件读取。不把原始目录路径交给浏览器枚举。不支持所需系统能力时拒绝，没有不安全的路径遍历回退。上限为 512 个文件、2048 个条目、16 层目录、总计 64 MiB；空目录树不提交。
- 快照根权限为 `0700`。提交前重新校验观察到的元素及其目录模式；提交后比较浏览器实际 `FileList` 的全部 `webkitRelativePath` 和大小。已派发但不能确认时，不要自动再上传，先核对业务端。
- **等页面真正上传完毕再删除快照**，即便脚本已经退出。成功或派发后结果不明时均保留快照；再次选择请换新路径。不要编辑快照或让不受信任的进程写入它。确认上传后可运行 `Remove-Item -LiteralPath $stage -Recurse`。

实测 Linux Headless Chromium 156：原生 CDP 目录选择能保留嵌套相对路径，但移除底层文件后再读会报 `NotFoundError`。因此这里只传私有副本，而不依赖 Chromium 怎样处理源树中的链接。真实 CLI 回归在脚本退出后改写源文件，再通过页面读取内容并提交原生 `FormData`；两个不同层级的同名文件在 HTTP 请求中保留了各自路径和原始内容。命令：设置 `AAD_TEST_CHROME` 后运行 `node --test tests/browser/directory-upload.test.mjs`。

## 在新标签页确认授权

把以下代码存为 `consent.js`。一次调用先点击业务页的授权按钮，在授权页上确认，再读取原页回执。不要提前调用 `page.click`：`openPopup` 本身负责点击，并在点击前记录已有标签页。

```javascript
page.openPopup({role: "button", name: "开始授权"}, inputs.consentUrl);
page.wait({role: "button", name: "允许"});
page.click({role: "button", name: "允许"});
page.returnToOpener();
page.wait({name: "授权成功"});
return page.present({name: "授权成功"});
```

```powershell
aad browser script ./consent.js --url 'http://localhost:8080/account' --allow-input --inputs '{"consentUrl":"http://localhost:9090/consent"}'
```

如果授权页还会打开身份验证页，在授权页再调用一次 `openPopup`。第一次 `returnToOpener()` 回到授权页，第二次回到业务页。业务页可以在同一段脚本末尾读回执。

每一层都核对浏览器报告的 opener 标签页和 frame。多个新页同时匹配会报歧义。等到截止时间仍未匹配时结束，点击只派发一次。返回时子页可以已经自行关闭，父页必须仍然存在。显式资源白名单会话禁止切页。[Chromium 弹窗验收](../tests/browser/script-popup.test.mjs)

Firefox 使用同一份 `consent.js`，在上述命令中加上 `--browser-bidi-port 9223`，端口换成当前 Firefox 的 BiDi 端口。Firefox 即使对 `noopener` 新页也可能报告历史 `originalOpener`。脚本要求该 ID 和私有环境中读到的当前 `window.opener` 上下文同时匹配发起操作的 frame。上下文树不完整或超过 256 个上下文时，无法建立基线。Linux 无头 Firefox 实测覆盖两层弹窗、确认后立即关闭、回到发起操作的 iframe 读回执，以及相同 URL 从其他 frame 或通过 `noopener` 打开的情况。[Firefox 弹窗验收](../tests/browser/firefox-script-popup.test.mjs)
