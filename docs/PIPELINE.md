# OOOSplat 处理流程与原理

这份文档说明 OOOSplat 把一段视频变成一个 `final.ply` 的完整过程：每一步做什么、
为什么要这么做、由哪个引擎执行、产物落在哪里、以及这一步失败时通常是什么原因。

对应的代码入口是 [`src-tauri/src/pipeline/runner.rs`](../src-tauri/src/pipeline/runner.rs)
的 `run_project`，阶段枚举在 [`pipeline/state.rs`](../src-tauri/src/pipeline/state.rs)。

---

## 一、总览

一句话概括：**视频里其实藏着几百个不同视角的相机，先把这些相机的位置找回来，
再用它们去拟合一团三维高斯球。**

```
input.mp4
   │
   ├── ①  ffprobe        读元数据（时长/分辨率/帧率/旋转）
   ├── ②  规划           按质量档位算出抽帧速率
   ├── ③  ffmpeg         均匀抽帧 → frames/*.jpg
   │
   │        ↓ 以下三步统称 SfM（Structure from Motion，运动恢复结构）
   ├── ④  colmap         feature_extractor    每张图找特征点
   ├── ⑤  colmap         sequential_matcher   相邻图之间配对
   ├── ⑥  colmap/glomap  mapper               解出相机位姿 + 稀疏点云
   ├── ⑦  校验           注册率是否达标
   │
   ├── ⑧  brush          3D Gaussian Splatting 训练
   └── ⑨  发布           校验 PLY 头 → final.ply
```

前七步解决的是「相机在哪」，第八步解决的是「场景长什么样」。
两者不能颠倒：没有相机位姿，训练无从谈起。

### 阶段一览

| # | 阶段枚举 | 引擎 | 全局进度区间 | 主要产物 |
|---|---|---|---|---|
| ① | `ProbingVideo` | FFprobe | 0 – 5% | `VideoInfo` |
| ② | `PlanningFrames` | — | 5 – 7% | `FramePlan` |
| ③ | `ExtractingFrames` | FFmpeg | 7 – 20% | `work/frames/frame_%06d.jpg` |
| ④ | `ExtractingFeatures` | COLMAP | 20 – 32% | `work/colmap/database.db` |
| ⑤ | `Matching` | COLMAP | 32 – 45% | 同上（写回同一个库） |
| ⑥ | `Reconstructing` | COLMAP 或 GLOMAP | 45 – 58% | `work/colmap/sparse/…` |
| ⑦ | `ValidatingReconstruction` | — | 58 – 60% | `ReconstructionReport` |
| ⑧ | `TrainingSplats` | Brush | 60 – 98% | `work/brush/final.ply.tmp` |
| ⑨ | `Exporting` | — | 98 – 100% | `final.ply` |

进度区间来自 [`pipeline/progress.rs`](../src-tauri/src/pipeline/progress.rs)。
它是**固定权重**，不是实测耗时 —— 所以进度条走到 58% 时，实际上可能已经耗掉了九成时间
（见文末的实测数据）。

### 项目目录布局

```
<projects_root>/20260823-101500_charge/
├── source/input.mp4          原视频副本，项目自包含
├── work/
│   ├── frames/               ③ 抽出的 JPEG，也是 COLMAP 的图像输入
│   ├── colmap/
│   │   ├── database.db       ④⑤ 特征与匹配都写进这一个 SQLite 库
│   │   └── sparse/0/         ⑥ cameras.bin / images.bin / points3D.bin
│   └── brush/dataset/        ⑧ 喂给 Brush 的标准 COLMAP 数据集
├── logs/                     ffprobe / ffmpeg / colmap / glomap / brush.log
├── project.json              元数据
├── state.json                阶段断点
└── final.ply                 ⑨ 最终产物
```

---

## 二、逐阶段说明

### ① ProbingVideo — 读视频信息

**做什么**：调 `ffprobe` 输出 JSON，解析出时长、宽高、帧率、总帧数、编码、旋转角。

**原理**：后面每一步的参数都从这里推导。其中两个字段值得单独说：

- **帧率** 优先取 `avg_frame_rate`，回退到 `r_frame_rate`。两者都是分数形式
  （`30000/1001` 而不是 `29.97`），所以代码里是按分子/分母解析的。
- **旋转角** 优先取 `side_data_list[].rotation`，回退到旧的 `tags.rotate`。
  手机竖拍的视频画面在文件里往往是横的，靠这个字段告诉播放器转 90°。
  丢掉它不会报错，但抽出来的帧会整体躺倒，重建结果静默地错。
  Linux 移植时之所以坚持换掉 Ubuntu 自带的 ffprobe 4.4，就是因为它读不出这个 section。

**总帧数** 取 `nb_frames`，取不到就用 `时长 × 帧率` 估算。

**失败点**：时长 < 0.25 秒、无视频轨道、宽高或帧率非法，都会在这里直接拒绝，
不会浪费后面的算力。

代码：[`video/probe.rs`](../src-tauri/src/video/probe.rs)

---

### ② PlanningFrames — 规划抽帧

**做什么**：把「质量档位」翻译成一个具体的抽帧速率。

```
sampling_fps     = 原视频帧率 × retention_ratio
estimated_frames = 总帧数     × retention_ratio
```

**原理**：这里在做一个取舍。

SfM 的代价随图像数量**超线性**增长（匹配是两两配对，重建里的光束法平差是稠密求解），
但重建质量并不随图像数量线性提升 —— 相邻两帧视角几乎相同，第二帧提供的新信息很少，
反而放大了误匹配的机会。所以合理的做法是**均匀降采样**，让相邻帧之间有可观的视差，
同时保证覆盖不断档。

注意策略是 `UniformRatioFrameSelection` —— **均匀**抽，不是挑清晰的抽。
代码里明确写了不做模糊帧过滤（见 [`video/extract.rs`](../src-tauri/src/video/extract.rs) 的注释），
`frames/` 就是最终交给 COLMAP 的全集。均匀的好处是相邻帧的时间间隔恒定，
这正是第 ⑤ 步顺序匹配所依赖的前提。

代码：[`video/frame_plan.rs`](../src-tauri/src/video/frame_plan.rs)

---

### ③ ExtractingFrames — 抽帧

**做什么**：

```bash
ffmpeg -hide_banner -nostdin -nostats -y -i input.mp4 \
       -vf "fps=<sampling_fps>,scale='min(1920,iw)':'min(1920,ih)':force_original_aspect_ratio=decrease" \
       -q:v 2 -start_number 1 -progress pipe:1 \
       frames/frame_%06d.jpg
```

**原理**：

- `fps=` 滤镜做的是**重采样**而不是「每 N 帧取一帧」。它按目标时间轴取最近邻帧，
  所以对变帧率（VFR）视频也能得到时间上均匀的序列 —— 手机录的视频经常是 VFR。
- `scale` 把长边压到 1920 以内且保持宽高比。SIFT 特征在 1080p 量级已经足够密集，
  再高只是让特征提取和匹配更慢。`force_original_aspect_ratio=decrease` 保证只缩不放。
- `-q:v 2` 是 JPEG 的高质量档（数值越小越好）。压缩痕迹会被 SIFT 当成特征，所以不能省。
- `-progress pipe:1` 让 ffmpeg 往 stdout 吐 `frame=123` 这样的行，
  进度解析器 `parse_ffmpeg_frame` 就靠它把进度条推起来。

**安全检查**：开跑前会扫描输出目录，只要发现已有 JPEG 就直接报错停止。
这是为了避免两次不同参数的抽帧结果混在一起，产生一个谁也说不清的图像集。

代码：[`engines/ffmpeg.rs`](../src-tauri/src/engines/ffmpeg.rs)

---

### ④ ExtractingFeatures — 特征提取

**做什么**：

```bash
colmap feature_extractor \
  --database_path database.db \
  --image_path ../frames \
  --ImageReader.camera_model SIMPLE_RADIAL \
  --ImageReader.single_camera 1 \
  --FeatureExtraction.use_gpu <0|1>
```

**原理**：对每张图跑 SIFT，得到一组**关键点**（位置、尺度、朝向）和每个点的
**128 维描述子**。描述子的意义是：同一个三维点在不同视角、不同光照、不同尺度下拍出来，
它的描述子应当仍然接近。这是后面一切匹配的基础。

两个参数是针对「视频」这个场景专门设的：

- **`--ImageReader.single_camera 1`** —— 告诉 COLMAP 所有图像来自**同一台**相机。
  这不是优化，是先验知识：它们本来就是同一段视频的帧。这样内参（焦距、主点、畸变）
  只需要解一组而不是几百组，未知量骤减，标定精度反而更高。
- **`--ImageReader.camera_model SIMPLE_RADIAL`** —— 一个焦距 + 主点 + 一个径向畸变系数。
  对手机/相机镜头够用，比 `OPENCV` 那种多系数模型更不容易在数据不足时发散。

`--FeatureExtraction.use_gpu` 由 CLI 的 `--compute` 决定。SIFT 是这条流水线里
**少数真正吃 GPU** 的环节之一，开了之后几百张图只要二十几秒。

**产物**：一个 SQLite 库 `database.db`。COLMAP 全程只用这一个库，
后面的匹配结果也写回同一个文件 —— 这也是为什么 GLOMAP 必须和写库的 COLMAP 版本一致，
否则表结构对不上，会以 `SQLite error: SQL logic error` 的形式炸掉。

**进度解析**：COLMAP 输出形如 `Processed file [12/615]`，由 `parse_bracket_progress` 提取。

代码：[`engines/colmap.rs`](../src-tauri/src/engines/colmap.rs)

---

### ⑤ Matching — 特征匹配

**做什么**：

```bash
colmap sequential_matcher \
  --database_path database.db \
  --SequentialMatching.overlap 10 \
  --FeatureMatching.use_gpu <0|1>
```

**原理**：匹配要回答的问题是「哪两张图看的是同一块东西」。

朴素做法是**穷举匹配**（exhaustive）：N 张图两两配对，代价是 O(N²)。
615 张图就是约 19 万对，每对还要做几千个描述子的最近邻搜索 —— 这会成为整条流水线的灾难。

但视频有一个额外的结构：**帧号相邻 ⇒ 视角相邻**。这正是第 ② 步坚持均匀抽帧换来的。
于是 `sequential_matcher` 只匹配每张图和它后面 `overlap=10` 张，
代价从 O(N²) 降到 O(N × 10)。这就是为什么匹配阶段只花了 25 秒，而不是几小时。

代价是：**顺序匹配看不见回环**。如果绕着物体转了一圈回到起点，
第 1 帧和第 600 帧其实拍的是同一面，但顺序匹配不会去配对它们，
累积的漂移就没有机会被闭合。COLMAP 的 `--SequentialMatching.loop_detection`
可以补上这一点，目前没有启用。

匹配完还会跑几何验证：用 RANSAC 估计两图之间的基础矩阵/本质矩阵，
把不满足对极几何的匹配当作外点剔除。留下来的叫 **inlier matches**，才会进入下一步。

---

### ⑥ Reconstructing — 求解相机位姿（整条流水线的瓶颈）

这一步有两个可选后端，由 `--mapper` 切换。产物格式完全一致，所以下游无需改动。

#### 6a. COLMAP：增量式 SfM（默认）

```bash
colmap mapper --database_path database.db --image_path ../frames --output_path sparse
```

流程是一个不断长大的循环：

1. **选种子对** —— 挑一对匹配充分、基线又足够长的图，三角化出最初的一小片点云。
2. **注册下一张图** —— 找一张能看到最多已有三维点的图，用 PnP（Perspective-n-Point）
   从「2D 点 ↔ 3D 点」的对应关系解出它的位姿。
3. **三角化新点** —— 新图带来了新的视角，把之前只被一张图看到的特征升级成三维点。
4. **局部 BA** —— 只优化刚加进来的这几台相机和它们看到的点。
5. 回到第 2 步，直到没有图能再注册。

**中途会周期性地插入一次全局操作**，日志里就是那行
`Retriangulation and Global bundle adjustment`：

- **Retriangulation（重三角化）** —— 位姿在增量过程中一直在被修正。用**当前更准的位姿**
  回头去重算那些之前失败或被判为外点的匹配，往往能救回一批三维点。
  点变多 ⇒ 约束变多 ⇒ 位姿更准。
- **Global BA（全局光束法平差）** —— 把**所有**相机位姿、所有三维点、相机内参放在一起，
  以「三维点重投影到每张图上的像素误差」为目标做非线性最小二乘（Levenberg–Marquardt）。
  这是纠正累积漂移的唯一手段：局部 BA 只能保证局部一致，误差会像滚雪球一样沿着序列累积。

**为什么这一步这么慢**：全局 BA 的未知量是 `相机数 × 位姿参数 + 点数 × 3`，
几十万量级。虽然 Ceres 用 Schur 补（消掉点，只解相机组成的 reduced camera system）
大幅压缩了规模，但它在整个增量过程中要被反复触发，而且触发得越晚代价越高。
615 帧的实测里，这一步吃掉了 **73 分钟，占全流程的 89.7%**。而且它基本是 **CPU 密集**的，
GPU 在这七十多分钟里几乎闲着。

#### 6b. GLOMAP：全局式 SfM（可选）

思路完全不同：不逐张累加，而是一次性求解全部相机。

1. **旋转平均（rotation averaging）** —— 匹配已经给出了每一对图之间的**相对**旋转。
   把所有相对旋转当作约束，解一个全局一致的**绝对**旋转集合。这一步是相对良性的优化问题。
2. **全局定位（global positioning）** —— 旋转固定后，再解位置。
3. **一次全局 BA** 收尾。

因为跳过了「注册一张 → BA 一次」的反复，图像多时通常快一个数量级。
代价是对匹配图的质量更敏感：顺序匹配这种稀疏的匹配图，全局方法不一定比增量方法稳。

> **现状**：GLOMAP 需要 COLMAP `b6b7b54` 这个特定版本（之后 `Rigid3d` 的 API 改了，
> GLOMAP 还没跟上），本机的 COLMAP 4.0.4 与之不兼容。
> `setup-engines-linux.sh` 会在构建前检查 `.source-ref` 并直接拒绝，附上重建命令。
> 详见 [MIGRATION.md](MIGRATION.md)。

#### 产物

无论哪个后端，输出都是三个 COLMAP 二进制文件：

| 文件 | 内容 |
|---|---|
| `cameras.bin` | 内参：相机模型、焦距、主点、畸变系数 |
| `images.bin` | 外参：每张图的旋转四元数 + 平移，以及它的 2D 点到 3D 点的对应 |
| `points3D.bin` | 稀疏点云：坐标、颜色、误差、能看到它的图像列表（track） |

三个文件的开头都是一个小端 `u64`，表示条目数量 —— 第 ⑦ 步就是直接读这 8 个字节。

---

### ⑦ ValidatingReconstruction — 校验

**做什么**：不启动任何引擎，纯读文件。

1. 三个 `.bin` 必须存在且大于 8 字节；
2. 数 `frames/` 里的 JPEG 得到 `input_images`；
3. 读 `images.bin` 头 8 字节得到 `registered_images`，读 `points3D.bin` 得到 `points_3d`；
4. `registered_ratio = registered_images / input_images`，据此分级：

| 注册率 | 判定 | 行为 |
|---|---|---|
| ≥ 80% | `Good` | 继续 |
| ≥ 50% | `Warning` | 继续，但在结果里带一条警告 |
| < 50% | `Failed` | **中止**，不进入训练 |

**为什么要卡这一刀**：Brush 训练是整条流水线里第二贵的环节（GPU 上十几分钟到几小时）。
如果只有三成图像被成功注册，说明相机轨迹本身就是残缺的，训练出来一定是废品。
在这里花零点几秒挡住，比跑完再发现划算得多。

**`best_sparse_model` 的存在理由**：增量 mapper 遇到「场景断成了两半」时
（比如中途镜头被挡、或者转场太快匹配断链），会输出 `sparse/0`、`sparse/1`……
多个互不连通的子模型。而 GLOMAP 只产一个模型，且直接写在 `sparse/` 根目录下 ——
这是增量 mapper 从不产生的布局。所以候选集合里既包含每个子目录，也包含根目录本身，
逐个校验后取**注册图像最多**的那个。

代码：[`reconstruction/validator.rs`](../src-tauri/src/reconstruction/validator.rs)

---

### ⑧ TrainingSplats — 高斯泼溅训练

#### 先准备数据集

`prepare_brush_dataset` 把结果整理成 Brush 认识的标准 COLMAP 布局：

```
work/brush/dataset/
├── images/          ← frames/ 的**硬链接**（失败才回退到复制）
└── sparse/0/        ← 上一步选中的那个模型的三个 .bin
```

用硬链接是因为几百张 1080p JPEG 有几百 MB，同一个卷上硬链接是零成本的；
跨卷或文件系统不支持时才复制。

#### 训练

```bash
brush --total-steps <N> --max-resolution <R> \
      --export-every <N> --export-path work/brush \
      --export-name final.ply.tmp \
      work/brush/dataset
```

**原理**：3D Gaussian Splatting 用一堆**各向异性三维高斯球**来表示场景。
每个高斯有：中心位置、协方差（拆成缩放 + 旋转四元数）、不透明度、
以及用球谐系数（SH）表示的方向相关颜色。

训练是一个纯粹的**可微渲染 + 梯度下降**循环：

1. **初始化** —— 用第 ⑥ 步的稀疏点云作为初始高斯的位置。这就是为什么 `points_3d` 重要：
   点云太稀，高斯的起点就差，收敛慢且容易留下空洞。
2. **渲染** —— 取一张训练图，用它的已知位姿把所有高斯投影（splat）到图像平面，
   按深度排序做 alpha 混合，得到一张渲染图。整个过程是可微的。
3. **算 loss** —— 渲染图和真实照片之间的 L1 + D-SSIM。
4. **反传** —— 梯度同时更新高斯的位置、形状、朝向、透明度、颜色。
5. **自适应密度控制** —— 周期性地：梯度大且高斯太小的地方**克隆**（欠重建），
   梯度大且高斯太大的地方**分裂**（过重建），几乎透明的**删除**。
   高斯的数量因此是训练中自己长出来的，不是预设的。
6. 回到第 2 步，跑满 `total-steps`。

**注意**：相机位姿在这里是**固定不动的真值**。3DGS 不解相机 —— 这正是前面七步的全部意义。

**参数**：

- `--total-steps` 迭代次数，是质量/时间最直接的旋钮。
- `--max-resolution` 训练时图像的长边上限。降低它显著提速，但细节上限也随之降低。
- `--export-every N` 配上 `--total-steps N`，效果是**只在最后导出一次**，不写中间产物。

**这一步是真正的 GPU 负载**，也是全流水线唯一能靠换显卡明显加速的环节。

**进度**：Brush 不打印可解析的百分比，所以观察器走 `Heartbeat` 模式，
只报「还活着 + 已用时」，进度条在这一段是不确定态。

代码：[`engines/brush.rs`](../src-tauri/src/engines/brush.rs)

---

### ⑨ Exporting — 校验并发布

**做什么**：读 PLY 头部（最多 256 KB），确认它是一个**高斯**点云而不是普通点云，
然后 `rename` 成项目根目录下的 `final.ply`。

检查清单：

- 以 `ply\n` 开头，含 `end_header`；
- `element vertex N` 且 `N > 0` —— 这就是最终的高斯数量；
- 必须同时存在 `x/y/z`、`f_dc_0`（SH 直流项，即基础颜色）、`opacity`、`scale_0`、`rot_0`
  这几个属性。缺任何一个都说明这不是 3DGS 格式。

用 `rename` 而不是 `copy`：同一文件系统内的重命名是原子的，
不会出现「`final.ply` 已存在但只写了一半」的中间状态。

代码：[`reconstruction/ply.rs`](../src-tauri/src/reconstruction/ply.rs)

---

## 三、两个全局开关和质量档位

### `--compute cpu | gpu`

只影响 COLMAP 的两个开关：`--FeatureExtraction.use_gpu` 和 `--FeatureMatching.use_gpu`。
默认是 `cpu`，因为 Windows 分发版打包的是无 CUDA 的 COLMAP。

**它管不到 mapper**。mapper（第 ⑥ 步，占九成时间）无论如何都是 CPU 密集的，
`--compute gpu` 对它一秒都省不了。

### `--mapper colmap | glomap`

只影响第 ⑥ 步。两者写出相同的三个 `.bin`，第 ⑦ 步之后完全不感知用了哪个。
GLOMAP 在 `check_all()` 里被刻意排除 —— 它是可选引擎，不齐不该阻止桌面端启动。

### 质量档位

| 档位 | 保留比例 | Brush 迭代 | 训练分辨率上限 |
|---|---|---|---|
| `fast` | 30% | 8,000 | 1200 |
| `balanced`（默认） | 50% | 15,000 | 1600 |
| `high` | 100% | 30,000 | 2000 |

一个档位同时拨动两端：**保留比例**决定 SfM 的代价（超线性，影响最大的是第 ⑥ 步），
**迭代次数和分辨率**决定训练的代价。所以从 `balanced` 调到 `high`，
总耗时的增幅远不止一倍。

代码：[`presets/quality.rs`](../src-tauri/src/presets/quality.rs)

---

## 四、进度与事件模型

每个事件（[`pipeline/event.rs`](../src-tauri/src/pipeline/event.rs)）带四类信息：

- `kind`：`Stage`（阶段切换）/ `Progress`（有具体数字）/ `Log`（引擎原文）/
  `Heartbeat`（只证明活着）
- `stage_progress`：本阶段内的 0–100
- `progress`：映射到全局 0–100，公式是 `区间起点 + (区间终点 − 起点) × 阶段进度`
- `sequence`：单调递增，UI 靠它保序

四种解析模式对应四种引擎输出格式：

| 模式 | 用在 | 认的格式 |
|---|---|---|
| `Ffmpeg` | ③ | `frame=123` |
| `BracketProgress` | ④⑤ | `... [12/615]` |
| `Mapper` | ⑥ COLMAP | `num_reg_frames=87`，或数 `Registering image #` 的出现次数 |
| `Glomap` / `Brush` | ⑥ GLOMAP、⑧ | 无可解析进度，只发心跳 |

不匹配任何模式的行，若含 `bundle` / `register` / `triang` / `elapsed` /
`warning` / `error` / `writing` / `loading` 之一，会作为日志转发到界面。

> **已知缺陷**：第 ⑥ 步 COLMAP 打印 `Elapsed time` 那一行时，
> `Mapper` 模式解不出数字，会走到日志分支并把 `stage` 报成上一阶段，
> 界面上表现为进度从 45% 跳回 32%。不影响实际执行。

`state.json` 记录了 `features_complete` / `matching_complete` / `reconstruction_complete`。
**目前没有断点续跑** —— 这些标记的用途是：桌面端启动时若发现某个项目状态是 `Running`
但没有活跃任务，就把它标记为 `Interrupted`。

---

## 五、实测的时间分布

615 帧、`--compute gpu`、默认 COLMAP mapper、RTX 3080 Ti + 20 核：

| 阶段 | 耗时 | 占比 | 瓶颈 |
|---|---|---|---|
| ①② 探测 + 规划 | < 1 秒 | — | — |
| ③ 抽帧 | 5 秒 | 0.1% | CPU |
| ④ 特征提取 | 20.6 秒 | 0.4% | GPU |
| ⑤ 顺序匹配 | 25.4 秒 | 0.5% | GPU |
| **⑥ 重建（mapper）** | **73.0 分钟** | **89.7%** | **CPU** |
| ⑦ 校验 | < 1 秒 | — | — |
| ⑧ Brush 训练 | 7 分 28 秒 | 9.2% | GPU |
| ⑨ 发布 | < 1 秒 | — | — |
| **合计** | **81 分 26 秒** | | |

结论很直白：

1. **要提速，只有第 ⑥ 步值得动** —— 换 GLOMAP，或者降低保留比例减少图像数。
   在这条曲线上，换更好的显卡对总时间的影响不到一成。
2. **拆分 CPU / GPU 服务器不划算**。GPU 在第 ⑥ 步的 73 分钟里确实闲着，
   但按 ¥1.08/小时算也就浪费一块三，而拆分要来回搬运几百 MB 的帧目录，
   还得给流水线加上分阶段的子命令（目前只有 `extract` 是独立可调的）。
3. **进度条的固定权重和实际耗时严重不符**：第 ⑥ 步只占进度条 13 个百分点，
   却占了九成时间。看着卡在 45%–58% 是正常的。

---

## 六、按阶段排错

| 症状 | 阶段 | 通常原因 |
|---|---|---|
| `FFprobe 无法解码这个文件` / `No match for section 'stream_side_data'` | ① | ffprobe 版本太旧（< 5.0），换静态构建 |
| `抽帧目录中已有 JPEG` | ③ | 上次的残留，删掉 `work/frames/` 重来 |
| `FFmpeg 未输出任何画面` | ③ | 抽帧速率算出 0，或视频轨道实际不可解 |
| 特征提取极慢 | ④ | 没走 GPU，确认 `--compute gpu` 且 `colmap -h` 显示 `with CUDA` |
| `SQLite error: SQL logic error` | ⑥ | GLOMAP 链接的 COLMAP 与写库的不是同一个版本 |
| 重建阶段挂很久 | ⑥ | 正常。看 `logs/colmap.log` 里的 `num_reg_frames` 是否还在涨 |
| `稀疏重建输出不完整` | ⑦ | mapper 没写全，多半是上一步崩了，看日志尾部 |
| 注册率过低被中止 | ⑦ | 拍摄本身的问题：视差不足、纹理太少、运动模糊、光照剧变 |
| `PLY 缺少 Gaussian 属性` | ⑨ | Brush 导出的不是 3DGS 格式，检查参数与版本 |
| 引擎全部报缺失 | 任意 | 工作目录不对，用 `OOOSPLAT_ENGINE_DIR` 显式指定 |

---

## 相关文档

- [LINUX.md](LINUX.md) —— Linux 部署与引擎构建
- [MIGRATION.md](MIGRATION.md) —— Windows → Linux 移植过程与踩坑记录
