# 用 JavaScript 操作浏览器

`aad browser-script` 和 MCP 的 `browser_script` 执行同一种 JavaScript。每次调用绑定一个已经打开的标签页。脚本可以写函数、循环、分支，处理数组和字符串，读取页面后决定下一步，并把结果作为 JSON 返回。脚本使用同步的 `page` 方法调用 AAD 的浏览器驱动，运行时随 `aad` 一起分发。

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
aad browser-script ./drafts.js --url 'http://localhost:8080/drafts' --browser-port 9222 --allow-input --inputs '{"drafts":["周一","周二"]}'
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
| `page.act(locator, action)` | 同样的定位方式，可执行 `select`、`check`、`hover`、`navigate`、`back` 等动作。页面导航使用驱动提供的页面元素作为目标。 |

脚本在独立的 JS 环境中运行，通过 `page` 访问浏览器。它没有网页的 `window`、`document`、原始 CDP 会话、Node 模块或宿主文件接口。文件内容是函数体，使用 `return` 返回 JSON。输入和源码各限 64 KiB，每次最多 500 次 `page` 调用，默认 30 秒，可用 `timeout_ms` 或 CLI 的 `--timeout-ms` 调至 120 秒。

成功时返回 `result`、`page_calls`、`completed_actions` 和 `elapsed_ms`。页面调用出错后整段停止，报告保留之前已完成的动作数，以及驱动报告的错误和动作效果。读状态失败时，可以先用 `page.present` 判断可选字段是否存在。
