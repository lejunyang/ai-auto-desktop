# 可编程 Recipe

`aad validate program.yaml` 检查整个程序，包括不会运行的分支。`aad run program.yaml` 将同一份程序交给工作流引擎。使用 `format: recipe/2` 选择下面的语法。已有的无 `format` 分阶段 Recipe、录制导出格式和 Workflow 描述符继续使用各自原有的语义。

## 遍历订单并核对结果

```yaml
kind: Recipe
format: recipe/2
name: submit-orders
inputs: [orders]
targets:
  order: {role: textbox, name: Order}
  save: {role: button, name: Save}
phases:
  - app: {provider: browser.cdp, title: Recipe orders}
    steps:
      - set: {submitted: 0}
      - foreach: "${{ orders }}"
        as: row
        index_as: index
        max_items: 50
        steps:
          - if: "${{ row.selected }}"
            then:
              - {locator: order, action: {type: fill, value: "${{ row.number }}"}}
              - {locator: save, action: {type: click}}
              - set: {submitted: "${{ submitted + 1 }}"}
            else:
              - {locator: save, check: {present: true}}
      - set: {attempt: 0, ready: false}
      - while: "${{ not ready }}"
        max_iterations: 3
        steps:
          - id: status
            read: [{as: saved, locator: {name: Saved}, property: present}]
          - set: {ready: "${{ status.values.saved }}", attempt: "${{ attempt + 1 }}"}
outputs: {submitted: "${{ submitted }}", checks: "${{ attempt }}"}
```

```powershell
aad validate ./program.yaml
aad run ./program.yaml --inputs '{"orders":[{"number":"ORD-1","selected":true}]}' --permission desktop.observe --permission desktop.input
```

- `inputs` 列出必需的 JSON 值。`set` 同时计算各字段的右值，再写入。首次赋值声明变量，后续赋值可更新进入本层时已存在的可写变量。
- 应用阶段顺序执行并共享顶层名字。循环体和两个分支各有自己的作用域。该层新声明的名字仅在本层后续步骤和内层可见。循环元素和可选的从零开始的索引可以遮蔽外层同名值，离开循环后恢复外层含义。输入、循环绑定、已命名动作的结果均为只读。
- 在表达式中直接写名字，例如 `row.number`。整段 `${{ ... }}` 保留 JSON 类型，在文本中的表达式会插值。表达式沿用工作流引擎的比较、布尔运算、索引和算术语法。布尔常量写 `True` 和 `False`。Recipe 名字使用小写字母开头的 1–48 位小写字母、数字和下划线，排除双下划线与保留字。
- 控制步骤沿用录制步骤的应用选择、定位器、参数校验、权限和操作语义。`id: status` 让后续步骤以 `status.values.saved` 取命名读取值。运行日志里的步骤 ID 包含编译序号以及显式指定的 ID。顶层 `outputs` 明确选择要交给调用方的值。
- `foreach`、`if`、`while` 均接受一个完整表达式。`foreach` 在进入循环体前检查数组长度，超出 `max_items` 即报错。`while` 每轮前重新计算条件，达到 `max_iterations` 后若条件仍为真则报错。此前已成功的动作保留效果。
- 一个程序最多 500 个编译后步骤、16 层嵌套，执行时间上限为 10 分钟。编译器按循环上限计算各步骤的最多访问次数，合计最多 100,000 次，两个分支都计入。数据依赖的属性缺失、类型不匹配、页面状态和权限等在执行到相应步骤时检查。

## 按版本调用子配方

把下面的文件保存为主程序目录下的 `lib/enter-order.yaml`。同一个文件可以在不同阶段和循环内多次调用。

```yaml
kind: Subrecipe
format: recipe/2
name: enter_order
version: "1.2.0"
parameters: [number]
phases:
  - app: {provider: browser.cdp, title: Recipe orders}
    steps:
      - {locator: {name: Order}, action: {type: fill, value: "${{ number }}"}}
      - id: receipt
        read: [{as: typed, locator: {name: Order}, property: value}]
results: {value: "${{ receipt.values.typed }}"}
```

在主程序的阶段内调用，`with` 的键与所有形参一一对应。`save` 的键是要取回的结果名，值是调用方的本地名字。

```yaml
kind: Recipe
format: recipe/2
name: check-entry
phases:
  - app: {provider: browser.cdp, title: Recipe orders}
    steps:
      - call: {file: lib/enter-order.yaml, name: enter_order, version: "1.2.0"}
        with: {number: ORD-42}
        save: {value: entered}
outputs: {entered: "${{ entered }}"}
```

实参在进入子配方时一次求值并保留 JSON 类型。每次调用有自己的参数、变量、步骤结果和定位器别名。子配方的所有 `results` 在正常结束时同时求值。调用方可省略 `save` 或只选部分结果。新接收的名字为只读，也可把结果存入之前用 `set` 声明的可写变量，例如跨循环保留最后一笔回执。

`file` 相对当前调用文件解析，嵌套调用也遵循这个规则。规范化后的文件必须在主程序的目录树内。版本采用精确的 `MAJOR.MINOR.PATCH` 发布号，显式匹配 `name` 和 `version`。发布者如需区分文件内容，应随改动更新版本并保留旧版文件；版本号本身没有内容哈希。一次编译只读取同一子文件一次。

校验会展开所有调用，包括未进入的分支，检查版本、实参、返回值映射、循环调用和重复键。子程序与主程序共同使用步骤数、深度和执行预算。单份源码上限为 2 MiB，所有子文件按调用位置展开后的源码总量也以 2 MiB 为限。
