use std::{
    env::consts::EXE_SUFFIX,
    ffi::OsString,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    error::Result,
    process::{ProcessManager, ProcessSpec},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    Ffmpeg,
    Ffprobe,
    Colmap,
    Brush,
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
    let bundled_cuda = path.parent().is_some_and(runtime_contains_cuda);
    let cpu_only = if bundled_cuda {
        Some(false)
    } else if explicit_cpu {
        Some(true)
    } else {
        None
    };
    let first_line = help
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_owned());
    let detail = match cpu_only {
        Some(true) => "三个必需命令可启动，帮助输出明确报告无 CUDA".into(),
        Some(false) => "运行目录中发现 CUDA 运行时，拒绝将其标记为 CPU 版本".into(),
        None => "命令可启动，但帮助输出未明确证明这是 CPU/no-CUDA 构建".into(),
    };
    EngineStatus {
        kind: EngineKind::Colmap,
        path: path.to_path_buf(),
        exists: true,
        can_start: successful,
        version: first_line,
        cpu_only,
        detail,
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
        ["cudart", "cublas", "cudnn", "cuda.dll"]
            .iter()
            .any(|needle| name.contains(needle))
    })
}

pub async fn require_cpu_colmap(paths: &EnginePaths) -> Result<()> {
    let status = check_colmap(&paths.colmap).await;
    if status.cpu_only == Some(true) && status.can_start {
        Ok(())
    } else {
        Err(crate::error::SplatError::UnsupportedEngine(status.detail))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
