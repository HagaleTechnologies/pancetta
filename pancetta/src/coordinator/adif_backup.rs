//! Changed startup snapshots of the ADIF source of truth.
use pancetta_config::DatabaseConfig;
use pancetta_qso::logbook_lock::LogbookLock;
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::Duration,
};

fn backup_directory(source: &Path, configured: &str, home: Option<&Path>) -> io::Result<PathBuf> {
    if configured == "~" || configured.starts_with("~/") || configured.starts_with("~\\") {
        let home = home.ok_or_else(|| io::Error::other("home directory unavailable"))?;
        return Ok(home.join(configured.get(2..).unwrap_or("")));
    }
    let directory = Path::new(configured);
    Ok(if directory.is_absolute() {
        directory.to_path_buf()
    } else {
        source.parent().unwrap_or(Path::new(".")).join(directory)
    })
}

/// Only our complete timestamped names belong to this rotation.
fn owned_name(name: &str) -> bool {
    let Some(stem) = name
        .strip_prefix("qsos_adi_")
        .and_then(|s| s.strip_suffix(".adi"))
    else {
        return false;
    };
    let parts: Vec<_> = stem.split('_').collect();
    (parts.len() == 3 || parts.len() == 4)
        && parts
            .iter()
            .zip([8, 6, 6, 6])
            .all(|(p, n)| p.len() == n && p.bytes().all(|c| c.is_ascii_digit()))
}

fn snapshots(directory: &Path) -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_file() && entry.file_name().to_str().is_some_and(owned_name) {
            paths.push(entry.path());
        }
    }
    // Base name precedes its collision suffix, which must sort after it.
    paths.sort_by_key(|p| p.file_stem().map(|s| s.to_owned()));
    Ok(paths)
}

pub(super) async fn startup(source: &Path, settings: &DatabaseConfig) -> io::Result<()> {
    if !settings.adif_backup_enabled || !source.try_exists()? {
        return Ok(());
    }
    let lock = LogbookLock::open(source)?;
    let _guard = lock
        .lock_within(Duration::from_secs(1), || {})
        .await?
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "ADIF backup logbook lock busy"))?;
    let directory = backup_directory(
        source,
        &settings.adif_backup_directory,
        dirs::home_dir().as_deref(),
    )?;
    snapshot(
        source,
        &directory,
        settings.adif_backup_retain_count,
        &chrono::Utc::now().format("%Y%m%d_%H%M%S_%6f").to_string(),
        |f, bytes| {
            f.write_all(bytes)?;
            f.sync_all()
        },
    )
}

/// The writer seam lets tests fail after a partial temporary write.
fn snapshot(
    source: &Path,
    directory: &Path,
    retain: usize,
    stamp: &str,
    write: impl FnOnce(&mut fs::File, &[u8]) -> io::Result<()>,
) -> io::Result<()> {
    if retain == 0 {
        return Err(io::Error::other("ADIF backup retention must be positive"));
    }
    let bytes = match fs::read(source) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    fs::create_dir_all(directory)?;
    let mut copies = snapshots(directory)?;
    let unchanged = match copies.last() {
        Some(path) => fs::read(path)? == bytes,
        None => false,
    };
    let mut published: Option<PathBuf> = None;
    if !unchanged {
        let temp = directory.join(format!(".qsos_adi_{}.tmp", uuid::Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        let result = (|| {
            write(&mut file, &bytes)?;
            for suffix in 0..=999_999 {
                let name = if suffix == 0 {
                    format!("qsos_adi_{stamp}.adi")
                } else {
                    format!("qsos_adi_{stamp}_{suffix:06}.adi")
                };
                let final_path = directory.join(name);
                match fs::hard_link(&temp, &final_path) {
                    Ok(()) => {
                        published = Some(final_path);
                        return Ok(());
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e),
                }
            }
            Err(io::Error::other(
                "ADIF backup filename collisions exhausted",
            ))
        })();
        drop(file);
        let cleanup = fs::remove_file(&temp);
        result?;
        cleanup?;
        copies = snapshots(directory)?;
    }
    // A backward wall clock can stamp the new snapshot before older ones; the
    // snapshot just published is still the newest, so never prune it first.
    if let Some(path) = published {
        copies.retain(|p| *p != path);
        copies.push(path);
    }
    for path in copies.iter().take(copies.len().saturating_sub(retain)) {
        fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const STAMP: &str = "20261113_121238_123456";
    fn run(source: &Path, directory: &Path, retain: usize) -> io::Result<()> {
        snapshot(source, directory, retain, STAMP, |f, b| {
            f.write_all(b)?;
            f.sync_all()
        })
    }

    #[test]
    fn backup_directory_resolution() {
        let source = Path::new("/station/qsos.adi");
        let home = Some(Path::new("/home/operator"));
        assert_eq!(
            backup_directory(source, "backups", home).unwrap(),
            Path::new("/station/backups")
        );
        assert_eq!(
            backup_directory(source, "/archive", home).unwrap(),
            Path::new("/archive")
        );
        for (s, expected) in [
            ("~", "/home/operator"),
            ("~/logs", "/home/operator/logs"),
            ("~\\logs", "/home/operator/logs"),
        ] {
            assert_eq!(
                backup_directory(source, s, home).unwrap(),
                Path::new(expected)
            );
        }
        assert!(backup_directory(source, "~", None).is_err());
    }

    #[test]
    fn unchanged_bytes_skip_backup() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("qsos.adi");
        let backups = dir.path().join("backups");
        for bytes in [b"".as_slice(), b"header only".as_slice()] {
            fs::write(&source, bytes).unwrap();
            run(&source, &backups, 7).unwrap();
            let before = snapshots(&backups).unwrap();
            fs::File::options()
                .write(true)
                .open(&source)
                .unwrap()
                .set_modified(std::time::SystemTime::now() + Duration::from_secs(60))
                .unwrap();
            run(&source, &backups, 7).unwrap();
            assert_eq!(snapshots(&backups).unwrap(), before);
            assert_eq!(fs::read(before.last().unwrap()).unwrap(), bytes);
        }
    }

    #[test]
    fn same_timestamp_preserves_previous_backup() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("qsos.adi");
        for n in 0..12 {
            fs::write(&source, [n]).unwrap();
            run(&source, dir.path(), 20).unwrap();
        }
        let copies = snapshots(dir.path()).unwrap();
        assert_eq!(copies.len(), 12);
        for (n, path) in copies.iter().enumerate() {
            assert_eq!(fs::read(path).unwrap(), [n as u8]);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
    }

    #[test]
    fn rotation_keeps_only_owned_adif_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("qsos.adi");
        let others = [
            "qso_backup_20261113.db",
            "qsos.adi.bak",
            "unrelated.adi",
            "qsos_adi_bad.adi",
            "qsos_adi_20261113_121238_123456_1.adi",
        ];
        for name in others {
            fs::write(dir.path().join(name), b"keep").unwrap();
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            &source,
            dir.path().join("qsos_adi_20000101_000000_000000.adi"),
        )
        .unwrap();
        for n in 0..5 {
            fs::write(&source, [n]).unwrap();
            run(&source, dir.path(), 7).unwrap();
        }
        run(&source, dir.path(), 2).unwrap(); // unchanged, lower retention
        let copies = snapshots(dir.path()).unwrap();
        assert_eq!(copies.len(), 2);
        assert_eq!(fs::read(&copies[0]).unwrap(), [3]);
        assert_eq!(fs::read(&copies[1]).unwrap(), [4]);
        for name in others {
            assert_eq!(fs::read(dir.path().join(name)).unwrap(), b"keep");
        }
        #[cfg(unix)]
        assert!(
            fs::symlink_metadata(dir.path().join("qsos_adi_20000101_000000_000000.adi"))
                .unwrap()
                .is_symlink()
        );
    }

    #[test]
    fn backward_clock_preserves_newest_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("qsos.adi");
        // A startup whose wall clock ran an hour ahead left this snapshot.
        fs::write(
            dir.path().join("qsos_adi_20261113_131238_123456.adi"),
            b"stale",
        )
        .unwrap();
        fs::write(&source, b"fresh").unwrap();
        run(&source, dir.path(), 1).unwrap();
        let copies = snapshots(dir.path()).unwrap();
        assert_eq!(copies.len(), 1);
        assert_eq!(fs::read(&copies[0]).unwrap(), b"fresh");
    }

    #[test]
    fn failed_publication_preserves_backups() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("qsos.adi");
        fs::write(&source, b"old").unwrap();
        run(&source, dir.path(), 7).unwrap();
        fs::write(&source, b"second").unwrap();
        run(&source, dir.path(), 7).unwrap();
        let before = snapshots(dir.path()).unwrap();
        assert_eq!(before.len(), 2);
        fs::write(&source, b"new").unwrap();
        assert!(snapshot(&source, dir.path(), 1, STAMP, |f, _| {
            f.write_all(b"partial")?;
            Err(io::Error::other("injected write failure"))
        })
        .is_err());
        assert_eq!(snapshots(dir.path()).unwrap(), before);
        assert!(!fs::read_dir(dir.path()).unwrap().any(|e| e
            .unwrap()
            .path()
            .extension()
            .is_some_and(|e| e == "tmp")));
        // Remove only this attempt's temp file after syncing to force a
        // deterministic hard-link publication failure.
        assert!(snapshot(&source, dir.path(), 1, STAMP, |f, bytes| {
            f.write_all(bytes)?;
            f.sync_all()?;
            for entry in fs::read_dir(dir.path())? {
                let path = entry?.path();
                if path.extension().is_some_and(|e| e == "tmp") {
                    fs::remove_file(path)?;
                }
            }
            Ok(())
        })
        .is_err());
        assert_eq!(snapshots(dir.path()).unwrap(), before);
        assert_eq!(fs::read(&before[0]).unwrap(), b"old");
    }

    #[tokio::test]
    async fn busy_logbook_skips_backup() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("qsos.adi");
        fs::write(&source, b"source").unwrap();
        let lock = LogbookLock::open(&source).unwrap();
        let _held = lock.try_lock().unwrap().unwrap();
        assert_eq!(
            startup(&source, &DatabaseConfig::default())
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(!dir.path().join("backups").exists());
    }
}
