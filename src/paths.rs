use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct BackupPaths {
    pub home: PathBuf,
}

struct DirSpec {
    path: PathBuf,
    excludes: Vec<String>,
    mode: ScanMode,
}

enum ScanMode {
    /// Recurse everything (minus excludes)
    Full,
    /// Top-level files + specific subdirs only
    Selective(Vec<String>),
}

impl BackupPaths {
    pub fn new() -> Self {
        let home = dirs_home();
        Self { home }
    }

    pub fn discover(
        &self,
        extra_excludes: &[String],
        extra_paths: &[std::path::PathBuf],
        exclude_paths: &[std::path::PathBuf],
    ) -> Vec<PathBuf> {
        let mut specs = self.build_specs();

        // Append user-configured extra paths as full scans
        for ep in extra_paths {
            specs.push(DirSpec {
                path: ep.clone(),
                excludes: vec![],
                mode: ScanMode::Full,
            });
        }

        let mut all_files = Vec::new();

        for spec in &mut specs {
            // Skip entire spec if its root is under an excluded path
            if exclude_paths.iter().any(|ex| path_under(ex, &spec.path)) {
                continue;
            }
            if !spec.path.exists() {
                continue;
            }
            spec.excludes.extend(extra_excludes.iter().cloned());
            let files = scan_dir(&spec.path, &spec.excludes, &spec.mode);
            // Filter out individual files that fall under an excluded path
            for f in files {
                if !exclude_paths.iter().any(|ex| path_under(ex, &f)) {
                    all_files.push(f);
                }
            }
        }

        all_files.sort();
        all_files.dedup();
        all_files
    }

    fn build_specs(&self) -> Vec<DirSpec> {
        let h = &self.home;
        let mut specs = Vec::new();

        // ~/.hermes (everything)
        specs.push(DirSpec {
            path: h.join(".hermes"),
            excludes: vec![],
            mode: ScanMode::Full,
        });

        // ~/.openclaw selective
        let openclaw_subdirs: Vec<String> = vec![
            "agents", "backups", "bin", "blockrun", "cache", "canvas",
            "completions", "credentials", "cron", "delivery-queue",
            "devices", "extensions", "flows", "hooks", "identity",
            "logs", "media", "memory", "memory-graph", "skills",
            "subagents", "tasks", "telegram", "tools",
        ].into_iter().map(String::from).collect();
        specs.push(DirSpec {
            path: h.join(".openclaw"),
            excludes: vec![],
            mode: ScanMode::Selective(openclaw_subdirs),
        });

        // ~/.openclaw workspace dirs — auto-discover instead of hardcoding
        let ws_subdirs: Vec<String> = vec![
            ".openclaw", "memory", "skills", ".omx", ".pi",
            "config", "hooks", "scripts", "lib",
        ].into_iter().map(String::from).collect();
        let openclaw_base = h.join(".openclaw");
        if openclaw_base.exists() {
            if let Ok(entries) = std::fs::read_dir(&openclaw_base) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with("workspace") && entry.path().is_dir() {
                        specs.push(DirSpec {
                            path: entry.path(),
                            excludes: vec![],
                            mode: ScanMode::Selective(ws_subdirs.clone()),
                        });
                    }
                }
            }
        }

        // ~/.openclaw-dev
        specs.push(DirSpec {
            path: h.join(".openclaw-dev"),
            excludes: vec![],
            mode: ScanMode::Full,
        });

        // ~/.hermes-venv
        specs.push(DirSpec {
            path: h.join(".hermes-venv"),
            excludes: vec![],
            mode: ScanMode::Full,
        });

        // ~/.local paths
        specs.push(DirSpec {
            path: h.join(".local/openclaw-dev"),
            excludes: vec![],
            mode: ScanMode::Full,
        });
        specs.push(DirSpec {
            path: h.join(".local/bin/openclaw"),
            excludes: vec![],
            mode: ScanMode::Full,
        });
        specs.push(DirSpec {
            path: h.join(".local/state/hermes"),
            excludes: vec![],
            mode: ScanMode::Full,
        });

        // ~/.config/sah-openclaw
        specs.push(DirSpec {
            path: h.join(".config/sah-openclaw"),
            excludes: vec![],
            mode: ScanMode::Full,
        });

        specs
    }

    /// Discover systemd user unit files matching hermes/openclaw/sah-openclaw
    pub fn discover_systemd_units(&self) -> Vec<PathBuf> {
        let dir = self.home.join(".config/systemd/user");
        if !dir.exists() {
            return Vec::new();
        }
        let mut results = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.contains("hermes") || name.contains("openclaw") || name.contains("sah-openclaw") {
                    let p = entry.path();
                    if p.is_file() {
                        results.push(p);
                    }
                }
            }
        }
        results
    }
}

fn dirs_home() -> PathBuf {
    dirs::home_dir().expect("HOME environment variable not set")
}

fn scan_dir(base: &Path, excludes: &[String], mode: &ScanMode) -> Vec<PathBuf> {
    match mode {
        ScanMode::Full => scan_full(base, excludes),
        ScanMode::Selective(subdirs) => scan_selective(base, excludes, subdirs),
    }
}

fn scan_full(base: &Path, excludes: &[String]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in WalkDir::new(base).follow_links(false) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let rel = path.strip_prefix(base).unwrap_or(path);
        let rel_str = rel.to_string_lossy();
        if excludes.iter().any(|ex| path_matches_exclude(&rel_str, ex)) {
            continue;
        }
        files.push(path.to_path_buf());
    }
    files
}

fn scan_selective(base: &Path, excludes: &[String], subdirs: &[String]) -> Vec<PathBuf> {
    let mut files = Vec::new();

    // Top-level files
    if let Ok(entries) = std::fs::read_dir(base) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file() {
                files.push(p);
            }
        }
    }

    // Allowed subdirs (full recurse)
    for sub in subdirs {
        let sub_path = base.join(sub);
        if sub_path.exists() {
            files.extend(scan_full(&sub_path, excludes));
        }
    }

    files
}

fn path_matches_exclude(rel: &str, exclude: &str) -> bool {
    // Match if any path component sequence matches the exclude pattern
    rel.contains(exclude)
}

/// Returns true if `target` equals `base` or is a descendant of `base`.
fn path_under(base: &Path, target: &Path) -> bool {
    target.starts_with(base)
}
