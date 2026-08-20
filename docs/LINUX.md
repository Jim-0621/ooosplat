# Linux + GPU 移植说明

本文档记录本分支相对上游（Windows 专用）的差异、部署步骤和待办项。

上游 OOOSplat 是 Windows 桌面应用，COLMAP 固定使用 CPU/no-CUDA 构建。本分支的目标是
**先在 Linux 上把 CLI 跑通并启用 GPU**，GUI 迁移暂不涉及。

## 为什么先做 CLI

`tauri` 的引用只存在于 `lib.rs` 的 `run_app()` 和 `commands/` 两处。`engines`、`process`、
`video`、`reconstruction`、`project`、`presets`、`pipeline` 这 7 个模块——也就是整条流水线
——本来就不依赖 Tauri。因此 `splatstudio` CLI 具备完整能力，不是功能阉割版。

## 前置要求

| 项 | 说明 |
|---|---|
| Ubuntu 22.04 / 24.04 x64 | 其他发行版需自行调整 `apt` 包名 |
| NVIDIA 驱动 + CUDA Toolkit | COLMAP 的 GPU 特征提取与匹配依赖它；`nvcc` 必须可用 |
| Rust stable | 见 <https://rustup.rs>，用于编译 CLI 和 Brush |
| Vulkan 驱动 | Brush 通过 wgpu 走 Vulkan 后端训练 |

CPU-only 的机器也能跑，但必须用 `--compute cpu`，那就失去了移植的意义。

## 部署步骤

```bash
git clone git@github.com:Jim-0621/ooosplat.git
cd ooosplat
git checkout linux-port
```

准备引擎（FFmpeg 走系统包，COLMAP 和 Brush 从源码构建）：

```bash
./scripts/setup-engines-linux.sh
```

单独重做某一个：

```bash
./scripts/setup-engines-linux.sh colmap
```

编译 CLI。`--no-default-features` 关掉 `gui` feature，Tauri 及其图形系统库完全不进依赖图：

```bash
cargo build --release --bin splatstudio --no-default-features --manifest-path src-tauri/Cargo.toml
```

自检四个引擎：

```bash
./src-tauri/target/release/splatstudio health
```

跑完整流水线：

```bash
./src-tauri/target/release/splatstudio generate input.mp4 \
  --projects-root ~/splat-projects --quality balanced --compute gpu
```

`--projects-root` 走的是 `ProjectManager::for_diagnostics`，不写全局项目索引，也不依赖
XDG 用户目录，是 Linux 上最省事的路径。

## 相对上游的改动

| 改动 | 影响 Windows 行为？ |
|---|---|
| `gui` feature 门控 Tauri（默认开启） | 否 |
| 引擎路径改用 `EXE_SUFFIX` | 否 |
| 用户目录回退到 `home_dir` | 否（Windows 上 `document_dir` 始终有值） |
| `ComputePolicy { Cpu, Gpu }`，默认 `Cpu` | 否（GUI 不传，仍是 `Cpu`） |
| `splatstudio --compute` 全局参数 | 新增，默认 `cpu` |

前两项和 `ComputePolicy` 对上游是无害的，将来可以单独提 PR 回上游。

## 已知未完成项

- **进程树终止**：`ProcessManager` 的 Windows Job Object 没有 Linux 对应实现。CLI 目前不
  接取消入口（`cancel()` 无调用点），Ctrl+C 由 shell 发给整个前台进程组，实际够用。GUI
  迁移时必须补上 `setsid` + `killpg`。
- **`trash` 回收站删除**：Linux 上依赖 freedesktop 规范目录，跨挂载点和无桌面环境会失败。
  CLI 路径不涉及。
- **引擎哈希锁定**：Windows 的 `manifest.json` 锁死了压缩包和可执行文件的 SHA-256。Linux
  侧是本地构建，`setup-engines-linux.sh` 只在结尾打印实际哈希供人工比对。
- **GLOMAP**：`mapper` 阶段是 CPU 串行的增量式 SfM，GPU 帮不上忙，很可能是最长的一段。
  换成全局式的 GLOMAP 是接口级兼容的（同样读 `database.db`，同样写 `sparse/0/*.bin`），
  预期收益比 CPU→GPU 更大。尚未实现。

## 性能对照

同一段素材、同一档位下逐阶段记录，用来判断后续优化方向。

| 阶段 | Windows CPU | Linux GPU | 备注 |
|---|---|---|---|
| 抽帧 | | | 两边都是 CPU，应该基本持平 |
| 特征提取 | | | 预期大幅下降 |
| 顺序匹配 | | | 预期大幅下降 |
| 相机重建 | | | **预期基本不变**，见上面 GLOMAP 一条 |
| Splat 训练 | | | 两边都用 GPU |
