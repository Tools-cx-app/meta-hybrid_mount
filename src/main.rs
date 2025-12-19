mod conf;
mod core;
mod defs;
mod mount;
mod utils;

use std::path::{Path, PathBuf};
use anyhow::{Context, Result};
use clap::Parser;
use mimalloc::MiMalloc;
use serde::Serialize;

use conf::{
    cli::{Cli, Commands},
    config::{Config, CONFIG_FILE_DEFAULT},
};
use core::{
    executor,
    inventory,
    planner,
    storage,
    modules,
    OryzaEngine, 
};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[derive(Serialize)]
struct DiagnosticIssueJson {
    level: String,
    context: String,
    message: String,
}

fn load_config(cli: &Cli) -> Result<Config> {
    if let Some(config_path) = &cli.config {
        return Config::from_file(config_path)
            .with_context(|| format!("Failed to load config from custom path: {}", config_path.display()));
    }
    
    match Config::load_default() {
        Ok(config) => Ok(config),
        Err(e) => {
            let is_not_found = e.root_cause().downcast_ref::<std::io::Error>()
                .map(|io_err| io_err.kind() == std::io::ErrorKind::NotFound)
                .unwrap_or(false);

            if is_not_found {
                Ok(Config::default())
            } else {
                Err(e).context(format!("Failed to load default config from {}", CONFIG_FILE_DEFAULT))
            }
        }
    }
}

fn check_zygisksu_enforce_status() -> bool {
    std::fs::read_to_string("/data/adb/zygisksu/denylist_enforce")
        .map(|s| s.trim() != "0")
        .unwrap_or(false)
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Some(command) = &cli.command {
        match command {
            Commands::GenConfig { output } => { 
                Config::default().save_to_file(output)
                    .with_context(|| format!("Failed to save generated config to {}", output.display()))?; 
                return Ok(()); 
            },
            Commands::ShowConfig => { 
                let config = load_config(&cli)?;
                let json = serde_json::to_string(&config)
                    .context("Failed to serialize config to JSON")?;
                println!("{}", json); 
                return Ok(()); 
            },
            Commands::SaveConfig { payload } => {
                let json_bytes = (0..payload.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&payload[i..i + 2], 16))
                    .collect::<Result<Vec<u8>, _>>()
                    .context("Failed to decode hex payload")?;
                let config: Config = serde_json::from_slice(&json_bytes)
                    .context("Failed to parse config JSON payload")?;
                config.save_to_file(CONFIG_FILE_DEFAULT)
                    .context("Failed to save config file")?;
                println!("Configuration saved successfully.");
                return Ok(());
            },
            Commands::SaveRules { module, payload } => {
                let json_bytes = (0..payload.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&payload[i..i + 2], 16))
                    .collect::<Result<Vec<u8>, _>>()
                    .context("Failed to decode hex payload")?;
                let _: inventory::ModuleRules = serde_json::from_slice(&json_bytes)
                    .context("Invalid rules JSON")?;
                let rules_dir = std::path::Path::new("/data/adb/meta-hybrid/rules");
                std::fs::create_dir_all(rules_dir)
                    .context("Failed to create rules directory")?;
                let file_path = rules_dir.join(format!("{}.json", module));
                std::fs::write(&file_path, json_bytes)
                    .with_context(|| format!("Failed to write rules file: {}", file_path.display()))?;
                println!("Rules for module '{}' saved.", module);
                return Ok(());
            },
            Commands::Storage => { 
                storage::print_status().context("Failed to retrieve storage status")?; 
                return Ok(()); 
            },
            Commands::Modules => { 
                let config = load_config(&cli)?;
                modules::print_list(&config).context("Failed to list modules")?; 
                return Ok(()); 
            },
            Commands::Conflicts => {
                let config = load_config(&cli)?;
                let module_list = inventory::scan(&config.moduledir, &config)
                    .context("Failed to scan modules for conflict analysis")?;
                let plan = planner::generate(&config, &module_list, &config.moduledir)
                    .context("Failed to generate plan for conflict analysis")?;
                let report = plan.analyze_conflicts();
                let json = serde_json::to_string(&report.details)
                    .context("Failed to serialize conflict report")?;
                println!("{}", json);
                return Ok(());
            },
            Commands::Diagnostics => {
                let config = load_config(&cli)?;
                let module_list = inventory::scan(&config.moduledir, &config)
                    .context("Failed to scan modules for diagnostics")?;
                let plan = planner::generate(&config, &module_list, &config.moduledir)
                    .context("Failed to generate plan for diagnostics")?;
                let issues = executor::diagnose_plan(&plan);
                let json_issues: Vec<DiagnosticIssueJson> = issues.into_iter().map(|i| DiagnosticIssueJson {
                    level: match i.level {
                        executor::DiagnosticLevel::Info => "Info".to_string(),
                        executor::DiagnosticLevel::Warning => "Warning".to_string(),
                        executor::DiagnosticLevel::Critical => "Critical".to_string(),
                    },
                    context: i.context,
                    message: i.message,
                }).collect();
                let json = serde_json::to_string(&json_issues)
                    .context("Failed to serialize diagnostics report")?;
                println!("{}", json);
                return Ok(());
            },
            Commands::HymoStatus => {
                let config = load_config(&cli)?;
                let status = mount::hymofs::HymoFs::get_kernel_status()
                    .context("Failed to retrieve HymoFS status")?;
                
                let json_val = serde_json::to_value(&status)?;
                if let Some(json_obj) = json_val.as_object() {
                    let mut extended_obj = json_obj.clone();
                    extended_obj.insert("stealth_active".to_string(), serde_json::Value::Bool(config.hymofs_stealth));
                    extended_obj.insert("debug_active".to_string(), serde_json::Value::Bool(config.hymofs_debug));
                    println!("{}", serde_json::Value::Object(extended_obj));
                } else {
                    println!("{}", json_val);
                }
                return Ok(());
            },
            Commands::HymoAction { action, value } => {
                let mut config = load_config(&cli)?;
                match action.as_str() {
                    "set-stealth" => {
                        let enable = value.as_ref().map(|s| s == "true").unwrap_or(false);
                        mount::hymofs::HymoFs::set_stealth(enable)
                            .context("Failed to set stealth mode")?;
                        config.hymofs_stealth = enable;
                        config.save_to_file(CONFIG_FILE_DEFAULT)?;
                        println!("Stealth mode set to {}", enable);
                    },
                    "set-debug" => {
                        let enable = value.as_ref().map(|s| s == "true").unwrap_or(false);
                        mount::hymofs::HymoFs::set_debug(enable)
                            .context("Failed to set debug mode")?;
                        config.hymofs_debug = enable;
                        config.save_to_file(CONFIG_FILE_DEFAULT)?;
                        println!("Debug mode set to {}", enable);
                    },
                    "reorder-mounts" => {
                        mount::hymofs::HymoFs::reorder_mnt_id()
                            .context("Failed to reorder mount IDs")?;
                        println!("Mount IDs reordered.");
                    },
                    _ => anyhow::bail!("Unknown action: {}", action),
                }
                return Ok(());
            }
        }
    }

    let mut config = load_config(&cli)?;
    config.merge_with_cli(
        cli.moduledir.clone(), 
        cli.mountsource.clone(), 
        cli.verbose, 
        cli.partitions.clone(), 
        cli.dry_run,
    );

    if check_zygisksu_enforce_status() {
        if config.allow_umount_coexistence {
            if config.verbose {
                println!(">> ZygiskSU Enforce!=0 detected, but Umount Coexistence enabled. Respecting user config.");
            }
        } else {
            if config.verbose {
                println!(">> ZygiskSU Enforce!=0 detected. Forcing DISABLE_UMOUNT to TRUE.");
            }
            config.disable_umount = true;
        }
    }
    
    if config.dry_run {
        env_logger::builder()
            .filter_level(if config.verbose { log::LevelFilter::Debug } else { log::LevelFilter::Info })
            .init();
        
        log::info!(":: DRY-RUN / DIAGNOSTIC MODE ::");
        let module_list = inventory::scan(&config.moduledir, &config)
            .context("Inventory scan failed")?;
        log::info!(">> Inventory: Found {} modules", module_list.len());
        
        let plan = planner::generate(&config, &module_list, &config.moduledir)
            .context("Plan generation failed")?;
        plan.print_visuals();
        
        log::info!(">> Analyzing File Conflicts...");
        let report = plan.analyze_conflicts();
        if report.details.is_empty() {
            log::info!("   No file conflicts detected. Clean.");
        } else {
            log::warn!("!! DETECTED {} FILE CONFLICTS !!", report.details.len());
            for c in report.details {
                log::warn!("   [{}] {} <== {:?}", c.partition, c.relative_path, c.contending_modules);
            }
        }

        log::info!(">> Running System Diagnostics...");
        let issues = executor::diagnose_plan(&plan);
        let mut critical_count = 0;
        for issue in issues {
            match issue.level {
                core::executor::DiagnosticLevel::Critical => {
                    log::error!("[CRITICAL][{}] {}", issue.context, issue.message);
                    critical_count += 1;
                },
                core::executor::DiagnosticLevel::Warning => {
                    log::warn!("[WARN][{}] {}", issue.context, issue.message);
                },
                core::executor::DiagnosticLevel::Info => {
                    log::info!("[INFO][{}] {}", issue.context, issue.message);
                }
            }
        }

        if critical_count > 0 {
            log::error!(">> ❌ DIAGNOSTICS FAILED: {} critical issues found.", critical_count);
            log::error!(">> Mounting now would likely result in a bootloop.");
            std::process::exit(1);
        } else {
            log::info!(">> ✅ Diagnostics passed. System looks healthy.");
        }
        return Ok(());
    }

    let _log_guard = utils::init_logging(config.verbose, Path::new(defs::DAEMON_LOG_FILE))
        .context("Failed to initialize logging")?;
    
    let camouflage_name = utils::random_kworker_name();
    if let Err(e) = utils::camouflage_process(&camouflage_name) {
        log::warn!("Failed to camouflage process: {:#}", e);
    }

    log::info!(">> Initializing Meta-Hybrid Mount Daemon...");
    log::debug!("Process camouflaged as: {}", camouflage_name);

    if config.disable_umount {
        log::warn!("!! Umount is DISABLED via config.");
    }

    utils::ensure_dir_exists(defs::RUN_DIR)
        .with_context(|| format!("Failed to create run directory: {}", defs::RUN_DIR))?;

    let mnt_base = PathBuf::from(defs::FALLBACK_CONTENT_DIR);
    let img_path = Path::new(defs::BASE_DIR).join("modules.img");
    
    OryzaEngine::new(config)
        .init_storage(&mnt_base, &img_path)
        .context("Failed to initialize storage")?
        .scan_and_sync()
        .context("Failed to scan and sync modules")?
        .generate_plan()
        .context("Failed to generate mount plan")?
        .execute()
        .context("Failed to execute mount plan")?
        .finalize()
        .context("Failed to finalize boot sequence")?;

    Ok(())
}