use std::path::{Path, PathBuf};
use anyhow::Result;
use walkdir::WalkDir;
use crate::{
    conf::config::Config, 
    defs, 
    core::{inventory::{self, Module, MountMode}, tree::{FsNode, Mutation, FileType, MountStrategy}}
};

#[derive(Debug)]
pub struct MountPlan {
    pub root: FsNode,
}

impl MountPlan {
    pub fn print_visuals(&self) {
        log::info!(">> Mount Plan Visuals:");
        self.draw_node(&self.root, "", true);
    }

    fn draw_node(&self, node: &FsNode, prefix: &str, is_last: bool) {
        if node.name.is_empty() && node.children.is_empty() { return; }

        let connector = if node.name == "/" { "" } else if is_last { "└── " } else { "├── " };
        
        let strategy_str = match &node.strategy {
            MountStrategy::Unresolved => "[UNRESOLVED]",
            MountStrategy::Passthrough => "[PASS]",
            MountStrategy::Overlay { .. } => "[OVERLAY]",
            MountStrategy::Hymo { .. } => "[HYMO]",
            MountStrategy::Bind { .. } => "[BIND]",
            MountStrategy::Magic => "[MAGIC]",
        };

        if !matches!(node.strategy, MountStrategy::Passthrough) || !node.children.is_empty() || node.name == "/" {
            log::info!("{}{}{} {} -> {:?}", prefix, connector, strategy_str, if node.name.is_empty() { "/" } else { &node.name }, node.path);
        }

        let child_prefix = if node.name == "/" { "" } else if is_last { "    " } else { "│   " };
        let new_prefix = format!("{}{}", prefix, child_prefix);
        
        let mut children: Vec<_> = node.children.values().collect();
        children.sort_by(|a, b| a.name.cmp(&b.name));
        
        for (i, child) in children.iter().enumerate() {
            self.draw_node(child, &new_prefix, i == children.len() - 1);
        }
    }
    
    pub fn analyze_conflicts(&self) -> ConflictReport {
        ConflictReport::default()
    }
}

#[derive(Debug, Default)]
pub struct ConflictReport {
    pub details: Vec<ConflictEntry>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ConflictEntry {
    pub partition: String,
    pub relative_path: String,
    pub contending_modules: Vec<String>,
}

pub fn generate(
    config: &Config, 
    modules: &[Module], 
    storage_root: &Path
) -> Result<MountPlan> {
    let mut root = FsNode::new("/", PathBuf::from("/"));

    for module in modules {
        let search_root = if matches!(module.rules.default_mode, MountMode::HymoFs) {
            Path::new(defs::HYMO_MIRROR_DIR)
        } else {
            storage_root
        };

        let mut content_path = search_root.join(&module.id);
        
        if !content_path.exists() {
            content_path = module.source_path.clone();
        }

        if !content_path.exists() { continue; }

        let partitions = get_target_partitions(config, &content_path);
        
        for part_name in partitions {
            let part_source = content_path.join(&part_name);
            if !part_source.exists() { continue; }

            for entry in WalkDir::new(&part_source).min_depth(1) {
                if let Ok(entry) = entry {
                    let path = entry.path();
                    if let Ok(relative) = path.strip_prefix(&content_path) {
                        insert_into_tree(&mut root, relative, path, module);
                    }
                }
            }
        }
    }

    resolve_tree(&mut root, config);

    Ok(MountPlan {
        root,
    })
}

fn get_target_partitions(config: &Config, module_root: &Path) -> Vec<String> {
    let mut targets = Vec::new();
    for &p in defs::BUILTIN_PARTITIONS {
        if module_root.join(p).exists() {
            targets.push(p.to_string());
        }
    }
    for p in &config.partitions {
        if !targets.contains(p) && module_root.join(p).exists() {
            targets.push(p.clone());
        }
    }
    targets
}

fn insert_into_tree(root: &mut FsNode, relative: &Path, real_source: &Path, module: &Module) {
    let mut current = root;
    let components: Vec<_> = relative.components().collect();
    
    let mut path_accumulator = PathBuf::from("/");

    for (i, component) in components.iter().enumerate() {
        let name = component.as_os_str().to_string_lossy().to_string();
        if name == "/" { continue; }
        path_accumulator.push(&name);
        
        current = current.get_or_create_child(&name);
        current.path = path_accumulator.clone();

        let is_last = i == components.len() - 1;
        if is_last {
            let ft = if real_source.is_symlink() {
                FileType::Symlink
            } else if real_source.is_dir() {
                FileType::Directory
            } else {
                FileType::File
            };
            
            let relative_str = relative.to_string_lossy();
            let mode = module.rules.get_mode(&relative_str);

            let mutation = Mutation {
                module_id: module.id.clone(),
                source_path: real_source.to_path_buf(),
                file_type: ft,
                mode,
            };
            
            current.insert_mutation(mutation);
        }
    }
}

fn resolve_tree(node: &mut FsNode, config: &Config) {
    for child in node.children.values_mut() {
        resolve_tree(child, config);
    }

    if node.name.is_empty() || node.name == "/" {
        node.strategy = MountStrategy::Passthrough;
        return;
    }

    if defs::BUILTIN_PARTITIONS.contains(&node.name.as_str()) && node.path.parent().map(|p| p == Path::new("/")).unwrap_or(false) {
        node.strategy = MountStrategy::Passthrough;
        return;
    }

    let top_mutation = node.mutations.first();
    if let Some(mut_) = top_mutation {
        if matches!(mut_.mode, inventory::MountMode::HymoFs) {
            if mut_.file_type == FileType::Directory {
                node.strategy = MountStrategy::Passthrough;
            } else {
                node.strategy = MountStrategy::Hymo { source: mut_.source_path.clone() };
            }
            return;
        }

        if matches!(mut_.mode, inventory::MountMode::Magic) {
             if mut_.file_type == FileType::Directory {
                node.strategy = MountStrategy::Magic;
             } else {
                node.strategy = MountStrategy::Bind { source: mut_.source_path.clone() };
             }
             return;
        }
    }

    let can_overlay = !config.force_ext4 
        && (node.mutations.is_empty() || node.mutations.iter().all(|m| m.file_type == FileType::Directory))
        && has_system_dir(&node.path);

    if can_overlay {
        let mut lowerdirs = Vec::new();
        for m in &node.mutations {
            lowerdirs.push(m.source_path.clone());
        }
        
        if !lowerdirs.is_empty() {
             node.strategy = MountStrategy::Overlay { lowerdirs };
             return;
        }
    }

    let has_modified_children = node.children.values().any(|c| !matches!(c.strategy, MountStrategy::Passthrough));
    
    if has_modified_children {
        // 修复：如果子节点有修改，但当前节点没有可用的 lowerdirs (Overlay内容)，
        // 强制使用 Magic 策略。
        // 之前的逻辑试图创建一个空的 Overlay，导致 "Invalid argument (os error 22)"。
        node.strategy = MountStrategy::Magic;
    } else {
        node.strategy = MountStrategy::Passthrough;
    }
}

fn has_system_dir(path: &Path) -> bool {
    path.is_dir()
}
