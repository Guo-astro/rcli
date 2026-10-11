//! Ownership receipts shared with install.ps1's Update-InPlace transaction.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use windows_sys::Win32::Storage::FileSystem::{
    FileDispositionInfo, SetFileInformationByHandle, DELETE, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_DISPOSITION_INFO, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u32,
    backups: Vec<Backup>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Backup {
    name: String,
    sha256: String,
}

fn locked_regular_file(path: &Path) -> Option<fs::File> {
    // Deny writes and replacement from open through verification and deletion.
    // Inspect the opened object, not a pathname which could have changed.
    let file = fs::OpenOptions::new()
        .access_mode(windows_sys::Win32::Foundation::GENERIC_READ | DELETE)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    (metadata.is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0)
        .then_some(file)
}

fn mark_for_delete(file: &fs::File) -> bool {
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: File owns a live handle with DELETE access. The buffer and size
    // match FileDispositionInfo, and File closes the handle after this call.
    unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        ) != 0
    }
}

fn receipt_id(name: &str) -> Option<&str> {
    let id = name.strip_prefix(".wally-update-")?.strip_suffix(".json")?;
    (id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())).then_some(id)
}

fn valid_backup(backup: &Backup, id: &str) -> bool {
    let suffix = format!(".wally-backup-{id}");
    let Some(original) = backup.name.strip_suffix(&suffix) else {
        return false;
    };
    !original.is_empty()
        && !backup
            .name
            .chars()
            .any(|c| c.is_control() || "\\/:*?\"<>|".contains(c))
        && !original.ends_with(['.', ' '])
        && backup.sha256.len() == 64
        && backup.sha256.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(super) fn sweep(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(receipt_id) else {
            continue;
        };
        let receipt_path = entry.path();
        // Bound malformed metadata without ever following paths from it.
        let Some(mut receipt_file) = locked_regular_file(&receipt_path) else {
            continue;
        };
        let mut text = Vec::new();
        if (&mut receipt_file)
            .take(1024 * 1024 + 1)
            .read_to_end(&mut text)
            .is_err()
            || text.len() > 1024 * 1024
        {
            continue;
        }
        let Ok(receipt) = serde_json::from_slice::<Receipt>(&text) else {
            continue;
        };
        if receipt.version != 1 || !receipt.backups.iter().all(|b| valid_backup(b, id)) {
            continue;
        }
        let mut complete = true;
        for backup in receipt.backups {
            let path = dir.join(&backup.name);
            match fs::symlink_metadata(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                _ => {}
            }
            let Some(mut file) = locked_regular_file(&path) else {
                complete = false;
                continue;
            };
            let mut hash = Sha256::new();
            if std::io::copy(&mut file, &mut hash).is_err() {
                complete = false;
                continue;
            }
            // A stale receipt cannot delete a backup whose contents changed.
            if !hex::encode(hash.finalize()).eq_ignore_ascii_case(&backup.sha256)
                || !mark_for_delete(&file)
            {
                complete = false;
            }
        }
        if complete {
            let _ = mark_for_delete(&receipt_file);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0123456789abcdef0123456789abcdef";

    fn fixture(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let name = format!("wally.exe.wally-backup-{ID}");
        let backup = dir.join(&name);
        fs::write(&backup, b"retired executable").unwrap();
        let receipt = dir.join(format!(".wally-update-{ID}.json"));
        fs::write(&receipt, serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "backups": [{"name": name, "sha256": hex::encode(Sha256::digest(b"retired executable"))}]
        })).unwrap()).unwrap();
        (backup, receipt)
    }

    #[test]
    fn committed_cleanup_preserves_unrecorded_and_legacy_backups() {
        let dir = tempfile::tempdir().unwrap();
        let (backup, receipt) = fixture(dir.path());
        let preserved = [
            "notes.old",
            "wally.exe.old",
            "runtime.dll.123.old",
            "notes.txt",
            "unknown.wally-backup-0123456789abcdef0123456789abcdef",
        ];
        for name in preserved {
            fs::write(dir.path().join(name), b"user backup").unwrap();
        }
        sweep(dir.path());
        assert!(!backup.exists());
        assert!(!receipt.exists());
        for name in preserved {
            assert_eq!(fs::read(dir.path().join(name)).unwrap(), b"user backup");
        }
    }

    #[test]
    fn interrupted_update_has_no_cleanup_authorization() {
        let dir = tempfile::tempdir().unwrap();
        let (backup, receipt) = fixture(dir.path());
        fs::rename(&receipt, receipt.with_extension("pending")).unwrap();
        sweep(dir.path());
        assert!(backup.exists());
    }

    #[test]
    fn changed_or_locked_backup_survives_until_safe_to_remove() {
        let dir = tempfile::tempdir().unwrap();
        let (backup, receipt) = fixture(dir.path());
        fs::write(&backup, b"user replacement").unwrap();
        sweep(dir.path());
        assert_eq!(fs::read(&backup).unwrap(), b"user replacement");
        assert!(receipt.exists());
        fs::write(&backup, b"retired executable").unwrap();
        let held = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&backup)
            .unwrap();
        sweep(dir.path());
        assert!(backup.exists());
        assert!(receipt.exists());
        drop(held);
        sweep(dir.path());
        assert!(!backup.exists());
        assert!(!receipt.exists());
    }

    #[test]
    fn hash_verification_blocks_writes_and_path_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let (backup, _) = fixture(dir.path());
        let mut file = locked_regular_file(&backup).unwrap();
        let mut hash = Sha256::new();
        std::io::copy(&mut file, &mut hash).unwrap();
        assert_eq!(hash.finalize(), Sha256::digest(b"retired executable"));
        assert!(fs::write(&backup, b"user replacement").is_err());
        assert!(fs::rename(&backup, dir.path().join("moved")).is_err());
        assert!(fs::remove_file(&backup).is_err());
        assert!(mark_for_delete(&file));
        drop(file);
        assert!(!backup.exists());
    }

    #[test]
    fn invalid_receipt_never_authorizes_partial_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let (backup, receipt) = fixture(dir.path());
        let valid: serde_json::Value =
            serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
        for invalid in [
            "../notes.old",
            "C:\\notes.old",
            "notes.old",
            "wally.exe.wally-backup-other",
        ] {
            let mut text = valid.clone();
            text["backups"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({"name": invalid, "sha256": "0".repeat(64)}));
            fs::write(&receipt, serde_json::to_vec(&text).unwrap()).unwrap();
            sweep(dir.path());
            assert!(backup.exists(), "{invalid}");
        }
        fs::write(&receipt, b"not json").unwrap();
        sweep(dir.path());
        assert!(backup.exists());
    }

    #[test]
    fn directory_at_recorded_path_is_never_removed() {
        let dir = tempfile::tempdir().unwrap();
        let (backup, receipt) = fixture(dir.path());
        fs::remove_file(&backup).unwrap();
        fs::create_dir(&backup).unwrap();
        fs::write(backup.join("user.txt"), b"keep").unwrap();
        sweep(dir.path());
        assert!(receipt.exists());
        assert_eq!(fs::read(backup.join("user.txt")).unwrap(), b"keep");
    }
}
