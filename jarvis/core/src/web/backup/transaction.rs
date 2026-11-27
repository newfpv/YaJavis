//! Before the first rename, persist a rollback plan on the same filesystem.
use super::{RESTORE_PLUGINS_FILE, allowed_backup_path, is_secret_path, sync_dir};
use serde::{Deserialize, Serialize};
use std::io::{ErrorKind, Write};
use std::{fs, io, path::Path};

const TX: &str = ".restore-transaction";
const PLAN: &str = "rollback.json";

#[derive(Serialize, Deserialize)]
struct Entry {
    path: String,
    existed: bool,
}

fn valid(path: &str) -> bool {
    path == RESTORE_PLUGINS_FILE || allowed_backup_path(path)
}

fn remove_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn retire(root: &Path) -> io::Result<()> {
    let tx = root.join(TX);
    remove_file(&tx.join(PLAN))?;
    sync_dir(&tx)?;
    fs::remove_dir_all(tx)?;
    sync_dir(root)
}

// Validate each existing directory separately, including during crash recovery.
fn prepare_parent(root: &Path, rel: &str, create: bool) -> io::Result<()> {
    let mut dir = root.to_path_buf();
    for component in Path::new(rel).parent().unwrap().components() {
        let child = dir.join(component);
        match fs::symlink_metadata(&child) {
            Ok(m) if m.is_dir() => (),
            Ok(_) => return Err(io::Error::other("invalid restore directory")),
            Err(e) if create && e.kind() == ErrorKind::NotFound => {
                fs::create_dir(&child)?;
                sync_dir(&dir)?;
            }
            Err(e) => return Err(e),
        }
        dir = child;
    }
    Ok(())
}

pub(crate) fn recover(root: &Path) -> io::Result<()> {
    let tx = root.join(TX);
    match fs::symlink_metadata(&tx) {
        Ok(m) if m.is_dir() => (),
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
        Ok(_) => return Err(io::Error::other("invalid restore journal directory")),
    }
    match fs::read(tx.join(PLAN)) {
        Ok(bytes) => {
            let entries: Vec<Entry> = serde_json::from_slice(&bytes)?;
            for (i, entry) in entries.iter().enumerate().rev() {
                if !valid(&entry.path) {
                    return Err(io::Error::other("invalid restore path"));
                }
                prepare_parent(root, &entry.path, false)?;
                let dst = root.join(&entry.path);
                if entry.existed {
                    let old = tx.join(format!("old-{i}"));
                    // Keep the original until rollback is durable, including across repeated restarts.
                    let replay = tx.join("replay");
                    remove_file(&replay)?;
                    fs::hard_link(old, &replay)?;
                    fs::rename(replay, &dst)?;
                } else {
                    remove_file(&dst)?;
                }
                if let Some(parent) = dst.parent().filter(|p| p.is_dir()) {
                    sync_dir(parent)?;
                }
            }
        }
        Err(e) if e.kind() == ErrorKind::NotFound => (),
        Err(e) => return Err(e),
    }
    retire(root)
}

pub(super) fn apply(root: &Path, changes: &[(&str, Option<&[u8]>)]) -> io::Result<()> {
    recover(root)?;
    let tx = root.join(TX);
    fs::create_dir(&tx)?;
    let result = (|| {
        let mut plan = Vec::with_capacity(changes.len());
        for (i, &(path, bytes)) in changes.iter().enumerate() {
            if !valid(path) {
                return Err(io::Error::other("invalid restore path"));
            }
            let dst = root.join(path);
            prepare_parent(root, path, true)?;
            let existed = match fs::symlink_metadata(&dst) {
                Ok(m) if m.is_file() => {
                    fs::hard_link(&dst, tx.join(format!("old-{i}")))?;
                    true
                }
                Ok(_) => {
                    return Err(io::Error::other(
                        "restore destination is not a regular file",
                    ));
                }
                Err(e) if e.kind() == ErrorKind::NotFound => false,
                Err(e) => return Err(e),
            };
            plan.push(Entry {
                path: path.into(),
                existed,
            });
            if let Some(bytes) = bytes {
                let mut opts = fs::OpenOptions::new();
                opts.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    opts.mode(if is_secret_path(path) { 0o600 } else { 0o644 });
                }
                let mut file = opts.open(tx.join(format!("new-{i}")))?;
                file.write_all(bytes)?;
                file.sync_all()?;
            }
        }
        jarvis_station::atomic(&tx.join(PLAN), &serde_json::to_vec(&plan)?)?;
        sync_dir(root)?;
        for (i, &(path, bytes)) in changes.iter().enumerate() {
            let dst = root.join(path);
            if bytes.is_some() {
                fs::rename(tx.join(format!("new-{i}")), &dst)?;
            } else {
                remove_file(&dst)?;
            }
            sync_dir(dst.parent().unwrap())?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        recover(root)
            .map_err(|rollback| io::Error::other(format!("{error}; rollback: {rollback}")))?;
        return Err(error);
    }
    retire(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interrupted_install_restores_replaced_files_and_removes_new_ones() {
        let root = std::env::temp_dir().join(crate::settings::rand_hex(12));
        let tx = root.join(TX);
        fs::create_dir_all(&tx).unwrap();
        fs::write(tx.join("old-0"), b"old").unwrap();
        fs::write(root.join("settings.json"), b"new").unwrap();
        fs::write(root.join("plugins.json"), b"new").unwrap();
        fs::write(
            tx.join(PLAN),
            br#"[{"path":"settings.json","existed":true},{"path":"plugins.json","existed":false}]"#,
        )
        .unwrap();
        recover(&root).unwrap();
        recover(&root).unwrap();
        assert_eq!(fs::read(root.join("settings.json")).unwrap(), b"old");
        assert!(!root.join("plugins.json").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
