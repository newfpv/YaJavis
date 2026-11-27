//! Веб-настройки на самой колонке (http://<ip колонки>:8080). Та же панель и тот же API, что у Go-версии.
//! Защита:
//!   - пароль (bcrypt), сессия — подписанная cookie (HttpOnly, SameSite=Strict), 30 дней;
//!   - перебор пароля: после 5 ошибок с адреса — пауза, растущая до 15 минут;
//!   - только домашняя сеть (по умолчанию): запросы не из локальных адресов отклоняются;
//!   - изменения только с заголовком X-Jarvis (форма с чужого сайта так не сможет);
//!   - ключи и ссылка VLESS обратно в браузер не отдаются — только «задан, …1234»;
//!   - строгий CSP, запрет встраивания во фреймы.

mod auth;
pub(crate) mod backup;
mod chatgpt;
mod config;
mod diagnostics;
mod device;
mod wake_profiles;
mod training;
mod homeassistant;
mod home_vendor;
mod keys;
mod memory;
mod music;
mod plugins;
pub mod state;
mod talk;
mod ui;
mod update;
mod vpn;

use crate::{plugins::Host, settings::Store, vpn::Proxy};
use axum::{
    Json, Router,
    body::Body,
    extract::{ConnectInfo, DefaultBodyLimit, Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{any, delete, get, post, put},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};

pub struct Web {
    pub store: Store,
    pub plugins: Arc<Host>,
    pub proxy: Arc<Proxy>,
    pub engine: Arc<crate::engine::Engine>,
    pub data_dir: PathBuf,
    pub started: Instant,
    pub telemetry: Arc<crate::telemetry::Telemetry>,
    pub updater: crate::updates::Updater,
    wake_update:Arc<tokio::sync::Mutex<()>>,
    fails: Mutex<HashMap<IpAddr, auth::Fail>>,
    reset_at: Mutex<Option<Instant>>,
}

pub type App = Arc<Web>;

impl Web {
    pub fn new(store: Store, plugins: Arc<Host>, proxy: Arc<Proxy>, data_dir: PathBuf, engine: Arc<crate::engine::Engine>) -> App {
        let telemetry=crate::telemetry::Telemetry::start(data_dir.clone());
        let updater = crate::updates::Updater::new(data_dir.clone(), store.clone(), plugins.clone());
        Arc::new(Web { store, plugins, proxy, engine, data_dir, telemetry, updater, wake_update:Arc::default(), started: Instant::now(), fails: Mutex::default(), reset_at: Mutex::default() })
    }
}

/// Пароль снят кнопками колонки: все входы недействительны, новый можно задать в течение 30 минут.
pub fn buttons_reset(app: &App) {
    auth::reset_password(app);
    log::info!("веб-настройки: пароль сброшен кнопками колонки");
    let e = app.engine.clone();
    std::thread::spawn(move || {
        e.leds.show(crate::led::Mode::Thinking, "#00b4ff", 70, std::time::Duration::from_secs(3));
        e.system_notice(
            &crate::buttons::reset_chime(),
            "Пароль веб-настроек сброшен. Откройте настройки и задайте новый в течение получаса.",
            "Повтори за мной: пароль Джарвиса сброшен, задайте новый в настройках",
        );
    });
}

/// Громкость колонки для панели (-1 — неизвестна).
pub fn vol() -> i64 {
    crate::speakervol::speaker_volume().unwrap_or(-1)
}

/// Ответ JSON с кодом.
pub fn reply(code: StatusCode, v: Value) -> Response {
    (code, Json(v)).into_response()
}

pub fn bad(msg: impl Into<String>) -> Response {
    reply(StatusCode::BAD_REQUEST, json!({"error": msg.into()}))
}

pub fn ok() -> Response {
    reply(StatusCode::OK, json!({"ok": true}))
}

pub fn router(app: App) -> Router {
    let training = Router::new()
        .merge(training::routes())
        .route("/api/train/wake/capture", post(wake_profiles::capture))
        .route("/api/train/wake/train", post(wake_profiles::train).layer(DefaultBodyLimit::max(crate::training_io::BODY_BYTES)))
        .route("/api/train/wake/test/{id}", post(wake_profiles::test))
        .route("/api/train/wake/profiles", get(wake_profiles::list).put(wake_profiles::upload).layer(DefaultBodyLimit::max(crate::wake_profiles::MAX_MODEL)))
        .route("/api/train/wake/active", put(wake_profiles::select))
        .route("/api/train/wake/profiles/{id}", delete(wake_profiles::remove))
        .route("/api/train/model", put(device::model).layer(DefaultBodyLimit::max(4<<20)))
        .route("/api/train/model/restore", post(device::restore))
        .route_layer(middleware::from_fn_with_state(app.clone(), wake_profiles::require_plugin));
    let api = Router::new().merge(training)
        .route("/api/state", get(state::state))
        .route("/api/update", get(update::get).put(update::put))
        .route("/api/update/check", post(update::check))
        .route("/api/update/install", post(update::install))
        .route("/api/update/upload", put(update::upload).layer(DefaultBodyLimit::max(jarvis_station::MAX_BINARY as usize)))
        .route("/api/stats", get(diagnostics::stats))
        .route("/api/logs", get(diagnostics::logs))
        .route("/api/logs/report", get(diagnostics::report))
        .route("/api/leds/preview", post(device::leds))
        .route("/api/preview", post(talk::preview))
        .route("/api/preview/status", get(talk::preview_status))
        .route("/api/preview/cancel", post(talk::preview_cancel))
        .route("/api/wake-replies", get(talk::wake_replies).delete(talk::wake_replies_clear))
        .route("/api/wake-replies/generate", post(talk::wake_replies_generate))
        .route("/api/wake-replies/cancel", post(talk::wake_replies_cancel))
        .route("/api/password", put(auth::change_password))
        .route("/api/settings", put(config::settings))
        .route("/api/keys", put(keys::put))
        .route("/api/gemini-keys", put(keys::gemini_pool))
        .route("/api/eleven-keys", put(keys::eleven_pool))
        .route("/api/elevenlabs/voices", get(keys::eleven_voices))
        .route("/api/tts/voices", get(keys::tts_voices))
        .route("/api/voices", post(keys::create_voice).delete(keys::delete_voice))
        .route("/api/profile", put(config::profile).delete(config::delete_profile))
        .route("/api/active", post(config::active))
        .route("/api/profiles/order", post(config::order))
        .route("/api/prompt", put(config::prompt).delete(config::delete_prompt))
        .route("/api/home-assistant", get(homeassistant::get).put(homeassistant::put).delete(homeassistant::delete))
        .route("/api/home-assistant/rooms/discover", post(homeassistant::rooms))
        .route("/api/xiaomi/login", post(home_vendor::xiaomi_login))
        .route("/api/xiaomi/qr", get(home_vendor::xiaomi_qr))
        .route("/api/xiaomi/refresh", post(home_vendor::xiaomi_refresh))
        .route("/api/xiaomi", axum::routing::delete(home_vendor::xiaomi_logout))
        .route("/api/smartlife/login", post(home_vendor::smartlife_login))
        .route("/api/smartlife/qr", get(home_vendor::smartlife_qr))
        .route("/api/smartlife/refresh", post(home_vendor::smartlife_refresh))
        .route("/api/smartlife", axum::routing::delete(home_vendor::smartlife_logout))
        .route("/api/plugins", get(plugins::list))
        .route("/api/plugins/catalog", get(plugins::catalog))
        .route("/api/plugins/install", post(plugins::install))
        .route("/api/plugins/update-all", post(plugins::update_all))
        .route("/api/plugins/upload", post(plugins::upload).layer(DefaultBodyLimit::max(crate::plugins::install::ZIP_MAX + 1)))
        .route("/api/plugins/warned", post(plugins::warned))
        .route("/api/plugins/{id}/enable", post(plugins::enable))
        .route("/api/plugins/{id}/settings", post(plugins::settings))
        .route("/api/plugins/{id}", delete(plugins::remove))
        .route("/api/onboarded", post(plugins::onboarded))
        .route("/api/talk", post(talk::talk))
        .route("/api/say", post(talk::say))
        .route("/api/chatgpt/login", post(chatgpt::login))
        .route("/api/chatgpt/accounts", post(chatgpt::accounts))
        .route("/api/chatgpt/usage", post(chatgpt::usage))
        .route("/api/chatgpt/live-stats", get(chatgpt::live_stats))
        .route("/api/chatgpt", delete(chatgpt::logout))
        .route("/api/backup", get(backup::download))
        .route("/api/backup/restore", post(backup::restore).layer(DefaultBodyLimit::max(jarvis_station::backup::MAX_ZIP + 1)))
        .route("/api/stop", post(talk::stop))
        .route("/api/journal", get(talk::journal))
        .route("/api/speaker/volume", post(talk::volume))
        .route("/api/music", get(music::get).post(music::post))
        .route("/api/youtube", put(music::set_cookies).delete(music::clear_cookies))
        .route("/api/youtube/login", post(music::youtube_login).delete(music::youtube_logout))
        .route("/api/youtube/library", get(music::youtube_library))
        .route("/api/youtube/playlist/{id}", get(music::youtube_playlist))
        .route("/api/youtube/like", post(music::youtube_like))
        .route("/api/youtube/search", get(music::youtube_search))
        .route("/api/vless", put(vpn::put))
        .route("/api/test/net", post(vpn::test_net))
        .route("/api/memory", get(memory::get).put(memory::put))
        .route("/api/memory/month", put(memory::month))
        .route("/api/memory/archive", get(memory::archive))
        .route("/api/memory/forget", post(memory::forget))
        .route("/api/memory/compact", post(memory::compact))
        .route("/api/chatgpt/newchat", post(memory::new_chat))
        .route_layer(middleware::from_fn_with_state(app.clone(), auth::require));
    Router::new()
        .route("/healthz", get(|State(app): State<App>| async move {
            Json(json!({
                "ready": true,
                "version": crate::version(),
                "mic": app.engine.mic.online(),
                "modules": crate::plugins::compiled_modules(),
                "boot_id": crate::plugins::install::boot_id(),
            }))
        }))
        .route("/api/login", post(auth::login))
        .route("/api/logout", post(auth::logout))
        .route("/api/setup", post(auth::setup))
        .merge(api)
        .route("/api/{*rest}", any(not_ported))
        .fallback(get(ui::serve))
        .layer(DefaultBodyLimit::max(256 << 10))
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
}

async fn not_ported(req: Request) -> Response {
    log::debug!("веб: неизвестный маршрут {} {}", req.method(), req.uri().path());
    reply(StatusCode::NOT_FOUND, json!({"error": "Неизвестный API"}))
}

fn local_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_loopback() || v.is_private() || v.is_link_local(),
        IpAddr::V6(v) => {
            if let Some(v4) = v.to_ipv4_mapped() {
                return local_ip(IpAddr::V4(v4));
            }
            v.is_loopback() || (v.segments()[0] & 0xfe00) == 0xfc00 || (v.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Общие правила для всех запросов.
async fn guard(State(app): State<App>, ConnectInfo(peer): ConnectInfo<SocketAddr>, req: Request, next: Next) -> Response {
    if app.store.with(|s| s.lan_only) && !local_ip(peer.ip()) {
        return (StatusCode::FORBIDDEN, "Доступ только из домашней сети").into_response();
    }
    if req.method() != Method::GET && req.headers().get("x-jarvis").is_none_or(|v| v != "1") {
        return (StatusCode::FORBIDDEN, "нужен заголовок X-Jarvis").into_response();
    }
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    let set = |h: &mut axum::http::HeaderMap, k: &'static str, v: &'static str| {
        h.insert(k, HeaderValue::from_static(v));
    };
    set(h, "content-security-policy", "default-src 'self'; img-src 'self' data: https://i.ytimg.com https://i9.ytimg.com https://yt3.ggpht.com https://lh3.googleusercontent.com https://*.ytimg.com https://*.ggpht.com https://*.googleusercontent.com https://avatars.yandex.net; media-src 'self' blob:; style-src 'self'; script-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'");
    set(h, "x-frame-options", "DENY");
    set(h, "x-content-type-options", "nosniff");
    set(h, "referrer-policy", "no-referrer");
    if !h.contains_key(header::CACHE_CONTROL) {
        set(h, "cache-control", "no-store");
    }
    resp
}

const FALLBACK_PORTS: &[u16] = &[8088, 8081, 8090];

fn normalize_bind(addr: &str) -> String {
    if addr.starts_with(':') { format!("0.0.0.0{addr}") } else { addr.to_string() }
}

async fn bind_listener(addr: &str, explicit: bool) -> std::io::Result<(tokio::net::TcpListener, u16, bool)> {
    bind_listener_for(addr, explicit, crate::hardware::station_model().id).await
}

async fn bind_listener_for(addr: &str, explicit: bool, model_id: &str) -> std::io::Result<(tokio::net::TcpListener, u16, bool)> {
    let primary = normalize_bind(addr);
    let host = primary.rsplit_once(':').map(|(h, _)| h).unwrap_or("0.0.0.0");
    // На Станции Стрит порт 8080 занимает штатный maind --platformd; при загрузке S98jarvisd стартует раньше
    // S99quasar, поэтому без этого правила порт прыгает с 8088 на 8080 и перехватывает порт у системы (#37).
    if !explicit && model_id == "street" && primary.ends_with(":8080") {
        for &port in FALLBACK_PORTS {
            if let Ok(listener) = tokio::net::TcpListener::bind(format!("{host}:{port}")).await {
                return Ok((listener, port, true));
            }
        }
    }
    match tokio::net::TcpListener::bind(&primary).await {
        Ok(listener) => {
            let port = listener.local_addr()?.port();
            Ok((listener, port, false))
        }
        Err(err) if !explicit && err.kind() == std::io::ErrorKind::AddrInUse => {
            for &port in FALLBACK_PORTS {
                if let Ok(listener) = tokio::net::TcpListener::bind(format!("{host}:{port}")).await {
                    return Ok((listener, port, true));
                }
            }
            Err(err)
        }
        Err(err) => Err(err),
    }
}

pub async fn serve(app: App, addr: &str, explicit: bool) -> std::io::Result<()> {
    let (listener, port, fallback) = bind_listener(addr, explicit).await?;
    if let Some(ready) = std::env::var_os("JARVIS_READY_PATH") {
        let ready_path = std::path::Path::new(&ready);
        if let Some(dir) = ready_path.parent() {
            let _ = jarvis_station::atomic(&dir.join("port"), port.to_string().as_bytes());
        }
        if let Ok(module) = std::env::var("JARVIS_INSTALL_MODULE") {
            app.plugins.install_module(&module).await.map_err(std::io::Error::other)?;
        }
        jarvis_station::atomic(ready_path, format!("{} {}", std::process::id(), crate::version()).as_bytes())?;
    }
    update::automatic(app.clone());
    if fallback {
        log::warn!("веб-настройки: порт 8080 занят другим процессом, панель открыта на http://<ip колонки>:{port}");
    } else {
        log::info!("веб-настройки: http://<ip колонки>:{port}");
    }
    axum::serve(listener, router(app).into_make_service_with_connect_info::<SocketAddr>()).await
}

pub fn empty(code: StatusCode) -> Response {
    Response::builder().status(code).body(Body::empty()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bind_listener_falls_back_when_primary_port_is_busy() {
        let busy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let busy_port = busy.local_addr().unwrap().port();
        let addr = format!("127.0.0.1:{busy_port}");
        // Явно заданный порт не подменяется.
        assert!(bind_listener(&addr, true).await.is_err());
        // Автовыбор при занятом порте уходит на запасной свободный порт.
        let (listener, bound_port, fallback) = bind_listener(&addr, false).await.unwrap();
        assert!(fallback);
        assert!(FALLBACK_PORTS.contains(&bound_port));
        drop(listener);
        drop(busy);

        // На Станции Стрит порт 8080 пропускается заранее, чтобы не перехватывать его у maind --platformd при загрузке.
        let (street_listener, street_port, street_fallback) = bind_listener_for("127.0.0.1:8080", false, "street").await.unwrap();
        assert!(street_fallback);
        assert!(FALLBACK_PORTS.contains(&street_port));
        drop(street_listener);
    }
}
