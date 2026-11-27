//! Плагины. Лежат в <data>/plugins/<id>/, включённость и настройки — в <data>/plugins.json (0600).
//! Модули Rust включаются при сборке, Lua исполняется в песочнице внутри ядра.

pub mod catalog;
pub mod coresvc;
pub mod install;
mod builtin;
mod process;
mod official;
pub(crate) mod modules;
#[cfg(feature = "native-plugins")]
mod native_addon;
#[cfg(feature = "lua")]
mod lua;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pub const PLUGIN_API: i64 = 1;

pub fn compiled_modules() -> Vec<String> { modules::manifests().into_iter().map(|m|m.id).collect() }

pub fn command_text(text: &str) -> String {
    let normalized = crate::fastcmd::norm_text(text);
    normalized.strip_prefix("джарвис ").or_else(|| normalized.strip_prefix("jarvis "))
        .unwrap_or(&normalized).trim().into()
}

pub fn valid_id(id: &str) -> bool {
    let b = id.as_bytes();
    (2..=32).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

pub const CATEGORIES: &[(&str, &str, &str)] = &[
    ("assistant", "Ассистенты и голоса", "голосовые движки, нейросети, голоса"),
    ("home", "Умный дом", "свет, розетки, датчики, сценарии"),
    ("media", "Музыка и видео", "музыка, радио, подкасты, видео"),
    ("info", "Погода и справки", "погода, курсы, новости, транспорт"),
    ("productivity", "Дела и заметки", "списки, напоминания, календари, почта"),
    ("dev", "Кодинг и IT", "серверы, GitHub, мониторинг, сеть"),
    ("fun", "Игры и развлечения", "игры, шутки, викторины, случайности"),
    ("system", "Колонка и система", "подсветка, кнопки, слово активации, VPN"),
    ("other", "Другое", "всё, что не подошло выше"),
];

pub fn valid_category(id: &str) -> bool {
    CATEGORIES.iter().any(|c| c.0 == id)
}

pub fn categories_json() -> Value {
    CATEGORIES.iter().map(|c| json!({"id": c.0, "name": c.1, "hint": c.2})).collect()
}

/// Инструменты ядра: плагин не может их подменить.
pub const CORE_TOOLS: &[&str] = &[
    "smart_home", "play_music", "music_control", "alice_command", "recall_conversation", "edit_memory",
    "switch_profile", "review_current_wake",
    "home_assistant", "xiaomi_home", "smart_life",
];

pub fn valid_tool_name(n: &str) -> bool {
    (1..=64).contains(&n.len()) && n.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-') && !CORE_TOOLS.contains(&n)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Setting {
    pub key: String,
    pub label: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub hint: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Page {
    pub id: String,
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub icon: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub group: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub author: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub homepage: String,
    pub kind: String,
    pub main: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub arch: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub api: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub icon: String,
    pub category: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub provides: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub settings: Vec<Setting>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub min_core: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub reboot_on_update: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pages: Vec<Page>,
    /// Права Lua-плагина (что ему можно): network, say, tools, commands…
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub permissions: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub network_allow: Vec<String>,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

impl Manifest {
    pub fn provides(&self, cap: &str) -> bool {
        self.provides.iter().any(|c| c == cap)
    }

    pub fn validate(&self) -> Result<(), String> {
        let integ = ["homeassistant", "xiaomi", "smartlife", "yandexhome"];
        if !valid_id(&self.id) {
            return Err("id плагина: латиница в нижнем регистре, цифры и дефис, 2–32 символа".into());
        }
        if self.name.trim().is_empty() {
            return Err("у плагина нет name".into());
        }
        if !["lua", "module", "rust"].contains(&self.kind.as_str()) {
            return Err(if matches!(self.kind.as_str(), "js" | "native") {
                "Это пакет для старого ядра. Возьмите Lua-версию плагина или установите Rust-модуль из каталога"
            } else {
                "В plugin.json укажите kind: lua или kind: rust"
            }.into());
        }
        if !valid_category(&self.category) {
            let ids: Vec<_> = CATEGORIES.iter().map(|c| c.0).collect();
            return Err(format!("category плагина — одна из: {}", ids.join(", ")));
        }
        let official_rust_ok = self.kind == "rust"
            && (modules::official_rust(&self.id) || modules::official_lua_service(&self.id))
            && self.provides == [self.id.clone()];
        let official_lua_ok = self.kind == "lua"
            && modules::official_lua_service(&self.id)
            && self.provides == [self.id.clone()];
        if self.kind != "module"
            && (self.provides("vpn") || integ.iter().any(|c| self.provides(c)))
            && !(official_rust_ok || official_lua_ok)
        {
            return Err("Для этого подключения требуется официальный плагин".into());
        }
        if self.main.is_empty() || self.main.contains("..") || self.main.starts_with('/') || self.main.contains('\\') {
            return Err("main плагина — имя файла внутри архива".into());
        }
        if self.kind == "rust" && (!cfg!(target_os = "linux") || self.arch != arch()
            || self.api != PLUGIN_API || self.main.contains('/')
            || matches!(self.main.as_str(), "data" | "data.json" | "plugin.json" | ".")
            || !self.main.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)))
        {
            return Err("Для Rust-пакета нужны подходящая архитектура, api: 1 и имя бинарника в корне ZIP".into());
        }
        if self.kind == "module" && (self.id != self.main || self.provides != [self.main.clone()]) {
            return Err("Манифест встроенного модуля должен указывать его собственные id, main и provides".into());
        }
        if self.api > PLUGIN_API {
            return Err(format!("плагин написан под API {}, а этот Джарвис знает {PLUGIN_API} — обновите Джарвиса", self.api));
        }
        Ok(())
    }

    pub fn needs_newer_core(&self) -> bool {
        let v = crate::version();
        !self.min_core.is_empty() && v != "dev" && newer(&self.min_core, v)
    }
}

/// a новее b (0.9.10 > 0.9.9; хвост «-rs» и т.п. не считается).
pub fn newer(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.trim_start_matches('v').split(['-', '+']).next().unwrap_or("").split('.').map(|x| x.parse().unwrap_or(0)).collect()
    };
    let (a, b) = (parse(a), parse(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginState {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub removed: bool,
    pub enabled: bool,
    pub source: String,
    pub official: bool,
    pub installed_at: String,
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub settings: Map<String, Value>,
}

impl Default for PluginState {
    fn default() -> Self {
        PluginState { removed: false, enabled: true, source: "file".into(), official: false, installed_at: now_rfc3339(), settings: Map::new() }
    }
}

pub fn now_rfc3339() -> String {
    jiff::Timestamp::now().to_string()
}

pub type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Что ядро даёт плагинам.
pub struct Hooks {
    pub network: Box<dyn Fn() -> Value + Send + Sync>,
    /// Сервисы ядра для плагина: (id, method, params) → ответ.
    pub request: Box<dyn Fn(String, String, Value) -> BoxFut<Result<Value, String>> + Send + Sync>,
    /// Событие плагина; "exit" — процесс завершился.
    pub event: Box<dyn Fn(&Manifest, &str, Value) + Send + Sync>,
    pub say: Box<dyn Fn(String) + Send + Sync>,
    pub alice: Box<dyn Fn(String, bool) -> BoxFut<bool> + Send + Sync>,
    pub info: Box<dyn Fn() -> Value + Send + Sync>,
}

impl Default for Hooks {
    fn default() -> Self {
        Hooks {
            network: Box::new(|| json!({})),
            request: Box::new(|_, _, _| Box::pin(async { Err("метод ядра недоступен".to_string()) })),
            event: Box::new(|_, _, _| {}),
            say: Box::new(|_| {}),
            alice: Box::new(|_, _| Box::pin(async { false })),
            info: Box::new(|| json!({})),
        }
    }
}

#[derive(Clone)]
pub enum Runtime {
    Builtin(Arc<builtin::Builtin>),
    Official(Arc<official::Official>),
    Process(Arc<process::Process>),
    #[cfg(feature = "native-plugins")]
    Native(Arc<native_addon::Native>),
    #[cfg(feature = "lua")]
    Lua(Arc<lua::Script>),
}

impl Runtime {
    pub fn tools(&self) -> Vec<Value> {
        match self {
            Runtime::Builtin(_) | Runtime::Official(_) => vec![],
            Runtime::Process(n) => n.tools(),
            #[cfg(feature = "native-plugins")]
            Runtime::Native(n) => n.tools(),
            #[cfg(feature = "lua")]
            Runtime::Lua(n) => n.tools(),
        }
    }
    pub async fn call_tool(&self, name: &str, args: &str) -> Option<String> {
        match self {
            Runtime::Builtin(_) | Runtime::Official(_) => None,
            Runtime::Process(n) => n.call_tool(name, args).await,
            #[cfg(feature = "native-plugins")]
            Runtime::Native(n) => n.call_tool(name, args).await,
            #[cfg(feature = "lua")]
            Runtime::Lua(n) => n.call_tool(name, args).await,
        }
    }
    pub async fn command(&self, text: &str) -> Option<String> {
        match self {
            Runtime::Builtin(_) | Runtime::Official(_) => None,
            Runtime::Process(n) => n.request("command", command_text(text).into(), Duration::from_secs(2))
                .await.ok().and_then(|value| value.as_str().map(str::to_owned)),
            #[cfg(feature = "native-plugins")]
            Runtime::Native(n) => n.request("command", command_text(text).into(), Duration::from_secs(2))
                .await.ok().and_then(|value| value.as_str().map(str::to_owned)),
            #[cfg(feature = "lua")]
            Runtime::Lua(n) => n.command(text).await,
        }
    }
    pub fn configure(&self, settings: Map<String, Value>) {
        match self {
            Runtime::Builtin(_) | Runtime::Official(_) => {},
            Runtime::Process(n) => n.configure(settings),
            #[cfg(feature = "native-plugins")]
            Runtime::Native(n) => n.configure(settings),
            #[cfg(feature = "lua")]
            Runtime::Lua(n) => n.configure(settings),
        }
    }
    pub fn status(&self) -> String {
        match self {
            Runtime::Builtin(_) => "Встроен в ядро".into(),
            Runtime::Official(n) => if n.running() { "Официальный Rust-плагин".into() } else { "Процесс Rust-плагина завершился".into() },
            Runtime::Process(n) => if n.running() { "Rust-пакет".into() } else { "Процесс Rust-пакета завершился".into() },
            #[cfg(feature = "native-plugins")]
            Runtime::Native(_) => "Нативное расширение Rust".into(),
            #[cfg(feature = "lua")]
            Runtime::Lua(n) => n.status(),
        }
    }
    pub async fn stop(&self) {
        match self {
            Runtime::Builtin(b) => b.stop().await,
            Runtime::Official(n) => n.stop().await,
            Runtime::Process(n) => n.stop().await,
            #[cfg(feature = "native-plugins")]
            Runtime::Native(n) => n.stop(),
            #[cfg(feature = "lua")]
            Runtime::Lua(n) => n.stop().await,
        }
    }
    pub async fn request(&self, method:&str, params:Value, timeout:Duration)->Result<Value,String> {
        match self {
            Self::Builtin(b)=>b.request(method,params,timeout).await,
            Self::Official(n)=>n.request(method,params,timeout).await,
            Self::Process(n)=>n.request(method,params,timeout).await,
            #[cfg(feature = "native-plugins")]
            Self::Native(n)=>n.request(method,params,timeout).await,
            #[cfg(feature="lua")]
            Self::Lua(s) if s.supports_service() => s.service(method, params, timeout).await,
            #[cfg(feature="lua")]
            Self::Lua(_)=>Err("Lua-плагин не является провайдером подключения".into()),
        }
    }
    pub async fn notify(&self, method:&str, params:Value)->Result<(),String> {
        match self {
            _=>self.request(method,params,Duration::from_secs(3)).await.map(|_|()),
        }
    }

    fn supports_chat(&self) -> bool {
        match self {
            Runtime::Process(n) => n.supports_chat(),
            #[cfg(feature = "lua")]
            Runtime::Lua(n) => n.supports_chat(),
            #[cfg(feature = "native-plugins")]
            Runtime::Native(n) => n.supports_chat(),
            _ => false,
        }
    }

    fn supports_stt(&self) -> bool {
        match self {
            Runtime::Process(n) => n.supports_stt(),
            #[cfg(feature = "lua")]
            Runtime::Lua(n) => n.supports_stt(),
            #[cfg(feature = "native-plugins")]
            Runtime::Native(n) => n.supports_stt(),
            _ => false,
        }
    }

    fn supports_tts(&self) -> bool {
        match self {
            Runtime::Process(n) => n.supports_tts(),
            #[cfg(feature = "lua")]
            Runtime::Lua(n) => n.supports_tts(),
            #[cfg(feature = "native-plugins")]
            Runtime::Native(n) => n.supports_tts(),
            _ => false,
        }
    }

    fn running(&self) -> bool {
        match self {
            Runtime::Builtin(_) => true,
            Runtime::Process(n) => n.running(),
            Runtime::Official(n) => n.running(),
            #[cfg(feature = "lua")]
            Runtime::Lua(n) => n.running(),
            #[cfg(feature = "native-plugins")]
            _ => true,
        }
    }

    pub fn is_service(&self) -> bool {
        match self {
            Self::Builtin(_) => true,
            Self::Official(n) => n.running(),
            #[cfg(feature = "lua")]
            Self::Lua(s) => s.supports_service(),
            _ => false,
        }
    }
}

pub struct Plugin {
    pub manifest: Manifest,
    pub dir: PathBuf,
    pub rt: Option<Runtime>,
    pub err: String,
}

#[derive(Default)]
pub struct Inner {
    pub plugins: BTreeMap<String, Plugin>,
    pub state: BTreeMap<String, PluginState>,
}

pub struct Host {
    pub data_dir: PathBuf,
    pub hooks: Hooks,
    pub inner: Mutex<Inner>,
    install_lock: tokio::sync::Mutex<()>,
    pub catalog: catalog::Cache,
    wake_training_cached: AtomicBool,
    self_weak: std::sync::OnceLock<std::sync::Weak<Host>>,
    builtins: Mutex<BTreeMap<String, Arc<builtin::Builtin>>>,
}

pub fn arch() -> &'static str {
    if cfg!(target_arch = "arm") { "armv7" } else if cfg!(target_arch = "aarch64") { "arm64" } else { std::env::consts::ARCH }
}

impl Inner {
    pub fn state_mut(&mut self, id: &str) -> &mut PluginState {
        self.state.entry(id.to_string()).or_default()
    }

    /// Значения настроек плагина с умолчаниями из манифеста.
    pub fn settings_of(&mut self, id: &str) -> Map<String, Value> {
        let mut out = Map::new();
        if let Some(p) = self.plugins.get(id) {
            for s in &p.manifest.settings {
                if let Some(d) = &s.default {
                    out.insert(s.key.clone(), d.clone());
                }
            }
        }
        for (k, v) in &self.state_mut(id).settings {
            out.insert(k.clone(), v.clone());
        }
        out
    }
}

impl Host {
    pub async fn install_module(self:&Arc<Self>,id:&str)->Result<Manifest,String> {
        let raw=modules::archive(id)?;
        self.install(&raw,"core",true).await.map(|result| result.manifest)
    }
    pub fn new(data_dir: PathBuf, hooks: Hooks) -> Arc<Host> {
        let host = Arc::new(Host {
            data_dir,
            hooks,
            inner: Mutex::default(),
            install_lock: tokio::sync::Mutex::new(()),
            catalog: catalog::Cache::default(),
            wake_training_cached: AtomicBool::new(false),
            self_weak: std::sync::OnceLock::new(),
            builtins: Mutex::default(),
        });
        let _ = host.self_weak.set(Arc::downgrade(&host));
        host
    }

    fn builtin_provider(&self, id: &str) -> Result<Runtime, String> {
        if let Some(existing) = self.builtins.lock().unwrap().get(id).cloned() {
            return Ok(Runtime::Builtin(existing));
        }
        let weak = self.self_weak.get().cloned().ok_or("Ядро не инициализировано")?;
        let created = builtin::Builtin::start(id, weak)?;
        let mut builtins = self.builtins.lock().unwrap();
        let entry = builtins.entry(id.to_owned()).or_insert(created).clone();
        Ok(Runtime::Builtin(entry))
    }

    pub(crate) fn sync_wake_training(&self, g: &Inner) {
        let on = g.state.get("training").is_some_and(|s| s.enabled)
            && g.plugins.get("training").is_some_and(|p| {
                p.manifest.provides("training") && p.rt.as_ref().is_some_and(Runtime::is_service)
            });
        self.wake_training_cached.store(on, Ordering::Release);
    }

    pub(crate) fn refresh_wake_training(&self) {
        let g = self.inner.lock().unwrap();
        self.sync_wake_training(&g);
    }

    pub fn dir(&self) -> PathBuf {
        self.data_dir.join("plugins")
    }

    fn state_path(&self) -> PathBuf {
        self.data_dir.join("plugins.json")
    }

    pub fn save(&self, inner: &Inner) {
        if let Ok(b) = serde_json::to_vec_pretty(&inner.state) {
            let _ = crate::settings::write_atomic(&self.state_path(), &b, 0o600);
        }
    }

    pub fn official(&self, id: &str) -> bool {
        modules::is_embedded(id) || self.inner.lock().unwrap().state.get(id).is_some_and(|s| s.official)
    }

    /// Прочитать установленные плагины и запустить включённые.
    pub async fn load(self: &Arc<Self>) {
        let state: BTreeMap<String, Option<PluginState>> =
            std::fs::read(self.state_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let builtins = modules::manifests();
        let mut to_start = vec![];
        {
            let mut g = self.inner.lock().unwrap();
            for (id, st) in state {
                if let Some(st) = st {
                    g.state.insert(id, st);
                }
            }
            let mut dirs: Vec<_> = std::fs::read_dir(self.dir()).map(|r| r.flatten().collect()).unwrap_or_default();
            dirs.sort_by_key(|e| e.file_name());
            for e in dirs {
                let name = e.file_name().to_string_lossy().to_string();
                if !e.path().is_dir() || name.ends_with(".tmp") || name.ends_with(".old") {
                    continue;
                }
                let dir = e.path();
                if modules::is_embedded(&name) {
                    log::info!("плагины: {name} теперь встроен в ядро — удаляю старый пакет");
                    for path in [dir, self.dir().join(format!("{name}.tmp")), self.dir().join(format!("{name}.old"))] {
                        let _ = std::fs::remove_dir_all(path);
                    }
                    g.state.remove(&name);
                    continue;
                }
                let (mut m, broken) = match read_manifest(&dir) {
                    Ok(m) if m.id == name => (m, String::new()),
                    res => {
                        let why = match &res {
                            Ok(m) => format!("id в plugin.json ({}) не совпадает с папкой", m.id),
                            Err(e) => e.clone(),
                        };
                        if !valid_id(&name) {
                            log::info!("плагины: папка {name} пропущена: {why}");
                            continue;
                        }
                        log::info!("плагины: {name}: плагин повреждён: {why}");
                        let old = res.ok().unwrap_or_default();
                        let nm = if old.name.trim().is_empty() { name.clone() } else { old.name };
                        (
                            Manifest { id: name.clone(), name: nm, version: old.version, description: old.description, kind: old.kind, category: "other".into(), ..Default::default() },
                            format!("плагин повреждён: {why}"),
                        )
                    }
                };
                let builtin = if broken.is_empty() && m.kind == "module" {
                    builtins.iter().find(|manifest| manifest.id == m.id)
                } else { None };
                let builtin_state = builtin.is_some();
                if let Some(manifest) = builtin { m = manifest.clone(); }
                let later = broken.is_empty() && m.needs_newer_core();
                let err = if later { format!("заработает после обновления Джарвиса до {}", m.min_core) } else { broken.clone() };
                let state = g.state_mut(&m.id);
                if builtin_state {
                    state.source = "core".into();
                    state.official = true;
                }
                let enabled = state.enabled;
                if enabled && !later && broken.is_empty() {
                    to_start.push(m.id.clone());
                }
                g.plugins.insert(m.id.clone(), Plugin { manifest: m, dir, rt: None, err });
            }
            let pending_restore: std::collections::BTreeSet<String> = std::fs::read(
                self.data_dir.join(jarvis_station::backup::RESTORE_PLUGINS_FILE),
            )
            .ok()
            .and_then(|b| serde_json::from_slice::<Vec<jarvis_station::backup::BackupPlugin>>(&b).ok())
            .into_iter()
            .flatten()
            .map(|p| p.id)
            .collect();
            let ids: Vec<String> = g.state.keys().cloned().collect();
            for id in ids {
                // папку плагина удалили — забыть (кроме отметки «удалён руками» и отложенного восстановления бэкапа)
                if modules::is_embedded(&id)
                    || (!g.plugins.contains_key(&id)
                        && !g.state[&id].removed
                        && !pending_restore.contains(&id))
                {
                    g.state.remove(&id);
                }
            }
            self.save(&g);
            self.sync_wake_training(&g);
        }
        for id in to_start {
            self.start(&id).await;
        }
    }

    pub async fn start(self: &Arc<Self>, id: &str) {
        let (m, dir, settings) = {
            let mut g = self.inner.lock().unwrap();
            let Some(p) = g.plugins.get(id) else { return };
            let (m, dir) = (p.manifest.clone(), p.dir.clone());
            if m.needs_newer_core() {
                g.plugins.get_mut(id).unwrap().err = format!("заработает после обновления Джарвиса до {}", m.min_core);
                return;
            }
            let settings = g.settings_of(id);
            (m, dir, settings)
        };
        let res: Result<Runtime, String> = match m.kind.as_str() {
            "rust" if modules::built_in(&m.id) => {
                if self.official(id) {
                    official::Official::start(m.clone(), &dir, Arc::downgrade(self)).await.map(Runtime::Official)
                } else {
                    Err("Это имя зарезервировано для официального плагина".into())
                }
            }
            "rust" => process::Process::start(&m, &dir, (self.hooks.info)(), settings).await.map(Runtime::Process),
            "module" => {
                #[cfg(feature = "native-plugins")]
                let custom = native_addon::entries().find(|(manifest, _)| manifest.id == id).map(|(_, entry)| entry);
                #[cfg(feature = "native-plugins")]
                if let Some(entry) = custom {
                    native_addon::Native::start(entry, &m, self, id, dir, settings).map(|n| Runtime::Native(Arc::new(n)))
                } else {
                    Err("?????????? ????? ?? ???????????? ????????".into())
                }
                #[cfg(not(feature = "native-plugins"))]
                Err("?????????? ????? ?? ???????????? ????????".into())
            },
            #[cfg(feature = "lua")]
            "lua" if modules::official_lua_service(&m.id) && !self.official(id) => {
                Err("Это имя зарезервировано для официального плагина".into())
            }
            #[cfg(feature = "lua")]
            "lua" => lua::Script::start(self.clone(), m.clone(), dir, settings).await.map(Runtime::Lua),
            #[cfg(not(feature = "lua"))]
            "lua" => Err("Эта сборка не включает Lua: включите модуль lua при сборке".into()),
            k => Err(format!("неизвестный вид плагина {k}")),
        };
        let mut g = self.inner.lock().unwrap();
        let Some(p) = g.plugins.get_mut(id) else { return };
        match res {
            Ok(rt) => {
                p.manifest = m.clone();
                p.rt = Some(rt);
                p.err.clear();
                log::info!("плагины: {id} {} запущен", m.version);
            }
            Err(e) => {
                p.rt = None;
                log::info!("плагины: {id} не запустился: {e}");
                p.err = e;
            }
        }
        self.sync_wake_training(&g);
    }

    pub async fn stop(&self, id: &str) {
        let rt = {
            let mut g = self.inner.lock().unwrap();
            let rt = g.plugins.get_mut(id).and_then(|p| p.rt.take());
            self.sync_wake_training(&g);
            rt
        };
        if let Some(rt) = rt {
            rt.stop().await;
        }
    }

    pub fn running_ids(&self) -> Vec<String> {
        self.inner.lock().unwrap().plugins.iter()
            .filter(|(_, p)| p.rt.as_ref().is_some_and(Runtime::running))
            .map(|(id, _)| id.clone()).collect()
    }

    pub async fn stop_all(&self) {
        let rts: Vec<Runtime> = {
            let mut g = self.inner.lock().unwrap();
            let rts = g.plugins.values_mut().filter_map(|p| p.rt.take()).collect();
            self.sync_wake_training(&g);
            rts
        };
        for rt in rts {
            rt.stop().await;
        }
        let builtins: Vec<_> = std::mem::take(&mut *self.builtins.lock().unwrap()).into_values().collect();
        for rt in builtins {
            rt.stop().await;
        }
    }

    /// Запущенные плагины по алфавиту id (при одинаковых именах инструментов побеждает первый).
    pub fn running(&self) -> Vec<Runtime> {
        self.inner.lock().unwrap().plugins.values().filter_map(|p| p.rt.clone()).collect()
    }

    pub fn wake_training(&self) -> bool {
        self.wake_training_cached.load(Ordering::Acquire)
    }

    pub fn supports_chat(&self, id: &str) -> bool {
        self.inner.lock().unwrap().plugins.get(id)
            .and_then(|plugin| plugin.rt.as_ref())
            .is_some_and(Runtime::supports_chat)
    }

    pub fn supports_stt(&self, id: &str) -> bool {
        self.inner.lock().unwrap().plugins.get(id)
            .and_then(|plugin| plugin.rt.as_ref())
            .is_some_and(Runtime::supports_stt)
    }

    pub fn supports_tts(&self, id: &str) -> bool {
        self.inner.lock().unwrap().plugins.get(id)
            .and_then(|plugin| plugin.rt.as_ref())
            .is_some_and(Runtime::supports_tts)
    }

    pub fn text_plugins(&self) -> Vec<Value> {
        let installed = self.inner.lock().unwrap();
        installed.plugins.values().filter(|plugin| plugin.rt.as_ref().is_some_and(Runtime::supports_chat))
            .map(|plugin| json!({"id": plugin.manifest.id, "name": plugin.manifest.name})).collect()
    }

    pub fn stt_plugins(&self) -> Vec<Value> {
        let installed = self.inner.lock().unwrap();
        installed.plugins.values().filter(|plugin| plugin.rt.as_ref().is_some_and(Runtime::supports_stt))
            .map(|plugin| json!({"id": plugin.manifest.id, "name": plugin.manifest.name})).collect()
    }

    pub fn tts_plugins(&self) -> Vec<Value> {
        let installed = self.inner.lock().unwrap();
        installed.plugins.values().filter(|plugin| plugin.rt.as_ref().is_some_and(Runtime::supports_tts))
            .map(|plugin| json!({"id": plugin.manifest.id, "name": plugin.manifest.name})).collect()
    }

    pub async fn plugin_chat(&self, id: &str, request: Value) -> Result<Value, String> {
        let runtime = {
            let installed = self.inner.lock().unwrap();
            installed.plugins.get(id).and_then(|plugin| plugin.rt.as_ref())
                .filter(|runtime| runtime.supports_chat()).cloned()
                .ok_or_else(|| format!("Включите текстовый плагин «{id}» в «Подключениях»"))?
        };
        let result = match runtime {
            Runtime::Process(plugin) => plugin.request("llm.chat", request, Duration::from_secs(30)).await,
            #[cfg(feature = "lua")]
            Runtime::Lua(script) => script.chat(request).await,
            #[cfg(feature = "native-plugins")]
            Runtime::Native(plugin) => plugin.request("llm.chat", request, Duration::from_secs(30)).await,
            _ => return Err("Плагин не поддерживает текстовые запросы".into()),
        };
        result.map_err(|error| clip(&error, 300))
    }

    pub async fn plugin_stt(&self, id: &str, request: Value) -> Result<Value, String> {
        let runtime = {
            let installed = self.inner.lock().unwrap();
            installed.plugins.get(id).and_then(|plugin| plugin.rt.as_ref())
                .filter(|runtime| runtime.supports_stt()).cloned()
                .ok_or_else(|| format!("Включите плагин распознавания «{id}» в «Подключениях»"))?
        };
        let result = match runtime {
            Runtime::Process(plugin) => plugin.request("stt.transcribe", request, Duration::from_secs(45)).await,
            #[cfg(feature = "lua")]
            Runtime::Lua(script) => script.transcribe(request).await,
            #[cfg(feature = "native-plugins")]
            Runtime::Native(plugin) => plugin.request("stt.transcribe", request, Duration::from_secs(45)).await,
            _ => return Err("Плагин не поддерживает распознавание речи".into()),
        };
        result.map_err(|error| clip(&error, 300))
    }

    pub async fn plugin_tts(&self, id: &str, request: Value) -> Result<Value, String> {
        let runtime = {
            let installed = self.inner.lock().unwrap();
            installed.plugins.get(id).and_then(|plugin| plugin.rt.as_ref())
                .filter(|runtime| runtime.supports_tts()).cloned()
                .ok_or_else(|| format!("Включите плагин озвучки «{id}» в «Подключениях»"))?
        };
        let result = match runtime {
            Runtime::Process(plugin) => plugin.request("tts.speak", request, Duration::from_secs(60)).await,
            #[cfg(feature = "lua")]
            Runtime::Lua(script) => script.speak(request).await,
            #[cfg(feature = "native-plugins")]
            Runtime::Native(plugin) => plugin.request("tts.speak", request, Duration::from_secs(60)).await,
            _ => return Err("Плагин не поддерживает озвучку".into()),
        };
        result.map_err(|error| clip(&error, 300))
    }

    pub async fn plugin_tts_voices(&self, id: &str) -> Result<Value, String> {
        let runtime = {
            let installed = self.inner.lock().unwrap();
            installed.plugins.get(id).and_then(|plugin| plugin.rt.as_ref())
                .filter(|runtime| runtime.supports_tts()).cloned()
                .ok_or_else(|| format!("Включите плагин озвучки «{id}» в «Подключениях»"))?
        };
        let result = match runtime {
            Runtime::Process(plugin) => plugin.request("tts.voices", Value::Null, Duration::from_secs(20)).await,
            #[cfg(feature = "lua")]
            Runtime::Lua(script) => script.voices().await,
            #[cfg(feature = "native-plugins")]
            Runtime::Native(plugin) => plugin.request("tts.voices", Value::Null, Duration::from_secs(20)).await,
            _ => return Ok(json!([])),
        };
        match result {
            Ok(Value::Array(items)) => Ok(Value::Array(items)),
            Ok(v) if v["voices"].is_array() => Ok(v["voices"].clone()),
            Ok(_) => Ok(json!([])),
            Err(error) => {
                let lower = error.to_ascii_lowercase();
                if lower.contains("не поддерживает") || lower.contains("unsupported") || lower.contains("unknown method") {
                    Ok(json!([]))
                } else {
                    Err(clip(&error, 300))
                }
            }
        }
    }

    pub fn tools(&self) -> Vec<Value> {
        let mut seen = std::collections::HashSet::new();
        let mut out = vec![];
        for rt in self.running() {
            for t in rt.tools() {
                let name = t.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                if !name.is_empty() && seen.insert(name) {
                    out.push(t);
                }
            }
        }
        out
    }

    pub async fn call_tool(&self, name: &str, args: &str) -> Option<String> {
        for rt in self.running() {
            if let Some(r) = rt.call_tool(name, args).await {
                return Some(r);
            }
        }
        None
    }

    /// Быстрые команды плагинов: Some(ответ) — фраза разобрана без модели.
    pub async fn command(&self, text: &str) -> Option<String> {
        for rt in self.running() {
            if let Some(a) = rt.command(text).await {
                return Some(a);
            }
        }
        None
    }

    /// Запущенный провайдер cap: отдельный процесс или выбранный встроенный Rust-модуль.
    pub fn provider(&self, cap: &str) -> Result<Runtime, String> {
        self.provider_named(cap).map(|(_,runtime)|runtime)
    }
    fn provider_named(&self,cap:&str)->Result<(String,Runtime),String> {
        if modules::is_embedded(cap) {
            return Ok((cap.to_owned(), self.builtin_provider(cap)?));
        }
        let g = self.inner.lock().unwrap();
        // Выбранный отдельный модуль владеет подключением и после выключения:
        // старый объединённый плагин не должен незаметно включать его обратно.
        let dedicated = g.plugins.contains_key(cap) || g.state.get(cap).is_some_and(|s|s.removed);
        let mut found = None;
        let mut rank = None;
        for p in g.plugins.values() {
            if dedicated && p.manifest.id != cap {continue;}
            if !p.manifest.provides(cap) {
                continue;
            }
            if let Some(rt) = p.rt.as_ref().filter(|r| r.is_service()) {
                let current=(p.manifest.id==cap,p.manifest.kind=="module",g.state.get(&p.manifest.id).is_some_and(|s|s.official));
                if rank.is_none_or(|r|current>r) {
                    rank=Some(current);
                    found = Some((p.manifest.id.clone(),rt.clone()));
                }
            }
        }
        found.ok_or_else(|| format!("нет запущенного плагина «{cap}»"))
    }
    /// Панели нужен выбранный провайдер, включая намеренно выключенный отдельный модуль.
    pub fn service_providers(&self)->Value {
        let mut out=Map::new();
        for (service,cap) in [("yandex","yandexhome"),("homeassistant","homeassistant"),("xiaomi","xiaomi"),("smartlife","smartlife")] {
            let selected=self.provider_named(cap).ok().map(|(id,_)|id);
            let available=selected.is_some();
            let plugin=selected.unwrap_or_else(||{let g=self.inner.lock().unwrap();if g.plugins.contains_key(cap) || g.state.get(cap).is_some_and(|s|s.removed) {cap.into()}else{"home".into()}});
            out.insert(service.into(),json!({"plugin":plugin,"available":available}));
        }
        Value::Object(out)
    }

    pub async fn set_enabled(self: &Arc<Self>, id: &str, on: bool) -> Result<(), String> {
        let _operation = self.install_lock.lock().await;
        let was = {
            let mut g = self.inner.lock().unwrap();
            if !g.plugins.contains_key(id) {
                return Err("нет такого плагина".into());
            }
            let st = g.state_mut(id);
            let was = st.enabled;
            st.enabled = on;
            self.save(&g);
            self.sync_wake_training(&g);
            was
        };
        if on && !was {
            self.start(id).await;
        }
        if !on && was {
            self.stop(id).await;
        }
        Ok(())
    }

    /// Сохранить настройки плагина. Пустой пароль — оставить прежний (панель не видит ключи целиком).
    pub fn set_settings(&self, id: &str, input: &Map<String, Value>) -> Result<(), String> {
        let (settings, rt) = {
            let mut g = self.inner.lock().unwrap();
            let Some(p) = g.plugins.get(id) else { return Err("нет такого плагина".into()) };
            let defs = p.manifest.settings.clone();
            let st = g.state_mut(id);
            for s in &defs {
                let Some(v) = input.get(&s.key) else { continue };
                if s.kind == "password" && v.as_str().is_some_and(str::is_empty) {
                    continue;
                }
                if s.kind == "select" && !s.options.is_empty() && !s.options.iter().any(|o| Some(o.as_str()) == v.as_str()) {
                    return Err(format!("«{}»: нет такого варианта", s.label));
                }
                st.settings.insert(s.key.clone(), v.clone());
            }
            self.save(&g);
            let rt = g.plugins.get(id).and_then(|p| p.rt.clone());
            (g.settings_of(id), rt)
        };
        if let Some(rt) = rt {
            rt.configure(settings);
        }
        Ok(())
    }

    pub async fn uninstall(&self, id: &str) -> Result<(), String> {
        let _operation = self.install_lock.lock().await;
        let dir = self.inner.lock().unwrap().plugins.get(id).map(|p| p.dir.clone()).ok_or("нет такого плагина")?;
        self.stop(id).await;
        for path in [dir, self.dir().join(format!("{id}.old")), self.dir().join(format!("{id}.tmp"))] {
            std::fs::remove_dir_all(path).or_else(|error| {
                if error.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(error) }
            }).map_err(|error| error.to_string())?;
        }
        let mut g = self.inner.lock().unwrap();
        g.plugins.remove(id);
        match g.state.get(id).cloned() {
            Some(st) if st.official => {
                g.state.insert(id.into(), PluginState { removed: true, enabled: false, source: st.source, official: true, ..Default::default() });
            }
            _ => {
                g.state.remove(id);
            }
        }
        self.save(&g);
        self.sync_wake_training(&g);
        log::info!("плагины: удалён {id}");
        Ok(())
    }

    /// Список установленных плагинов для манифеста резервной копии.
    pub fn backup_plugins(&self) -> Vec<jarvis_station::backup::BackupPlugin> {
        let g = self.inner.lock().unwrap();
        g.plugins
            .values()
            .filter(|p| !modules::is_embedded(&p.manifest.id))
            .map(|p| {
                let st = g.state.get(&p.manifest.id);
                jarvis_station::backup::BackupPlugin {
                    id: p.manifest.id.clone(),
                    kind: p.manifest.kind.clone(),
                    source: st.map(|s| s.source.clone()).unwrap_or_default(),
                    official: st.is_some_and(|s| s.official),
                    version: p.manifest.version.clone(),
                    enabled: st.is_some_and(|s| s.enabled),
                }
            })
            .collect()
    }

    /// Переустановить недостающие Rust-плагины, сохранённые в restore-plugins.json после восстановления бэкапа.
    pub async fn restore_pending_plugins(self: &Arc<Self>) {
        let path = self.data_dir.join(jarvis_station::backup::RESTORE_PLUGINS_FILE);
        let Ok(raw) = std::fs::read(&path) else {
            return;
        };
        let Ok(mut pending) =
            serde_json::from_slice::<Vec<jarvis_station::backup::BackupPlugin>>(&raw)
        else {
            let _ = std::fs::remove_file(&path);
            return;
        };
        if pending.is_empty() {
            let _ = std::fs::remove_file(&path);
            return;
        }
        // VPN ставим первым, чтобы последующие загрузки могли идти через настроенный туннель при необходимости.
        pending.sort_by_key(|p| if p.id == "vpn" { (0, p.id.clone()) } else { (1, p.id.clone()) });

        let mut remaining = Vec::new();
        for p in pending {
            let already = self.inner.lock().unwrap().plugins.contains_key(&p.id);
            if already {
                continue;
            }
            let res: Result<(), String> = if compiled_modules().iter().any(|m| m == &p.id) {
                self.install_module(&p.id).await.map(|_| ())
            } else if catalog::valid_repo(&p.source) {
                let req = install::InstallRequest {
                    id: p.id.clone(),
                    repo: p.source.clone(),
                    expected_version: String::new(),
                    update: false,
                };
                self.install_repo(&req, || true).await.map(|_| ())
            } else {
                Err("нет репозитория источника".into())
            };
            match res {
                Ok(()) => {
                    if !p.enabled {
                        let _ = self.set_enabled(&p.id, false).await;
                    }
                    log::info!("бэкап: переустановлен плагин {}", p.id);
                    if p.id == "vpn" {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
                Err(err) => {
                    log::warn!("бэкап: не удалось переустановить плагин {}: {}", p.id, err);
                    remaining.push(p);
                }
            }
        }
        if remaining.is_empty() {
            let _ = std::fs::remove_file(&path);
        } else if let Ok(data) = serde_json::to_vec_pretty(&remaining) {
            let _ = crate::settings::write_atomic(&path, &data, 0o600);
        }
    }

    /// Для панели: установленные плагины, их настройки (ключи — только хвостом) и состояние.
    pub fn list(&self) -> Vec<Value> {
        let mut rows = vec![];
        {
            let mut g = self.inner.lock().unwrap();
            let ids: Vec<String> = g.plugins.keys().cloned().collect();
            for id in ids {
                let mut vals = g.settings_of(&id);
                let st = g.state_mut(&id).clone();
                let p = &g.plugins[&id];
                for s in p.manifest.settings.iter().filter(|s| s.kind == "password") {
                    if let Some(v) = vals.get(&s.key).and_then(Value::as_str).filter(|v| !v.is_empty()) {
                        let tail = if v.len() <= 8 { String::new() } else { v[v.len() - 4..].to_string() };
                        vals.insert(s.key.clone(), format!("•••{tail}").into());
                    }
                }
                rows.push((p.manifest.clone(), st, p.rt.clone(), p.err.clone(), p.dir.clone(), vals));
            }
        }
        let mut out: Vec<Value> = rows
            .into_iter()
            .map(|(m, st, rt, err, dir, vals)| {
                let mut v = serde_json::to_value(&m).unwrap_or(json!({}));
                let o = v.as_object_mut().unwrap();
                o.insert("enabled".into(), st.enabled.into());
                o.insert("running".into(), rt.as_ref().is_some_and(Runtime::running).into());
                o.insert("official".into(), st.official.into());
                o.insert("source".into(), st.source.into());
                if !err.is_empty() {
                    o.insert("error".into(), err.into());
                }
                if let Some(s) = rt.map(|r| r.status()).filter(|s| !s.is_empty()) {
                    o.insert("status".into(), s.into());
                }
                o.insert("values".into(), Value::Object(vals));
                o.insert("size".into(), dir_size(&dir).into());
                v
            })
            .collect();
        out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        out
    }

    /// Коротко для /api/state: какие плагины стоят и включены.
    pub fn summary(&self) -> Value {
        let g = self.inner.lock().unwrap();
        let inst: Vec<&String> = g.plugins.keys().collect();
        let on: Vec<&String> = g.plugins.keys().filter(|id| g.state.get(*id).is_some_and(|s| s.enabled)).collect();
        json!({"installed": inst, "enabled": on, "builtin": modules::EMBEDDED_VOICE_IDS})
    }


}

pub fn read_manifest(dir: &Path) -> Result<Manifest, String> {
    let b = std::fs::read(dir.join("plugin.json")).map_err(|e| e.to_string())?;
    let m: Manifest = serde_json::from_slice(&b).map_err(|e| format!("plugin.json: {e}"))?;
    m.validate()?;
    Ok(m)
}

pub fn dir_size(dir: &Path) -> u64 {
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => n += dir_size(&e.path()),
                Ok(_) => n += e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => {}
            }
        }
    }
    n
}

pub fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n).collect::<String>() + "…" }
}

pub const SHUTDOWN_WAIT: Duration = Duration::from_secs(2);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_versions() {
        assert!(valid_id("vpn") && valid_id("my-plugin2") && !valid_id("A") && !valid_id("-x") && !valid_id("x"));
        assert!(newer("0.9.10", "0.9.9") && !newer("0.9.71", "0.9.71-rs") && newer("v1.0.0", "0.9.99"));
        assert!(!valid_tool_name("smart_home") && valid_tool_name("coin_flip"));
    }

    #[tokio::test]
    async fn embedded_voice_providers_work_and_clean_legacy_plugins() {
        let dir = std::env::temp_dir().join(format!("jarvis-builtin-{}", crate::settings::rand_hex(6)));
        for id in ["chatgpt", "gemini"] {
            let legacy = dir.join("plugins").join(id);
            std::fs::create_dir_all(&legacy).unwrap();
            std::fs::write(
                legacy.join("plugin.json"),
                json!({"id":id,"name":id,"version":"0.1.0","kind":"rust","category":"assistant","main":"plugin","api":1,"provides":[id]}).to_string(),
            ).unwrap();
            std::fs::write(legacy.join("plugin"), b"old-binary").unwrap();
        }
        let mut st = BTreeMap::new();
        st.insert("chatgpt", PluginState { official: true, ..Default::default() });
        std::fs::write(dir.join("plugins.json"), serde_json::to_vec(&st).unwrap()).unwrap();

        let host = Host::new(dir.clone(), Hooks::default());
        host.load().await;
        assert!(!dir.join("plugins/chatgpt").exists());
        assert!(!dir.join("plugins/gemini").exists());
        assert!(host.list().is_empty());

        for id in modules::EMBEDDED_VOICE_IDS {
            let rt = host.provider(id).unwrap();
            assert!(rt.running());
            assert!(rt.is_service());
            assert!(host.official(id));
        }
        assert_eq!(host.provider("chatgpt").unwrap().request("login.status", Value::Null, Duration::from_secs(2)).await.unwrap(), Value::Null);
        assert_eq!(host.summary()["builtin"], json!(modules::EMBEDDED_VOICE_IDS));

        let fake_zip = install::tests::make_zip(&[
            ("plugin.json", r#"{"id":"chatgpt","name":"Old","version":"0.2.5","kind":"rust","category":"assistant","main":"plugin","api":1,"provides":["chatgpt"]}"#),
            ("plugin", "stub"),
        ]);
        match host.install(&fake_zip, crate::DEFAULT_REPO, true).await {
            Err(err) => assert!(err.contains("встроен"), "{err}"),
            Ok(_) => panic!("embedded provider zip must be rejected"),
        }

        host.stop_all().await;
        std::fs::remove_dir_all(dir).unwrap();
    }
}
