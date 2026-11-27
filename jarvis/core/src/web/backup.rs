//! Резервное копирование и восстановление настроек, памяти и данных плагинов.

mod transaction;
pub(crate) use transaction::recover;

use super::{App, bad, reply};
use crate::{hardware, plugins::install::free_mb};
use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{StatusCode, header},
    response::Response,
};
use jarvis_station::{
    backup::{
        BackupFile, BackupManifest, BackupPlugin, DATA_PREFIX, FORMAT, MANIFEST_NAME, MAX_FILES,
        MAX_SINGLE_FILE, MAX_UNPACKED, MAX_ZIP, RESTORE_PLUGINS_FILE, SCHEMA_VERSION,
        allowed_backup_path, is_embedded_voice_id, is_secret_path, is_valid_plugin_id,
        pending_rust_plugins, sha256_bytes, validate_manifest, verify_and_prepare_file,
    },
    sync_dir,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Cursor, Read, Write},
    path::Path,
};

#[derive(Debug, Serialize)]
pub(crate) struct RestoreReport {
    pub schema_version: u32,
    pub app_version: String,
    pub files: usize,
    pub pending_plugins: Vec<String>,
}

fn collect_root_files(data_dir: &Path, out: &mut BTreeSet<String>) {
    for name in [
        "settings.json",
        "plugins.json",
        "chatgpt-chat.json",
        "chatgpt-chat-archive.jsonl",
    ] {
        let path = data_dir.join(name);
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_file()) {
            out.insert(name.to_string());
        }
    }
    let pd = data_dir.join("plugins-data");
    if let Ok(entries) = fs::read_dir(pd) {
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            let rel = format!("plugins-data/{}", entry.file_name().to_string_lossy());
            if allowed_backup_path(&rel) {
                out.insert(rel);
            }
        }
    }
}

fn walk_dir(dir: &Path, prefix: &str, out: &mut BTreeSet<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_symlink() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".tmp") || name.ends_with(".old") || name.ends_with(".write") {
            continue;
        }
        let rel = format!("{prefix}/{name}");
        if ft.is_dir() {
            walk_dir(&entry.path(), &rel, out);
        } else if ft.is_file() && allowed_backup_path(&rel) {
            out.insert(rel);
        }
    }
}

fn collect_plugin_files(data_dir: &Path, out: &mut BTreeSet<String>) {
    let Ok(entries) = fs::read_dir(data_dir.join("plugins")) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_symlink() || !ft.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().to_string();
        if !is_valid_plugin_id(&id) || is_embedded_voice_id(&id) {
            continue;
        }
        let plugin_dir = entry.path();
        let is_lua = fs::read(plugin_dir.join("plugin.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .is_some_and(|v| v["kind"] == "lua");
        let base = format!("plugins/{id}");
        if is_lua {
            walk_dir(&plugin_dir, &base, out);
        } else {
            let data_json = plugin_dir.join("data.json");
            if fs::symlink_metadata(&data_json).is_ok_and(|m| m.file_type().is_file()) {
                out.insert(format!("{base}/data.json"));
            }
            let data_sub = plugin_dir.join("data");
            if fs::symlink_metadata(&data_sub).is_ok_and(|m| m.file_type().is_dir()) {
                walk_dir(&data_sub, &format!("{base}/data"), out);
            }
        }
    }
}

pub(crate) fn build_backup_archive(
    data_dir: &Path,
    plugins: Vec<BackupPlugin>,
    app_version: &str,
    model: &str,
    arch: &str,
    created_at: &str,
) -> Result<Vec<u8>, String> {
    let mut paths = BTreeSet::new();
    collect_root_files(data_dir, &mut paths);
    collect_plugin_files(data_dir, &mut paths);
    if !paths.contains("settings.json") {
        return Err("Файл settings.json не найден на колонке".into());
    }
    if paths.len() > MAX_FILES {
        return Err("Слишком много файлов для резервной копии".into());
    }

    let mut manifest_files = Vec::new();
    let mut contents = Vec::new();
    let mut total = 0u64;
    for rel in paths {
        let full = data_dir.join(&rel);
        let meta = fs::symlink_metadata(&full).map_err(|e| format!("{rel}: {e}"))?;
        if meta.len() > MAX_SINGLE_FILE as u64 {
            return Err(format!("Файл {rel} слишком большой для бэкапа"));
        }
        let bytes = fs::read(&full).map_err(|e| format!("{rel}: {e}"))?;
        if bytes.len() > MAX_SINGLE_FILE {
            return Err(format!("Файл {rel} слишком большой для бэкапа"));
        }
        total = total
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| "Превышен размер бэкапа".to_string())?;
        if total > MAX_UNPACKED as u64 {
            return Err("Данные превышают допустимый размер бэкапа".into());
        }
        manifest_files.push(BackupFile {
            path: rel.clone(),
            sha256: sha256_bytes(&bytes),
            size: bytes.len() as u64,
        });
        contents.push((rel, bytes));
    }

    let manifest = BackupManifest {
        format: FORMAT.into(),
        schema_version: SCHEMA_VERSION,
        app_version: app_version.into(),
        created_at: created_at.into(),
        model: model.into(),
        arch: arch.into(),
        files: manifest_files,
        plugins,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;

    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    zip.start_file(MANIFEST_NAME, opts)
        .map_err(|e| e.to_string())?;
    zip.write_all(&manifest_bytes).map_err(|e| e.to_string())?;
    for (rel, bytes) in contents {
        zip.start_file(format!("{DATA_PREFIX}{rel}"), opts)
            .map_err(|e| e.to_string())?;
        zip.write_all(&bytes).map_err(|e| e.to_string())?;
    }
    let archive = zip.finish().map_err(|e| e.to_string())?.into_inner();
    if archive.len() > MAX_ZIP {
        return Err("Архив бэкапа превышает допустимый размер".into());
    }
    Ok(archive)
}

pub(crate) fn parse_and_verify_backup(
    raw: &[u8],
) -> Result<(BackupManifest, Vec<(String, Vec<u8>)>), String> {
    if raw.len() > MAX_ZIP {
        return Err(format!("Архив бэкапа больше {} МБ", MAX_ZIP >> 20));
    }
    let mut zr =
        zip::ZipArchive::new(Cursor::new(raw)).map_err(|_| "Это не zip-архив бэкапа".to_string())?;

    let mut manifest_idx = None;
    let mut data_entries: BTreeMap<String, usize> = BTreeMap::new();
    for i in 0..zr.len() {
        let f = zr.by_index(i).map_err(|e| e.to_string())?;
        let name = f.name().replace('\\', "/");
        if f.is_dir() {
            continue;
        }
        if f.is_symlink() || !f.is_file() {
            return Err(format!("В бэкапе недопустимый файл: {name}"));
        }
        if name == MANIFEST_NAME {
            if manifest_idx.is_some() {
                return Err("В бэкапе несколько backup.json".into());
            }
            manifest_idx = Some(i);
            continue;
        }
        if let Some(rel) = name.strip_prefix(DATA_PREFIX)
            && data_entries.insert(rel.to_string(), i).is_some()
        {
            return Err(format!("Повторяющийся файл в архиве: {name}"));
        }
    }

    let manifest_idx = manifest_idx.ok_or_else(|| "В архиве отсутствует backup.json".to_string())?;
    let mut mf_bytes = Vec::new();
    {
        let mut mf = zr.by_index(manifest_idx).map_err(|e| e.to_string())?;
        mf.by_ref()
            .take((1 << 20) + 1)
            .read_to_end(&mut mf_bytes)
            .map_err(|e| e.to_string())?;
    }
    if mf_bytes.len() > (1 << 20) {
        return Err("Манифест бэкапа слишком большой".into());
    }
    let manifest: BackupManifest =
        serde_json::from_slice(&mf_bytes).map_err(|_| "Неверный формат backup.json".to_string())?;
    validate_manifest(&manifest)?;

    let mut total = 0usize;
    let mut verified = Vec::with_capacity(manifest.files.len());
    for entry in &manifest.files {
        let Some(&idx) = data_entries.get(&entry.path) else {
            return Err(format!("В архиве отсутствует файл {}", entry.path));
        };
        let mut raw_file = Vec::new();
        {
            let mut zf = zr.by_index(idx).map_err(|e| e.to_string())?;
            zf.by_ref()
                .take((MAX_SINGLE_FILE + 1) as u64)
                .read_to_end(&mut raw_file)
                .map_err(|e| e.to_string())?;
        }
        total = total
            .checked_add(raw_file.len())
            .ok_or_else(|| "Превышен размер распакованного бэкапа".to_string())?;
        if total > MAX_UNPACKED {
            return Err("Распакованный бэкап превышает допустимый размер".into());
        }
        let processed = verify_and_prepare_file(entry, &raw_file, &manifest)?;
        verified.push((entry.path.clone(), processed));
    }
    Ok((manifest, verified))
}

fn check_restore_space(data_dir: &Path, files: &[(String, Vec<u8>)]) -> Result<(), String> {
    let total_bytes: usize = files.iter().map(|(_, b)| b.len()).sum();
    let free = free_mb(data_dir);
    if free < ((total_bytes >> 20) + 2) as u64 {
        return Err(format!(
            "Мало места на колонке для восстановления: свободно {free} МБ"
        ));
    }
    Ok(())
}

pub(crate) fn apply_verified_backup(
    data_dir: &Path,
    manifest: &BackupManifest,
    files: &[(String, Vec<u8>)],
) -> Result<RestoreReport, String> {
    let pending: Vec<BackupPlugin> = pending_rust_plugins(manifest)
        .into_iter()
        .filter(|p| !data_dir.join("plugins").join(&p.id).join("plugin.json").is_file())
        .collect();
    let marker = if pending.is_empty() { None } else {
        Some(serde_json::to_vec_pretty(&pending).map_err(|e| e.to_string())?)
    };
    let mut changes: Vec<_> = files.iter().map(|(name, data)| (name.as_str(), Some(data.as_slice()))).collect();
    changes.push((RESTORE_PLUGINS_FILE, marker.as_deref()));
    transaction::apply(data_dir, &changes).map_err(|e| e.to_string())?;

    Ok(RestoreReport {
        schema_version: manifest.schema_version,
        app_version: manifest.app_version.clone(),
        files: files.len(),
        pending_plugins: pending.into_iter().map(|p| p.id).collect(),
    })
}

#[cfg(test)]
pub(crate) fn apply_backup_archive(data_dir: &Path, raw: &[u8]) -> Result<RestoreReport, String> {
    let (manifest, files) = parse_and_verify_backup(raw)?;
    apply_verified_backup(data_dir, &manifest, &files)
}

pub async fn download(State(app): State<App>) -> Response {
    let data_dir = app.data_dir.clone();
    let plugins = app.plugins.backup_plugins();
    let model = hardware::station_model().id.to_string();
    let arch = jarvis_station::arch().to_string();
    let created_at = jiff::Timestamp::now().to_string();
    let date = app.store.now().strftime("%Y-%m-%d").to_string();

    let result = tokio::task::spawn_blocking(move || {
        build_backup_archive(&data_dir, plugins, crate::version(), &model, &arch, &created_at)
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);

    let bytes = match result {
        Ok(b) => b,
        Err(error) => return bad(error),
    };
    let len = bytes.len();
    let disp = format!("attachment; filename=yajarvis-backup-{date}.zip");
    Response::builder()
        .header(header::CONTENT_TYPE, "application/zip")
        .header(header::CONTENT_DISPOSITION, disp)
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::CONTENT_LENGTH, len)
        .body(Body::from(bytes))
        .unwrap()
}

pub async fn restore(State(app): State<App>, body: Bytes) -> Response {
    if app.engine.talking() || app.engine.music.playing() {
        return bad("Остановите разговор и музыку перед восстановлением бэкапа");
    }
    static RESTORING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let Ok(guard) = RESTORING.try_lock() else {
        return bad("Восстановление уже выполняется");
    };
    let parsed = tokio::task::spawn_blocking(move || parse_and_verify_backup(&body))
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r);
    let (manifest, files) = match parsed {
        Ok(v) => v,
        Err(error) => return bad(error),
    };

    if let Err(error) = check_restore_space(&app.data_dir, &files) { return bad(error); }
    if app.engine.talking() || app.engine.music.playing() {
        return bad("Остановите разговор и музыку перед восстановлением бэкапа");
    }
    // Complete the transaction even if the uploading client disconnects.
    tokio::spawn(async move {
        let running = app.plugins.running_ids();
        app.plugins.stop_all().await;

        let data_dir = app.data_dir.clone();
        let report = tokio::task::spawn_blocking(move || {
            apply_verified_backup(&data_dir, &manifest, &files)
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r);

        match report {
            Ok(report) => {
                log::info!(
                    "бэкап: восстановлено {} файлов (от версии {}, отложено плагинов: {})",
                    report.files,
                    report.app_version,
                    report.pending_plugins.len()
                );
                if std::env::var_os("JARVIS_READY_PATH").is_some() {
                    tokio::spawn(async move {
                        let _guard = guard;
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        std::process::exit(0);
                    });
                }
                reply(StatusCode::OK, json!({
                    "ok": true,
                    "schema_version": report.schema_version,
                    "app_version": report.app_version,
                    "files": report.files,
                    "pending_plugins": report.pending_plugins,
                }))
            }
            Err(error) => {
                for id in running { app.plugins.start(&id).await; }
                bad(error)
            }
        }
    }).await.unwrap_or_else(|e| bad(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::rand_hex;
    use jarvis_station::backup::BackupPlugin;

    #[test]
    fn backup_and_restore_roundtrip_filters_wav_and_tracks_pending_rust_plugins() {
        let src = std::env::temp_dir().join(format!("jarvis-bk-src-{}", rand_hex(6)));
        let dst = std::env::temp_dir().join(format!("jarvis-bk-dst-{}", rand_hex(6)));
        fs::create_dir_all(src.join("plugins-data")).unwrap();
        fs::create_dir_all(src.join("plugins/weather")).unwrap();
        fs::create_dir_all(src.join("plugins/youtube/data")).unwrap();
        fs::create_dir_all(src.join("plugins/training/data/wake-profiles")).unwrap();

        fs::write(
            src.join("settings.json"),
            r#"{"active":"default","city":"Москва","vol_scale":2,"place_moved":true}"#,
        )
        .unwrap();
        fs::write(
            src.join("plugins.json"),
            r#"{"weather":{"enabled":true},"youtube":{"enabled":true},"chatgpt":{"enabled":true}}"#,
        )
        .unwrap();
        fs::write(src.join("chatgpt-chat.json"), r#"{"profile":"Имя"}"#).unwrap();
        fs::write(src.join("chatgpt-chat-archive.jsonl"), "{\"user\":\"hi\",\"bot\":\"ok\"}\n").unwrap();
        fs::write(src.join("plugins-data/weather.json"), r#"{"last":"sunny"}"#).unwrap();
        fs::write(
            src.join("plugins/weather/plugin.json"),
            r#"{"id":"weather","name":"Погода","version":"1.1.0","kind":"lua","main":"main.lua","category":"info"}"#,
        )
        .unwrap();
        fs::write(src.join("plugins/weather/main.lua"), "return {}").unwrap();
        fs::write(
            src.join("plugins/youtube/plugin.json"),
            r#"{"id":"youtube","name":"YouTube","version":"1.0.0","kind":"rust","main":"youtube","category":"media"}"#,
        )
        .unwrap();
        fs::write(src.join("plugins/youtube/youtube"), b"ELF-BINARY").unwrap();
        fs::write(src.join("plugins/youtube/data/cookies.txt"), "cookie=abc").unwrap();
        fs::write(src.join("plugins/training/data/wake-profiles/abc.bin"), b"MODEL").unwrap();
        fs::write(src.join("plugins/training/data/pos_001.wav"), b"RIFF-AUDIO").unwrap();

        let archive = build_backup_archive(
            &src,
            vec![
                BackupPlugin {
                    id: "weather".into(),
                    kind: "lua".into(),
                    source: "newfpv/YaJarvis".into(),
                    official: true,
                    version: "1.1.0".into(),
                    enabled: true,
                },
                BackupPlugin {
                    id: "youtube".into(),
                    kind: "rust".into(),
                    source: "newfpv/YaJarvis".into(),
                    official: true,
                    version: "1.0.0".into(),
                    enabled: true,
                },
            ],
            "0.10.15",
            "lite",
            "arm64",
            "2026-11-22T12:00:00Z",
        )
        .unwrap();

        fs::create_dir_all(&dst).unwrap();
        let report = apply_backup_archive(&dst, &archive).unwrap();
        assert_eq!(report.app_version, "0.10.15");
        assert_eq!(report.pending_plugins, vec!["youtube".to_string()]);
        assert!(dst.join("settings.json").is_file());
        assert!(dst.join("chatgpt-chat.json").is_file());
        assert!(dst.join("plugins/weather/main.lua").is_file());
        assert!(dst.join("plugins-data/weather.json").is_file());
        assert!(dst.join("plugins/youtube/data/cookies.txt").is_file());
        assert!(dst.join("plugins/training/data/wake-profiles/abc.bin").is_file());
        // Native ELF binary and training audio .wav must NOT be included.
        assert!(!dst.join("plugins/youtube/youtube").exists());
        assert!(!dst.join("plugins/training/data/pos_001.wav").exists());
        // Embedded chatgpt state stripped from plugins.json.
        let pjson: Value = serde_json::from_slice(&fs::read(dst.join("plugins.json")).unwrap()).unwrap();
        assert!(pjson.get("chatgpt").is_none());
        assert_eq!(pjson["youtube"]["enabled"], true);

        let _ = fs::remove_dir_all(src);
        let _ = fs::remove_dir_all(dst);
    }

    #[test]
    fn restore_rejects_corrupt_checksum_and_traversal() {
        let src = std::env::temp_dir().join(format!("jarvis-bk-bad-{}", rand_hex(6)));
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("settings.json"), r#"{"active":"default"}"#).unwrap();
        let mut archive = build_backup_archive(
            &src,
            vec![],
            "0.10.15",
            "lite",
            "arm64",
            "2026-11-22T12:00:00Z",
        )
        .unwrap();
        // Flip one byte inside compressed zip stream -> parse/crc/sha must fail cleanly.
        let mid = archive.len() / 2;
        archive[mid] ^= 0xff;
        assert!(parse_and_verify_backup(&archive).is_err());

        // Reject manifest pointing outside allowlist.
        let mut bad_zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default();
        let payload = b"evil";
        let manifest = BackupManifest {
            format: FORMAT.into(),
            schema_version: 1,
            app_version: "0.10.15".into(),
            created_at: "2026-11-22T12:00:00Z".into(),
            model: "lite".into(),
            arch: "arm64".into(),
            files: vec![BackupFile {
                path: "../evil".into(),
                sha256: sha256_bytes(payload),
                size: payload.len() as u64,
            }],
            plugins: vec![],
        };
        bad_zip.start_file(MANIFEST_NAME, opts).unwrap();
        bad_zip.write_all(&serde_json::to_vec(&manifest).unwrap()).unwrap();
        bad_zip.start_file("data/../evil", opts).unwrap();
        bad_zip.write_all(payload).unwrap();
        let raw = bad_zip.finish().unwrap().into_inner();
        assert!(parse_and_verify_backup(&raw).is_err());

        let _ = fs::remove_dir_all(src);
    }
}
