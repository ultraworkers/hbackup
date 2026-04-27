mod backup;
mod manifest;
mod paths;
mod restore;
mod upload_drive;

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Deserialize;

use backup::BackupOptions;
use restore::RestoreOptions;

#[derive(Parser)]
#[command(name = "hbackup", about = "Backup and restore Hermes Agent + OpenClaw installations")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a backup archive
    Backup {
        /// Dry run: show what would be backed up without creating archive
        #[arg(long)]
        dry_run: bool,

        /// Additional exclude patterns (repeatable)
        #[arg(long = "exclude", short = 'x')]
        excludes: Vec<String>,

        /// Output archive path
        #[arg(long, short)]
        output: Option<PathBuf>,
    },

    /// Restore from a backup archive
    Restore {
        /// Path to the archive to restore
        archive: PathBuf,

        /// Dry run: show what would be restored
        #[arg(long)]
        dry_run: bool,

        /// Overwrite existing files
        #[arg(long)]
        force: bool,
    },

    /// List available backups
    List,

    /// Upload a backup archive via scp/rsync or Google Drive
    Upload {
        /// Archive to upload
        archive: PathBuf,

        /// Destination (scp/rsync format). Falls back to config file.
        destination: Option<String>,

        /// Upload to Google Drive via rclone instead of scp/rsync
        #[arg(long)]
        drive: bool,

        /// Google Drive remote name (default: gdrive)
        #[arg(long, default_value = "gdrive")]
        drive_remote: String,

        /// Google Drive folder path (default: root)
        #[arg(long, default_value = "")]
        drive_folder: String,
    },

    /// Run backup then upload (for cron)
    Auto,

    /// Setup guide for Google Drive integration
    Setup {
        /// What to set up
        #[arg(default_value = "drive")]
        component: String,
    },
}

#[derive(Deserialize)]
struct Config {
    upload: Option<UploadConfig>,
    paths: Option<PathsConfig>,
}

#[derive(Deserialize, Default, Clone)]
struct PathsConfig {
    /// Additional paths to include in every backup.
    #[serde(default)]
    extra: Vec<String>,
    /// Paths to exclude from every backup (entire directory trees).
    #[serde(default)]
    exclude: Vec<String>,
}

#[derive(Deserialize)]
struct UploadConfig {
    destination: Option<String>,
    method: Option<String>,
    #[serde(default)]
    drive_remote: String,
    #[serde(default)]
    drive_folder: String,
}

fn expand_path(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(s)
}

fn paths_config_to_vecs(pc: Option<PathsConfig>) -> (Vec<PathBuf>, Vec<PathBuf>) {
    match pc {
        None => (Vec::new(), Vec::new()),
        Some(pc) => (
            pc.extra.iter().map(|s| expand_path(s)).collect(),
            pc.exclude.iter().map(|s| expand_path(s)).collect(),
        ),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Backup { dry_run, excludes, output } => {
            let config = load_config();
            let (extra_paths, exclude_paths) = paths_config_to_vecs(config.and_then(|c| c.paths));
            let opts = BackupOptions { dry_run, excludes, output, extra_paths, exclude_paths };
            backup::run_backup(&opts)?;
        }
        Commands::Restore { archive, dry_run, force } => {
            let opts = RestoreOptions { archive, dry_run, force };
            restore::run_restore(&opts)?;
        }
        Commands::List => {
            cmd_list()?;
        }
        Commands::Upload { archive, destination, drive, drive_remote, drive_folder } => {
            if drive {
                upload_drive::upload_to_drive(&archive, &drive_remote, &drive_folder)?;
            } else {
                cmd_upload(&archive, destination.as_deref())?;
            }
        }
        Commands::Auto => {
            cmd_auto()?;
        }
        Commands::Setup { component } => {
            match component.as_str() {
                "drive" => upload_drive::print_drive_setup_guide(),
                _ => eprintln!("Unknown component '{}'. Available: drive", component),
            }
        }
    }

    Ok(())
}

fn cmd_list() -> Result<()> {
    let home = dirs::home_dir().context("Could not determine home directory")?;
    let backup_dir = home.join("backups");
    if !backup_dir.exists() {
        eprintln!("No backups directory found at {}", backup_dir.display());
        return Ok(());
    }

    let mut entries: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    for entry in fs::read_dir(&backup_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("hermes-openclaw-backup-") && name.ends_with(".tar.zst") {
            let meta = entry.metadata()?;
            entries.push((entry.path(), meta.len(), meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH)));
        }
    }

    entries.sort_by_key(|(_, _, t)| *t);

    if entries.is_empty() {
        println!("No backups found.");
        return Ok(());
    }

    for (path, size, _) in &entries {
        println!(
            "{}  {}",
            backup::human_size(*size),
            path.file_name().unwrap().to_string_lossy(),
        );
    }

    Ok(())
}

fn load_config() -> Option<Config> {
    let home = dirs::home_dir()?;
    let path = home.join(".config/hbackup/config.toml");
    let content = fs::read_to_string(&path).ok()?;
    toml::from_str(&content).ok()
}

fn cmd_upload(archive: &PathBuf, destination: Option<&str>) -> Result<()> {
    let dest = if let Some(d) = destination {
        d.to_string()
    } else {
        let config = load_config().context("No destination provided and no config file found at ~/.config/hbackup/config.toml")?;
        config.upload
            .and_then(|u| u.destination)
            .context("No upload.destination in config file")?
    };

    let config = load_config();
    let method = config
        .and_then(|c| c.upload)
        .and_then(|u| u.method)
        .unwrap_or_else(|| "scp".to_string());

    eprintln!("Uploading {} to {} via {}", archive.display(), dest, method);

    let status = match method.as_str() {
        "rsync" => Command::new("rsync")
            .args(["--progress", "-avz"])
            .arg(archive)
            .arg(&dest)
            .status()?,
        _ => Command::new("scp")
            .arg(archive)
            .arg(&dest)
            .status()?,
    };

    if !status.success() {
        anyhow::bail!("{} exited with {}", method, status);
    }

    eprintln!("Upload complete.");
    Ok(())
}

fn cmd_auto() -> Result<()> {
    let config = load_config();
    let (extra_paths, exclude_paths) =
        paths_config_to_vecs(config.as_ref().and_then(|c| c.paths.clone()));
    let opts = BackupOptions {
        dry_run: false,
        excludes: vec![],
        output: None,
        extra_paths,
        exclude_paths,
    };
    let archive = backup::run_backup(&opts)?;

    if let Some(config) = load_config() {
        if let Some(upload) = config.upload {
            if upload.destination.is_some() {
                cmd_upload(&archive, None)?;
            } else if !upload.drive_remote.is_empty() {
                upload_drive::upload_to_drive(&archive, &upload.drive_remote, &upload.drive_folder)?;
            } else {
                eprintln!("No upload destination configured, skipping upload.");
            }
        } else {
            eprintln!("No upload config found, skipping upload.");
        }
    } else {
        eprintln!("No config file found, skipping upload.");
    }

    Ok(())
}
