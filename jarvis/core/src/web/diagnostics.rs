use super::{App, reply};
use axum::{
    body::Body,
    extract::{Query, State},
    http::{StatusCode, header},
    response::Response,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashMap},
    fmt::Write as _,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

pub async fn stats(State(app): State<App>, Query(query): Query<HashMap<String, String>>) -> Response {
    let seconds = query.get("range").and_then(|n| n.parse::<i64>().ok()).unwrap_or(900);
    reply(
        StatusCode::OK,
        json!({"points": app.telemetry.since(seconds), "every": crate::telemetry::EVERY}),
    )
}

fn log_path(data_dir: &Path, source: &str) -> PathBuf {
    match source {
        "alice" => Path::new("/data/quasar/daemons_logs/maind.log").to_path_buf(),
        "media" => Path::new("/data/quasar/daemons_logs/mediad.log").to_path_buf(),
        _ => {
            let standard = Path::new("/tmp/jarvisd/jarvisd.log");
            if standard.exists() {
                standard.to_path_buf()
            } else {
                data_dir.join("jarvisd.log")
            }
        }
    }
}

pub async fn logs(State(app): State<App>, Query(query): Query<HashMap<String, String>>) -> Response {
    let source = match query.get("src").map(String::as_str) {
        Some("alice") => "alice",
        Some("media") => "media",
        _ => "jarvis",
    };
    let path = log_path(&app.data_dir, source);
    let n = query
        .get("n")
        .and_then(|n| n.parse::<usize>().ok())
        .filter(|n| (1..=2000).contains(n))
        .unwrap_or(400);
    let filter = query.get("q").map(|q| q.trim().to_lowercase()).unwrap_or_default();
    if filter.len() > 4096 {
        return super::bad("Слишком длинный фильтр");
    }
    match tokio::task::spawn_blocking(move || tail(&path, n, &filter)).await {
        Ok(Ok(lines)) => reply(StatusCode::OK, json!({"src": source, "lines": lines})),
        _ => reply(StatusCode::OK, json!({"src": source, "lines": [], "error": "журнала пока нет"})),
    }
}

fn tail(path: &Path, n: usize, filter: &str) -> std::io::Result<Vec<String>> {
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let window = if filter.is_empty() {
        (n as u64 * 300).clamp(64 << 10, 1 << 20)
    } else {
        1 << 20
    };
    let offset = size.saturating_sub(window);
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(window).read_to_end(&mut bytes)?;
    if offset > 0 {
        if let Some(end) = bytes.iter().position(|b| *b == b'\n') {
            bytes.drain(..=end);
        } else {
            bytes.clear();
        }
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<String> = text
        .lines()
        .rev()
        .filter(|line| filter.is_empty() || line.to_lowercase().contains(filter))
        .take(n)
        .map(|line| {
            if line.len() <= 1200 {
                line.to_string()
            } else {
                let mut end = 1200;
                while !line.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}…", &line[..end])
            }
        })
        .collect();
    lines.reverse();
    Ok(lines)
}

fn push_secret(out: &mut BTreeSet<String>, value: &str) {
    let trimmed = value.trim();
    if trimmed.len() >= 6 {
        out.insert(trimmed.to_string());
    }
}

fn collect_secrets(s: &crate::settings::Settings) -> Vec<String> {
    let mut set = BTreeSet::new();
    push_secret(&mut set, &s.openai_key);
    push_secret(&mut set, &s.gemini_key);
    for key in &s.gemini_keys {
        push_secret(&mut set, key);
    }
    push_secret(&mut set, &s.grok_key);
    push_secret(&mut set, &s.elevenlabs_key);
    for entry in &s.eleven_keys {
        push_secret(&mut set, &entry.key);
    }
    push_secret(&mut set, &s.yandex_token);
    push_secret(&mut set, &s.vless);
    push_secret(&mut set, &s.password_hash);
    push_secret(&mut set, &s.session_key);

    if let Some(token) = s.home_assistant.get("token").and_then(Value::as_str) {
        push_secret(&mut set, token);
    }
    for field in ["refresh", "access", "id_token", "email"] {
        if let Some(val) = s.chatgpt.get(field).and_then(Value::as_str) {
            push_secret(&mut set, val);
        }
        for account in &s.chatgpt_accounts {
            if let Some(val) = account.auth.get(field).and_then(Value::as_str) {
                push_secret(&mut set, val);
            }
        }
    }
    for field in ["ssecurity", "service_token", "passToken"] {
        if let Some(val) = s.xiaomi.get(field).and_then(Value::as_str) {
            push_secret(&mut set, val);
        }
    }
    for field in ["refresh_token", "access_token", "secret"] {
        if let Some(val) = s.tuya.get(field).and_then(Value::as_str) {
            push_secret(&mut set, val);
        }
    }

    let mut list: Vec<String> = set.into_iter().collect();
    list.sort_by(|a, b| b.len().cmp(&a.len()));
    list
}

fn token_end(s: &str, start: usize) -> usize {
    for (idx, ch) in s[start..].char_indices() {
        if ch.is_whitespace() || matches!(ch, '&' | '"' | '\'' | ',' | ')' | ']' | '}' | '<' | '>') {
            return start + idx;
        }
    }
    s.len()
}

fn header_line_end(s: &str, start: usize) -> usize {
    for (idx, ch) in s[start..].char_indices() {
        if matches!(ch, '\r' | '\n' | '"' | '\'' | ',' | ')' | ']' | '}') {
            return start + idx;
        }
    }
    s.len()
}

/// Вырезает явные секреты и типовые шаблоны ключей/ссылок VPN/токенов из диагностического текста.
pub(crate) fn redact_secrets(text: &str, extra_secrets: &[String]) -> String {
    let mut out = text.to_string();
    for secret in extra_secrets {
        if secret.len() >= 6 && out.contains(secret.as_str()) {
            out = out.replace(secret.as_str(), "[REDACTED]");
        }
    }

    #[derive(Clone, Copy)]
    enum Kind {
        WholeToken,
        KeepPrefix,
        Jwt,
        AuthHeader,
        CookieHeader,
    }

    const PATTERNS: &[(&str, Kind)] = &[
        ("vless://", Kind::WholeToken),
        ("vmess://", Kind::WholeToken),
        ("trojan://", Kind::WholeToken),
        ("ss://", Kind::WholeToken),
        ("aiza", Kind::WholeToken),
        ("sk-", Kind::WholeToken),
        ("xai-", Kind::WholeToken),
        ("eyj", Kind::Jwt),
        ("bearer ", Kind::KeepPrefix),
        ("authorization:", Kind::AuthHeader),
        ("cookie:", Kind::CookieHeader),
        ("refresh_token=", Kind::KeepPrefix),
        ("access_token=", Kind::KeepPrefix),
        ("password=", Kind::KeepPrefix),
        ("secret=", Kind::KeepPrefix),
        ("token=", Kind::KeepPrefix),
        ("key=", Kind::KeepPrefix),
    ];

    let mut cursor = 0;
    while cursor < out.len() {
        let slice = &out[cursor..];
        let lower = slice.to_ascii_lowercase();
        let mut best: Option<(usize, usize, Kind)> = None;
        for &(pat, kind) in PATTERNS {
            if let Some(rel) = lower.find(pat) {
                let start = cursor + rel;
                if !out.is_char_boundary(start) || !out.is_char_boundary(start + pat.len()) {
                    continue;
                }
                if best.is_none_or(|(b_start, _, _)| start < b_start) {
                    best = Some((start, pat.len(), kind));
                }
            }
        }
        let Some((start, plen, kind)) = best else {
            break;
        };

        let after_pat = start + plen;
        let (repl_start, repl_end) = match kind {
            Kind::WholeToken => (start, token_end(&out, after_pat)),
            Kind::KeepPrefix => {
                let end = token_end(&out, after_pat);
                if end == after_pat {
                    cursor = after_pat;
                    continue;
                }
                (after_pat, end)
            }
            Kind::Jwt => {
                let end = token_end(&out, start);
                let tok = &out[start..end];
                if tok.len() >= 24 && tok.contains('.') {
                    (start, end)
                } else {
                    cursor = after_pat;
                    continue;
                }
            }
            Kind::AuthHeader => {
                let mut val_start = after_pat;
                while val_start < out.len() && matches!(out.as_bytes()[val_start], b' ' | b'\t') {
                    val_start += 1;
                }
                let end = {
                    let tail_lower = out[val_start..].to_ascii_lowercase();
                    if let Some(scheme_len) = ["bearer ", "basic ", "token "]
                        .iter()
                        .find(|p| tail_lower.starts_with(**p))
                        .map(|p| p.len())
                    {
                        token_end(&out, val_start + scheme_len)
                    } else {
                        token_end(&out, val_start)
                    }
                };
                if end <= val_start {
                    cursor = after_pat;
                    continue;
                }
                (val_start, end)
            }
            Kind::CookieHeader => {
                let mut val_start = after_pat;
                while val_start < out.len() && matches!(out.as_bytes()[val_start], b' ' | b'\t') {
                    val_start += 1;
                }
                let end = header_line_end(&out, val_start);
                if end <= val_start {
                    cursor = after_pat;
                    continue;
                }
                (val_start, end)
            }
        };

        out.replace_range(repl_start..repl_end, "[REDACTED]");
        cursor = repl_start + "[REDACTED]".len();
    }

    out
}

pub(crate) struct ReportData {
    pub timestamp: String,
    pub version: String,
    pub arch: String,
    pub model_name: String,
    pub model_id: String,
    pub uptime_s: u64,
    pub mic_online: bool,
    pub mic_why: String,
    pub leds_available: bool,
    pub wake_model_ready: bool,
    pub speaker_volume: i64,
    pub free_data_mb: u64,
    pub profiles_count: usize,
    pub active_engine: String,
    pub active_model: String,
    pub active_classic: bool,
    pub wake_enabled: bool,
    pub wake_sensitivity: i64,
    pub wake_check: bool,
    pub wake_interrupt: bool,
    pub wake_reply_enabled: bool,
    pub voice_error_alice: bool,
    pub mic_boost_db: i64,
    pub mic_gain_db: i64,
    pub follow_up_s: f64,
    pub train_mode: bool,
    pub train_voice_review: bool,
    pub lan_only: bool,
    pub home_via: String,
    pub radio_off: bool,
    pub gemini_keys_count: usize,
    pub eleven_keys_count: usize,
    pub has_openai: bool,
    pub has_grok: bool,
    pub has_chatgpt: bool,
    pub has_vless: bool,
    pub vless_error: String,
    pub vpn_enabled_services: Vec<String>,
    pub initial_audio: Value,
    pub plugins: Value,
    pub telemetry: Option<crate::telemetry::Point>,
    pub jarvis_log: Result<Vec<String>, String>,
    pub alice_log: Result<Vec<String>, String>,
    pub media_log: Result<Vec<String>, String>,
}

pub(crate) fn render_report(data: &ReportData, extra_secrets: &[String]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# YaJarvis diagnostic report");
    let _ = writeln!(out, "generated_at: {}", data.timestamp);
    let _ = writeln!(
        out,
        "note: секреты настроек не включены; API-ключи, VLESS-ссылки и токены автоматически заменены на [REDACTED]."
    );
    let _ = writeln!(out);

    let _ = writeln!(out, "[system]");
    let _ = writeln!(out, "version: {}", data.version);
    let _ = writeln!(out, "arch: {}", data.arch);
    let _ = writeln!(out, "model: {} ({})", data.model_name, data.model_id);
    let _ = writeln!(out, "uptime_s: {}", data.uptime_s);
    let _ = writeln!(out, "mic_online: {}", data.mic_online);
    if !data.mic_why.is_empty() {
        let _ = writeln!(out, "mic_why: {}", redact_secrets(&data.mic_why, extra_secrets));
    }
    let _ = writeln!(out, "leds_available: {}", data.leds_available);
    let _ = writeln!(out, "wake_model_ready: {}", data.wake_model_ready);
    let _ = writeln!(out, "speaker_volume: {}", data.speaker_volume);
    let _ = writeln!(out, "free_data_mb: {}", data.free_data_mb);
    let _ = writeln!(out);

    let _ = writeln!(out, "[settings]");
    let _ = writeln!(out, "profiles_count: {}", data.profiles_count);
    let _ = writeln!(out, "active_profile_engine: {}", data.active_engine);
    let _ = writeln!(out, "active_profile_model: {}", data.active_model);
    let _ = writeln!(out, "active_profile_classic: {}", data.active_classic);
    let _ = writeln!(out, "wake_enabled: {}", data.wake_enabled);
    let _ = writeln!(out, "wake_sensitivity: {}", data.wake_sensitivity);
    let _ = writeln!(out, "wake_check: {}", data.wake_check);
    let _ = writeln!(out, "wake_interrupt: {}", data.wake_interrupt);
    let _ = writeln!(out, "wake_reply_enabled: {}", data.wake_reply_enabled);
    let _ = writeln!(out, "voice_error_alice: {}", data.voice_error_alice);
    let _ = writeln!(out, "mic_boost_db: {}", data.mic_boost_db);
    let _ = writeln!(out, "mic_gain_db: {}", data.mic_gain_db);
    let _ = writeln!(out, "follow_up_s: {}", data.follow_up_s);
    let _ = writeln!(out, "train_mode: {}", data.train_mode);
    let _ = writeln!(out, "train_voice_review: {}", data.train_voice_review);
    let _ = writeln!(out, "lan_only: {}", data.lan_only);
    let _ = writeln!(out, "home_via: {}", data.home_via);
    let _ = writeln!(out, "radio_off: {}", data.radio_off);
    let _ = writeln!(out, "gemini_keys_count: {}", data.gemini_keys_count);
    let _ = writeln!(out, "eleven_keys_count: {}", data.eleven_keys_count);
    let _ = writeln!(out, "has_openai: {}", data.has_openai);
    let _ = writeln!(out, "has_grok: {}", data.has_grok);
    let _ = writeln!(out, "has_chatgpt: {}", data.has_chatgpt);
    let _ = writeln!(out, "has_vless: {}", data.has_vless);
    if !data.vless_error.is_empty() {
        let _ = writeln!(
            out,
            "vless_error: {}",
            redact_secrets(&data.vless_error, extra_secrets)
        );
    }
    let _ = writeln!(
        out,
        "vpn_enabled_services: {}",
        if data.vpn_enabled_services.is_empty() {
            "(none)".to_string()
        } else {
            data.vpn_enabled_services.join(", ")
        }
    );
    let _ = writeln!(out);

    let _ = writeln!(out, "[initial_audio]");
    let _ = writeln!(
        out,
        "{}",
        redact_secrets(
            &serde_json::to_string(&data.initial_audio).unwrap_or_else(|_| "null".into()),
            extra_secrets
        )
    );
    let _ = writeln!(out);

    let _ = writeln!(out, "[plugins]");
    let _ = writeln!(
        out,
        "{}",
        redact_secrets(
            &serde_json::to_string(&data.plugins).unwrap_or_else(|_| "null".into()),
            extra_secrets
        )
    );
    let _ = writeln!(out);

    let _ = writeln!(out, "[telemetry]");
    match &data.telemetry {
        Some(point) => {
            let _ = writeln!(
                out,
                "{}",
                serde_json::to_string(point).unwrap_or_else(|_| "null".into())
            );
        }
        None => {
            let _ = writeln!(out, "(данных телеметрии пока нет)");
        }
    }
    let _ = writeln!(out);

    for (title, lines) in [
        ("[jarvisd.log]", &data.jarvis_log),
        ("[maind.log]", &data.alice_log),
        ("[mediad.log]", &data.media_log),
    ] {
        let _ = writeln!(out, "{title}");
        match lines {
            Ok(items) if !items.is_empty() => {
                for line in items {
                    let _ = writeln!(out, "{}", redact_secrets(line, extra_secrets));
                }
            }
            Ok(_) => {
                let _ = writeln!(out, "(журнал пуст)");
            }
            Err(note) => {
                let _ = writeln!(out, "{note}");
            }
        }
        let _ = writeln!(out);
    }

    out
}

pub async fn report(State(app): State<App>) -> Response {
    let s = app.store.get();
    let extra_secrets = collect_secrets(&s);
    let model = crate::hardware::station_model();
    let active = s.active_profile();
    let active_model = if !active.live_model.is_empty() {
        active.live_model.clone()
    } else if !active.llm_model.is_empty() {
        active.llm_model.clone()
    } else {
        active.backend_model.clone()
    };
    let (_, vless_error) = app.proxy.status();
    let vpn_enabled_services = super::state::NET_SERVICES
        .iter()
        .filter(|(id, _, _)| super::state::use_vpn(&s, id))
        .map(|(id, _, _)| (*id).to_string())
        .collect();

    let data_dir = app.data_dir.clone();
    let (speaker_volume, free_data_mb, jarvis_log, alice_log, media_log) =
        tokio::task::spawn_blocking(move || {
            let vol = super::vol();
            let free = crate::plugins::install::free_mb(&data_dir);
            let read_tail = |src: &str, max: usize| {
                tail(&log_path(&data_dir, src), max, "").map_err(|_| "(журнала нет)".to_string())
            };
            (
                vol,
                free,
                read_tail("jarvis", 400),
                read_tail("alice", 100),
                read_tail("media", 100),
            )
        })
        .await
        .unwrap_or((-1, 0, Err("(ошибка чтения)".into()), Err("(ошибка чтения)".into()), Err("(ошибка чтения)".into())));

    let payload = ReportData {
        timestamp: jiff::Timestamp::now().to_string(),
        version: crate::version().to_string(),
        arch: jarvis_station::arch().to_string(),
        model_name: model.name.to_string(),
        model_id: model.id.to_string(),
        uptime_s: app.started.elapsed().as_secs(),
        mic_online: app.engine.mic.online(),
        mic_why: app.engine.mic.why(),
        leds_available: app.engine.leds.available(),
        wake_model_ready: app
            .engine
            .listener
            .ready
            .load(std::sync::atomic::Ordering::Relaxed),
        speaker_volume,
        free_data_mb,
        profiles_count: s.profiles.len(),
        active_engine: active.engine.clone(),
        active_model,
        active_classic: active.is_classic(),
        wake_enabled: s.wake_enabled,
        wake_sensitivity: s.wake_sensitivity,
        wake_check: s.wake_check,
        wake_interrupt: s.wake_interrupt,
        wake_reply_enabled: s.wake_reply_enabled,
        voice_error_alice: s.voice_error_alice,
        mic_boost_db: s.mic_boost_db,
        mic_gain_db: s.mic_gain_db,
        follow_up_s: s.follow_up_s,
        train_mode: s.train_mode,
        train_voice_review: s.train_voice_review,
        lan_only: s.lan_only,
        home_via: s.home_via.clone(),
        radio_off: s.radio_off,
        gemini_keys_count: crate::gemini_pool::effective_keys(&s.gemini_keys, &s.gemini_key).len(),
        eleven_keys_count: s.eleven_keys.len(),
        has_openai: !s.openai_key.trim().is_empty(),
        has_grok: !s.grok_key.trim().is_empty(),
        has_chatgpt: crate::settings::has_all(&s.chatgpt, &["refresh", "account"]),
        has_vless: !s.vless.trim().is_empty(),
        vless_error,
        vpn_enabled_services,
        initial_audio: app.engine.initial_audio_diag(),
        plugins: app.plugins.summary(),
        telemetry: app.telemetry.since(300).into_iter().last(),
        jarvis_log,
        alice_log,
        media_log,
    };

    let body = render_report(&payload, &extra_secrets).into_bytes();
    let safe_model: String = model
        .id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') { c } else { '-' })
        .collect();
    let date = s.now().strftime("%Y-%m-%d").to_string();
    let disp = format!("attachment; filename=\"yajarvis-diag-{safe_model}-{date}.txt\"");

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(header::CONTENT_DISPOSITION, disp)
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::CONTENT_LENGTH, body.len())
        .body(Body::from(body))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_tail_filters_caps_bytes_and_preserves_utf8() {
        let path = std::env::temp_dir().join(format!("jarvis-logs-{}", crate::settings::rand_hex(8)));
        std::fs::write(&path, format!("old\nОшибка первая\nOK\nошибка {}\n", "я".repeat(900))).unwrap();
        let lines = tail(&path, 2, "ошибка").unwrap();
        assert_eq!(lines[0], "Ошибка первая");
        assert!(lines[1].ends_with('…'));
        assert!(lines[1].len() <= 1203);
        assert_eq!(tail(&path, 1, "").unwrap().len(), 1);
        std::fs::write(&path, format!("{}\nlast\n", "x".repeat(2 << 20))).unwrap();
        assert_eq!(tail(&path, 400, "").unwrap(), vec!["last"]);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn redact_secrets_strips_explicit_keys_urls_jwt_and_patterns() {
        let mut s = crate::settings::Settings::default();
        s.gemini_key = "AIzaSyExampleKey123456".into();
        s.vless = "vless://uuid-1111@vpn.example.com:443?type=tcp&security=reality#Home".into();
        s.password_hash = "$2b$12$verysecrethashvalue".into();
        s.home_assistant = json!({"url": "http://192.168.1.10:8123", "token": "ha-long-token-987654"});
        s.chatgpt = json!({"email": "user@example.com", "refresh": "rt_secret_refresh_123"});
        s.chatgpt_accounts.push(crate::chatgpt_accounts::Account {
            id: "acc2".into(),
            auth: json!({
                "email": "backup@example.com",
                "refresh": "rt_inactive_secret_456",
                "access": "at_inactive_789",
                "id_token": "idt_inactive_000"
            }),
            live: Default::default(),
            status: String::new(),
            retry_after: 0,
            services: Default::default(),
        });
        let extras = collect_secrets(&s);

        let sample = concat!(
            "ошибка подключения https://x?key=AIzaSyAnother999&a=1 ",
            "link=vless://uuid-1111@vpn.example.com:443?type=tcp&security=reality#Home ",
            "auth Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.sig ",
            "Cookie: sid=topsecret; theme=dark\n",
            "openai sk-proj-secret123 grok xai-secret999 ",
            "ha=ha-long-token-987654 email=user@example.com ",
            "inactive=backup@example.com tok=rt_inactive_secret_456 acc=at_inactive_789 idt=idt_inactive_000 ",
            "hash=$2b$12$verysecrethashvalue refresh_token=tok_xyz777"
        );
        let red = redact_secrets(sample, &extras);
        for forbidden in [
            "AIzaSyAnother999",
            "uuid-1111",
            "vpn.example.com",
            "eyJhbGciOiJIUzI1NiJ9",
            "topsecret",
            "sk-proj-secret123",
            "xai-secret999",
            "ha-long-token-987654",
            "user@example.com",
            "backup@example.com",
            "rt_inactive_secret_456",
            "at_inactive_789",
            "idt_inactive_000",
            "verysecrethashvalue",
            "tok_xyz777",
        ] {
            assert!(!red.contains(forbidden), "leaked secret `{forbidden}` in `{red}`");
        }
        assert!(red.contains("ошибка подключения"));
        assert!(red.contains("key=[REDACTED]"));
        assert!(red.contains("Authorization: [REDACTED]"));
        assert!(red.contains("Cookie: [REDACTED]"));
        assert!(red.contains("refresh_token=[REDACTED]"));
    }

    #[test]
    fn render_report_omits_secrets_and_includes_sections() {
        let mut s = crate::settings::Settings::default();
        s.openai_key = "sk-my-open-key-123456".into();
        let extras = collect_secrets(&s);

        let payload = ReportData {
            timestamp: "2026-11-26T09:50:00Z".into(),
            version: "0.10.23".into(),
            arch: "arm64".into(),
            model_name: "Станция Мини 2".into(),
            model_id: "yandexmini_2".into(),
            uptime_s: 42,
            mic_online: true,
            mic_why: String::new(),
            leds_available: true,
            wake_model_ready: true,
            speaker_volume: 5,
            free_data_mb: 120,
            profiles_count: 1,
            active_engine: "gemini".into(),
            active_model: "gemini-3.8-live".into(),
            active_classic: false,
            wake_enabled: true,
            wake_sensitivity: 5,
            wake_check: false,
            wake_interrupt: true,
            wake_reply_enabled: true,
            voice_error_alice: true,
            mic_boost_db: 0,
            mic_gain_db: 0,
            follow_up_s: 6.0,
            train_mode: false,
            train_voice_review: false,
            lan_only: true,
            home_via: "alice".into(),
            radio_off: false,
            gemini_keys_count: 1,
            eleven_keys_count: 0,
            has_openai: true,
            has_grok: false,
            has_chatgpt: false,
            has_vless: false,
            vless_error: String::new(),
            vpn_enabled_services: vec!["gemini".into()],
            initial_audio: json!({"status": "ok"}),
            plugins: json!({"installed": ["vpn"], "enabled": ["vpn"]}),
            telemetry: None,
            jarvis_log: Ok(vec!["line1 bad key sk-my-open-key-123456".into()]),
            alice_log: Err("(журнала нет)".into()),
            media_log: Ok(vec![]),
        };
        let report_text = render_report(&payload, &extras);
        assert!(report_text.contains("[system]"));
        assert!(report_text.contains("[settings]"));
        assert!(report_text.contains("[initial_audio]"));
        assert!(report_text.contains("[plugins]"));
        assert!(report_text.contains("[telemetry]"));
        assert!(report_text.contains("[jarvisd.log]"));
        assert!(report_text.contains("[maind.log]"));
        assert!(report_text.contains("[mediad.log]"));
        assert!(!report_text.contains("sk-my-open-key-123456"));
        assert!(report_text.contains("line1 bad key [REDACTED]"));
    }
}
