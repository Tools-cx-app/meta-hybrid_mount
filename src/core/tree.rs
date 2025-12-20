use std::collections::HashMap;
use std::path::PathBuf;
use crate::core::inventory::MountMode;

#[derive(Debug, Clone, PartialEq, Eq, Copy)]
pub enum FileType {
    File,
    Directory,
    Symlink,
}

#[derive(Debug, Clone)]
pub struct Mutation {
    pub module_id: String,
    pub source_path: PathBuf,
    pub file_type: FileType,
    pub mode: MountMode,
}

#[derive(Debug, Clone)]
pub enum MountStrategy {
    Unresolved,
    Passthrough,
    Overlay {
        lowerdirs: Vec<PathBuf>,
    },
    Hymo {
        source: PathBuf,
    },
    Bind {
        source: PathBuf,
    },
    Magic,
}

#[derive(Debug, Clone)]
pub struct FsNode {
    pub name: String,
    pub path: PathBuf,
    pub mutations: Vec<Mutation>,
    pub children: HashMap<String, FsNode>,
    pub strategy: MountStrategy,
}

impl FsNode {
    pub fn new(name: &str, path: PathBuf) -> Self {
        Self {
            name: name.to_string(),
            path,
            mutations: Vec::new(),
            children: HashMap::new(),
            strategy: MountStrategy::Unresolved,
        }
    }

    pub fn get_or_create_child(&mut self, name: &str) -> &mut FsNode {
        self.children
            .entry(name.to_string())
            .or_insert_with(|| {
                FsNode::new(name, self.path.join(name))
            })
    }

    pub fn insert_mutation(&mut self, mutation: Mutation) {
        self.mutations.push(mutation);
    }
}