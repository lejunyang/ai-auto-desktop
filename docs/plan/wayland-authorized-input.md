# Wayland 授权传输与输入边界

更新于 2026-11-22。本文涵盖 Qt 5 Widgets 窗口定向输入及 RemoteDesktop 授权传输。

## 已验证的 Qt 5 Widgets 输入

启用目标侧插件后，`aad do key` 在已聚焦控件上发送 Qt 按键事件，`aad do click` 在通过窗口内命中检查的控件中心发送一次左键按下和释放。控制端重新观察元素，核对原窗口、AT-SPI 唯一总线名与进程，并将请求发往同一应用连接。应用 GUI 线程把目标和窗口路径还原成实际 QWidget，校验当前窗口、焦点或命中后同步派发，释放前复核。Qt 5 自身的 AT-SPI 适配器使用同一套对象 ID 和 D-Bus 连接。[Qt 对象路径解析](https://raw.githubusercontent.com/qt/qtbase/5.15/src/platformsupport/linuxaccessibility/atspiadaptor.cpp) · [Qt 辅助功能连接](https://raw.githubusercontent.com/qt/qtbase/5.15/src/platformsupport/linuxaccessibility/dbusconnection.cpp)

私有 KWin 5.27.5、Qt 5.15.8、无 DISPLAY 环境下，Qt 表单记录了两对键盘事件及一对鼠标事件。Ctrl+A 选中了完整发票号，后续按键替换了文本，点击触发一次提交。相同 PID 且标题相同的非活动窗口、被子控件覆盖的按钮、密码输入框以及未加载插件的应用均未收到所请求的事件。发行版 KDialog 22.12.3 的输入框经键盘替换和点击确认后，进程以状态 0 退出并输出替换后的字符。

GNU Linux 发布包提供预编译插件，各 Linux 包均提供可用目标应用的 Qt 重编译的源码。启动示例、支持的键名与手势范围见[插件使用文档](../../crates/aad-linux/qt-wayland-input/README.md)。

从仓库根目录运行以下命令可在独占的私有 KWin 会话复验，需要 KWin Wayland、Qt 5 开发工具、PyQt5、KDialog、AT-SPI 与 D-Bus。

```powershell
osdk exec -- cargo build --locked -p aad-cli
$pluginRoot = Join-Path (Resolve-Path target).Path 'wayland-input-qt5'
./crates/aad-linux/qt-wayland-input/build.ps1 -BuildDirectory $pluginRoot
./tests/linux/verify-wayland-form.ps1 -Aad ./target/debug/aad -QtInputPluginRoot $pluginRoot
```

## 当前交付

`aad-linux::wayland_portal` 已实现真实 D-Bus RemoteDesktop/ScreenCast 授权传输。它尚未接入 CLI/MCP，也不提供任何输入事件。`PortalClient::connect` 只连接和固定 portal 的唯一 D-Bus owner；只有显式 `authorize(AuthorizationIntent::UserRequested, ...)` 才会创建会话并调用可能显示授权对话框的 `Start`。

| 层 | 当前能力 | 验证边界 |
|---|---|---|
| 授权 | CreateSession → SelectDevices → SelectSources → Start；固定 keyboard/pointer、单 monitor、persist_mode=0 | 私有 D-Bus 的真实 zbus fake portal 已验证；GNOME/KDE Wayland 授权 UI **[待真机验收]** |
| 请求生命周期 | 调用前订阅精确 owner/path 的 Response，校验返回 handle；取消、期限、拒绝、错误响应清理 Request/Session | 提前发送 Response、错误路径、缺失 stream、拒绝、取消和期限测试通过 |
| 会话生命周期 | 每个公共会话读取/FD 获取前轮询 Closed 与 portal owner 变化；失效增加 generation，关闭拥有的 FD；Drop 关闭会话及独立客户端连接 | Closed、portal 消失、FD EOF 和失败清理通过；当前没有后台通知线程，调用者必须持续 refresh |
| FD 传输 | 每会话至多一次 ConnectToEIS 和 OpenPipeWireRemote，FD 由会话持有 | fake portal 的 Unix FD 传递及回收通过；未启动 libei 握手或 PipeWire 视频消费 |
| 坐标映射 | 仅接受唯一 mapping_id、相同逻辑 origin/size、非零稳定 pipewire-serial、未裁剪/旋转的帧；旧 generation、变化帧和越界比例拒绝 | 纯映射单测通过；实际多屏、缩放、旋转和热插拔 **[待真机验收]** |
| Portal 输入 | RemoteDesktop 只做授权与生命周期管理 | 已验证的按键和点击通过上文所述的应用侧 Qt 接口派发 |

`e1e2d47` 提交授权与清理，`df4de21` 提供独立私有 D-Bus 验证入口；`117c599` 进一步将映射绑定到随机会话身份与 revision。即使新会话返回相同 stream serial 和 mapping_id，也不能复用旧映射；每次映射调用还必须比较当前 EI region，位置或大小变化即拒绝。跨会话重放和 region 移动负例均通过。

授权结果只说明设备和屏幕访问被允许，不能说明下一次键盘事件一定进入某个已观察元素。窗口身份、真正焦点、命中关系和 scope 仍须独立证明，不能用授权成功替代。

## 持久授权的事实修正

RemoteDesktop v2 已定义 `persist_mode`：0 不持久化，1 持续到应用退出，2 持续到撤销；同时支持单次使用的 `restore_token`，恢复成功后需使用新的 token。实际是否授予、撤销和恢复行为取决于 portal 后端及用户选择，不能写成“每次必弹窗、协议无持久授权”。组合 RemoteDesktop/ScreenCast 会话的持久策略只应传给 RemoteDesktop。本实现选择 0，不接收调用者 restore token，不存储或输出响应中的 token；会话和客户端不实现 Debug，错误只包含固定枚举。[RemoteDesktop 规范](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html)

ScreenCast `position`/`size` 属于 compositor 逻辑坐标，不应当作像素坐标。`mapping_id` 可关联 EI region，但多个设备可能各自有一个匹配 region；当前遇到多重匹配拒绝。v6 的 `pipewire-serial` 解决 node ID 被重用的问题；旧后端缺少 serial 时允许完成授权，禁止建立本层的坐标映射。[ScreenCast 规范](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html)

## 可推进的窗口绑定路线

以下是基于官方 API 的实现方案，尚未连接真实 Wayland compositor；不能作为当前支持声明。

| compositor | 可读取的官方信息 | 仍需补齐的证明 |
|---|---|---|
| KWin | scripting API 的 internalId、pid、activeWindow、windowAt、frameGeometry、clientGeometry，以及窗口/焦点变更信号 | 安装并显式启用只读 compositor bridge；固定 KWin 的 D-Bus 唯一 owner及重启 generation；证实窗口与 AT-SPI 顶层对象的直接关联。公开脚本 API 中尚未找到通用 AX object 绑定，不能按标题猜测 |
| Mutter/GNOME Shell | 当前 Meta API 52 暴露 Window 的 a11y-dbus-name/a11y-object-path、get_id、几何及 Display.get_focus_window | 通过显式启用的 Shell extension 在 compositor 进程读取；能力探测真实版本与 toolkit 是否填充这两个属性；再核对 a11y bus 的唯一 owner、UID/PID/starttime 和对象祖先链。属性缺失则拒绝，不退回标题匹配 |

依据：[KWin scripting API](https://develop.kde.org/docs/plasma/kwin/api/)、[Meta.Window](https://gnome.pages.gitlab.gnome.org/mutter/meta/class.Window.html)、[Meta.Display](https://gnome.pages.gitlab.gnome.org/mutter/meta/class.Display.html)。这些是 compositor 内部脚本/库接口，不是普通客户端可直接调用的通用桌面 D-Bus API。Mutter 的 `get_pid` 官方文档还明确提示该值可能由客户端伪造，因此不能单独用于可信身份判断。[Meta.Window.get_pid](https://gnome.pages.gitlab.gnome.org/mutter/meta/method.Window.get_pid.html)

bridge 的下一步应先只读：返回 compositor generation、窗口稳定 ID、直接 AX 对象引用、client geometry、真实焦点及锁屏/overview/grab 状态；任何信息缺失、owner 变化或多窗口歧义均拒绝绑定。生产输入仍应在派发前后复核这些事实，并保留 AT-SPI fresh identity、protected、scope 和命中检查。只读 bridge 不应自行激活窗口、修改 compositor 配置或安装扩展。

## PipeWire 与 EI 的后续实现

PipeWire 消费层应使用授权 FD 创建受限 remote，并以稳定 serial 指定目标流；读取实际协商格式、帧尺寸、crop/transform 元数据，帧变化立即废弃映射。第一阶段可仅接受明确的完整、未变换 CPU 帧；DMA-BUF、旋转和裁剪应在各自变换验证后单独支持，不能只以 buffer 宽高推断。[PipeWire 视频元数据](https://pipewire.pages.freedesktop.org/pipewire/meta_8h_source.html)

EI 层应先完成设备发现、region/keymap 读取与 pause/resume/remove/disconnect 生命周期，再接派发。每次设备 generation 变化均废弃旧授权输入状态，清理仅释放本调用拥有的输入；取消必须贯穿等待、逐帧派发和清理。ConnectToEIS 的 FD 本身不是已就绪的 keyboard/pointer 设备。没有可信窗口/焦点证明时，即使 EI 握手成功也不派发。[libei 客户端 API](https://libinput.pages.freedesktop.org/libei/api/group__libei.html)

## 可复现验证

```powershell
osdk exec -- cargo test -p aad-linux --lib
osdk exec -- cargo clippy -p aad-linux --all-targets -- -D warnings
pwsh -NoProfile -File tests/linux/verify-wayland-portal.ps1
```

最后一项创建私有 D-Bus 并运行 fake portal，不接触用户 portal、不显示对话框、不操作 GUI。它测试的是协议传输和清理，而非 Wayland 输入效果。真实验收还需要 GNOME/KDE Wayland 环境，覆盖接受/拒绝/撤销、双窗口同 PID、遮挡、锁屏、焦点切换、多屏缩放、热插拔、portal 重启与取消后的按键释放。
