//! Настройки — один JSON на колонке (/data/opt/jarvisd/settings.json, права 0600).
//! Формат тот же, что у Go-версии: файл читается и пишется обеими в обе стороны.
//! Поля, которые ядро ещё не знает (их ведут модули), не теряются: лежат в `extra`.
//! Пишем атомарно: во временный файл, потом rename — выключение питания не оставит полфайла.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
};

pub const MAX_TRAIN_LIMIT_MB: i64 = 100;

/// Go пишет пустой список как null: читаем его как пустой.
pub fn nd<'de, D: serde::Deserializer<'de>, T: Default + Deserialize<'de>>(d: D) -> Result<T, D::Error> {
    Option::<T>::deserialize(d).map(Option::unwrap_or_default)
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LedSettings {
    pub enabled: bool,
    pub brightness: i64,
    pub listening: String,
    pub thinking: String,
    pub speaking: String,
}

pub fn default_led() -> LedSettings {
    LedSettings {
        enabled: true,
        brightness: 45,
        listening: "#ffffff".into(),
        thinking: "#00b4ff".into(),
        speaking: "#5a2dff".into(),
    }
}

/// Ночной режим: по расписанию голос тише, кольцо тусклее, музыка не громче.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct NightSettings {
    pub enabled: bool,
    pub from: String,
    pub to: String,
    pub voice: i64,
    pub bright: i64,
    pub speaker: i64,
}

impl Default for NightSettings {
    fn default() -> Self {
        NightSettings { enabled: false, from: "23:00".into(), to: "08:00".into(), voice: 20, bright: 10, speaker: 3 }
    }
}

impl NightSettings {
    /// Пустые поля — значениями по умолчанию.
    pub fn fill(&mut self) {
        let d = NightSettings::default();
        if hhmm(&self.from).is_none() {
            self.from = d.from;
        }
        if hhmm(&self.to).is_none() {
            self.to = d.to;
        }
        self.voice = self.voice.clamp(5, 100);
        self.bright = self.bright.clamp(0, 100);
        self.speaker = self.speaker.clamp(1, 10);
    }
}

pub fn hhmm(v: &str) -> Option<i64> {
    let (h, m) = v.trim().split_once(':')?;
    let (h, m): (i64, i64) = (h.trim().parse().ok()?, m.trim().parse().ok()?);
    ((0..=23).contains(&h) && (0..=59).contains(&m)).then_some(h * 60 + m)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub color: i64,
    pub mode: String,
    pub engine: String,
    pub live_model: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "nd")]
    pub gemini_models: Vec<String>,
    pub voice: String,
    pub backend_model: String,
    pub effort: String,
    pub priority: bool,
    pub web_search: bool,
    pub idle_s: f64,
    pub prompt: String,
    pub llm_model: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub llm_engine: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub llm_plugin: String,
    pub tts_voice: String,
    pub tts_model: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub tts_engine: String,
    pub stt_engine: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub stt_model: String,
    pub volume: i64,
    pub city: String,
    pub timezone: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub led: Option<LedSettings>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

pub const BACKEND_MODELS: &[&str] = &["gpt-6-luna", "gpt-6.2-sol", "gpt-6.2-astra", "gpt-6.1-terra", "gpt-6.1-sol", "gpt-6-sol", "gpt-6-astra", "gpt-5.6-luna"];
pub const ELEVEN_DEFAULT_MODEL: &str = "eleven_flash_v2_5";

impl Profile {
    pub fn is_classic(&self) -> bool {
        self.engine != "eleven_realtime"
            && (self.mode == "classic" || self.engine == "elevenlabs" || self.engine == "classic")
    }

    pub fn effective_stt_engine(&self) -> &str {
        if self.stt_engine.is_empty() {
            "elevenlabs"
        } else {
            &self.stt_engine
        }
    }

    pub fn effective_tts_engine(&self) -> &str {
        if !self.tts_engine.is_empty() {
            &self.tts_engine
        } else if self.engine == "openai" {
            "openai"
        } else {
            "elevenlabs"
        }
    }

    pub fn effective_llm_engine(&self) -> &str {
        if !self.llm_engine.is_empty() {
            &self.llm_engine
        } else if !self.llm_plugin.is_empty() {
            &self.llm_plugin
        } else {
            ""
        }
    }

    pub fn effective_llm_model(&self) -> &str {
        if !self.llm_model.is_empty() {
            &self.llm_model
        } else {
            &self.backend_model
        }
    }

    fn classic_defaults(&mut self) {
        self.mode = "classic".into();
        if self.tts_engine.is_empty() {
            self.tts_engine = if self.engine == "openai" {
                "openai"
            } else {
                "elevenlabs"
            }
            .into();
        }
        if self.engine.is_empty() || self.engine == "live" {
            self.engine = "classic".into();
        }
        if self.stt_engine.is_empty() {
            self.stt_engine = "elevenlabs".into();
        }
        if self.llm_engine.is_empty() && !self.llm_plugin.is_empty() {
            self.llm_engine = self.llm_plugin.clone();
        }
        if self.llm_model.is_empty() {
            self.llm_model = self.backend_model.clone();
        }
        if matches!(self.effective_llm_engine(), "" | "openai" | "chatgpt") {
            if self.llm_model.is_empty() {
                self.llm_model = BACKEND_MODELS[0].into();
            }
            if !self.llm_model.is_empty() {
                self.backend_model = self.llm_model.clone();
            }
        }
        if self.tts_model.is_empty() {
            self.tts_model = match self.effective_tts_engine() {
                "elevenlabs" => ELEVEN_DEFAULT_MODEL.into(),
                "openai" => "gpt-4o-mini-tts".into(),
                _ => String::new(),
            };
        }
    }

    /// Пустые громкость, город и пояс ассистента берутся из общих настроек.
    fn fill_defaults(&mut self, volume: i64, city: &str, tz: &str, led: &LedSettings) {
        if self.is_classic() {
            self.classic_defaults();
        }
        if self.volume <= 0 {
            self.volume = if volume > 0 { volume } else { 40 };
        }
        self.city = city.into();
        self.timezone = tz.into();
        if self.led.is_none() {
            self.led = Some(led.clone());
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Prompt {
    pub id: String,
    pub name: String,
    pub text: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomVoice {
    pub id: String,
    pub name: String,
    pub prompt: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ElevenKey {
    pub key: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub until: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub why: String,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MemorySettings {
    #[serde(skip_serializing_if = "is_false")]
    pub off: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub home_assistant: Value,
    pub password_hash: String,
    pub session_key: String,

    pub openai_key: String,
    pub gemini_key: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[serde(deserialize_with = "nd")]
    pub gemini_keys: Vec<String>,
    pub grok_key: String,
    pub chatgpt: Value,
    pub chatgpt_accounts: Vec<crate::chatgpt_accounts::Account>,
    pub chatgpt_active: String,
    pub chatgpt_auto_switch: bool,
    pub xiaomi: Value,
    pub tuya: Value,
    pub elevenlabs_key: String,
    pub yandex_token: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[serde(deserialize_with = "nd")]
    pub eleven_keys: Vec<ElevenKey>,

    pub vless: String,
    pub proxy_yandex: bool,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    #[serde(deserialize_with = "nd")]
    pub vpn: BTreeMap<String, bool>,

    #[serde(deserialize_with = "nd")]
    pub profiles: Vec<Profile>,
    pub active: String,
    #[serde(deserialize_with = "nd")]
    pub prompts: Vec<Prompt>,
    #[serde(deserialize_with = "nd")]
    pub live_voices: Vec<CustomVoice>,

    pub wake_enabled: bool,
    #[serde(default = "default_true")]
    pub wake_interrupt: bool,
    pub wake_reply_enabled: bool,
    #[serde(default = "default_true")]
    pub voice_error_alice: bool,
    pub wake_profile: String,
    pub wake_check: bool,
    pub wake_sensitivity: i64,
    pub mic_boost_db: i64,
    pub mic_gain_db: i64,
    pub follow_up_s: f64,
    pub memory: MemorySettings,
    pub city: String,
    pub timezone: String,
    pub place_moved: bool,
    pub speaker_room: String,
    pub volume: i64,
    pub vol_scale: i64,
    pub lan_only: bool,
    pub home_via: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    #[serde(deserialize_with = "nd")]
    pub home_use: BTreeMap<String, bool>,

    pub led: LedSettings,
    pub dev_mode: bool,
    pub onboarded: bool,
    pub plugins_warned: bool,
    pub language: String,
    pub radio_off: bool,
    pub night: NightSettings,

    pub train_mode: bool,
    #[serde(default)]
    pub train_voice_review: bool,
    pub train_limit_mb: i64,

    pub update_repo: String,
    pub update_check: bool,
    pub update_auto: bool,
    pub no_stats: bool,
    pub auto_once: i64,

    /// Поля модулей и будущих версий: ядро их не трогает, но и не теряет.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

pub const DEFAULT_PROMPT: &str = "Ты — Джарвис, голосовой помощник в умной колонке. Говоришь по-русски, коротко и по делу, с лёгкой иронией, как дворецкий из фильма. Обращаешься к хозяину на «ты».";
const BRO_PROMPT: &str = "Ты — Джарвис, но общаешься с хозяином как близкий кореш: «братишка», «бро», на «ты», с юмором и подколами, можешь крепко выразиться к месту. Говоришь коротко, живо, по-пацански, но по делу и честно. Если чего-то не знаешь — так и говоришь.";

impl Default for Settings {
    fn default() -> Self {
        let mut extra = Map::new();
        extra.insert("bg_to".into(), Value::from("gdrive"));
        Settings {
            home_assistant: Value::Object(Map::new()),
            password_hash: String::new(),
            session_key: rand_hex(32),
            openai_key: String::new(),
            gemini_key: String::new(),
            gemini_keys: vec![],
            grok_key: String::new(),
            chatgpt: Value::Object(Map::new()),
            chatgpt_accounts: vec![], chatgpt_active: String::new(), chatgpt_auto_switch: false,
            xiaomi: Value::Object(Map::new()),
            tuya: Value::Object(Map::new()),
            elevenlabs_key: String::new(),
            yandex_token: String::new(),
            eleven_keys: vec![],
            vless: String::new(),
            proxy_yandex: false,
            vpn: BTreeMap::new(),
            profiles: vec![Profile {
                id: "p1".into(),
                name: "Джарвис".into(),
                color: 1,
                mode: "live".into(),
                effort: "low".into(),
                priority: true,
                web_search: true,
                idle_s: 8.0,
                prompt: "jarvis".into(),
                ..Default::default()
            }],
            active: "p1".into(),
            prompts: vec![
                Prompt { id: "jarvis".into(), name: "Джарвис".into(), text: DEFAULT_PROMPT.into() },
                Prompt { id: "bro".into(), name: "Братишка".into(), text: BRO_PROMPT.into() },
            ],
            live_voices: vec![],
            wake_enabled: true,
            wake_interrupt: true,
            wake_reply_enabled: true,
            voice_error_alice: true,
            wake_profile: String::new(),
            wake_check: false,
            wake_sensitivity: 5,
            mic_boost_db: 0,
            mic_gain_db: 0,
            follow_up_s: 6.0,
            memory: MemorySettings::default(),
            city: String::new(),
            timezone: "UTC".into(),
            place_moved: false,
            speaker_room: String::new(),
            volume: 40,
            vol_scale: 0,
            lan_only: true,
            home_via: "alice".into(),
            home_use: BTreeMap::new(),
            led: default_led(),
            dev_mode: false,
            onboarded: false,
            plugins_warned: false,
            language: "ru".into(),
            radio_off: false,
            night: NightSettings::default(),
            train_mode: false,
            train_voice_review: false,
            train_limit_mb: 2,
            update_repo: String::new(),
            update_check: true,
            update_auto: true,
            no_stats: false,
            auto_once: 1,
            extra,
        }
    }
}

pub fn rand_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("случайные числа");
    hex::encode(b)
}

/// Домашние каналы: через что Джарвис может управлять домом.
pub const HOME_CHANNELS: &[&str] = &["alice", "yandexmusic", "yandex", "smartlife", "xiaomi", "homeassistant"];

/// Все перечисленные поля заданы (вход в Xiaomi, Tuya, ChatGPT — правила те же, что в Go).
pub fn has_all(v: &Value, keys: &[&str]) -> bool {
    keys.iter().all(|k| v.get(k).and_then(Value::as_str).is_some_and(|s| !s.is_empty()))
}

impl Settings {
    pub fn active_profile_ref(&self) -> Option<&Profile> {
        self.profiles
            .iter()
            .find(|p| p.id == self.active)
            .or_else(|| self.profiles.first())
    }

    pub fn active_profile(&self) -> Profile {
        self.active_profile_ref()
            .cloned()
            .unwrap_or_else(|| Settings::default().profiles.remove(0))
    }

    /// Если у активного профиля ещё не выбран голосовой движок, выбрать только что подключённый.
    pub fn auto_select_engine(&mut self, service: &str) {
        let engine = match service {
            "chatgpt" | "openai" | "gemini" | "grok" => service,
            "elevenlabs" => "eleven_realtime",
            _ => return,
        };
        let idx = self.profiles.iter().position(|p| p.id == self.active).unwrap_or(0);
        if let Some(p) = self.profiles.get_mut(idx) && p.engine.trim().is_empty() {
            p.engine = engine.into();
            if p.mode.trim().is_empty() {
                p.mode = "live".into();
            }
        }
    }

    pub fn prompt_text(&self, id: &str) -> String {
        self.prompts.iter().find(|p| p.id == id).map(|p| p.text.clone()).unwrap_or_else(|| DEFAULT_PROMPT.into())
    }

    /// Время в часовом поясе хозяина: часы колонки идут по Гринвичу.
    pub fn now(&self) -> jiff::Zoned {
        let tz = jiff::tz::TimeZone::get(&self.timezone).unwrap_or(jiff::tz::TimeZone::UTC);
        jiff::Timestamp::now().to_zoned(tz)
    }

    pub fn is_night_in(&self, tz: &jiff::tz::TimeZone) -> bool {
        if !self.night.enabled {
            return false;
        }
        let (Some(from), Some(to)) = (hhmm(&self.night.from), hhmm(&self.night.to)) else { return false };
        if from == to {
            return false;
        }
        let t = jiff::Timestamp::now().to_zoned(tz.clone());
        let now = t.hour() as i64 * 60 + t.minute() as i64;
        if from < to { now >= from && now < to } else { now >= from || now < to }
    }

    /// Сейчас ночь по расписанию (интервал может переходить через полночь).
    pub fn is_night(&self) -> bool {
        let tz = jiff::tz::TimeZone::get(&self.timezone).unwrap_or(jiff::tz::TimeZone::UTC);
        self.is_night_in(&tz)
    }

    /// Подсветка активного ассистента с общим выключателем.
    pub fn led_for(&self) -> LedSettings {
        let mut l = self.led.clone();
        if let Some(p) = self.active_profile_ref().and_then(|p| p.led.as_ref()) {
            l.brightness = p.brightness;
            l.listening = p.listening.clone();
            l.thinking = p.thinking.clone();
            l.speaking = p.speaking.clone();
        }
        if self.is_night() {
            l.brightness = l.brightness.min(self.night.bright);
        }
        l
    }

    pub fn voice_volume_in(&self, tz: &jiff::tz::TimeZone) -> i64 {
        let mut v = self.volume;
        let pv = self.active_profile_ref().map_or(0, |p| p.volume);
        if pv > 0 {
            v = pv;
        }
        if self.is_night_in(tz) {
            v = v.min(self.night.voice);
        }
        v
    }

    /// Громкость голоса активного ассистента.
    pub fn voice_volume(&self) -> i64 {
        let tz = jiff::tz::TimeZone::get(&self.timezone).unwrap_or(jiff::tz::TimeZone::UTC);
        self.voice_volume_in(&tz)
    }

    pub fn home_connected(&self, ch: &str) -> bool {
        match ch {
            "yandex" => !self.yandex_token.is_empty(),
            "smartlife" => has_all(&self.tuya, &["endpoint", "refresh_token"]),
            "xiaomi" => has_all(&self.xiaomi, &["user_id", "ssecurity", "service_token"]),
            "homeassistant" => has_all(&self.home_assistant, &["url", "token"]),
            _ => true, // Алиса и Яндекс Музыка — всегда есть
        }
    }

    pub fn use_home(&self, ch: &str) -> bool {
        if !self.home_connected(ch) {
            return false;
        }
        if let Some(v) = self.home_use.get(ch) {
            return *v;
        }
        if ch == "alice" {
            return self.home_via != "jarvis";
        }
        true
    }

    pub fn home_use_state(&self) -> BTreeMap<String, bool> {
        HOME_CHANNELS.iter().map(|ch| (ch.to_string(), self.use_home(ch))).collect()
    }

    /// ElevenLabsKey = первый ключ с квотой; если отдыхают все — тот, что освободится раньше.
    fn pick_eleven(&mut self) {
        if self.eleven_keys.is_empty() {
            self.elevenlabs_key.clear();
            return;
        }
        let now = jiff::Timestamp::now().as_second();
        let mut best = 0;
        for (i, k) in self.eleven_keys.iter().enumerate() {
            if !(k.until != 0 && now < k.until) {
                self.elevenlabs_key = k.key.clone();
                return;
            }
            if k.until < self.eleven_keys[best].until {
                best = i;
            }
        }
        self.elevenlabs_key = self.eleven_keys[best].key.clone();
    }

    fn add_eleven_key(&mut self, key: &str) {
        if self.eleven_keys.iter().any(|k| k.key == key) {
            return;
        }
        self.eleven_keys.push(ElevenKey { key: key.into(), ..Default::default() });
        self.pick_eleven();
    }
}

struct Hot {
    wake_enabled: AtomicBool,
    wake_interrupt: AtomicBool,
    mic_boost_db: AtomicI64,
    mic_gain_db: AtomicI64,
    wake_sensitivity: AtomicI64,
    train_mode: AtomicBool,
    train_voice_review: AtomicBool,
    train_limit_mb: AtomicI64,
    tz: RwLock<jiff::tz::TimeZone>,
}

impl Hot {
    fn parse_tz(name: &str) -> jiff::tz::TimeZone {
        jiff::tz::TimeZone::get(name).unwrap_or(jiff::tz::TimeZone::UTC)
    }

    fn new(s: &Settings) -> Self {
        Self {
            wake_enabled: AtomicBool::new(s.wake_enabled),
            wake_interrupt: AtomicBool::new(s.wake_interrupt),
            mic_boost_db: AtomicI64::new(s.mic_boost_db),
            mic_gain_db: AtomicI64::new(s.mic_gain_db),
            wake_sensitivity: AtomicI64::new(s.wake_sensitivity),
            train_mode: AtomicBool::new(s.train_mode),
            train_voice_review: AtomicBool::new(s.train_voice_review),
            train_limit_mb: AtomicI64::new(s.train_limit_mb),
            tz: RwLock::new(Self::parse_tz(&s.timezone)),
        }
    }

    fn sync(&self, s: &Settings) {
        self.wake_enabled.store(s.wake_enabled, Ordering::Relaxed);
        self.wake_interrupt.store(s.wake_interrupt, Ordering::Relaxed);
        self.mic_boost_db.store(s.mic_boost_db, Ordering::Relaxed);
        self.mic_gain_db.store(s.mic_gain_db, Ordering::Relaxed);
        self.wake_sensitivity.store(s.wake_sensitivity, Ordering::Relaxed);
        self.train_mode.store(s.train_mode, Ordering::Relaxed);
        self.train_voice_review.store(s.train_voice_review, Ordering::Relaxed);
        self.train_limit_mb.store(s.train_limit_mb, Ordering::Relaxed);
        *self.tz.write().unwrap() = Self::parse_tz(&s.timezone);
    }
}

/// Хранилище настроек: читать можно без гонок (копия), менять — через update().
#[derive(Clone)]
pub struct Store {
    inner: Arc<RwLock<Settings>>,
    path: Arc<PathBuf>,
    hot: Arc<Hot>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Store> {
        let path = path.as_ref().to_path_buf();
        let data = match fs::read(&path) {
            Ok(d) => Some(d),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        let Some(data) = data else {
            let s = Settings { vol_scale: 2, ..Default::default() }; // новая установка — сразу в новой шкале громкости
            let hot = Arc::new(Hot::new(&s));
            let st = Store { inner: Arc::new(RwLock::new(s)), path: Arc::new(path), hot };
            st.save(&st.inner.read().unwrap())?;
            st.apply_seed()?;
            return Ok(st);
        };
        let mut s: Settings = serde_json::from_slice(&data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut dirty = migrate(&mut s);
        if s.auto_once < 1 || !contains(&data, b"\"auto_once\"") {
            // автообновление теперь по умолчанию: один раз включаем и тем, кто ставил раньше
            s.update_check = true;
            s.update_auto = true;
            s.auto_once = 1;
            dirty = true;
        }
        let hot = Arc::new(Hot::new(&s));
        let st = Store { inner: Arc::new(RwLock::new(s)), path: Arc::new(path), hot };
        if dirty {
            st.save(&st.inner.read().unwrap())?;
        }
        st.apply_seed()?;
        Ok(st)
    }

    pub fn wake_enabled(&self) -> bool { self.hot.wake_enabled.load(Ordering::Relaxed) }
    pub fn wake_interrupt(&self) -> bool { self.hot.wake_interrupt.load(Ordering::Relaxed) }
    pub fn mic_boost_db(&self) -> i64 { self.hot.mic_boost_db.load(Ordering::Relaxed) }
    pub fn mic_gain_db(&self) -> i64 { self.hot.mic_gain_db.load(Ordering::Relaxed) }
    pub fn wake_sensitivity(&self) -> i64 { self.hot.wake_sensitivity.load(Ordering::Relaxed) }
    pub fn train_mode(&self) -> bool { self.hot.train_mode.load(Ordering::Relaxed) }
    pub fn train_voice_review(&self) -> bool { self.hot.train_voice_review.load(Ordering::Relaxed) }
    pub fn train_limit_mb(&self) -> i64 { self.hot.train_limit_mb.load(Ordering::Relaxed) }
    pub fn timezone(&self) -> jiff::tz::TimeZone { self.hot.tz.read().unwrap().clone() }
    pub fn now(&self) -> jiff::Zoned { jiff::Timestamp::now().to_zoned(self.timezone()) }
    pub fn is_night(&self) -> bool {
        let tz = self.timezone();
        self.with(|s| s.is_night_in(&tz))
    }
    pub fn voice_volume(&self) -> i64 {
        let tz = self.timezone();
        self.with(|s| s.voice_volume_in(&tz))
    }
    pub fn led_for(&self) -> LedSettings {
        let tz = self.timezone();
        self.with(|s| {
            let mut l = s.led.clone();
            if let Some(p) = s.active_profile_ref().and_then(|p| p.led.as_ref()) {
                l.brightness = p.brightness;
                l.listening = p.listening.clone();
                l.thinking = p.thinking.clone();
                l.speaking = p.speaking.clone();
            }
            if s.is_night_in(&tz) {
                l.brightness = l.brightness.min(s.night.bright);
            }
            l
        })
    }

    /// Копия настроек.
    pub fn get(&self) -> Settings {
        self.inner.read().unwrap().clone()
    }

    /// Прочитать часть настроек без копии всего (то, что нужно часто: каждый кадр микрофона).
    pub fn with<R>(&self, f: impl FnOnce(&Settings) -> R) -> R {
        f(&self.inner.read().unwrap())
    }

    /// Изменить и сохранить. Ошибка в f — изменения откатываются.
    pub fn update<E: From<io::Error>>(&self, f: impl FnOnce(&mut Settings) -> Result<(), E>) -> Result<(), E> {
        let mut g = self.inner.write().unwrap();
        let mut next = g.clone();
        f(&mut next)?;
        self.save(&next)?;
        self.hot.sync(&next);
        *g = next;
        Ok(())
    }

    fn save(&self, s: &Settings) -> io::Result<()> {
        let data = serde_json::to_vec_pretty(s).map_err(io::Error::other)?;
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        write_atomic(&self.path, &data, 0o600)
    }

    /// Начальные настройки от установщика (seed.json рядом с settings.json): один раз, потом файл удаляется.
    fn apply_seed(&self) -> io::Result<()> {
        let path = self.path.with_file_name("seed.json");
        let data = match fs::read(&path) {
            Ok(data) => data,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Seed {
            password_hash: String,
            openai_key: String,
            vless: String,
            timezone: String,
            yandex_token: String,
            elevenlabs_key: String,
            city: String,
            prompts: Vec<Prompt>,
            active_prompt: String,
            profile: Option<Profile>,
        }
        let seed: Seed = serde_json::from_slice(&data)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Некорректный seed.json"))?;
        self.update(|s| {
            if !seed.password_hash.is_empty() && seed.password_hash != s.password_hash {
                s.password_hash = seed.password_hash.clone();
                s.session_key = rand_hex(32);
            }
            let set = |dst: &mut String, v: &str| {
                if !v.is_empty() {
                    *dst = v.into()
                }
            };
            set(&mut s.openai_key, &seed.openai_key);
            set(&mut s.vless, &seed.vless);
            set(&mut s.timezone, &seed.timezone);
            set(&mut s.yandex_token, &seed.yandex_token);
            set(&mut s.city, &seed.city);
            if !seed.elevenlabs_key.is_empty() {
                s.add_eleven_key(&seed.elevenlabs_key);
            }
            for np in &seed.prompts {
                let mut found = false;
                for p in s.prompts.iter_mut().filter(|p| p.name == np.name) {
                    p.text = np.text.clone();
                    found = true;
                }
                if !found && !np.name.is_empty() && !np.text.is_empty() {
                    s.prompts.push(Prompt { id: format!("c{}", rand_hex(4)), name: np.name.clone(), text: np.text.clone() });
                }
            }
            let active = s.active.clone();
            if let Some(sp) = &seed.profile {
                for p in s.profiles.iter_mut().filter(|p| p.id == active) {
                    let mut np = sp.clone();
                    np.id = p.id.clone();
                    np.color = np.color.max(1);
                    if np.prompt.is_empty() {
                        np.prompt = p.prompt.clone();
                    }
                    *p = np;
                }
            }
            if !seed.active_prompt.is_empty() {
                if let Some(c) = s.prompts.iter().find(|c| c.name == seed.active_prompt).map(|c| c.id.clone()) {
                    for p in s.profiles.iter_mut().filter(|p| p.id == active) {
                        p.prompt = c.clone();
                    }
                }
            }
            Ok::<(), io::Error>(())
        })?;
        jarvis_station::sync_dir(path.parent().unwrap_or(Path::new(".")))?;
        fs::remove_file(path)
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Переносы старых форматов. true — файл надо переписать.
fn migrate(s: &mut Settings) -> bool {
    let mut dirty = crate::chatgpt_accounts::ensure(s);
    if s.session_key.is_empty() {
        s.session_key = rand_hex(32);
    }
    if !s.place_moved {
        // город и пояс общие — у колонки; берём те, что были у активного ассистента
        if let Some(p) = s.profiles.iter().find(|p| p.id == s.active) {
            if !p.timezone.is_empty() {
                s.timezone = p.timezone.clone();
            }
            if !p.city.is_empty() {
                s.city = p.city.clone();
            }
        }
        s.place_moved = true;
        dirty = true;
    }
    if s.eleven_keys.is_empty() && !s.elevenlabs_key.is_empty() {
        s.eleven_keys = vec![ElevenKey { key: s.elevenlabs_key.clone(), ..Default::default() }];
        dirty = true;
    }
    s.pick_eleven();
    if s.gemini_keys.is_empty() && !s.gemini_key.is_empty() {
        s.gemini_keys = vec![s.gemini_key.clone()];
    }
    let (vol, city, tz, led) = (s.volume, s.city.clone(), s.timezone.clone(), s.led.clone());
    for p in &mut s.profiles {
        p.fill_defaults(vol, &city, &tz, &led);
    }
    if s.vol_scale < 2 {
        // новая шкала громкости голоса: прежние 100% стали 50%, звучит так же, а выше — запас ×2
        s.volume = (s.volume / 2).max(5);
        for p in &mut s.profiles {
            p.volume = (p.volume / 2).max(5);
        }
        s.night.voice = (s.night.voice / 2).max(5);
        s.vol_scale = 2;
        dirty = true;
    }
    dirty
}

/// Записать файл атомарно: временный файл рядом, потом rename.
pub fn write_atomic(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    let tmp = path.with_extension(format!(
        "{}tmp",
        path.extension().map(|e| format!("{}.", e.to_string_lossy())).unwrap_or_default()
    ));
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(mode);
        }
        #[cfg(not(unix))]
        let _ = mode;
        use io::Write;
        let mut f = opts.open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_survives_parse_and_write_errors_and_replays_idempotently() {
        let dir = std::env::temp_dir().join(format!("jarvis-seed-{}", rand_hex(6)));
        let settings = dir.join("settings.json");
        let seed = dir.join("seed.json");
        let store = Store::open(&settings).unwrap();
        fs::write(&seed, b"{").unwrap();
        assert_eq!(store.apply_seed().unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(&seed).unwrap(), b"{");

        let bytes = br#"{"city":"Test City","password_hash":"test-hash"}"#;
        fs::write(&seed, bytes).unwrap();
        let blocked = dir.join("settings.json.tmp");
        fs::create_dir(&blocked).unwrap();
        assert!(store.apply_seed().is_err());
        assert_eq!(fs::read(&seed).unwrap(), bytes);
        fs::remove_dir(blocked).unwrap();
        store.apply_seed().unwrap();
        assert!(!seed.exists());
        let key = store.get().session_key;
        fs::write(&seed, bytes).unwrap();
        let restored = Store::open(settings).unwrap().get();
        assert_eq!(restored.city, "Test City");
        assert_eq!(restored.session_key, key);
        assert!(!seed.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn keeps_unknown_fields() {
        let dir = std::env::temp_dir().join(format!("jarvis-st-{}", rand_hex(4)));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("settings.json");
        fs::write(&p, r#"{"auto_once":1,"place_moved":true,"vol_scale":2,"live_voices":null,"vpn":null,"youtube_refresh":"x","chatgpt":{"plan":"plus"},"profiles":[{"id":"p1","name":"Д","volume":30,"future":1}],"active":"p1"}"#).unwrap();
        let st = Store::open(&p).unwrap();
        st.update(|s| {
            s.volume = 33;
            Ok::<(), io::Error>(())
        })
        .unwrap();
        let v: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["youtube_refresh"], "x");
        assert_eq!(v["chatgpt"]["plan"], "plus");
        assert_eq!(v["profiles"][0]["future"], 1);
        assert_eq!(v["volume"], 33);
        assert_eq!(v["profiles"][0]["volume"], 30);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn migrates_volume_scale() {
        let mut s: Settings = serde_json::from_str(r#"{"volume":80,"profiles":[{"id":"p1","volume":60}],"active":"p1","night":{"voice":20}}"#).unwrap();
        assert!(migrate(&mut s));
        assert_eq!((s.volume, s.profiles[0].volume, s.night.voice, s.vol_scale), (40, 30, 10, 2));
    }

    #[test]
    fn night_window() {
        assert_eq!(hhmm("23:05"), Some(23 * 60 + 5));
        assert_eq!(hhmm("24:00"), None);
    }

    #[test]
    fn classic_profile_migration_and_custom_pipeline_preservation() {
        let mut legacy: Settings = serde_json::from_str(
            r#"{"profiles":[{"id":"p1","mode":"classic","engine":"elevenlabs","backend_model":"gpt-6-sol","llm_plugin":"openrouter-lua"}]}"#,
        )
        .unwrap();
        migrate(&mut legacy);
        let lp = &legacy.profiles[0];
        assert!(lp.is_classic());
        assert_eq!(lp.effective_stt_engine(), "elevenlabs");
        assert_eq!(lp.effective_tts_engine(), "elevenlabs");
        assert_eq!(lp.effective_llm_engine(), "openrouter-lua");
        assert_eq!(lp.effective_llm_model(), "gpt-6-sol");
        assert_eq!(lp.tts_model, ELEVEN_DEFAULT_MODEL);

        let mut custom: Settings = serde_json::from_str(
            r#"{"profiles":[{"id":"p2","mode":"classic","engine":"classic","stt_engine":"openai","llm_engine":"gemini","llm_model":"gemini-3.8-flash","tts_engine":"my-tts","tts_model":""}]}"#,
        )
        .unwrap();
        migrate(&mut custom);
        let cp = &custom.profiles[0];
        assert!(cp.is_classic());
        assert_eq!(cp.effective_stt_engine(), "openai");
        assert_eq!(cp.effective_llm_engine(), "gemini");
        assert_eq!(cp.effective_llm_model(), "gemini-3.8-flash");
        assert_eq!(cp.effective_tts_engine(), "my-tts");
        assert_eq!(cp.tts_model, "");
    }
}
