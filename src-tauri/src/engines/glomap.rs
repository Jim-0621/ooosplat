use std::path::{Path, PathBuf};

use crate::{
    error::{Result, SplatError},
    process::{ProcessManager, ProcessObserver, ProcessSpec},
};

/// GLOMAP mirrors `colmap mapper`'s option names and writes the same
/// cameras/images/points3D files, so swapping backends needs no changes
/// downstream. The working directory matches the COLMAP path for the same
/// reason: `images` is the ASCII-only relative path that keeps Unicode and
/// UNC project roots working.
pub async fn map(
    executable: &Path,
    database: &Path,
    images: &Path,
    output: &Path,
    log: PathBuf,
    manager: &ProcessManager,
    observer: Option<ProcessObserver>,
) -> Result<()> {
    tokio::fs::create_dir_all(output).await?;
    let result = manager
        .run(ProcessSpec {
            executable: executable.to_path_buf(),
            args: vec![
                "mapper".into(),
                "--database_path".into(),
                database.into(),
                "--image_path".into(),
                images.into(),
                "--output_path".into(),
                output.into(),
            ],
            working_directory: Some(database.parent().unwrap_or(output).to_path_buf()),
            log_path: Some(log),
            observer,
        })
        .await?;
    if result.success {
        Ok(())
    } else {
        Err(SplatError::Process(format!(
            "GLOMAP 退出码 {:?}",
            result.exit_code
        )))
    }
}
