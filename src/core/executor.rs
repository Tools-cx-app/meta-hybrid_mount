use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use anyhow::{Context, Result};
use rustix::mount::{mount_change, MountPropagationFlags};
use crate::{
    conf::config::Config, 
    mount::{overlay, hymofs::HymoFs, magic}, 
    utils,
    core::{planner::MountPlan, tree::{FsNode, MountStrategy, FileType}},
    defs
};

#[derive(Default)]
struct ExecutionStats {
    pub overlay: HashSet<String>,
    pub hymo: HashSet<String>,
    pub magic: HashSet<String>,
}

pub struct ExecutionResult {
    pub overlay_module_ids: Vec<String>,
    pub hymo_module_ids: Vec<String>,
    pub magic_module_ids: Vec<String>,
}

pub fn execute(plan: &MountPlan, config: &Config) -> Result<ExecutionResult> {
    if HymoFs::is_available() {
        let _ = HymoFs::clear();
        let _ = HymoFs::set_stealth(config.hymofs_stealth);
        let _ = HymoFs::set_debug(config.hymofs_debug);
        let _ = utils::ensure_dir_exists(defs::HYMO_MIRROR_DIR);
    }

    let mut stats = ExecutionStats::default();
    execute_node(&plan.root, config, &mut stats)?;

    let mut overlay_ids: Vec<String> = stats.overlay.into_iter().collect();
    let mut hymo_ids: Vec<String> = stats.hymo.into_iter().collect();
    let mut magic_ids: Vec<String> = stats.magic.into_iter().collect();
    
    overlay_ids.sort();
    hymo_ids.sort();
    magic_ids.sort();

    Ok(ExecutionResult {
        overlay_module_ids: overlay_ids,
        hymo_module_ids: hymo_ids,
        magic_module_ids: magic_ids,
    })
}

fn execute_node(node: &FsNode, config: &Config, stats: &mut ExecutionStats) -> Result<()> {
    match &node.strategy {
        MountStrategy::Unresolved | MountStrategy::Passthrough => {
            for child in node.children.values() {
                execute_node(child, config, stats)?;
            }
        },
        MountStrategy::Overlay { lowerdirs } => {
            for m in &node.mutations { stats.overlay.insert(m.module_id.clone()); }
            ensure_mountpoint(&node.path, FileType::Directory);
            let lower_strings: Vec<String> = lowerdirs.iter().map(|p| p.to_string_lossy().to_string()).collect();
            if let Err(e) = overlay::mount_overlay(&node.path.to_string_lossy(), &lower_strings, None, None, config.disable_umount) {
                log::warn!("Overlay failed for {}: {}", node.path.display(), e);
            }
            // Overlay 覆盖了整个目录，无需递归子节点
        },
        MountStrategy::Hymo { source } => {
            if let Some(m) = node.mutations.first() { stats.hymo.insert(m.module_id.clone()); }
            if HymoFs::is_available() {
                // 物理复制源文件到 Mirror，确保稳定性
                match copy_hymo_source(source, &node.path) {
                    Ok(copied_source) => {
                        // 注入规则。注意：这里只处理当前节点（文件）。
                        // Planner 已经确保了目录类型的节点不会被分配为 Hymo 策略。
                        if let Err(e) = HymoFs::add_rule(
                            &node.path.to_string_lossy(),
                            &copied_source.to_string_lossy(),
                            0
                        ) {
                            log::warn!("HymoFS add_rule failed: {}", e);
                        }
                    },
                    Err(e) => log::warn!("Failed to prepare Hymo source: {}", e),
                }
            }
        },
        MountStrategy::Bind { source } => {
            if let Some(m) = node.mutations.first() { stats.magic.insert(m.module_id.clone()); }
            let ft = if source.is_dir() { FileType::Directory } else { FileType::File };
            ensure_mountpoint(&node.path, ft);
            if let Err(e) = overlay::bind_mount(source, &node.path, config.disable_umount) {
                log::warn!("Bind failed {} -> {}: {}", source.display(), node.path.display(), e);
            }
        },
        MountStrategy::Magic => {
            if let Some(m) = node.mutations.first() { stats.magic.insert(m.module_id.clone()); }
            // 收集当前目录下一级需要由模块修改的文件名
            let exclusions: HashSet<String> = node.children.iter()
                .filter(|(_, child)| !matches!(child.strategy, MountStrategy::Passthrough))
                .map(|(name, _)| name.clone())
                .collect();
            
            // 构建 tmpfs 骨架，跳过 exclusions 中的文件
            if let Err(e) = magic::populate_skeleton(&node.path, &exclusions, config.disable_umount) {
                log::error!("Skeleton failed for {}: {}", node.path.display(), e);
                return Ok(());
            }

            // 递归处理子节点，将模块文件挂载到刚才跳过的空位上
            for child in node.children.values() {
                execute_node(child, config, stats)?;
            }
            let _ = mount_change(&node.path, MountPropagationFlags::PRIVATE);
        },
    }
    Ok(())
}

fn copy_hymo_source(source: &Path, target_path: &Path) -> Result<PathBuf> {
    let relative = target_path.to_string_lossy().trim_start_matches('/').to_string();
    let mirror_path = Path::new(defs::HYMO_MIRROR_DIR).join(relative);
    
    if let Some(parent) = mirror_path.parent() {
        fs::create_dir_all(parent)?;
    }

    if source.is_dir() {
        utils::sync_dir(source, &mirror_path)
            .with_context(|| format!("Failed to copy Hymo directory to {}", mirror_path.display()))?;
    } else {
        fs::copy(source, &mirror_path)
            .with_context(|| format!("Failed to copy Hymo file to {}", mirror_path.display()))?;
        utils::copy_path_context(source, &mirror_path).ok();
    }
    
    Ok(mirror_path)
}

fn ensure_mountpoint(path: &Path, file_type: FileType) {
    if path.exists() { return; }
    match file_type {
        FileType::Directory => { let _ = fs::create_dir_all(path); },
        _ => {
            if let Some(parent) = path.parent() { let _ = fs::create_dir_all(parent); }
            let _ = fs::File::create(path);
        }
    }
}