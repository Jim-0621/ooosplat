use std::{
    env::consts::EXE_SUFFIX,
    ffi::OsString,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    error::{Result, SplatError},
    process::{ProcessManager, ProcessSpec},
};

/// Which COLMAP build the pipeline is allowed to drive.
///
/// The Windows product bundles the CPU/no-CUDA release so installs stay
/// driver independent, and refuses anything carrying a CUDA runtime. A CUDA
/// build is opt-in and moves feature extraction and matching onto the GPU;
/// the mapper is incremental and stays CPU bound under either policy.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum ComputePolicy {
    #[default]
    Cpu,
    Gpu,
}

impl ComputePolicy {
    pub const fn uses_gpu(self) -> bool {
        matches!(self, Self::Gpu)
    }

    /// COLMAP spells its GPU switches as "1"/"0".
    pub const fn colmap_use_gpu(self) -> &'static str {
        if self.uses_gpu() {
            "1"
        } else {
            "0"
        }
    }

    /// Shown in stage messages so the log says which device actually ran.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Cpu => "CPU",
            Self::Gpu => "GPU",
        }
    }
}

impl std::fmt::Display for ComputePolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Cpu => "cpu",
            Self::Gpu => "gpu",
        })
    }
}

/// Which engine turns the match graph into camera poses and a sparse model.
///
/// COLMAP's mapper is incremental: it registers one image at a time and
/// re-runs bundle adjustment over a model that keeps growing, which is what
/// makes it the longest stage of a run. GLOMAP is global -- rotation
/// averaging, then global positioning, then a single bundle adjustment -- so
/// the expensive step runs once instead of repeatedly. Both are CPU bound;
/// the win is algorithmic, not hardware.
///
/// They read the same database and write the same cameras/images/points3D
/// files, so validation and the Brush dataset step are unaffected.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum MapperBackend {
    #[default]
    Colmap,
    Glomap,
}

impl MapperBackend {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Colmap => "COLMAP",
            Self::Glomap => "GLOMAP",
        }
    }
}

impl std::fmt::Display for MapperBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Colmap => "colmap",
            Self::Glomap => "glomap",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    Ffmpeg,
    Ffprobe,
    Colmap,
    Brush,
    /// Optional. Never reported by check_all -- the desktop app disables the
    /// start button when any returned engine is unhealthy, and GLOMAP is not
    /// part of the bundled set.
    Glomap,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineStatus {
    pub kind: EngineKind,
    pub path: PathBuf,
    pub exists: bool,
    pub can_start: bool,
    pub version: Option<String>,
    pub cpu_only: Option<bool>,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct EnginePaths {
    pub root: PathBuf,
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub colmap: PathBuf,
    pub brush: PathBuf,
    /// Only present when the GLOMAP mapper backend was provisioned.
    pub glomap: PathBuf,
}

/// Bundled engines keep the same layout on every platform; only the
/// executable extension differs (".exe" on Windows, empty elsewhere).
fn executable(name: &str) -> String {
    format!("{name}{EXE_SUFFIX}")
}

impl EnginePaths {
    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            ffmpeg: root.join("ffmpeg").join(executable("ffmpeg")),
            ffprobe: root.join("ffmpeg").join(executable("ffprobe")),
            colmap: root.join("colmap").join("bin").join(executable("colmap")),
            brush: root.join("brush").join(executable("brush_app")),
            glomap: root.join("glomap").join("bin").join(executable("glomap")),
            root,
        }
    }

    pub fn discover(resource_dir: Option<&Path>) -> Self {
        if let Some(value) = std::env::var_os("OOOSPLAT_ENGINE_DIR") {
            return Self::from_root(value);
        }

        let current = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let candidates = [
            resource_dir.map(|path| path.join("engines")),
            Some(current.join("engines")),
            Some(current.join("..").join("engines")),
        ];
        let root = candidates
            .into_iter()
            .flatten()
            .find(|path| path.is_dir())
            .unwrap_or_else(|| current.join("engines"));
        Self::from_root(root)
    }

    pub async fn check_all(&self) -> Vec<EngineStatus> {
        let (ffmpeg, ffprobe, colmap, brush) = tokio::join!(
            check_basic(EngineKind::Ffmpeg, &self.ffmpeg, &["-version"]),
            check_basic(EngineKind::Ffprobe, &self.ffprobe, &["-version"]),
            check_colmap(&self.colmap),
            check_basic(EngineKind::Brush, &self.brush, &["--help"]),
        );
        vec![ffmpeg, ffprobe, colmap, brush]
    }
}

fn missing(kind: EngineKind, path: &Path) -> EngineStatus {
    EngineStatus {
        kind,
        path: path.to_path_buf(),
        exists: false,
        can_start: false,
        version: None,
        cpu_only: None,
        detail: format!("未找到 {}", path.display()),
    }
}

async fn check_basic(kind: EngineKind, path: &Path, args: &[&str]) -> EngineStatus {
    if !path.is_file() {
        return missing(kind, path);
    }
    let manager = ProcessManager::new();
    let result = manager
        .run(ProcessSpec {
            executable: path.to_path_buf(),
            args: args.iter().map(OsString::from).collect(),
            working_directory: path.parent().map(Path::to_path_buf),
            log_path: None,
            observer: None,
        })
        .await;

    match result {
        Ok(output) => {
            let combined = format!("{}\n{}", output.stdout, output.stderr);
            let first_line = combined
                .lines()
                .find(|line| !line.trim().is_empty())
                .map(|line| line.trim().to_owned());
            EngineStatus {
                kind,
                path: path.to_path_buf(),
                exists: true,
                can_start: output.success,
                version: first_line,
                cpu_only: None,
                detail: if output.success {
                    "引擎可启动".into()
                } else {
                    format!("帮助命令退出码：{:?}", output.exit_code)
                },
            }
        }
        Err(error) => EngineStatus {
            kind,
            path: path.to_path_buf(),
            exists: true,
            can_start: false,
            version: None,
            cpu_only: None,
            detail: error.to_string(),
        },
    }
}

async fn check_colmap(path: &Path) -> EngineStatus {
    if !path.is_file() {
        return missing(EngineKind::Colmap, path);
    }
    let manager = ProcessManager::new();
    let mut help = String::new();
    let mut successful = true;
    for args in [
        vec!["feature_extractor", "-h"],
        vec!["sequential_matcher", "-h"],
        vec!["mapper", "-h"],
    ] {
        match manager
            .run(ProcessSpec {
                executable: path.to_path_buf(),
                args: args.into_iter().map(OsString::from).collect(),
                working_directory: path.parent().map(Path::to_path_buf),
                log_path: None,
                observer: None,
            })
            .await
        {
            Ok(output) => {
                successful &= output.success;
                help.push_str(&output.stdout);
                help.push_str(&output.stderr);
            }
            Err(error) => {
                return EngineStatus {
                    kind: EngineKind::Colmap,
                    path: path.to_path_buf(),
                    exists: true,
                    can_start: false,
                    version: None,
                    cpu_only: None,
                    detail: error.to_string(),
                }
            }
        }
    }

    let lower = help.to_ascii_lowercase();
    let explicit_cpu = [
        "cuda: no",
        "cuda support: no",
        "without cuda",
        "no cuda support",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    // The bundled Windows engine set ships the CUDA runtime beside the binary.
    // A source build on Linux links against the system toolkit instead, so
    // fall back to the banner COLMAP prints for itself.
    let bundled_cuda = path.parent().is_some_and(runtime_contains_cuda);
    let explicit_cuda = lower.contains("with cuda") || lower.contains("cuda: yes");
    let (cpu_only, detail) = if bundled_cuda {
        (Some(false), "运行目录中发现 CUDA 运行时，拒绝将其标记为 CPU 版本")
    } else if explicit_cpu {
        (Some(true), "三个必需命令可启动，帮助输出明确报告无 CUDA")
    } else if explicit_cuda {
        (Some(false), "三个必需命令可启动，帮助输出报告为 CUDA 构建")
    } else {
        (None, "命令可启动，但帮助输出未明确证明这是 CPU/no-CUDA 构建")
    };
    let first_line = help
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_owned());
    EngineStatus {
        kind: EngineKind::Colmap,
        path: path.to_path_buf(),
        exists: true,
        can_start: successful,
        version: first_line,
        cpu_only,
        detail: detail.into(),
    }
}

fn runtime_contains_cuda(directory: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if path.is_dir() {
            return runtime_contains_cuda(&path);
        }
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        ["cudart", "cublas", "cudnn", "cuda.dll", "libcuda.so"]
            .iter()
            .any(|needle| name.contains(needle))
    })
}

pub async fn require_colmap_policy(paths: &EnginePaths, policy: ComputePolicy) -> Result<()> {
    let status = check_colmap(&paths.colmap).await;
    if !status.can_start {
        return Err(SplatError::UnsupportedEngine(status.detail));
    }
    let satisfied = match policy {
        // Shipping the no-CUDA release is what keeps Windows installs driver
        // independent, so a CUDA runtime beside the binary is a hard stop.
        ComputePolicy::Cpu => status.cpu_only == Some(true),
        // A build reporting itself as "without CUDA" can never drive the GPU.
        // Anything else passes: source and distribution builds link CUDA from
        // the system, not from the engine directory, so absence of CUDA files
        // next to the binary proves nothing.
        ComputePolicy::Gpu => status.cpu_only != Some(true),
    };
    if satisfied {
        return Ok(());
    }
    let reported = status.version.unwrap_or(status.detail);
    Err(SplatError::UnsupportedEngine(match policy {
        ComputePolicy::Cpu => format!("需要 CPU/no-CUDA 构建的 COLMAP：{reported}"),
        ComputePolicy::Gpu => {
            format!("COLMAP 自报为 no-CUDA 构建，无法满足 GPU 策略：{reported}")
        }
    }))
}

/// Deliberately separate from check_all: GLOMAP is optional, and the desktop
/// app disables the start button whenever any engine check_all returns is
/// unhealthy. Only callers that selected the backend should ask for this.
pub async fn check_glomap(path: &Path) -> EngineStatus {
    check_basic(EngineKind::Glomap, path, &["mapper", "-h"]).await
}

pub async fn require_mapper_backend(paths: &EnginePaths, backend: MapperBackend) -> Result<()> {
    match backend {
        // Already covered by the four bundled engines.
        MapperBackend::Colmap => Ok(()),
        MapperBackend::Glomap => {
            let status = check_glomap(&paths.glomap).await;
            if status.can_start {
                Ok(())
            } else if status.exists {
                Err(SplatError::EngineStart {
                    engine: "GLOMAP".into(),
                    detail: status.detail,
                })
            } else {
                Err(SplatError::EngineMissing(status.path.display().to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colmap_stays_the_default_mapper() {
        assert_eq!(MapperBackend::default(), MapperBackend::Colmap);
    }

    #[test]
    fn glomap_sits_beside_colmap_in_the_engine_layout() {
        let paths = EnginePaths::from_root("engines");
        assert_eq!(
            paths.glomap.file_name().and_then(|name| name.to_str()),
            Some(format!("glomap{EXE_SUFFIX}").as_str())
        );
        assert!(paths.glomap.parent().unwrap().ends_with("bin"));
    }

    #[test]
    fn engine_paths_use_the_platform_executable_extension() {
        let paths = EnginePaths::from_root("engines");
        for (path, stem) in [
            (&paths.ffmpeg, "ffmpeg"),
            (&paths.ffprobe, "ffprobe"),
            (&paths.colmap, "colmap"),
            (&paths.brush, "brush_app"),
        ] {
            assert_eq!(
                path.file_name().and_then(|name| name.to_str()),
                Some(format!("{stem}{EXE_SUFFIX}").as_str())
            );
        }
    }

    #[test]
    fn engine_layout_is_identical_on_every_platform() {
        let paths = EnginePaths::from_root("engines");
        assert!(paths.ffprobe.parent().unwrap().ends_with("ffmpeg"));
        assert!(paths.colmap.parent().unwrap().ends_with("bin"));
    }
}
