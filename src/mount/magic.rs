use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use anyhow::{Context, Result};
use rustix::fs::{chmod, chown, Gid, Mode, Uid};
use rustix::mount::{mount_change, MountPropagationFlags};
use crate::defs;
use crate::utils::{self, lgetfilecon, lsetfilecon};
use crate::mount::overlay::bind_mount;

pub fn populate_skeleton(
    target: &Path, 
    exclusions: &HashSet<String>, 
    disable_umount: bool
) -> Result<()> {
    let target_str = target.to_string_lossy();
    let relative_path = target_str.trim_start_matches('/');
    
    let mirror_dir = Path::new(defs::RUN_DIR).join("mirror").join(relative_path);
    fs::create_dir_all(&mirror_dir).with_context(|| format!("Failed to create mirror dir: {:?}", mirror_dir))?;

    // 1. 备份原目录到 mirror
    // 这里使用 bind_mount 将整个原目录挂载到 mirror 位置，作为只读的“底包”
    bind_mount(target, &mirror_dir, disable_umount)?;
    
    // 2. 确保 mirror 是私有的
    let _ = mount_change(&mirror_dir, MountPropagationFlags::PRIVATE);

    // 3. 在目标位置挂载 tmpfs
    utils::mount_tmpfs(target, crate::defs::OVERLAY_SOURCE)?;

    // 4. 恢复属性 (SELinux/Perms)
    if let Err(e) = clone_attr(&mirror_dir, target) {
        log::warn!("Failed to clone attr for tmpfs root {}: {}", target.display(), e);
    }

    // 5. 恢复内容 (Magic Mount 核心逻辑)
    // 遍历 mirror (原系统文件)，将不在 exclusions 中的文件恢复回来
    for entry in fs::read_dir(&mirror_dir)?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        
        let src_path = entry.path();
        let dst_path = target.join(&name);

        // 如果该文件在 exclusions 中，说明模块要修改它（替换或注入）
        // 我们只需创建节点占位，Executor 后续会进行覆盖挂载
        if exclusions.contains(&name) {
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                let _ = fs::create_dir(&dst_path);
                let _ = clone_attr(&src_path, &dst_path);
            } else {
                let _ = fs::File::create(&dst_path);
                let _ = clone_attr(&src_path, &dst_path);
            }
            continue;
        }

        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            let _ = fs::create_dir(&dst_path);
            let _ = clone_attr(&src_path, &dst_path);
            let _ = bind_mount(&src_path, &dst_path, disable_umount);
        } else if file_type.is_symlink() {
            if let Ok(target_link) = fs::read_link(&src_path) {
                let _ = std::os::unix::fs::symlink(&target_link, &dst_path);
                if let Ok(ctx) = lgetfilecon(&src_path) {
                    let _ = lsetfilecon(&dst_path, &ctx);
                }
            }
        } else {
            let _ = fs::File::create(&dst_path);
            let _ = clone_attr(&src_path, &dst_path);
            let _ = bind_mount(&src_path, &dst_path, disable_umount);
        }
    }

    Ok(())
}

fn clone_attr(src: &Path, dst: &Path) -> Result<()> {
    let meta = src.symlink_metadata()?;
    
    let mode = Mode::from_raw_mode(meta.permissions().mode());
    let _ = chmod(dst, mode);

    let uid = Uid::from_raw(meta.uid());
    let gid = Gid::from_raw(meta.gid());
    let _ = chown(dst, Some(uid), Some(gid));

    if let Ok(ctx) = lgetfilecon(src) {
        let _ = lsetfilecon(dst, &ctx);
    }

    Ok(())
}