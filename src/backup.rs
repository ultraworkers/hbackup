use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use chrono::Local;
use indicatif::{ProgressBar, ProgressStyle};

use crate::manifest::Manifest;
use crate::paths::BackupPaths;

pub struct BackupOptions {
    pub dry_run: bool,
    pub excludes: Vec<String>,
    pub output: Option<PathBuf>,
    pub extra_paths: Vec<PathBuf>,
    pub exclude_paths: Vec<PathBuf>,
}

pub fn run_backup(opts: &BackupOptions) -> Result<PathBuf> {
    let bp = BackupPaths::new();
    let home = &bp.home;

    // Discover files
    eprintln!("Discovering files...");
    let mut files = bp.discover(&opts.excludes, &opts.extra_paths, &opts.exclude_paths);
    files.extend(bp.discover_systemd_units());
    files.sort();
    files.dedup();

    eprintln!("Found {} files to back up", files.len());

    if opts.dry_run {
        for f in &files {
            println!("{}", f.display());
        }
        eprintln!("Dry run: {} files would be archived", files.len());
        return Ok(PathBuf::new());
    }

    // Output path
    let now = Local::now();
    let default_name = format!("hermes-openclaw-backup-{}.tar.zst", now.format("%Y%m%d-%H%M%S"));
    let output = opts.output.clone().unwrap_or_else(|| {
        home.join("backups").join(&default_name)
    });

    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).context("Creating output directory")?;
    }

    // Build manifest
    let hostname = gethostname();
    let mut manifest = Manifest::new(
        now.to_rfc3339(),
        hostname,
    );

    for f in &files {
        let size = fs::metadata(f).map(|m| m.len()).unwrap_or(0);
        let rel = f.strip_prefix(home)
            .map(|r| r.to_string_lossy().to_string())
            .unwrap_or_else(|_| f.to_string_lossy().to_string());
        manifest.add_file(rel, size);
    }

    // Create tar.zst
    let out_file = File::create(&output).context("Creating archive file")?;
    let encoder = zstd::stream::Encoder::new(out_file, 3)?;
    let mut archive = tar::Builder::new(encoder);

    // Write manifest as first entry
    let manifest_json = serde_json::to_string_pretty(&manifest)?;
    let manifest_bytes = manifest_json.as_bytes();
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest_bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    archive.append_data(&mut header, "manifest.json", manifest_bytes)?;

    // Progress bar
    let pb = ProgressBar::new(files.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} {msg}")
            .unwrap()
            .progress_chars("=>-"),
    );

    // Temp dir for sqlite backups
    let tmp_dir = std::env::temp_dir().join(format!("hbackup-{}", std::process::id()));
    let mut tmp_created = false;

    for file_path in &files {
        let rel = file_path.strip_prefix(home)
            .map(|r| r.to_string_lossy().to_string())
            .unwrap_or_else(|_| file_path.to_string_lossy().to_string());

        pb.set_message(truncate_path(&rel, 50));

        // Handle SQLite databases
        let actual_path = if is_sqlite_db(file_path) {
            if !tmp_created {
                fs::create_dir_all(&tmp_dir)?;
                tmp_created = true;
            }
            match sqlite_safe_copy(file_path, &tmp_dir) {
                Ok(tmp_path) => tmp_path,
                Err(e) => {
                    eprintln!("Warning: SQLite backup failed for {}: {}, using direct copy", rel, e);
                    file_path.clone()
                }
            }
        } else {
            file_path.clone()
        };

        if let Err(e) = append_file(&mut archive, &actual_path, &rel) {
            eprintln!("Warning: failed to add {}: {}", rel, e);
        }

        pb.inc(1);
    }

    pb.finish_with_message("done");

    // Finalize
    let encoder = archive.into_inner()?;
    encoder.finish()?;

    // Cleanup temp
    if tmp_created {
        let _ = fs::remove_dir_all(&tmp_dir);
    }

    let size = fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "Backup complete: {} ({} files, {})",
        output.display(),
        manifest.file_count,
        human_size(size),
    );

    Ok(output)
}

fn append_file<W: Write>(
    archive: &mut tar::Builder<W>,
    file_path: &Path,
    archive_path: &str,
) -> Result<()> {
    let mut f = File::open(file_path)
        .with_context(|| format!("Opening {}", file_path.display()))?;
    let meta = f.metadata()?;
    let mut header = tar::Header::new_gnu();
    header.set_size(meta.len());
    header.set_mode(0o644);
    header.set_mtime(
        meta.modified()
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    );
    header.set_cksum();
    archive.append_data(&mut header, archive_path, &mut f)?;
    Ok(())
}

fn is_sqlite_db(path: &Path) -> bool {
    path.extension().map(|e| e == "db").unwrap_or(false)
}

fn sqlite_safe_copy(db_path: &Path, tmp_dir: &Path) -> Result<PathBuf> {
    let fname = db_path.file_name().unwrap().to_string_lossy().to_string();
    let tmp_path = tmp_dir.join(&fname);
    let status = Command::new("sqlite3")
        .arg(db_path.to_string_lossy().as_ref())
        .arg(format!(".backup '{}'", tmp_path.display()))
        .status()
        .context("Running sqlite3")?;
    if !status.success() {
        anyhow::bail!("sqlite3 .backup exited with {}", status);
    }
    Ok(tmp_path)
}

fn gethostname() -> String {
    Command::new("hostname")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

fn truncate_path(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("...{}", &s[s.len() - max + 3..])
    }
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    for unit in UNITS {
        if size < 1024.0 {
            return format!("{:.1} {}", size, unit);
        }
        size /= 1024.0;
    }
    format!("{:.1} PB", size)
}
