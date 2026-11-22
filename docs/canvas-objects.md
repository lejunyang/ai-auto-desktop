# 在画布里找对象

在 Chromium 中打开目标页面。下面的脚本在名为“结算流程”的画布上寻找两个叫“审批”的节点，点击从“付款”直接连过去的那个。编辑器公布了对象数据时，AAD 使用编辑器给出的 ID、标签、边界和有向连线。画布只有像素时，AAD 从可见图像识别流程图，并用 Tesseract 读取形状里的文字。

```js
const diagram = {name: "结算流程"};
return page.clickObject(diagram, {label: "审批", after: "付款"}, {language: "zh-Hans"});
```

将脚本存为 `approval.js`，用已打开页面的准确地址执行。像素模式读中文需要本机安装相应的 Tesseract 语言数据。

```powershell
aad browser script ./approval.js --browser-port 9222 --url 'http://localhost:8080/flow' --allow-input
```

## 看识别结果，再选对象

```powershell
aad browser read --url 'http://localhost:8080/flow' --canvas-objects '{"name":"结算流程"}' --preview ./flow.svg
```

`objects` 列出 ID、类型、标签和相对于画布左上角的边界。`edges` 中的 `from` 和 `to` 指向同一次读取返回的 ID。方向或端点无法确认的线标为 `uncertain`。`after` 匹配直接前驱的完整标签，`before` 匹配直接后继的完整标签，只使用已确认的连线。也可以只按 `kind` 查找没有文字的形状。

`--preview` 把同一次读取的截图和编号框保存到一个新的 SVG 文件，编号对应 JSON 里的 `number`。画面中有多个相似图标时，可以看图选取，再把那个对象的 ID 和此次读取的 `digest` 交给点击接口。

```js
return page.clickObject(
  {name: "白板"},
  {id: inputs.id, digest: inputs.digest},
  {description: "a printer icon"}
);
```

按语义点击会重新找对象，适合节点移动后的流程图。按像素候选 ID 点击会核对此前的摘要，适合用户刚从预览里选定一个对象。匹配不唯一会返回 `CONTROL.AMBIGUOUS` 和候选信息。鼠标移入后，驱动会在按下前再核对编辑器数据或画布像素。如果对象中心落在另一个已识别对象上，此次点击会返回 `CONTROL.OCCLUDED`。

## 识别来源

| 来源 | 可读取的内容 | 使用条件 |
| --- | --- | --- |
| `editor` | 应用提供的对象 ID、类型、标签、边界和连线，支持重名和无标签对象 | 应用在可交互的 canvas 元素上发布 `data-aad-scene`。 |
| `flowchart` | 单色背景上轮廓清晰的矩形、圆角矩形、菱形及带箭头的直线、直角折线 | 图形需要完整可见。交叉、遮挡或难以判断方向的连线不能用于关系定位。 |
| `vision` | 本机模型按描述识别图标、手绘内容或照片中的物体，返回候选区域 | 设置本机模型和 Python 环境，或设置兼容的本地提供程序。识别效果取决于模型、描述和图像。 |

默认 `auto` 优先读取编辑器对象，否则读取流程图。提供 `description` 会选择视觉模型。CLI 使用 `--source`，脚本使用 `options.source`。MCP 提供 `control_read_canvas_objects` 和 `control_act` 的 `click_object` 动作，两者都使用已观察的画布目标。

编辑器模式直接读取公开的对象数据，点击时目标位置必须能接收指针事件。像素读取和截图预览沿用 Chromium 的可见画布校验。[尺寸、显示比例和遮挡要求](testing/canvas-grid-ocr.md#capture-requirements)

### 发布编辑器数据

Fabric.js 应用可以直接接入仓库提供的适配器。它在重绘后更新可见对象和边界，并把数据发布到实际接收鼠标事件的 canvas。[Fabric.js 接入示例](../plugins/canvas_scene/README.md)

应用应在重绘、平移、缩放及修改连线时更新属性。边界采用当前画布中可见对象的归一化比例。

```js
canvas.dataset.aadScene = JSON.stringify({
  objects: [
    {id:"pay", kind:"rectangle", label:"付款", box:{fx:0.05,fy:0.2,fw:0.2,fh:0.15}},
    {id:"approve", kind:"diamond", label:"审批", box:{fx:0.55,fy:0.2,fw:0.2,fh:0.15}}
  ],
  edges: [{from:"pay", to:"approve"}]
});
```

### 配置本机视觉模型

内置提供程序在本机 CPU 上执行 OWL-ViT 或 OWLv2。Python 环境需要 PyTorch、torchvision、Pillow 和 transformers。本地模型目录应含兼容的 safetensors 权重、配置、图像处理器和 tokenizer。以下命令使用 PowerShell 7。

```powershell
$env:AAD_CANVAS_VISION_PYTHON = '/opt/vision/bin/python'
$env:AAD_CANVAS_VISION_MODEL = '/models/owlv2'
aad browser read --url 'http://localhost:8080/board' --canvas-objects '{"name":"白板"}' --description 'a printer icon' --preview ./printers.svg
```

自有模型可通过 `AAD_CANVAS_VISION_PROVIDER` 指定可信本地可执行文件的绝对路径。程序从标准输入读取这块画布的 PNG 和一句描述，经标准输出返回候选区域。[输入输出格式](../crates/aad-control/src/browser/canvas_vision.py)

例如，要点右上角的打印机，并要求模型得分至少为 0.5，可以使用 `page.clickObject({name:"白板"}, {region:"top_right",min_confidence:0.5}, {description:"a printer icon"})`。方位按候选框的中心判定，阈值按当前模型返回的分数筛选。
