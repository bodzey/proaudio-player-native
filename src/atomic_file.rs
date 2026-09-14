use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn temporary_path(path: &Path, sequence: u64) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("state");
    path.with_file_name(format!(".{name}.{}.{}.tmp", std::process::id(), sequence))
}

/// Atomically replace a small persistent file and make the rename durable.
///
/// A unique `create_new` temporary file avoids concurrent-writer collisions.
/// `sync_all` on both the file and its parent protects the last valid value
/// across process crashes and sudden power loss.
pub fn write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("не вдалося створити {}", parent.display()))?;

    let (temporary, mut file) = loop {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = temporary_path(path, sequence);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temporary)
        {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("не вдалося створити {}", temporary.display()))
            }
        }
    };

    let result = (|| -> Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("не вдалося атомарно оновити {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_file_without_leaving_temporary_data() {
        let directory = std::env::temp_dir().join(format!(
            "proaudio-atomic-file-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let path = directory.join("settings.yaml");

        write(&path, b"first\n", 0o600).unwrap();
        write(&path, b"second\n", 0o600).unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"second\n");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }
}
