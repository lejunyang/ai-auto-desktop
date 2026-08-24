# Windows 与 macOS 虚拟机能力审计

> 审计日期：2026-08-24。本审计只执行只读查询，没有创建、启动或修改虚拟机、内核模块、网络或系统服务。

## 结论

当前机器不能直接、可靠地作为 Windows 或 macOS 的硬件加速虚拟机宿主。它本身是运行在 OpenStack/KVM 上的二级来宾，宿主没有向来宾暴露 Intel VT-x；当前系统没有 `vmx`/`svm` CPU 标志、没有 `/dev/kvm`，也没有已加载的 KVM 模块。

资源配额足够：64 个 vCPU、251 GiB 内存，`/data00` 约有 895 GiB 可用空间。阻塞项不是容量，而是嵌套虚拟化能力、完整虚拟化软件栈和安装介质。

## 当前主机证据

```text
架构：x86_64
CPU：Intel Xeon Platinum 8260，64 vCPU
Hypervisor vendor：KVM
Virtualization type：full
DMI system vendor：ByteDance Inc.
DMI product：OpenStack Nova
systemd-detect-virt：kvm
/dev/kvm：不存在
vmx/svm 标志：均不存在
```

内核配置包含 `CONFIG_KVM=m`、`CONFIG_KVM_INTEL=m` 和 `CONFIG_KVM_AMD=m`，但当前没有硬件虚拟化标志，单独加载模块不能解决问题。

## 软件栈与镜像

当前只安装了 `qemu-img` 等磁盘工具；未发现可执行的 `qemu-system-x86_64`、libvirt、`virsh`、`virt-install`、VirtualBox、VMware、OVMF 或 `swtpm`。项目目录、`~/Downloads` 和常见 VM 目录中也没有发现 Windows/macOS ISO、QCOW2、VHD/VHDX、VMDK 或已有 VM 定义。

`/dev/net/tun` 存在，但当前用户没有 `CAP_NET_ADMIN`，不能自行创建 TAP 或桥接网络。未来若宿主提供 KVM，可以先用 QEMU user-mode NAT 做测试；标准 libvirt NAT/bridge 仍需管理员预配置。

## Windows 测试方案

推荐顺序：

1. 使用真实 Windows CI runner 或远程 Windows VM，这是当前最快且最可信的方案。
2. 让云平台为当前实例开启 nested virtualization，使来宾出现 `vmx` 与 `/dev/kvm`，再安装 QEMU/KVM、OVMF、swtpm 与 Windows 镜像。
3. 切换到裸机 Linux 宿主并启用 VT-x/VT-d。
4. 仅在必要时使用 QEMU TCG 软件模拟；它理论可行但会非常慢，不适合持续的桌面自动化测试。

真实 Windows runner 应覆盖 UIAutomationCore、comtypes、UIA tree/pattern、UIPI、UAC、安全桌面、Job Object、DPI、多显示器和真实应用矩阵。

## macOS 测试方案

当前机器不是 Apple 硬件，且没有 KVM。macOS 的安装与虚拟化通常要求 Apple 品牌硬件；在当前 x86_64 OpenStack Linux 来宾中搭建 Hackintosh/QEMU 方案既不符合常规许可条件，也无法提供可靠的桌面自动化测试结果。

推荐使用：

- 实体 Mac；
- Apple 硬件上的本地 macOS VM；
- 合规的托管 Mac 或 Mac bare-metal 服务；
- 由当前 Linux 主机通过远程测试代理调度上述 Mac 节点。

Apple Silicon macOS 也不适合作为当前 x86_64 主机上的全系统跨架构模拟目标。

## 最终判断

- Windows：当前本机不具备高效 VM 条件，应接远程真实 Windows runner；若平台后续开放 `/dev/kvm`，再考虑本地 KVM。
- macOS：必须接入 Apple 硬件测试节点，不在当前机器上尝试非标准虚拟化。
- Linux：以当前本机 KDE Plasma 5.27 + X11 为第一目标。虽然启动 shell 没有继承
  `DISPLAY`，测试辅助已从同 UID 的 `kwin_x11` 恢复受控会话环境，并完成真实 AT-SPI
  registry、进程协议和有界 snapshot smoke；生产驱动不会扫描 `/proc`。当前默认走 Gio
  只读 fallback，Qt bridge/真实写动作仍需补齐依赖和自有 fixture 后再资格验证。
