//! Config file management: init, reset, backup, restore, and template generation.

use crate::args::ConfigAction;
use anyhow::{Context, Result};
use directories::ProjectDirs;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Generate default configuration content.
pub(crate) fn generate_default_config() -> String {
    leopardwm_ipc::config_template::render_default_config()
}

/// Get the default config file path.
pub(crate) fn default_config_path() -> Option<PathBuf> {
    ProjectDirs::from("", "", "leopardwm").map(|dirs| dirs.config_dir().join("config.toml"))
}

/// Handle the init command (generate default config).
fn handle_init(output: Option<PathBuf>, force: bool, profile: Option<String>) -> Result<()> {
    let path = output
        .or_else(default_config_path)
        .context("Could not determine config path. Use --output to specify a path.")?;

    if path.exists() && !force {
        anyhow::bail!(
            "Config file already exists at: {}\nUse --force to overwrite.",
            path.display()
        );
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }

    let config_content = match profile.as_deref() {
        Some("laptop") => generate_profile_config("laptop"),
        Some("ultrawide") => generate_profile_config("ultrawide"),
        Some("developer") => generate_profile_config("developer"),
        Some(other) => anyhow::bail!(
            "Unknown profile '{}'. Available: developer, laptop, ultrawide",
            other
        ),
        None => generate_default_config(),
    };
    atomic_write_config(&path, config_content.as_bytes())
        .with_context(|| format!("Failed to write config file: {}", path.display()))?;

    if let Some(name) = &profile {
        println!("Created config file ({} profile): {}", name, path.display());
    } else {
        println!("Created config file: {}", path.display());
    }
    println!("\nNote: The daemon creates a default config on first run.");
    println!("Use this command to regenerate or apply a profile preset.");
    println!("Run 'leopardwm-cli reload' to apply changes while daemon is running.");

    Ok(())
}

/// Generate config content for a named profile.
pub(crate) fn generate_profile_config(profile: &str) -> String {
    use leopardwm_ipc::config_template::{render_config, TemplateOverrides};

    let (gap, outer_gap, centering, name) = match profile {
        "laptop" => (6, 6, "center", Some("laptop")),
        "ultrawide" => (12, 16, "just_in_view", Some("ultrawide")),
        "developer" => (10, 10, "center", Some("developer")),
        _ => (10, 10, "center", None),
    };

    render_config(&TemplateOverrides {
        gap: Some(gap),
        outer_gap: Some(outer_gap),
        centering_mode: Some(centering),
        profile_name: name,
    })
}

/// Get the backup path for a config file.
pub(crate) fn config_backup_path(config_path: &std::path::Path) -> PathBuf {
    config_path.with_extension("toml.bak")
}

static CONFIG_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn create_config_temp_file(destination: &Path) -> io::Result<(PathBuf, File)> {
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let file_name = destination.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "config destination must include a file name",
        )
    })?;

    for _ in 0..128 {
        let sequence = CONFIG_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{}.{}.{}.tmp",
            file_name.to_string_lossy(),
            std::process::id(),
            sequence
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary config file",
    ))
}

/// Write a complete sibling temporary file, flush it, then atomically replace
/// the live config. A failed write or rename leaves the old destination intact.
fn atomic_replace_config_with<F>(destination: &Path, write_contents: F) -> Result<()>
where
    F: FnOnce(&mut File) -> io::Result<()>,
{
    let (temporary, mut file) = create_config_temp_file(destination).with_context(|| {
        format!(
            "Failed to create temporary config near {}",
            destination.display()
        )
    })?;

    let write_result = write_contents(&mut file)
        .and_then(|()| file.flush())
        .and_then(|()| file.sync_all());
    drop(file);

    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| {
            format!(
                "Failed to write temporary config for {}",
                destination.display()
            )
        });
    }

    if let Err(error) = fs::rename(&temporary, destination) {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| {
            format!(
                "Failed to atomically replace config destination {}",
                destination.display()
            )
        });
    }

    Ok(())
}

fn atomic_write_config(destination: &Path, content: &[u8]) -> Result<()> {
    atomic_replace_config_with(destination, |file| file.write_all(content))
}

fn atomic_copy_config(source: &Path, destination: &Path) -> Result<()> {
    let mut source_file = File::open(source)
        .with_context(|| format!("Failed to open config source {}", source.display()))?;
    atomic_replace_config_with(destination, |destination_file| {
        io::copy(&mut source_file, destination_file).map(|_| ())
    })
}

/// Handle config subcommands (init, reset, backup, restore).
pub(crate) fn handle_config(action: ConfigAction) -> Result<()> {
    let config_path = default_config_path().context("Could not determine config path.")?;
    let backup_path = config_backup_path(&config_path);

    match action {
        ConfigAction::Init {
            output,
            force,
            profile,
        } => {
            return handle_init(output, force, profile);
        }
        ConfigAction::Reset => {
            if config_path.exists() {
                atomic_copy_config(&config_path, &backup_path)
                    .with_context(|| format!("Failed to backup to {}", backup_path.display()))?;
                println!("Backed up current config to: {}", backup_path.display());
            }
            let config_content = generate_default_config();
            atomic_write_config(&config_path, config_content.as_bytes())
                .with_context(|| format!("Failed to reset: {}", config_path.display()))?;
            println!("Config reset to defaults: {}", config_path.display());
            println!("Run 'leopardwm-cli reload' to apply if daemon is running.");
        }
        ConfigAction::Backup => {
            if !config_path.exists() {
                anyhow::bail!("No config file found at: {}", config_path.display());
            }
            atomic_copy_config(&config_path, &backup_path)
                .with_context(|| format!("Failed to backup to {}", backup_path.display()))?;
            println!("Config backed up to: {}", backup_path.display());
        }
        ConfigAction::Restore => {
            if !backup_path.exists() {
                anyhow::bail!("No backup found at: {}", backup_path.display());
            }
            atomic_copy_config(&backup_path, &config_path)
                .with_context(|| format!("Failed to restore from {}", backup_path.display()))?;
            println!("Config restored from: {}", backup_path.display());
            println!("Run 'leopardwm-cli reload' to apply if daemon is running.");
        }
    }
    Ok(())
}

#[cfg(test)]
mod atomic_tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "leopardwm-config-atomic-{name}-{}-{}",
            std::process::id(),
            CONFIG_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn failed_atomic_write_keeps_live_config_bytes() {
        let dir = test_dir("failed-write");
        fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.toml");
        fs::write(&config, b"old = true\n").unwrap();

        let error = atomic_replace_config_with(&config, |_| {
            Err(io::Error::other("injected write failure"))
        })
        .unwrap_err();

        assert!(format!("{error:#}").contains("injected write failure"));
        assert_eq!(fs::read(&config).unwrap(), b"old = true\n");
        assert!(
            fs::read_dir(&dir).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")),
            "failed replacement leaves no temporary config behind"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_copy_replaces_only_after_complete_source_write() {
        let dir = test_dir("copy");
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("config.toml.bak");
        let destination = dir.join("config.toml");
        fs::write(&source, b"restored = true\n").unwrap();
        fs::write(&destination, b"old = true\n").unwrap();

        atomic_copy_config(&source, &destination).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"restored = true\n");
        let _ = fs::remove_dir_all(&dir);
    }
}
