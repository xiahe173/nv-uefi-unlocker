# nv-uefi-unlocker

[中文](README.md) | [English](README.en.md)

UEFI 预驱动 NVIDIA GPU 解锁器（Windows，Rust）。在 NVIDIA 驱动加载**之前**的
UEFI DXE 时窗执行完整固件事务：SEC2 scrubber → FWSEC-FRTS（WPR2）→ 签名
Booter 启动 GSP-RM →（门控后）V67 四写 payload → 读回验证 → FLR → 链式交回
Windows Boot Manager。

**当前状态：M4 闭环。** 已在 RTX 4060 Laptop（`10de:28e0`，AD107M）真机完成
四写解锁并双端验证——固件侧 marker `0x67b00001` + 四 PLM 读回，Windows 侧
FP16 GEMM 29.89 → 49.33 TFLOPS（1.65×）。完整报告见
[docs/ada28e0-uefi-report.md](docs/ada28e0-uefi-report.md)（bar1 报告格式：
判据表、链条 15 步、关键修复、社区致谢）。

该卡此前未被社区验证（最近的公开 canary 是 28e1，同芯片 ASUS 移动端）。

## 使用（RTX 40 系笔记本/台式，AD10x 家族）

### U 盘根布局（FAT32；`make-usbroot.ps1` 自动产出，手摆按下表）

| 文件 | 作用 | 必需 |
| --- | --- | --- |
| `BOOTX64.EFI` | 本工具（先备份 U 盘原有同名文件） | ✅ |
| `booter_load_ad102.bin` | AD102-family Booter bundle（BTL1） | ✅ |
| `gsp_boot_ad102.bin` + `gsp_boot_desc_ad102.bin` | GSP-RM boot 段 + v5 描述符 | ✅ |
| `fwimage_ad102.bin` | GSP-RM 固件镜像（~80 MiB，radix3 装载） | ✅ |
| `fwsignature_ad102.bin` | GSP-RM stock 签名（WprMeta 引用） | ✅ |
| `scrubber_ad10x.bin` | 内核同款 SEC2 scrubber（BSI handoff 1→3） | ✅ |
| `v67-ad102-four-plm.bin` | **V67 四写 payload——放上即解锁模式** | 可选 |
| `tx-require-d0.txt` | 入口非 D0（休眠恢复）时直接 abort、零写入 | 可选 |
| `\loader.log` | （运行产物）全部阶段日志，evidence record | 输出 |

不放 `v67-*.bin` 即为**回放/诊断模式**：走完全链但零解锁写入（刷存在感
的安全/取证模式）。日志中 `family 'xxx' transaction track not enabled`
表示该芯片族尚未开放事务（当前仅 ad102 系启用）。

### 操作流程

1. BIOS：关 Secure Boot。显示模式/进入方式按下面的实测表选——**推荐独显直连**，
   混合模式只在"重启、且 dGPU 有电"这一种进入方式下可用（见原因分析）。
2. U 盘启动 → 版本屏 → **空格确认**（30 秒无操作自动回 Windows）。
3. 之后事务完成会先倒计时 5 秒，按任意键终止自动启动并返回固件；超时后进入默认引导流程。事务失败仍会自动链回 bootmgfw。完整日志落 U 盘 `\loader.log`。默认查找顺序固定为 **bootmgfw → 各发行版 shim/grub/systemd-boot → U 盘 `\EFI\BOOT\BOOTX64.EFI`**；最后一档通用路径仍保留独立的 5 秒窗口，按任意键也可返回固件 shell。
4. 每次冷启动重放一次（解锁易失，无持久化、无刷写、断电即清）。

V67 payload 与全部资产再生成：`cargo run -p assets -- generate-ad102` /
`release/make-usbroot.ps1`（从钉 SHA-256 的 610.43.03 公开输入本地再生，
产物 hash 硬校验；期望值见报告哈希表）。

### 显示模式 × 上次离开方式：可用性实测（G614JV / 4060L / 三模完整支持，2026-10-26）

| 上次离开方式 | 本次启动后的 dGPU | 本工具 |
| --- | --- | --- |
| 混合模式 · 纯 iGPU 关机 | 冷：无 GOP，BAR0 全 `0xffffffff` | ❌ claim 前置即报 "dGPU powered off (D3cold)" |
| 混合模式 · 混合关机 → 重启 | 热：有 GOP，寄存器可读 | ✅ 全链跑通（四写 + 选择器穿 FLR，即 evidence 里的成功日志） |
| 混合模式 · 休眠/唤醒之后 · D3 门控态 | 停在 D3、域门控（VGA_WORKSPACE 读回 `0xbadf1301`） | ❌ claim 前置即报 |
| 混合模式 · 休眠/唤醒之后 · 事务可跑 | 寄存器可读、全链能绿，但卡是热的 | ⚠️ 事务绿 ≠ 解锁有效：出口后可能被作废，先看 `HANDOFF-OUT` 并做 Windows 侧读回（`docs/warm-resume-entry.md`） |
| 独显直连 · 休眠或重启 | 始终是 panel owner、始终有电 | ✅ 均可用 |

操作建议：**从 Windows 选"重启"**（或冷启后直接从启动菜单进 U 盘）再运行；不要用
"关机再开"的 iGPU-only 路径，也不要在休眠唤醒后直接进 U 盘。已实现 PM 预检：入口
非 D0 会打 `PM: WARM ENTRY` 并把卡拉回 D0（读回确认），要更严格就在 U 盘根目录放
空文件 `\tx-require-d0.txt`——此时 warm 入口直接 abort、零写入零复位。

**原因分析（不是模式好坏，而是固件为谁无条件跑 POST）**

固件只在"自己要用来显示的那块 GPU"上无条件执行 VBIOS POST/devinit：

- 独显直连时 dGPU 是 panel owner。冷启、热重启、S3/S4 唤醒后的任何路径，固件都必须
  重新初始化它，dGPU 一直有电、有 GOP、寄存器域可读——所以这里休眠和重启都可用。
- 混合模式下 iGPU 能承担显示，固件对 dGPU 只是尽力而为：冷启与混合重启路径会 POST
  它（因此"混合关机→重启"可用，这也正是那份 4060 成功日志的状态）；但
  - 纯 iGPU 关机会让 EC 在 POST 之前就断掉 dGPU 供电（D3cold），BAR0 全 1；
  - 休眠走的是 resume 路径而不是 POST：固件不重跑 devinit，EC 把 dGPU 停在 D3，
    PRI 域门控——VGA_WORKSPACE 读回 `0xbadf1301` 就是它在寄存器层的投影。
- 本工具在驱动加载前工作，没有 ACPI 解释器，无法像 Windows 驱动那样跑
  `_PS0`/PR3 给卡上电，因此这三种失败态只能靠换进入方式规避，不能靠代码绕过。

## 与 nvoc kmd 功耗墙解锁配合使用

本工具与 **[Skyworks-Neo/nvoc](https://github.com/Skyworks-Neo/nvoc)** 的 kmd
功耗墙解锁**兼容且正交**：本工具在 UEFI 预驱动窗口解除 tensor 发射速率钳制
（FEAT/WPR/WPRCFG PLM），nvoc kmd 在操作系统内解除功耗/电压墙——两者作用于
不同层级，可叠加，推荐配合使用以获得极限性能。

### 组合实测极限（RTX 4060 Laptop 28e0，FP16 GEMM + nvoc）

> 本文所有 FP 精度吞吐数据均由
> [Skyworks-Neo/nvoc](https://github.com/Skyworks-Neo/nvoc) 的
> **[cli-stressor-cuda-rs](https://github.com/Skyworks-Neo/nvoc/tree/main/cli-stressor-cuda-rs)**
> 测得。

| 工况 | 数值 |
| --- | --- |
| FP16 GEMM 4096（仅本工具解锁） | 49.0–49.3 TFLOPS（stock 29.89，1.65×） |
| FP16 GEMM 8192（+nvoc 功耗墙） | **60.8–63.1 TFLOPS**（相对 stock ≈ **2.1×**） |
| GPU 核心时钟峰值 | **3045 MHz** @ 1.200 V |
| 板级功耗峰值 | **206.8 W**（芯片 ~163.7 W + 显存 21.8 W + PWR_SRC 24.8 W；16-pin 164.4 W @ 19.8 V） |
| 温度 | Hot Spot 峰值 **101.8–104.4 °C**，核心 79–81 °C，显存 58–66 °C |
| 限流原因 | PerCap Reason 在 **Pwr**（功耗墙）与 **Thrm**（热墙）间切换 |

判读：解锁后卡在满载时先顶功耗墙（Pwr），nvoc 放开功耗墙后转为顶热墙
（Thrm）——**3045 MHz / 206.8 W / 104 °C Hot Spot 就是这块 8 GB 4060
Laptop 的物理极限区间**，再往上任何优化都只能来自降温。

### 解锁的本质：被禁用的晶体管恢复工作，而非超频

被钳制的 SM 发射率是**物理关闭的tensor数据通路**（manufacturing
segmentation），不是降频或限功率——频率、电压、功耗墙都与它无关。对照
实验（RTX 4060 Laptop，ASUS ROG Strix G16 **G614JV**，FP16 tensor，
同卡同精度唯二变量为发射率与功率墙）：

| 状态 | 频率 | 芯片功耗 | 墙 | FP16 tensor |
| --- | --- | --- | --- | --- |
| stock | **2.52 GHz** | 95~105 W | **电压墙** | 30.5 TFLOPS |
| 只解锁 SM 发射率 | **2.37 GHz** | 130-140 W | **撞功耗墙** | 47.50 TFLOPS |
| 只解锁功率（+nvoc 放开电压墙+极限超频） | **3.00 GHz** | 155–160 W | **不撞功耗墙** | 35.9 TFLOPS |
| 解锁 SM 发射率+功率（+nvoc 放开电压墙+极限超频） | 平均 **2.89 GHz** | 190–208 W | 撞温度墙+供电极限 | **64.30 TFLOPS** |

发射率解锁后频率**下降 3.7%**、功耗上升 34%，吞吐却提升 **79%**——多出的
每 FLOP 都不可能来自频率或供电，只能来自**完整晶体管不再空转**： segmentation
掩码放行后，原本每个时钟沿都在执行 NOP 的挂起槽位重新装载真实指令，
参与发射的tensor核心数量恢复设计全集——功耗上升正是空转变工作的直接体现。
这也是为什么单纯超频/超压/放功耗墙永远追不平差距（stock 在 3.0 GHz、不撞
任何墙也只有 35.9 T），而解锁在更低频率下直接翻倍。

> **⚠️ 硬件风险警告**
> 两项解锁叠加后芯片功耗与热密度会**大幅超出原厂设计包络**（上表为实测
> 极限：板级 200+ W、Hot Spot 105 °C 附近持续）。使用前必须自行确认并
> 持续监控：
> - **散热**：液金/硅脂状态、风扇策略、均热板与热管规格——Hot Spot 与核心
>   温差 >20 °C 时意味着界面材料已接近极限；
> - **供电**：适配器功率、VRM 余量、16-pin/电源链路温升（实测 16-pin
>   164 W @ 19.8 V）；
> - **监控**：GPU-Z/HWiNFO 的 Board Power、Hot Spot、PerCap Reason 三项
>   常驻；Hot Spot 持续 >100 °C 属于裸奔区。
>
> **因超功耗、超压、超温导致的芯片、供电或整机物理损坏，本项目概不负责**
> ——解锁是易失的，但烧坏的硅片不会恢复。

## 工作原理速览

NVIDIA 的算力钳制是 PRI 总线上的硬件门（PLM，默认 `0xffffff8f` = 仅
heavy-secure 固件可写），主机/驱动写受保护寄存器被静默丢弃。唯一解锁路径是
让**已签名的固件自己**在特权态写——本工具在 UEFI DXE 窗口（驱动加载前的唯一
等价时窗）复刻内核驱动的完整引导链，并在"送 Booter"一步注入社区逆向出的
V67 载荷：

```
claim(断 GOP/驱动+独占) → scrubber(BSI handoff 1→3) → FWSEC-FRTS(WPR2)
→ booter(正确签名+BROM 验签, 邮箱=WprMeta) → [V67: 签名 DMA 无界拷贝
溢出 DMEM 栈 → ROP → FEAT/WPR/WPRCFG/FBPA 四写 + marker 0x67b00001]
→ selector 写入(PLM 开后主机写生效) → 签名还原 → FLR(冷卡交还) → bootmgfw
```

全程只读回验证（marker + HALT + PLM 逐项读回 + FLR 后 AON 存活），失败即
拒绝并 FLR；无持久化、无刷写、断电即清。详见
[docs/ada28e0-uefi-report.md](docs/ada28e0-uefi-report.md)。

## 兼容性矩阵

判定依据：本工具按**芯片家族**路由（同一家族共用同一 Booter/payload 资产，
与上游 `ad102_family()`/GA10x 路由一致）。层级含义：
✅ 实测验证（本机跑通全链+payload）；🟢 上游硅验证（同家族在 kmod 侧已验证，
本 EFI 路径同构，预期兼容）；🟡 路径存在未验证（代码就绪，无硅证据）；
❌ 不兼容。

| 家族 | 卡（device ID） | 层级 |
| --- | --- | --- |
| AD107 | 4060 Laptop **28e0** | ✅ 实测（本报告） |
| AD107 | 4060 2882 / 4060 Laptop 28a0 / 4050M 28e1 | 🟢 上游硅验证（28e1 为 2.00× 实测报告） |
| AD107 | 4050 Laptop **28a1** | ✅ 实测（2026-10-26，v0.1.3）：全链通过（WPR2 验证→booter marker→四写→选择器→签名还原→FLR 后 AON 存活）；FP16 tensor **22 → 38-42 TFLOPS（1.7-1.9×）**，完整性自检绿。门控值 `0xbadf1301` 走容毒回退 |
| AD106 | 4060Ti 2803 / 4060Ti 16G 2805 | 🟢 上游硅验证 |
| AD104 | 4070Ti 2782 / 4070S 2783 / 4070 2786 | 🟢 上游硅验证 |
| AD103 | 4080S 2702 / 4080 2704 / 4070TiS 2705 / 4070 2709 / 4090M 2757 | 🟢 上游硅验证 |
| AD102 | 4090 2684 / 4090D 2685 / 4070TiS 2689 | 🟢 上游硅验证 |
| GA10x | 3090/3080 2204/2206、3070 系 2484–248c、3060 系 2501/2503–2509/252f/2544、3050 2584、2560、20c2/20f1 | 🟡 上游 kmod 验证，本 EFI 路径无硅证据 |
| Turing 及更早 | 20 系/16 系/Pascal… | ❌ 无事务轨（Turing 资产已备未启用；Turing 无已知钳制） |
| Blackwell (50 系) / GA100 | — | ❌ 链路不同，不在范围 |

**"40 系全部都可以吗？"——AD10x（40 系）理论上全部在列**：同家族资产+上游
逐硅验证构成高置信预期，但除 28e0 外均未经本 EFI 实测。首次使用建议先跑
**回放模式**（不放 `v67-*.bin`）：走到 `TX: execution verified` 即链路通，
再上 payload。

**不兼容/异常时**：请提交 U 盘根的 `\loader.log` 全文到
[Issues](https://github.com/physicsdolphin/nv-uefi-unlocker/issues)（日志含
完整阶段轨迹，是我们诊断的唯一依据；发前可脱敏机器信息）。也可以在
[Discussions](https://github.com/physicsdolphin/nv-uefi-unlocker/discussions)
分享成功/失败数据点（卡型号 + 关键日志行 + 判据值），帮助扩展矩阵。

### 已知平台限制：AMD 主板固件 IOMMU（AMD-Vi）→ `booter status 0x89`

**现象**：日志走到 `POST-BOOT: booter status 0x89 (nonzero = failed)`，事务以
EIO 中止（issue #1，4050 LP 28E1 / AMD 台式机，已由用户关闭 BIOS 相关选项
闭环解决）。`0x89`–`0x90` 是 booter 的 WPR 描述符校验（`wpr_desc_validate`）
错误区间：booter 经 DMA 读回的 256 字节 WprMeta 与写入内容不符。

**机理**：booter 阶段是全链**第一个主机侧 DMA** 消费者（之前的 scrubber/
FWSEC 全是 portal 拷贝与 PROM 读取）。loader 交给 booter 的是裸物理地址，
依赖 boot services 阶段的恒等映射；部分 AMD 台式主板的固件 IOMMU（AMD-Vi）
在该阶段做地址翻译，设备 DMA 读到垃圾，描述符校验即失败。

**处置**：进 BIOS 关闭 **IOMMU（AMD-Vi）**，重跑回放模式（预期
`MAILBOX0=0`），再上 payload。

**实测不影响**（无需关闭）：

- **SVM（AMD-V）**：R9000P 7945HX 开启 SVM（任务管理器"虚拟化：启用"）
  解锁照常工作；
- **Intel VT-x（虚拟化）与 VT-d**：验证机开启状态下全链实测通过。

> 术语注意：任务管理器的"虚拟化"只反映 SVM/VT-x；IOMMU（AMD-Vi）是独立的
> 固件 DMA 重映射选项，只能在 BIOS 里确认，与"虚拟化"显示无关。

> 维护者**非全职**，可能无法随时查看并回复 issue，敬请谅解——日志会保留，
> 有进展会回。

## 仓库结构

```
loader/    EFI 应用 crate（no_std, x86_64-unknown-uefi, Rust + uefi-rs）
assets/    host 侧资产管线（钉 hash 下载/校验/抽取/载荷生成，纯 Rust）
sim/       行为级模拟器 + 事务状态机测试（无 GPU 开发）
docs/      报告、里程碑证据（evidence-as-commits）、寄存器冷值表、开放问题
```

## 安全宪法（副本，与 AGENTS.md §1 等义，违反任何一条即停工）

1. **读回是唯一成功判据**。状态码/返回值/中断/自检输出一律不构成执行证明。
2. **STOP/异常后同会话绝不重试**；完全断电后走正常 Windows 引导恢复。
3. 只读链式加载 ESP 的 `\EFI\Microsoft\Boot\bootmgfw.efi`，**永不覆写 ESP
   任何文件**；替换 U 盘 BOOTX64.EFI 前先备份原有。
4. 无常驻引导项：只从 U 盘/启动菜单一次性进入；不动 NVRAM BootOrder。
5. 每张卡钉死身份（PCI vendor:device + ROM/PROM hash）；换卡/换 ROM 即拒绝。
6. NVIDIA 固件不分发、不入 git；从钉 SHA-256 的官方输入本地再生；
   密钥/解密产物/载荷永不入库。
7. 写操作分级门控：M1 前零写；M3 进入前需用户显式确认；M4/M5 单独立项。
8. 先 mock 后真机：事务代码必须先在行为级模拟器全绿。
9. Secure Boot 关闭或自签证书入 db（优先后者）。
10. 许可：GPL-2.0-only。

## 回滚手册

### 场景 A：黑屏（链式启动失败/事务中断）
未动任何固件配置与 ESP 文件 → 强制重启 → 走原 Windows 引导项即恢复。
U 盘 BOOTX64.EFI 不在启动链上时机器根本不会进我们的代码。

### 场景 B：STOP/异常（事务中途失败）
同会话**绝不重试**（固件可能仍在运行、DMA 缓冲可能仍被占用）→ 完全关机断电
（笔记本按厂商流程放空 EC，ASUS 为拔电按住电源 40 s）→ 拔 U 盘 → 正常
Windows 引导 → 检查设备管理器/事件日志驱动状态 → 记录证据后再考虑下一次。

### 场景 C：Windows 驱动不认卡（交棒后异常）
设备管理器看错误码；事件日志查 nvlddmkm/Xid。解锁效果是易失覆盖（断电即失），
一次冷启动即回到出厂状态。若仍异常：再次完全断电；最后手段 DDU 重装驱动
（本工具不写任何持久存储，驱动状态异常只可能来自交棒时序，不是永久损坏）。

## 构建

```
cargo build --release   # 宿主侧 crate（assets/sim）——loader 不在 default-members
cargo efi               # EFI 只读构建（M0/M1 诊断模式）
cargo efi-tx            # gated 事务构建（含写路径；仅显式需要时使用）
cargo xt                # 全部宿主测试（assets 19 + sim 44）
```

裸 `cargo build --workspace` 会以宿主三元组构建 loader 并报错——loader 只能
为 `x86_64-unknown-uefi` 构建，请用上面的别名。

## 社区贡献与参照源

本项目是社区工作的直接受益者与衍生者，完整致谢见
[docs/ada28e0-uefi-report.md](docs/ada28e0-uefi-report.md) 的同名章节。核心：

- [gpu-unlock](https://github.com/Zagitta/gpu-unlock)（GPL-2.0）——事务链移植母本
- [gpu-unlock-ada](https://github.com/JustAResearcher/gpu-unlock-ada)（GPL-2.0）——
  AD10x 四写链源头；28e1 报告（Windroid234）是本项目直接依据
- [cmp170hx](https://github.com/Consensus-Protocol/cmp170hx)——PLM 语义与验证方法论
- [cmp90hx-unlock](https://github.com/WildFlash1st/cmp90hx-unlock)——V67 链源头
- [AD103_win_unlocker](https://huggingface.co/TOTORONG/AD103_win_unlocker)（TOTORONG）——
  Windows EFI 载具工艺先例
- [open-gpu-kernel-modules](https://github.com/NVIDIA/open-gpu-kernel-modules)（MIT）——
  引导路径一手权威

许可：**GPL-2.0-only**（`LICENSE`）。NVIDIA 固件与 VBIOS 版权归 NVIDIA，按其
原许可条款从钉版公开包本地再生，不在本仓库分发。
