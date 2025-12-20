use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use anyhow::Result;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use crate::{defs, conf::config};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Copy)]
#[serde(rename_all = "lowercase")]
pub enum MountMode {
    Overlay,
    HymoFs,
    Magic,
    Ignore,
    Auto,
}

impl Default for MountMode {
    fn default() -> Self {
        MountMode::Auto
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModuleRules {
    #[serde(default)]
    pub default_mode: MountMode,
    #[serde(default)]
    pub paths: HashMap<String, MountMode>, 
}

impl ModuleRules {
    pub fn load(module_dir: &Path, module_id: &str) -> Self {
        let mut rules = ModuleRules::default();
        
        let legacy_config = module_dir.join("mount_rules.txt");
        if let Ok(content) = fs::read_to_string(&legacy_config) {
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') { continue; }
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    let mode = match parts[0].to_lowercase().as_str() {
                        "overlay" => MountMode::Overlay,
                        "hymo" | "hymofs" => MountMode::HymoFs,
                        "magic" | "tmpfs" => MountMode::Magic,
                        "ignore" | "skip" => MountMode::Ignore,
                        _ => MountMode::Auto,
                    };
                    rules.paths.insert(parts[1].trim_start_matches('/').to_string(), mode);
                }
            }
        }

        let internal_config = module_dir.join("hybrid_rules.json");
        if let Ok(content) = fs::read_to_string(&internal_config) {
            if let Ok(r) = serde_json::from_str::<ModuleRules>(&content) {
                rules.default_mode = r.default_mode;
                rules.paths.extend(r.paths);
            }
        }

        let user_rules_dir = Path::new("/data/adb/meta-hybrid/rules");
        let user_config = user_rules_dir.join(format!("{}.json", module_id));
        if let Ok(content) = fs::read_to_string(&user_config) {
            if let Ok(user_rules) = serde_json::from_str::<ModuleRules>(&content) {
                rules.default_mode = user_rules.default_mode;
                rules.paths.extend(user_rules.paths);
            }
        }
        rules
    }

    pub fn get_mode(&self, relative_path: &str) -> MountMode {
        let clean_path = relative_path.trim_start_matches('/');
        if let Some(mode) = self.paths.get(clean_path) {
            return *mode;
        }
        for (path, mode) in &self.paths {
            if clean_path.starts_with(format!("{}/", path).as_str()) {
                return *mode;
            }
        }
        if self.default_mode == MountMode::Auto {
            MountMode::Overlay
        } else {
            self.default_mode
        }
    }
}

#[derive(Debug, Clone)]
pub struct Module {
    pub id: String,
    pub source_path: PathBuf,
    pub rules: ModuleRules,
}

pub fn scan(source_dir: &Path, _config: &config::Config) -> Result<Vec<Module>> {
    if !source_dir.exists() {
        return Ok(Vec::new());
    }

    let dir_entries = fs::read_dir(source_dir)?
        .collect::<std::io::Result<Vec<_>>>()?;

    let mut modules: Vec<Module> = dir_entries
        .into_par_iter()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_dir() { return None; }
            
            let id = entry.file_name().to_string_lossy().to_string();
            
            if id == "meta-hybrid" || id == "lost+found" || id == ".git" { 
                return None; 
            }
            
            if path.join(defs::DISABLE_FILE_NAME).exists() || 
               path.join(defs::REMOVE_FILE_NAME).exists() || 
               path.join(defs::SKIP_MOUNT_FILE_NAME).exists() { 
                return None; 
            }
            
            let rules = ModuleRules::load(&path, &id);
            
            Some(Module {
                id,
                source_path: path,
                rules,
            })
        })
        .collect();

    modules.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(modules)
}