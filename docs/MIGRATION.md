# 从 Windows 迁移到 Linux + GPU：过程记录

本文记录 OOOSplat 从 Windows 桌面应用迁移到 Linux 云服务器（NVIDIA GPU）的完整过程：
做了什么改动、为什么这么改、踩了哪些坑、怎么解决的。

部署操作步骤见 [LINUX.md](LINUX.md)，本文只讲**过程与原因**。

- 分支：`linux-port`，28 个提交
- 目标机器：AutoDL 容器实例，RTX 3080 Ti 12 GB / Xeon 20 核 / Ubuntu 22.04.1 / CUDA 11.8
- 用途：个人学习

---

## 一、策略：为什么先做 CLI

上游是 Tauri 2 桌面应用（WebView2 + React）。直觉上"移植 GUI 应用到 Linux"是个大工程，
因为 Tauri 在 Linux 上要链接 WebKitGTK 那一整套图形系统库。

但先做了一件事：查 `tauri` 到底被谁引用。

结果是 `tauri` 只出现在两个地方——`lib.rs` 的 `run_app()` 和 `commands/` 目录。而
`engines`、`process`、`video`、`reconstruction`、`project`、`presets`、`pipeline` 这 7 个
模块，也就是**整条流水线**，一行 Tauri 都没有。

这个发现改变了整个方案：`splatstudio` CLI 不是"功能阉割版"，它本来就具备完整能力。
于是策略定为**先让 CLI 在 Linux 上跑通，GUI 以后再说**。

验证方式是给 Tauri 加 feature 门控（提交 `98145de`）：

```toml
[features]
default = ["gui"]
gui = ["dep:tauri", "dep:tauri-build", "dep:tauri-plugin-dialog", "dep:tauri-plugin-opener"]

[[bin]]
name = "ooo-splat"
required-features = ["gui"]
```

`build.rs` 和 `lib.rs` 里对应加 `#[cfg(feature = "gui")]`。默认仍然开启，所以 Windows
的构建行为一个字都没变。

实测结果：`cargo build --no-default-features` 编译 **67 个 crate、6.4 MiB**，依赖图里
**没有 tauri、webkit2gtk-sys、gtk、soup**。完整的 Tauri 构建是 400+ crate。门控生效了。

---

## 二、代码改动

全部改动都保持"默认值等同原行为"，Windows 端零影响。

| 提交 | 改动 | 为什么 |
|---|---|---|
| `98145de` | `gui` feature 门控 Tauri | CLI 不再拖入图形系统库 |
| `3f7e5cb` | 引擎路径用 `std::env::consts::EXE_SUFFIX` | Windows 是 `colmap.exe`，Linux 是 `colmap` |
| `3f7e5cb` | 用户目录回退到 `home_dir` | Linux 无桌面环境时 `document_dir` 返回 `None` |
| `7ebb873` | `ComputePolicy { Cpu, Gpu }` | 上游把 `use_gpu` 硬编码成 `0`，因为它打包的是 no-CUDA 版 COLMAP |
| `5f44258` | `MapperBackend { Colmap, Glomap }` + `engines/glomap.rs` | 为替换最慢的阶段留出口子 |
| `5f44258` | `best_sparse_model` 把 `sparse/` 根目录也当候选 | GLOMAP 只产出一个模型且直接写在根目录，COLMAP 写 `sparse/0`、`sparse/1` |
| `d1cc814` | `check_colmap` 识别 `with CUDA` 横幅 | 见下文坑 12 |
| `35b7e5e` | CLI 进度行加 `[HH:MM:SS]` 时间戳 | 阶段耗时能直接从屏幕读出来 |

有一处**差点犯的错**值得记下来：最初想把 GLOMAP 加进 `EnginePaths::check_all()`。
但 `App.tsx:246` 是 `disabled={... || missingEngines.length > 0}`——GUI 只要有任何一个
引擎异常就禁用「开始生成」按钮。GLOMAP 是可选组件，Windows 上永远不存在，加进去会让
按钮**永久变灰**。所以 `check_all()` 保持恰好 4 个引擎，GLOMAP 只在 CLI 的 `health`
子命令里单独追加上报。

---

## 三、踩坑记录

按实际发生顺序。每一条都对应仓库里的一个提交。

### 1. 加速代理对 crates.io 是反效果

第一次 `cargo build` 花了 **46 分 31 秒**，日志里几十个 300 秒超时。

原因：为了 clone GitHub 开了 AutoDL 的 `/etc/network_turbo`，而它自己的提示写着
"开启加速后对访问其他资源如 pip 源等会**更慢**"。它加速 GitHub，同时拖慢 crates.io。

解决：配 rsproxy 镜像；并且把脚本改成 GitHub 源码**只在缺少目标 ref 时才 fetch**
（`069dd50`），这样可以先开代理预克隆，再关代理跑脚本。

后来还发现 Brush 的 `Cargo.toml` 里有 GitHub 上的 git 依赖，又出现同样的矛盾。最终解法
是**只让 github.com 走代理**：

```bash
git config --global http.https://github.com/.proxy "$http_proxy"
```

配合 cargo 的 `git-fetch-with-cli = true`，cargo 抓 git 依赖时走 git（读这条配置），
下载 crate 包时走自己的 HTTP（直连镜像）。矛盾消解。

### 2. 脚本没有执行权限

`./scripts/setup-engines-linux.sh: Permission denied`。

Windows 上创建的文件，Git 记录的模式是 `100644`。修复：`git update-index --chmod=+x`
（`67a4e34`）。

### 3. Ubuntu 的 Ceres 太旧

`libceres-dev` 是 2.0.0，COLMAP 4.x 和 GLOMAP 都要求更新的版本。

解决：脚本里加 `ceres` 组件，从源码编译 2.2.0 装到 `/usr/local`；同时**从 COLMAP 的
apt 列表里删掉 `libceres-dev`**，否则 CMake 可能解析到系统那个旧的（`d5e8abf`）。

### 4. 容器里没有 sudo

AutoDL 以 root 运行且不装 sudo，脚本里每个 `sudo apt-get` 都会失败。

```bash
SUDO=""
[ "$(id -u)" -ne 0 ] && SUDO="sudo"
```

### 5. 编译工具链装晚了

`CMake was unable to find a build program corresponding to "Ninja"`。

`build-essential` 和 `ninja-build` 只写在 COLMAP 那一段，但 **Ceres 排在 COLMAP 前面**，
而且同样用 Ninja 生成器。提取成公共函数 `require_build_tools`（`a52323c`）。

### 6. COLMAP 4.x 需要 OpenImageIO

`Could not find a package configuration file provided by "OpenImageIO"`。

COLMAP 从 3.12 起把图像 IO 从 FreeImage 换成了 OIIO。加 `libopenimageio-dev`（`2fd7684`）。

### 7. OIIO 的 CMake target 指向另一个包

```
The imported target "OpenImageIO::iconvert" references the file "/usr/bin/iconvert"
but this file does not exist.
```

Ubuntu 22.04 的打包缺陷：`libopenimageio-dev` 导出的 CMake target 引用了
`iconvert`、`oiiotool` 这些命令行工具，但工具装在 `openimageio-tools` 包里，而我们的
`--no-install-recommends` 正好把它拦掉了。补上该包（`78fac89`）。

### 8. `CUDA_ARCH=8.6` 是错的写法

```
nvcc fatal : Unsupported gpu architecture 'compute_8'
```

CMake 的 `CMAKE_CUDA_ARCHITECTURES` 要的是 **`86`**，不带小数点。`8.6` 被解析成 `8`，
于是传给 nvcc 的是不存在的 `compute_8`。脚本里做归一化，两种写法都接受（`b0b8133`）。

### 9. ONNX Runtime 几百 MB 下载卡死

COLMAP 4.x 在**配置阶段**就要从 GitHub 下载 ONNX Runtime 二进制包。关了代理跑 apt 的
时候，这个下载几乎不动。

关键判断：ONNX 在 COLMAP 里**只服务于深度学习特征点**（ALIKED 等），本流程跑的是传统
SIFT，一次都不会调用；而且它是 CUDA 12 编的，在 CUDA 11.8 上本来就会退回 CPU（CMake
自己警告了）。

解决：`-DONNX_ENABLED=OFF`（`7344a09`）。

中途一度考虑退回 COLMAP 3.11.1（那是最后一个不依赖 OIIO 和 ONNX 的版本）——**这个想法
是错的**，`colmap.rs` 传的是 4.x 参数名 `--FeatureExtraction.use_gpu`，3.x 叫
`--SiftExtraction.use_gpu`，编译能过但一运行就报未知参数。

### 10. cc1plus 被 OOM killer 杀掉

```
c++: fatal error: Killed signal terminated program cc1plus
```

ninja 默认按核心数并发（20 路），COLMAP 里 PoissonRecon、bundle adjustment、CUDA kernel
这些编译单元每个吃一两 GB。

第一版修复按 `/proc/meminfo` 的 `MemTotal` 封顶——**没用**，因为容器里 `free` 和
`/proc/meminfo` 报的是**宿主机**内存（125 GB），算出来还是 20 路。第二版改成优先读
cgroup 限额，那才是 OOM killer 真正执行的数字（`5fa31ac`、`c6b1639`）。

### 11. 关掉 ONNX 之后，配置阶段仍然要联网

```
error: downloading '.../PoseLib/archive/f119951....zip' failed
status_code: 28  "Timeout was reached"
```

COLMAP 通过 FetchContent 拉 PoseLib。这个包很小，开着代理几秒就完事——但必须知道
"配置阶段要开代理、apt 阶段要关代理"这个约束（`8e878ff`、`952a56b`）。

### 12. `health` 报五个引擎全部"未找到"

`EnginePaths::discover` 是按**当前工作目录**找 `./engines`，在 `/root` 下运行就去找
`/root/engines`。用 `OOOSPLAT_ENGINE_DIR` 指死（`3e194b1`）。

紧接着还有一个：路径对了之后，COLMAP 的 `cpuOnly` 报 `null`。原来 CUDA 的判据是
"运行目录里有没有 CUDA 运行时库"——这是 Windows 打包布局的特征，Linux 源码构建链接的是
系统 CUDA，目录里什么都没有。补一个正向判据：认 COLMAP 自己打印的 `with CUDA`
横幅（`d1cc814`）。

### 13. ffprobe 4.4 不认 `stream_side_data`

```
No match for section 'stream_side_data'
```

Ubuntu 22.04 的 ffmpeg 是 4.4.2，而 `probe_video` 请求的 `stream_side_data` section
是 5.0 之后才有的。

这里**没有选择改 Rust 代码去迁就旧版**：这个字段读的是视频旋转元数据，手机竖拍的素材
全靠它。丢掉的话 `VideoInfo.rotation` 恒为 0，抽出来的帧方向就是错的——COLMAP 照样能跑，
但重建结果会歪，而且**不会报任何错**。宁可换引擎版本。

改成从 GitHub 拉 BtbN 的静态构建（`9494ac1`）。

### 14. 静态 FFmpeg 一次改动带出三个问题

第一次下载 404——BtbN 的 latest release 已经不再发布 n7.1 了，只有 n8.1 和 n9.0。

顺带修掉另外两个（`97cedd7`）：

- 失败的下载在缓存里留了个 9 字节的空壳，`[ -s "$archive" ]` 会认为"已下载"直接跳过。
  改成先落 `.part` 再改名。
- 旧版本把 `engines/ffmpeg/ffmpeg` 做成指向 `/usr/bin/ffmpeg` 的符号链接，而 `install`
  **会跟随符号链接写入目标**——等于把系统的 ffmpeg 覆盖掉。安装前先 `rm -f`。

### 15. GLOMAP 要求 CMake 3.28

Ubuntu 22.04 只有 3.22.1。加了一个 `cmake` 组件，装 Kitware 官方预编译包到
`/usr/local`（`789ba13`）。副作用是好的：3.31 支持 `CUDA_ARCHITECTURES=native`，坑 8
那个限制自动解除。

### 16. GLOMAP 打不开数据库

```
SQLite error: SQL logic error
No registered database factory succeeded.
```

GLOMAP 的 `FETCH_COLMAP` 默认 `ON`——它通过 FetchContent **自己又拉了一份 COLMAP 编进去**
（配置阶段那 19 分钟就花在这里）。那份 COLMAP 的数据库 schema 和写库的 4.0.4 对不上。

解决方向：`-DFETCH_COLMAP=OFF`，链接我们装好的那一份（`7ce353e`）。

### 17. imported target 的作用域

```
Target "glomap" links to: colmap::colmap  but the target was not found.
```

`colmap::colmap` 明明导出了。问题在作用域：GLOMAP 的 `find_package(COLMAP)` 写在
`thirdparty/CMakeLists.txt` 里，而 CMake 中 `find_package` 创建的 IMPORTED target
**默认只在当前目录及其子目录可见**，`glomap/` 是它的**兄弟目录**，看不到。
`FETCH_COLMAP=ON` 时没这个问题，因为 FetchContent 创建的是真实 target，全局可见。

解决：`-DCMAKE_FIND_PACKAGE_TARGETS_GLOBAL=ON`（CMake 3.24+，刚好坑 15 装了 3.31）
（`70597ff`）。

### 18. GLOMAP 与 COLMAP 4.0.4 的 API 不兼容（**未解决**）

```
error: invalid use of member function
  'Eigen::Map<...> colmap::Rigid3d::translation() const' (did you forget the '()' ?)
```

COLMAP 把 `Rigid3d::rotation` / `translation` 从成员变量改成了返回 `Eigen::Map` 的
访问器，GLOMAP 代码里还在往它们赋值。GLOMAP 把 COLMAP 钉在 `b6b7b54` 就是这个原因。

要走通只能让两边都用 `b6b7b54`。已确认那个版本的三个参数名
（`FeatureExtraction.use_gpu`、`FeatureMatching.use_gpu`、`SequentialMatching.overlap`）
和 4.0.4 一致，对 `colmap.rs` 是透明的。但需要重编 COLMAP（约 25 分钟），**本次未做**。

脚本已经把这个约束固化下来（`d065f12`）：`setup_colmap` 把源码 ref 写进
`engines/colmap/.source-ref`，`setup_glomap` 配置前核对，版本不符直接报错并给出重编命令。

---

## 四、最终引擎组成

| 引擎 | 版本 | 来源 |
|---|---|---|
| FFmpeg / FFprobe | n8.1 静态构建 | BtbN GitHub release |
| Ceres Solver | 2.2.0 | 源码 → `/usr/local` |
| COLMAP | 4.0.4，CUDA on / GUI off / ONNX off | 源码 → `engines/colmap` |
| Brush | v0.3.0 | cargo |
| CMake | 3.31.6 | Kitware 预编译包 → `/usr/local` |
| GLOMAP | `99806d0` | 未完成，见坑 18 |

---

## 五、实测结果

素材 `charge.mp4`（33 MB），均衡档，615 帧，`--compute gpu`，默认 COLMAP mapper。

| 阶段 | 耗时 | 占比 |
|---|---|---|
| 探测 + 抽帧 | 5 秒 | 0.1% |
| `feature_extractor` | 20.6 秒 | 0.4% |
| `sequential_matcher` | 25.4 秒 | 0.5% |
| **`mapper`（增量式重建）** | **73.0 分钟** | **89.7%** |
| Brush 训练 | 7 分 28 秒 | 9.2% |
| **合计** | **81 分 26 秒** | |

重建质量指标（注册率、三维点数、高斯数）需要从 `generate` 输出的 JSON 补录。

### 结论

**GPU 对特征提取和匹配的效果是决定性的。**615 张图，特征提取 20.6 秒、顺序匹配 25.4
秒。作为参照，Windows 基准机（i7-12700，纯 CPU）处理 317 张图用了 3.1 分和 15.8 分。
即使算上素材不同，量级差异是明确的。

**但 mapper 吃掉了 89.7% 的时间，而它 GPU 帮不上忙。**增量式 SfM 的循环是"注册一张图 →
局部 BA"，每当模型增长 10% 还要触发一次"重新三角化 + 全局 BA"，这是串行的、CPU 的、
对图片数超线性的。

因此：

- **拆 CPU / GPU 两台服务器不划算。**mapper 的 73 分钟里 GPU 闲置，按 ¥1.08/小时算浪费
  约 **¥1.3**；代价是几百 MB 帧图来回搬两趟，外加给 CLI 加阶段拆分。不值。
- **唯一值得做的优化是换 mapper 算法**，也就是 GLOMAP。全局式 SfM 用旋转平均 + 平移平均
  一次性解出所有位姿，只跑一次全局 BA，没有那个反复触发的重量级循环。这是坑 18 未完成
  的事。

### 关于与 Windows 基准的对比

[LINUX.md](LINUX.md) 里那张性能对照表**没有填 Linux 列**，是有意的：Windows 基准跑的是
`doll`（317 帧），Linux 跑的是 `charge`（615 帧），素材和帧数都不同，逐阶段直接对比会
产生误导。要得到干净的对比数据，需要用同一段素材在两边各跑一次。

上表的**内部占比**是自洽的，不受这个问题影响。

---

## 六、后续

- [ ] 用 `COLMAP_TAG=b6b7b54...` 重编 COLMAP，完成 GLOMAP 对比
- [ ] 补录本次运行的重建质量指标
- [ ] 同素材跑一次 Windows / Linux 对比，填 [LINUX.md](LINUX.md) 的性能表
- [ ] 修进度解析：`Matching` 阶段末尾百分比会从 45% 跳回 32%，`Elapsed time` 行被误判
- [ ] GUI 迁移需要补 `ProcessManager` 的 Linux 进程组终止（`setsid` + `killpg`）
- [ ] `98145de`、`7ebb873`、`3f7e5cb` 三个提交对上游无害，可以单独提 PR
