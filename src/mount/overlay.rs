use anyhow::{Context, Result, bail};
use log::{info, warn};
use std::path::{Path, PathBuf};
use std::fs;
use std::os::fd::AsRawFd;
use std::time::{SystemTime, UNIX_EPOCH};
use std::ffi::CString;
use rustix::{fd::AsFd, fs::CWD, mount::*};
use crate::defs::{KSU_OVERLAY_SOURCE, RUN_DIR};
use crate::utils::send_unmountable;

const PAGE_LIMIT: usize = 4000;

struct StagedMountGuard {
    mounts: Vec<PathBuf>,
    committed: bool,
}

impl Drop for StagedMountGuard {
    fn drop(&mut self) {
        if !self.committed {
            for path in self.mounts.iter().rev() {
                let _ = unmount(path, UnmountFlags::DETACH);
                let _ = fs::remove_dir(path);
            }
        }
    }
}

pub fn mount_overlayfs(
    lower_dirs: &[String],
    lowest: &str,
    upperdir: Option<PathBuf>,
    workdir: Option<PathBuf>,
    dest: impl AsRef<Path>,
    disable_umount: bool,
) -> Result<()> {
    let lowerdir_config = lower_dirs
        .iter()
        .map(|s| s.as_ref())
        .chain(std::iter::once(lowest))
        .collect::<Vec<_>>()
        .join(":");

    if lowerdir_config.len() < PAGE_LIMIT {
        return do_mount_overlay(
            &lowerdir_config,
            upperdir,
            workdir,
            dest,
            disable_umount
        );
    }

    info!("!! Lowerdir params too long ({} bytes), switching to staged mount.", lowerdir_config.len());
    
    if upperdir.is_some() || workdir.is_some() {
        bail!("Staged mount not supported for RW overlay (upperdir/workdir present)");
    }

    mount_overlayfs_staged(lower_dirs, lowest, dest, disable_umount)
}

fn mount_overlayfs_staged(
    lower_dirs: &[String],
    lowest: &str,
    dest: impl AsRef<Path>,
    disable_umount: bool,
) -> Result<()> {
    let mut batches: Vec<Vec<String>> = Vec::new();
    let mut current_batch: Vec<String> = Vec::new();
    let mut current_len = 0;
    const SAFE_CHUNK_SIZE: usize = 3500;

    for dir in lower_dirs {
        if current_len + dir.len() + 1 > SAFE_CHUNK_SIZE {
            batches.push(current_batch);
            current_batch = Vec::new();
            current_len = 0;
        }
        current_batch.push(dir.clone());
        current_len += dir.len() + 1;
    }
    if !current_batch.is_empty() {
        batches.push(current_batch);
    }

    let staging_root = Path::new(RUN_DIR).join("staging");
    if !staging_root.exists() {
        fs::create_dir_all(&staging_root).context("failed to create staging dir")?;
    }

    let mut current_base = lowest.to_string();
    let mut guard = StagedMountGuard {
        mounts: Vec::new(),
        committed: false,
    };
    
    for (i, batch) in batches.iter().rev().enumerate() {
        let is_last_layer = i == batches.len() - 1; 
        
        let target_path = if is_last_layer {
            dest.as_ref().to_path_buf()
        } else {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .context("System time is before UNIX EPOCH")?
                .as_nanos();
                
            let stage_dir = staging_root.join(format!("stage_{}_{}", timestamp, i));
            fs::create_dir_all(&stage_dir)
                .with_context(|| format!("Failed to create stage dir {:?}", stage_dir))?;
            stage_dir
        };

        let lowerdir_str = batch
            .iter()
            .map(|s| s.as_str())
            .chain(std::iter::once(current_base.as_str()))
            .collect::<Vec<_>>()
            .join(":");

        do_mount_overlay(
            &lowerdir_str,
            None,
            None,
            &target_path,
            disable_umount
        )?;

        if !is_last_layer {
            guard.mounts.push(target_path.clone());
            current_base = target_path.to_string_lossy().to_string();
        }
    }

    guard.committed = true;
    Ok(())
}

fn do_mount_overlay(
    lowerdir_config: &str,
    upperdir: Option<PathBuf>,
    workdir: Option<PathBuf>,
    dest: impl AsRef<Path>,
    disable_umount: bool,
) -> Result<()> {
    let upperdir_s = upperdir
        .filter(|up| up.exists())
        .map(|e| e.display().to_string());
    let workdir_s = workdir
        .filter(|wd| wd.exists())
        .map(|e| e.display().to_string());

    let result = (|| {
        let fs = fsopen("overlay", FsOpenFlags::FSOPEN_CLOEXEC)?;
        let fs = fs.as_fd();
        
        fsconfig_set_string(fs, "lowerdir", lowerdir_config)?;
        
        if let (Some(upperdir), Some(workdir)) = (&upperdir_s, &workdir_s) {
            fsconfig_set_string(fs, "upperdir", upperdir)?;
            fsconfig_set_string(fs, "workdir", workdir)?;
        }
        
        fsconfig_set_string(fs, "source", KSU_OVERLAY_SOURCE)?;
        fsconfig_create(fs)?;
        
        let mount = fsmount(fs, FsMountFlags::FSMOUNT_CLOEXEC, MountAttrFlags::empty())?;
        move_mount(
            mount.as_fd(),
            "",
            CWD,
            dest.as_ref(),
            MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH,
        )
    })();

    if let Err(fsopen_err) = result {
        let mut data = format!("lowerdir={lowerdir_config}");
        if let (Some(upperdir), Some(workdir)) = (upperdir_s, workdir_s) {
            data = format!("{data},upperdir={upperdir},workdir={workdir}");
        }
        
        let data_c = CString::new(data)
            .context("Invalid string for mount data")?;
            
        mount(
            KSU_OVERLAY_SOURCE,
            dest.as_ref(),
            "overlay",
            MountFlags::empty(),
            Some(data_c.as_c_str()),
        ).with_context(|| format!("Legacy mount failed (fsopen also failed: {})", fsopen_err))?;
    }

    if !disable_umount {
        let _ = send_unmountable(dest.as_ref());
    }

    Ok(())
}

pub fn bind_mount(from: impl AsRef<Path>, to: impl AsRef<Path>, disable_umount: bool) -> Result<()> {
    let tree = open_tree(
        CWD,
        from.as_ref(),
        OpenTreeFlags::OPEN_TREE_CLOEXEC
            | OpenTreeFlags::OPEN_TREE_CLONE
            | OpenTreeFlags::AT_RECURSIVE,
    ).with_context(|| format!("open_tree failed for {}", from.as_ref().display()))?;

    move_mount(
        tree.as_fd(),
        "",
        CWD,
        to.as_ref(),
        MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH,
    ).with_context(|| format!("move_mount failed to {}", to.as_ref().display()))?;

    if !disable_umount {
        let _ = send_unmountable(to.as_ref());
    }
    Ok(())
}

pub fn mount_overlay(
    target_root: &str,
    module_roots: &[String],
    workdir: Option<PathBuf>,
    upperdir: Option<PathBuf>,
    disable_umount: bool,
) -> Result<()> {
    let root_file = fs::File::open(target_root)
        .with_context(|| format!("failed to open target root {}", target_root))?;
    let stock_root = format!("/proc/self/fd/{}", root_file.as_raw_fd());
    mount_overlayfs(module_roots, &stock_root, upperdir, workdir, target_root, disable_umount)
}