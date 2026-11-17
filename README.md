# 猫咪斗恶龙2 修改器 2.0（Rust 版）

《Cat Quest II》单机修改器 2.0。相比 1.x 的 Python + tkinter 版本，本版本用 Rust + egui 完整重写：
保留原有的全部功能与数值口径，界面改为原生圆角窗口，进程内存读写、Mono 运行时调用和后台维持器全部在 Rust 中实现。

> 只适用于单机模式，仅读写本机游戏进程。请勿用于联机或任何在线场景。

## 功能

### 一键功能

| 功能 | 说明 |
| --- | --- |
| 连接并识别角色 | 自动找到带可见窗口的游戏进程，识别主角/同伴的全部属性地址 |
| 无敌 | 写入游戏原生「无法受伤」标记（UnitFlags `0x40000`），并用锁血兜底；换场景/翻滚后自动修复 |
| 秒杀 | 近战与远程（魔法属性）伤害同时拉满，一击必杀且不溢出 |
| 移动速度 | 按原始移速等比放大（1.5x / 2x / 3x / 5x / 10x），后台持续锁住 |
| 一键满级 | 走游戏自己的 `ReplaceExperience` / 升级流程，一路升到 999 级，属性和存档同步成长 |
| 一键满金币 | 调用游戏自己的 `ReplaceGold`，金币直接到 20 亿并正常存档 |
| 全部恢复原样 | 停止所有后台线程，把血量、伤害、魔法、移速、免伤标记写回原值 |

### 强迫症修改

| 功能 | 说明 |
| --- | --- |
| 最大蓝条 | 持续把蓝量顶在属性上限，并通过 `ReplaceMana` 触发 HUD 刷新 |
| 游戏整体速度 | Unity `Time.timeScale` 倍速，移动、动画、战斗一起缩放 |
| 全屏拾取 | 后台巡逻场上掉落物，掉落即自动拾取，音效与入账由游戏完成 |

### 原理说明

界面第三个页签保留了原版的原理说明：实体组件槽位、Mono 类名解析、远程调用 stub、场景切换后的自动修复等。

## 使用方法

1. 启动《猫咪斗恶龙2》，进入可以自由移动的游戏世界（不要停在主菜单）。
2. 运行 `CatQuestIITrainer.exe`。
3. 点「连接并识别角色」，等待状态栏显示识别完成。
4. 按需打开开关；修改立即写回游戏，切回游戏即可看到效果。
5. 退出游戏前建议点「全部恢复原样」。

如果界面卡在「处理中」，点右下角「状态刷新」即可强制复位并重新附加、识别。

## 从源码构建

需要 Rust 工具链（stable，支持 edition 2024）和 Windows x64 环境。

```powershell
$env:Path = "C:\mingw64\bin;$env:USERPROFILE\.cargo\bin;$env:Path"
cargo build --release
```

产物：`target\release\CatQuestIITrainer.exe`。

运行测试：

```powershell
cargo test
```

## 项目结构

```text
src/
  main.rs     程序入口 / eframe 启动
  gui.rs      三个页签、自绘开关与分段选择、异步任务回传
  memory.rs   进程查找、OpenProcess、内存读写、虚拟内存区域枚举、DWM 圆角
  scan.rs     精确扫描/再次扫描/范围扫描、类名解析、组件聚类、全堆反查
  mono.rs     mono-2.0-bdwgc.dll RVA、x64 远程调用 stub、Mono 会话封装
  locate.rs   玩家实体识别（Mono 快速通道 + 全堆扫描回退）
  keepers.rs  锁血/免伤/最大蓝条/游戏倍速/全屏拾取后台线程
  actions.rs  一键满级、一键满金币、Time.timeScale
```

### 关键实现

- **Mono 类名解析**：对象头 `vtable → MonoVTable.klass → MonoClass.name`，按 8 字节步长试探
  `MonoClass` 的 `name` 字段偏移（不同 Mono 版本差异大）。
- **实体槽位校验**：玩家实体必须同时满足 `StatsComponent(0)`、`PlayerControlledComponent(-0x160)`、
  `HealthComponent(-0x260)`、`MoveSpeedComponent(-0x1B0)`，同伴额外有 `AIPlayerComponent(-0x3D0)`。
- **远程调用**：在游戏进程内分配 RWX 页，写入 x64 stub（`sub rsp,0x28` → 参数 → `call r10` →
  `mov [slot],rax` → `ret`），用 `CreateRemoteThread` 执行并回读结果，不注入 DLL。
- **参数传递兼容**：int 参数使用「地址 == 值」的自引用缓冲，兼容 Mono 的直接传值、传指针、指针数组三种约定。
- **后台自愈**：每 0.5 秒校验一次实体槽位；组件被 Replace 换新时自动更新地址并补写标记/伤害/移速。

## 兼容性

- 只为 Windows x64 构建；窗口圆角依赖 Windows 11 的 DWM，Windows 10 上会以直角正常显示。
- 游戏更新后 Mono RVA、实体槽位或方法名可能变化，届时需要重新核对 `mono.rs` / `scan.rs` 中的常量。
- 修改器不联网、不写游戏文件、不使用图片资源；中文字体优先加载系统 `Microsoft YaHei`。

## 免责声明

本项目仅供学习逆向工程、Win32 内存操作和 Rust GUI 开发使用。请自行承担使用风险，并在修改前备份存档。
