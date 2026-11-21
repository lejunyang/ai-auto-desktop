# 真实请求闸门验收 · 2026-11-21

Linux、无头 Microsoft Edge，分支 `feat/request-gate`，浏览器实现提交 `f15d1a4`。真实规则拒绝向回环主机发送路径以 `/gate-e2e/deny` 开头的 POST。有效令牌下，放行的 POST 到达一次；无效令牌下重复同一动作，新增收件为零。

构建与复验命令如下。设置 `AAD_TEST_CHROME` 为本机 Edge 或 Chromium 的可执行文件，保留当前会话的 `OMEM_URL`、`OMEM_TOKEN`。

```sh
osdk exec -- cargo build --locked -p aad-cli
node tests/browser/verify-real-request-gate.mjs
```

1. 读取范围。脚本用 `Authorization: Bearer <会话令牌>` 请求 `GET ${OMEM_URL}/api/operation-gate/http`。HTTP 200 返回：

   ```json
   {"revision":"a6c4113124db717a","watch":[{"host":"127.0.0.1","methods":["POST"]}]}
   ```

2. 启动页面。脚本在回环随机端口启动记录请求的 HTTP 服务，用 `--headless=new --remote-debugging-port=0` 打开页面。Deny、Allow 分别向下表中的路径发送 POST，Read 发送 GET。

3. 依次点击 Deny、Allow、Read。每次调用同一条分支 CLI 命令，各次脚本中的 `name` 分别为对应的按钮文字。

   ```sh
   target/debug/aad browser-script <点击脚本> --url <测试页> --browser-port <调试端口> --allow-input
   ```

   ```js
   page.click({role:"button",name:"Deny"}); return true;
   ```

   | 按钮 | 方法和路径 | 动作返回 | 服务器新增收件 |
   | --- | --- | --- | --- |
   | Deny | POST /gate-e2e/deny/thing | BROWSER.REQUEST_BLOCKED，deny | 0 |
   | Allow | POST /gate-e2e/allow/thing | result=true | 1 |
   | Read | GET /gate-e2e/read | result=true | 1 |

   Deny 的规则文案为「omem 的操作规则「临时：验证请求闸门（验完删）」不允许 Agent 执行这项操作。」恢复提示为 `The rule forbids this request. Do not try another route.`

4. 仅将子进程的 `OMEM_TOKEN` 换为无效测试值，重复第 3 步的 Allow 点击。动作返回 `BROWSER.REQUEST_BLOCKED`，拦下的是 `POST /gate-e2e/allow/thing`，裁决为 `unavailable`，原因为 `request gate token expired`，提示为 `Restore the request gate before retrying.`。服务器收件数保持不变。

5. 回读服务器的完整收件清单，按到达顺序如下。脚本同时断言业务收件的方法、路径、顺序和次数，退出码为 0。

   ```text
   GET  /
   GET  /favicon.ico
   POST /gate-e2e/allow/thing
   GET  /gate-e2e/read
   ```
