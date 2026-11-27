//! Главный цикл: микрофон → слово «Джарвис» → разговор с моделью → динамик. Алиса имеет приоритет:
//! позвали её — Джарвис сразу замолкает и закрывает разговор.

use crate::{
    alice,
    audio::{Mic, Player},
    chatgpt, fastcmd,
    journal::Journal,
    led::{Leds, Mode},
    plugins::Host,
    rules,
    session::Session,
    settings::{Profile, Settings, Store},
    tools::Tools,
    wake::Listener,
};
use jarvis_dsp::Wake;
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::RecvTimeoutError,
    },
    time::{Duration, Instant},
};

const RATE: usize = 16000;
pub(crate) const INTERRUPTED: &str = "озвучку прервали";
#[path="text.rs"]
mod text;
#[path="voice_preview.rs"]
pub(crate) mod voice_preview;
#[path="wake_reply.rs"]
mod wake_reply;

#[derive(Default)]
struct St {
    live: Option<Arc<dyn Session>>,
    busy: bool,
    holding: bool,   // разговора с моделью ещё нет — звук с момента слова копится в hold
    hold: Vec<i16>,  // уйдёт модели целиком, когда разговор откроется
    aborted: bool,   // «Стоп» или «Алиса», пока разговор ещё не открыт
    ring: VecDeque<i16>,  // последние ~3 с микрофона: само слово и начало фразы
    last_wake: Option<Instant>,
    last_loud: Option<Instant>,
    pending_wake: bool,
    echo: EchoWindow,
    input_at: Option<Instant>,
    playback_started_at: Option<Instant>,
}

const ECHO_TAIL: Duration = Duration::from_millis(450);
const WAKE_INTERRUPT_AEC_GRACE: Duration = Duration::from_millis(550);
const WAKE_INTERRUPT_MIN_PROB: f32 = 0.88;

/// Перебивать ответ словом активации только когда колонка реально говорит, после окна сходимости VQE AEC
/// на начале фразы и с повышенным порогом уверенности (чтобы остаточное эхо первого слова не обрывало ответ).
fn should_wake_interrupt(
    enabled: bool,
    playing: bool,
    recent_wake: bool,
    playback_started_at: Option<Instant>,
    wake_prob: f32,
    now: Instant,
) -> bool {
    if !enabled || !playing || recent_wake {
        return false;
    }
    if playback_started_at.is_some_and(|t| now.saturating_duration_since(t) < WAKE_INTERRUPT_AEC_GRACE) {
        return false;
    }
    wake_prob >= WAKE_INTERRUPT_MIN_PROB
}

fn samples_to_ms(samples: u64) -> u64 {
    samples * 1000 / RATE as u64
}

/// Диагностические счётчики передачи начального аудиобуфера в голосовую сессию (без PCM и текста).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct InitialAudioStats {
    pub preroll_samples: usize,
    pub preroll_fed_samples: usize,
    pub hold_samples: usize,
    pub fed_samples: usize,
    pub dropped_preroll_samples: usize,
    pub dropped_hold_samples: usize,
    pub has_wake_reply: bool,
    pub heard_input: bool,
    pub aborted: bool,
}

impl InitialAudioStats {
    fn to_json(self) -> serde_json::Value {
        serde_json::json!({
            "preroll_samples": self.preroll_samples,
            "preroll_ms": samples_to_ms(self.preroll_samples as u64),
            "preroll_fed_samples": self.preroll_fed_samples,
            "preroll_fed_ms": samples_to_ms(self.preroll_fed_samples as u64),
            "hold_samples": self.hold_samples,
            "hold_ms": samples_to_ms(self.hold_samples as u64),
            "fed_samples": self.fed_samples,
            "fed_ms": samples_to_ms(self.fed_samples as u64),
            "dropped_preroll_samples": self.dropped_preroll_samples,
            "dropped_preroll_ms": samples_to_ms(self.dropped_preroll_samples as u64),
            "dropped_hold_samples": self.dropped_hold_samples,
            "dropped_hold_ms": samples_to_ms(self.dropped_hold_samples as u64),
            "has_wake_reply": self.has_wake_reply,
            "heard_input": self.heard_input,
            "aborted": self.aborted,
        })
    }
}

#[derive(Default)]
struct InitialAudioDiag {
    activations: u64,
    with_wake_reply: u64,
    without_wake_reply: u64,
    aborted: u64,
    total_hold_samples: u64,
    total_fed_samples: u64,
    total_dropped_preroll_samples: u64,
    total_dropped_hold_samples: u64,
    last: Option<InitialAudioStats>,
}

impl InitialAudioDiag {
    fn record(&mut self, stats: InitialAudioStats) {
        self.activations = self.activations.saturating_add(1);
        if stats.aborted {
            self.aborted = self.aborted.saturating_add(1);
        } else if stats.has_wake_reply {
            self.with_wake_reply = self.with_wake_reply.saturating_add(1);
        } else {
            self.without_wake_reply = self.without_wake_reply.saturating_add(1);
        }
        self.total_hold_samples = self.total_hold_samples.saturating_add(stats.hold_samples as u64);
        self.total_fed_samples = self.total_fed_samples.saturating_add(stats.fed_samples as u64);
        self.total_dropped_preroll_samples = self
            .total_dropped_preroll_samples
            .saturating_add(stats.dropped_preroll_samples as u64);
        self.total_dropped_hold_samples = self
            .total_dropped_hold_samples
            .saturating_add(stats.dropped_hold_samples as u64);
        self.last = Some(stats);
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "activations": self.activations,
            "with_wake_reply": self.with_wake_reply,
            "without_wake_reply": self.without_wake_reply,
            "aborted": self.aborted,
            "total_hold_ms": samples_to_ms(self.total_hold_samples),
            "total_fed_ms": samples_to_ms(self.total_fed_samples),
            "total_dropped_preroll_ms": samples_to_ms(self.total_dropped_preroll_samples),
            "total_dropped_hold_ms": samples_to_ms(self.total_dropped_hold_samples),
            "last": self.last.map(InitialAudioStats::to_json),
        })
    }
}

/// Атомарная передача накопленного после wake word аудио (`st.hold`) и (если нет локального отклика)
/// хвоста `preroll` в голосовую сессию строго до публикации `st.live`.
/// Гарантирует сохранение порядка кадров, отсутствие дублей и сохранность `st.hold` при наличии кеша.
fn handoff_initial_audio(
    st: &mut St,
    engine: &str,
    preroll: &[i16],
    opening_at: Instant,
    mic_boost_db: i64,
    has_candidate_reply: bool,
    session: &Arc<dyn Session>,
) -> (bool, InitialAudioStats) {
    let hold_samples = st.hold.len();
    if st.aborted {
        st.hold.clear();
        st.holding = false;
        let stats = InitialAudioStats {
            preroll_samples: preroll.len(),
            preroll_fed_samples: 0,
            hold_samples,
            fed_samples: 0,
            dropped_preroll_samples: preroll.len(),
            dropped_hold_samples: hold_samples,
            has_wake_reply: false,
            heard_input: false,
            aborted: true,
        };
        return (false, stats);
    }

    let heard_input = st.input_at.is_some_and(|at| at >= opening_at);
    let play_reply = has_candidate_reply && !heard_input;

    // Если играем локальный отклик («Да?», «Слушаю.»), не пересылаем старый preroll с самим
    // словом активации, но ВСЕ накопленные после активации кадры из st.hold обязательно отдаём модели.
    let preroll_slice: &[i16] = if play_reply {
        &[]
    } else {
        let before = match engine {
            "chatgpt" => preroll.len().saturating_sub(RATE / 4),
            "gemini" => preroll.len().saturating_sub(RATE * 4 / 5),
            _ => 0,
        };
        &preroll[before..]
    };

    let mut first = Vec::with_capacity(preroll_slice.len() + st.hold.len());
    first.extend_from_slice(preroll_slice);
    first.extend_from_slice(&st.hold);

    if heard_input {
        session.dialog().heard_input(Instant::now());
    }
    if !first.is_empty() {
        session.feed(&boost(&first, mic_boost_db));
    }

    let stats = InitialAudioStats {
        preroll_samples: preroll.len(),
        preroll_fed_samples: preroll_slice.len(),
        hold_samples,
        fed_samples: first.len(),
        dropped_preroll_samples: preroll.len().saturating_sub(preroll_slice.len()),
        dropped_hold_samples: 0,
        has_wake_reply: play_reply,
        heard_input,
        aborted: false,
    };

    st.hold.clear();
    st.holding = false;
    st.live = Some(session.clone());
    (play_reply, stats)
}

#[derive(Default)]
struct EchoWindow {
    queued_until: Option<Instant>,
    mute_until: Option<Instant>,
}

impl EchoWindow {
    fn played(&mut self, now: Instant, pcm: &[i16], rate: usize) {
        self.played_with_tail(now, pcm, rate, ECHO_TAIL);
    }

    fn played_with_tail(&mut self, now: Instant, pcm: &[i16], rate: usize, tail: Duration) {
        let start = self.queued_until.map_or(now, |end| end.max(now));
        let end = start + Duration::from_secs_f64(pcm.len() as f64 / rate as f64);
        self.queued_until = Some(end);
        if peak(pcm) > 100.0 {
            self.mute_until = Some(end + tail);
        }
    }

    fn muted(&self, now: Instant) -> bool {
        self.mute_until.is_some_and(|until| until > now)
    }

    fn stopped(&mut self, now: Instant) {
        self.stopped_with_tail(now, ECHO_TAIL);
    }

    fn stopped_with_tail(&mut self, now: Instant, tail: Duration) {
        self.queued_until = None;
        if self.muted(now) {
            self.mute_until = Some(now + tail);
        }
    }
}

pub struct Engine {
    pub store: Store,
    pub host: Arc<Host>,
    pub proxy: Arc<crate::vpn::Proxy>,
    pub gemini_pool: Arc<crate::gemini_pool::GeminiPool>,
    pub mic: Arc<Mic>,
    pub player: Arc<Player>,
    pub leds: Arc<Leds>,
    pub journal: Arc<Journal>,
    pub tools: Arc<Tools>,
    pub music: Arc<crate::music::Music>,
    pub listener: Arc<Listener>,
    pub data: PathBuf,
    rt: tokio::runtime::Handle,
    st: Mutex<St>,
    reset_wake: AtomicBool,
    recording_word: AtomicBool,
    training_active: AtomicBool,
    training_until: Mutex<Option<Instant>>,
    speech_epoch: AtomicU64,
    speech_notify: tokio::sync::Notify,
    saying: AtomicBool,
    previews: voice_preview::Previews,
    wake_replies: wake_reply::WakeReplyManager,
    wake_reply_sequence: AtomicU64,
    cmds: std::sync::OnceLock<crate::cmdspot::CmdSpotter>,
    last_error_notice: Mutex<Option<(String, Instant)>>,
    error_speaking: AtomicBool,
    initial_audio: Mutex<InitialAudioDiag>,
}

fn same(a: &Arc<dyn Session>, b: &Arc<dyn Session>) -> bool {
    std::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b))
}

pub fn peak(pcm: &[i16]) -> f64 {
    pcm.iter().map(|v| v.unsigned_abs()).max().unwrap_or(0) as f64
}

pub fn loud(pcm: &[i16]) -> bool {
    pcm.iter().any(|&v| !(-600..=600).contains(&v))
}

struct InputActivity {
    background: f32,
    sustained: u32,
}

impl Default for InputActivity {
    fn default() -> Self {
        Self { background: 24.0, sustained: 0 }
    }
}

impl InputActivity {
    fn observe(&mut self, pcm: &[i16], listening: bool, boost_db: i64) -> bool {
        const SUSTAINED_NOISE_FRAMES: u32 = 200; // 4.0 с непрерывного фона без пауз (слово «Джарвис» ~0.7 с)
        let scale = if boost_db > 0 {
            10f32.powf(boost_db.min(24) as f32 / 20.0)
        } else {
            1.0
        };
        let min_level = (28.0 / scale).max(8.0);
        let min_sample = (75.0 / scale).max(20.0);
        let mut active = false;
        for frame in pcm.chunks(RATE / 50).filter(|frame| frame.len() >= RATE / 250) {
            let level = frame.iter().map(|v| u32::from(v.unsigned_abs())).sum::<u32>() as f32 / frame.len() as f32;
            let level_threshold = (self.background * 1.8).max(min_level);
            let sample_threshold = (self.background * 2.2).max(min_sample) as u16;
            let speech = level > level_threshold
                && frame.iter().filter(|v| v.unsigned_abs() > sample_threshold).count() >= frame.len() / 12;
            if !speech {
                self.sustained = 0;
                let rate = if level < self.background { 0.08 } else { 0.02 };
                let step = (level - self.background).clamp(-self.background, self.background.max(12.0));
                self.background = (self.background + step * rate).max(4.0);
            } else if !listening {
                // Не даём слову активации и короткой речи в комнате задирать фоновый порог,
                // но если шум держится непрерывно >4 с (вентилятор/вытяжка), плавно подстраиваемся.
                self.sustained = self.sustained.saturating_add(1);
                if self.sustained > SUSTAINED_NOISE_FRAMES {
                    let step = (level - self.background).clamp(0.0, self.background.max(20.0));
                    self.background += step * 0.02;
                }
            } else {
                self.sustained = 0;
            }
            active |= speech;
        }
        active
    }
}

/// Копия звука с усилением db (с ограничением по краям шкалы).
pub fn boost(pcm: &[i16], db: i64) -> Vec<i16> {
    if db <= 0 {
        return pcm.to_vec();
    }
    let k = 10f64.powf(db as f64 / 20.0);
    pcm.iter().map(|&v| (v as f64 * k).clamp(-32767.0, 32767.0) as i16).collect()
}

fn short(e: &str) -> String {
    if e.chars().count() > 160 { format!("{}…", e.chars().take(160).collect::<String>()) } else { e.into() }
}

fn command_settled(schedule: fastcmd::AliceSchedule, is_stop: bool, since: Duration, quiet_for: Duration) -> bool {
    match schedule {
        fastcmd::AliceSchedule::None if is_stop => since >= Duration::from_millis(350),
        fastcmd::AliceSchedule::None => since >= Duration::from_millis(450)
            && (quiet_for >= Duration::from_millis(450) || since >= Duration::from_millis(1600)),
        fastcmd::AliceSchedule::Ready => since >= Duration::from_millis(750)
            && (quiet_for >= Duration::from_millis(550) || since >= Duration::from_millis(2200)),
        fastcmd::AliceSchedule::Pending => since >= Duration::from_millis(2200)
            && quiet_for >= Duration::from_millis(750),
    }
}

fn lang_name(l: &str) -> &'static str {
    match l {
        "ru" => "русском",
        "en" => "английском",
        "uk" => "украинском",
        _ => "",
    }
}

/// Чего не хватает, чтобы говорить выбранным движком ("" — всё на месте).
pub fn key_missing(host: &Host, s: &Settings, engine: &str) -> String {
    let id = match engine {
        "" => return "Выберите голосовой движок в настройках ассистента".into(),
        "live" => "openai",
        "eleven_realtime" => "elevenlabs",
        id => id,
    };
    if host.provider(id).is_err() {
        return format!("Установите и включите плагин «{id}» в «Подключениях»");
    }
    match engine {
        "chatgpt" if !chatgpt::signed_in(&s.chatgpt) => "не выполнен вход в ChatGPT — войдите в «Подключениях»".into(),
        "chatgpt" => chatgpt::voice_limit().unwrap_or_default(),
        "live" | "openai" => if s.openai_key.is_empty(){"Добавьте ключ OpenAI в «Подключениях»".into()}else{String::new()},
        "gemini" => if s.gemini_key.is_empty() && s.gemini_keys.is_empty(){"Добавьте ключ Gemini в «Подключениях»".into()}else{String::new()},
        "grok" => if s.grok_key.is_empty(){"Добавьте ключ Grok в «Подключениях»".into()}else{String::new()},
        "elevenlabs"|"eleven_realtime"=>if s.elevenlabs_key.is_empty(){"Добавьте ключ ElevenLabs в «Подключениях»".into()}else{String::new()},
        other => format!("движок «{other}» ещё не перенесён в новое ядро — выберите ассистента на ChatGPT"),
    }
}

pub fn stt_missing(host: &Host, s: &Settings, p: &Profile) -> String {
    if !p.is_classic() {
        return key_missing(host, s, &p.engine);
    }
    match p.effective_stt_engine() {
        "openai" => key_missing(host, s, "openai"),
        "elevenlabs" => key_missing(host, s, "elevenlabs"),
        id => {
            if host.supports_stt(id) {
                String::new()
            } else {
                format!("Установите и включите плагин распознавания «{id}» в «Подключениях»")
            }
        }
    }
}

pub fn tts_missing(host: &Host, s: &Settings, p: &Profile) -> String {
    if !p.is_classic() {
        return key_missing(host, s, &p.engine);
    }
    match p.effective_tts_engine() {
        "openai" => key_missing(host, s, "openai"),
        "elevenlabs" => key_missing(host, s, "elevenlabs"),
        id => {
            if host.supports_tts(id) {
                String::new()
            } else {
                format!("Установите и включите плагин озвучки «{id}» в «Подключениях»")
            }
        }
    }
}

pub fn llm_missing(host: &Host, s: &Settings, p: &Profile) -> String {
    if !p.is_classic() {
        return key_missing(host, s, &p.engine);
    }
    match p.effective_llm_engine() {
        "" => {
            let any = (host.provider("openai").is_ok() && !s.openai_key.is_empty())
                || (host.provider("gemini").is_ok() && (!s.gemini_key.is_empty() || !s.gemini_keys.is_empty()))
                || (host.provider("grok").is_ok() && !s.grok_key.is_empty())
                || (host.provider("chatgpt").is_ok() && chatgpt::signed_in(&s.chatgpt));
            if any {
                String::new()
            } else {
                "Добавьте API-ключ или войдите в ChatGPT в «Подключениях»".into()
            }
        }
        "chatgpt" => {
            if host.provider("chatgpt").is_err() {
                "Установите и включите плагин «chatgpt» в «Подключениях»".into()
            } else if !chatgpt::signed_in(&s.chatgpt) {
                "не выполнен вход в ChatGPT — войдите в «Подключениях»".into()
            } else {
                String::new()
            }
        }
        id @ ("openai" | "live" | "gemini" | "grok") => key_missing(host, s, id),
        id => {
            if host.supports_chat(id) {
                String::new()
            } else {
                format!("Включите текстовый плагин «{id}» в «Подключениях»")
            }
        }
    }
}

pub fn profile_missing(host: &Host, s: &Settings, p: &Profile) -> String {
    if !p.is_classic() {
        return key_missing(host, s, &p.engine);
    }
    for msg in [stt_missing(host, s, p), llm_missing(host, s, p), tts_missing(host, s, p)] {
        if !msg.is_empty() {
            return msg;
        }
    }
    String::new()
}

pub struct WakeRecording(Arc<Engine>);

impl Drop for WakeRecording {
    fn drop(&mut self) {
        self.0.reset_wake.store(true, Ordering::Release);
        self.0.recording_word.store(false, Ordering::Release);
    }
}

impl Engine {
    pub fn recording_word(self: &Arc<Self>) -> WakeRecording {
        self.recording_word.store(true, Ordering::Release);
        WakeRecording(self.clone())
    }

    pub fn set_background_training(&self, minutes: Option<u32>) {
        self.training_active.store(minutes.is_some(), Ordering::Release);
        *self.training_until.lock().unwrap() = minutes.map(|mins| Instant::now() + Duration::from_secs(u64::from(mins) * 60 + 1));
    }
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rt: tokio::runtime::Handle,
        store: Store,
        host: Arc<Host>,
        proxy: Arc<crate::vpn::Proxy>,
        mic: Arc<Mic>,
        player: Arc<Player>,
        leds: Arc<Leds>,
        journal: Arc<Journal>,
        tools: Arc<Tools>,
        listener: Arc<Listener>,
        data: PathBuf,
        music: Arc<crate::music::Music>,
    ) -> Arc<Engine> {
        let gemini_pool = crate::gemini_pool::GeminiPool::load(data.join("gemini-quota.json"));
        Arc::new(Engine {
            store, host, proxy, gemini_pool, mic, player, leds, journal, tools, music, listener, data, rt,
            st: Mutex::default(), reset_wake: AtomicBool::new(false),
            recording_word: AtomicBool::new(false), training_active: AtomicBool::new(false), training_until: Mutex::new(None),
            speech_epoch: AtomicU64::new(0), speech_notify: tokio::sync::Notify::new(), saying: AtomicBool::new(false), previews: Default::default(),
            wake_replies: Default::default(), wake_reply_sequence: AtomicU64::new(0), cmds: Default::default(),
            last_error_notice: Mutex::default(), error_speaking: AtomicBool::new(false),
            initial_audio: Mutex::default(),
        })
    }

    /// Не содержащие речи счётчики передачи начального буфера (для диагностики issue #35).
    pub fn initial_audio_diag(&self) -> serde_json::Value {
        self.initial_audio.lock().unwrap().to_json()
    }

    /// Слушать микрофон в своём потоке: слово, звук в разговор, перебивание.
    pub fn run(self: &Arc<Self>, mut det: Box<dyn Wake>) {
        let w = Arc::downgrade(self);
        if let Some(c) = crate::cmdspot::CmdSpotter::new(
            &self.data,
            Arc::new(move |p: String| {
                if let Some(e) = w.upgrade() {
                    e.on_command(&p);
                }
            }),
        ) {
            let _ = self.cmds.set(c);
        }
        let e = self.clone();
        let training = crate::training_io::AudioBatch::channel(&self.rt, self.host.clone());
        std::thread::Builder::new()
            .name("engine".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                let (_, rx) = e.mic.subscribe(256);
                let mut batch = crate::training_io::AudioBatch::default();
                let mut activity = InputActivity::default();
                for raw in rx {
                    let training_enabled = e.host.wake_training();
                    if let Some(replacement)=e.listener.take_reload(&e.data,&e.store,training_enabled){det=replacement;}
                    let (live, busy, muted_input, input) = {
                        let mut st = e.st.lock().unwrap();
                        if st.ring.capacity() < 3 * RATE + RATE / 25 {
                            st.ring.reserve(3 * RATE + RATE / 25);
                        }
                        if raw.len() >= 3 * RATE {
                            st.ring.clear();
                            st.ring.extend(&raw[raw.len() - 3 * RATE..]);
                        } else {
                            let overflow = (st.ring.len() + raw.len()).saturating_sub(3 * RATE);
                            if overflow > 0 {
                                st.ring.drain(..overflow);
                            }
                            st.ring.extend(&raw[..]);
                        }
                        if loud(&raw) {
                            st.last_loud = Some(Instant::now());
                        }
                        if st.holding && st.live.is_none() && st.hold.len() < 20 * RATE {
                            st.hold.extend_from_slice(&raw);
                        }
                        // Эхо своего голоса уже вычел Yandex VQE: его опорный сигнал — петля с выхода
                        // на динамик (hw:0,0), куда идёт и наш aplay. Микрофон приходит уже чистым.
                        (st.live.clone(), st.busy, st.echo.muted(Instant::now()), raw.clone())
                    };
                    let boost_db = e.store.mic_boost_db();
                    if !muted_input && activity.observe(&input, busy, boost_db) {
                        let now = Instant::now();
                        e.st.lock().unwrap().input_at = Some(now);
                        if let Some(session) = &live {
                            session.dialog().heard_input(now);
                        }
                    }
                    if busy || live.is_some() {
                        e.leds.hear(peak(&raw) / 9000.0);
                    }
                    if let Some(c) = e.cmds.get() {
                        if busy || live.is_some() || e.music_playing() {
                            c.feed(&raw);
                        } else {
                            c.idle();
                        }
                    }
                    if e.reset_wake.swap(false, Ordering::Relaxed) {
                        det.reset();
                    }
                    let enabled = e.store.wake_enabled() && !e.recording_word.load(Ordering::Acquire);
                    let ready = e.listener.ready.load(Ordering::Relaxed);
                    let hit = ready && enabled && det.feed(&raw); // поток признаков непрерывный, и во время разговора
                    let prob = det.last_prob();
                    e.listener.last_prob.store(prob.to_bits(), Ordering::Relaxed);
                    if training_enabled {
                        let collect = e.store.train_mode() && e.listener.is_default_phrase();
                        let background = if e.training_active.load(Ordering::Relaxed) {
                            let mut until = e.training_until.lock().unwrap();
                            match *until {
                                Some(t) if t > Instant::now() => true,
                                _ => {
                                    *until = None;
                                    e.training_active.store(false, Ordering::Release);
                                    false
                                }
                            }
                        } else {
                            false
                        };
                        let wake_id = (!busy && live.is_none() && hit)
                            .then(|| format!("w{}", crate::settings::rand_hex(6)));
                        if collect || background {
                            let limit = e.store.train_limit_mb();
                            let discard = busy || live.is_some() || e.saying.load(Ordering::Relaxed) || e.player.playing() || !ready || !enabled;
                            if let Some(frame) = batch.push(&raw, hit, wake_id.as_deref(), discard, prob, collect, limit) {
                                let _ = training.try_send(frame);
                            }
                        } else {
                            batch.reset();
                        }
                        if live.is_none() && !busy && hit {
                            let p = det.wake_prob();
                            det.reset(); // хвост этого же слова не должен сработать ещё раз
                            e.trigger_with_wake("слово", p, wake_id);
                            continue;
                        }
                    } else {
                        batch.reset();
                    }
                    if let Some(live) = live {
                        if muted_input {
                            // Preserve the input clock for the provider's turn detector.
                            static ZERO: [i16; 320] = [0; 320];
                            if raw.len() <= ZERO.len() {
                                live.feed(&ZERO[..raw.len()]);
                            } else {
                                live.feed(&vec![0; raw.len()]);
                            }
                        } else if boost_db > 0 {
                            live.feed(&boost(&input, boost_db));
                        } else {
                            live.feed(&input);
                        }
                        // «Джарвис» во время разговора — как у Алисы: сразу замолкает и слушает
                        if hit {
                            let now = Instant::now();
                            let (recent, playback_started_at) = {
                                let st = e.st.lock().unwrap();
                                (
                                    st.last_wake.is_some_and(|t| now.saturating_duration_since(t) < Duration::from_millis(2500)),
                                    st.playback_started_at,
                                )
                            };
                            let wake_prob = det.wake_prob();
                            if should_wake_interrupt(
                                e.store.wake_interrupt(),
                                e.player.playing(),
                                recent,
                                playback_started_at,
                                wake_prob,
                                now,
                            ) {
                                det.reset();
                                e.interrupt(&live, wake_prob);
                            }
                        }
                        continue;
                    }
                    if busy {
                        continue;
                    }
                    if hit {
                        let p = det.wake_prob();
                        det.reset(); // хвост этого же слова не должен сработать ещё раз
                        e.trigger("слово", p);
                    }
                }
            })
            .expect("поток движка");
        self.watch_alice();
    }

    /// Системное сообщение колонки: сразу локальный сигнал (без сети и ключей — подтверждение есть всегда),
    /// затем голосом. Озвучка голосом ассистента ещё не перенесена — пока говорит штатная Алиса.
    pub fn system_notice(&self, chime: &[i16], text: &str, alice_text: &str) {
        if self.player.start(RATE).is_ok() {
            let _ = self.player.write(&vec![0; RATE / 4]); // тракт просыпается
            let _ = self.player.write(chime);
            self.player.finish(Duration::from_secs(4));
        }
        log::info!("системное сообщение: {text}");
        if !alice_text.is_empty()
            && let Err(e) = alice::text(alice_text, false)
        {
            log::info!("системное сообщение: Алиса недоступна: {e}");
        }
    }

    /// Состояние из событий музыкального модуля: без запросов на каждом кадре микрофона.
    fn music_playing(&self) -> bool {
        self.music.playing()
    }

    /// Встроенный распознаватель услышал команду, пока Джарвис занят.
    fn on_command(&self, phrase: &str) {
        let w = phrase.to_lowercase();
        let has = |s: &str| w.contains(s);
        if !self.talking() {
            if !self.music_playing() { return; }
            if has("стоп") || has("пауза") {
                let op = if has("пауза") { "pause" } else { "stop" };
                let _ = self.rt.block_on(self.music.transport(&self.host, op, serde_json::Value::Null));
                return;
            }
            if has("громче") || has("тише") {
                let level = crate::speakervol::speaker_volume().unwrap_or(5) + if has("тише") { -1 } else { 1 };
                let _ = crate::speakervol::set_alice_volume(level.clamp(0,10));
            }
            return;
        }
        if fastcmd::is_stop(phrase) || has("хватит") || has("стоп") {
            log::info!("Джарвис: «{phrase}» — разговор закрыт, музыка не тронута (встроенный распознаватель)");
            self.end_chat(&format!("сказали «{phrase}»"));
            self.journal.note("стоп", phrase, "");
        } else if has("пауза") {
            log::info!("Джарвис: «{phrase}» — стоп (встроенный распознаватель)");
            self.stop(&format!("сказали «{phrase}»"));
            self.journal.note("стоп", phrase, "");
        } else if has("громче") || has("тише") {
            let step = if has("тише") { -10 } else { 10 };
            let mut v = 0;
            // громкость голоса активного ассистента
            let _ = self.store.update(|s| -> Result<(), std::io::Error> {
                let id = s.active_profile().id.clone();
                if let Some(p) = s.profiles.iter_mut().find(|p| p.id == id) {
                    p.volume = (p.volume + step).clamp(10, 100);
                    v = p.volume;
                }
                Ok(())
            });
            log::info!("Джарвис: «{phrase}» — громкость {v}% (встроенный распознаватель)");
        } else {
            log::info!("Джарвис: распознаватель услышал «{phrase}» — не моя команда");
        }
    }

    /// Идёт ли разговор (или он открывается).
    pub fn talking(&self) -> bool {
        let st = self.st.lock().unwrap();
        st.busy || st.live.is_some() || self.saying.load(Ordering::Relaxed)
    }

    /// Начать разговор (по слову, по кнопке в настройках).
    pub fn trigger(self: &Arc<Self>, source: &str, prob: f32) {
        self.trigger_with_wake(source, prob, None);
    }

    fn trigger_with_wake(self: &Arc<Self>, source: &str, prob: f32, wake_id: Option<String>) {
        if source == "слово" && self.recording_word.load(Ordering::Acquire) { return; }
        let (clip, wake_at) = {
            let mut st = self.st.lock().unwrap();
            if st.busy || st.last_wake.is_some_and(|t| t.elapsed() < Duration::from_millis(1500)) {
                return;
            }
            let now = Instant::now();
            st.busy = true;
            st.echo = EchoWindow::default();
            st.playback_started_at = None;
            st.last_wake = Some(now);
            st.aborted = false;
            st.hold.clear();
            st.holding = true;
            (st.ring.make_contiguous().to_vec(), now)
        };
        let by_word = source == "слово";
        // как у Алисы: услышал слово — сразу кольцо и разговор; проверка имени расшифровкой — по галочке
        let sure = !by_word || prob >= 0.9 || !self.store.with(|s| s.wake_check);
        if sure {
            self.leds.set(Mode::Listening);
        }
        self.speech_epoch.fetch_add(1, Ordering::SeqCst);
        self.cancel_wake_reply_generation();
        self.reset_wake.store(true, Ordering::Relaxed);
        if by_word {
            log::info!("Джарвис: активация ({source}, уверенность {prob:.2})");
        } else {
            log::info!("Джарвис: активация ({source})");
        }
        let e = self.clone();
        std::thread::Builder::new()
            .name("converse".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                struct ConverseGuard<'a> {
                    engine: &'a Engine,
                    completed: bool,
                }
                impl Drop for ConverseGuard<'_> {
                    fn drop(&mut self) {
                        if !self.completed {
                            self.engine.player.stop(true);
                            if let Some(l) = self.engine.st.lock().unwrap_or_else(|p| p.into_inner()).live.take() {
                                l.close();
                            }
                            let mut st = self.engine.st.lock().unwrap_or_else(|p| p.into_inner());
                            st.pending_wake = false;
                            st.busy = false;
                            st.live = None;
                            st.aborted = false;
                            st.hold.clear();
                            st.holding = false;
                            st.echo = EchoWindow::default();
                            st.playback_started_at = None;
                            self.engine.reset_wake.store(true, Ordering::Relaxed);
                            self.engine.leds.set(Mode::Off);
                        }
                    }
                }
                let mut guard = ConverseGuard { engine: &e, completed: false };
                let preroll = if by_word { clip.clone() } else { vec![] };
                let failure = e.converse(by_word, sure, preroll, clip, wake_at, prob, wake_id);
                guard.completed = true;
                let queued = {
                    let mut st = e.st.lock().unwrap();
                    let q = st.pending_wake;
                    st.pending_wake = false;
                    st.busy = false;
                    st.live = None;
                    st.aborted = false;
                    st.hold.clear();
                    st.holding = false;
                    st.echo = EchoWindow::default();
                    st.playback_started_at = None;
                    q
                };
                e.reset_wake.store(true, Ordering::Relaxed);
                e.leds.set(Mode::Off);
                if let Some(failure) = failure && !queued {
                    e.announce_voice_failure(failure);
                }
                if queued {
                    e.trigger("слово", 1.0);
                }
            })
            .expect("поток разговора");
    }

    fn instructions(&self, s: &Settings, p: &Profile) -> String {
        let phrase = self.listener.phrase();
        let mut text = s.prompt_text(&p.prompt);
        if p.prompt == "jarvis" || p.prompt == "bro" {
            text = text.replacen("Ты — Джарвис", &format!("Ты — «{phrase}»"), 1);
        }
        let persona = text.clone();
        let n = lang_name(&s.language);
        if !n.is_empty() {
            text += &format!(
                "\n\nХозяин всегда говорит на {n} языке. Понимай его речь как {n}, даже если она звучит неразборчиво или похоже на другой язык, и отвечай только на {n}."
            );
        }
        if !s.city.is_empty() {
            text += &format!("\n\nХозяин живёт в городе {} — погода, время и новости по умолчанию для него.", s.city);
        }
        text += "\n\nЕсли тебя просят замолчать или закончить разговор, не отвечай вслух и не вызывай инструменты. Останавливай музыку только по явной просьбе про музыку.\n\nКогда для ответа нужен инструмент (поиск в интернете, погода, умный дом, музыка, сложная задача), \
можно ОДИН раз за ответ коротко сказать вслух живую фразу — «Секунду, гляну», «Сейчас поищу» — и сразу вызывать инструмент. \
Если вызываешь несколько инструментов подряд или повторяешь вызов — больше никаких таких фраз: молча дождись результата и скажи итог. \
На простые вопросы и быстрые команды (включить свет, громкость) отвечай сразу, без таких фраз.";
        if s.is_night() {
            text += "\n\nСейчас ночь, в доме могут спать: отвечай коротко и спокойно, без длинных списков.";
        }
        if p.engine == "gemini" {
            text += "\n\nЕсли человек только позвал тебя или пока думает и тянет «эээ», «ааа», дождись продолжения. Отвечай, когда он закончил мысль.";
            if p.web_search {
                text += " Используй Google Search, если просят поискать или нужны актуальные сведения. Если поиск ничего не нашёл, скажи об этом.";
            }
        }
        match &self.tools.memory {
            // общая память разговора: у всех ассистентов, переживает перезапуск
            Some(m) => text += &crate::memory::instructions(m, p.engine == "chatgpt"),
            None => {
                let h = self.journal.history();
                if !h.is_empty() {
                    let ctx: Vec<_> = h.iter().map(|t| serde_json::json!({"user": t.text, "jarvis": t.answer})).collect();
                    text += &format!("\n\nПредыдущие реплики разговора: {}", serde_json::Value::Array(ctx));
                }
            }
        }
        if p.engine == "chatgpt" && self.tools.memory.is_some() {
            // напоминание после истории: в длинном чате модель иначе сползает в обычный стиль ChatGPT
            let persona = persona.trim();
            let persona = if persona.chars().count() > 2500 { format!("{}…", persona.chars().take(2500).collect::<String>()) } else { persona.to_owned() };
            text += &format!(
                "\n\nГлавное, не забывай: ты — голосовой ассистент «{phrase}» внутри умной колонки Яндекс Станция у хозяина дома, \
а не ChatGPT в приложении. Говоришь вслух, коротко, своим характером. Что делать на колонке (музыка, YouTube, свет, \
поиск, погода, таймеры, будильники, напоминания) — передаёшь бэкенду, он делает: таймеры и будильники ставит Алиса \
на этой же колонке, так что не говори, что не умеешь. Твой характер:\n{persona}"
            );
        }
        text + rules::SPEAKER_CONTENT_RULES + rules::SPEAKER_WAIT_RULES + &rules::identity(&phrase)
    }

    /// Кто думает и управляет домом, когда голосовая модель передаёт задачу: модель подписки ChatGPT.
    fn brain(&self, p: &Profile) -> chatgpt::Brain {
        let (host, tools, p) = (self.host.clone(), self.tools.clone(), p.clone());
        Arc::new(move |task: String| {
            let (host, tools, p) = (host.clone(), tools.clone(), p.clone());
            Box::pin(async move {
                // вход и подготовка дома параллельно; инструменты зависят от загруженного каталога
                let t0 = Instant::now();
                let ((ctx, defs), auth) = tokio::join!(tools.prepare(), async {
                    host.provider("chatgpt")?.request("auth.token", serde_json::Value::Null, Duration::from_secs(20)).await
                });
                log::info!("бэкенд: контекст и инструменты за {:.2} с", t0.elapsed().as_secs_f64());
                let instr = format!("{}{ctx}", rules::BACKEND_RULES);
                chatgpt::ask_codex_with_history_detail(&host, auth?, &p, &instr, &[], &task, Some(&tools), defs).await
            })
        })
    }

    fn chat_thread(&self) -> String {
        if let Some(m) = &self.tools.memory {
            return m.thread();
        }
        std::fs::read(self.data.join("chatgpt-chat.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v.get("id").and_then(|x| x.as_str()).map(String::from))
            .unwrap_or_default()
    }

    /// Открыть разговор с моделью тем движком, который выбран у ассистента.
    fn new_session(&self, s: &Settings, p: &Profile) -> Arc<dyn Session> {
        if p.is_classic() {
            return crate::classic_voice::Classic::open_with(
                &self.rt,
                self.host.clone(),
                self.tools.clone(),
                self.journal.clone(),
                s.clone(),
                p.clone(),
                self.instructions(s, p),
                self.listener.phrase(),
                Some(self.gemini_pool.clone()),
                Some(self.proxy.clone()),
            );
        }
        if ["", "live", "openai", "gemini", "grok", "eleven_realtime"].contains(&p.engine.as_str()) {
            return crate::cloud_voice::Voice::open_with(
                &self.rt,
                self.host.clone(),
                Some(self.tools.clone()),
                self.journal.clone(),
                s.clone(),
                p.clone(),
                self.instructions(s, p),
                crate::cloud_voice::VoiceOptions {
                    gemini_pool: Some(self.gemini_pool.clone()),
                    store: Some(self.store.clone()),
                    proxy: Some(self.proxy.clone()),
                    allow_failover: true,
                },
            );
        }
        chatgpt::ChatGptSession::open(
            &self.rt,
            chatgpt::Open {
                host: self.host.clone(),
                voice: p.voice.clone(),
                thread: self.chat_thread().into(),
                instructions: self.instructions(s, p),
                brain: Some(self.brain(p)),
                journal: self.journal.clone(),
                memory: self.tools.memory.clone(),
                proxy: Some(self.proxy.clone()),
            },
        )
    }

    /// Выбранная текстовая модель памяти или очередь подключённых провайдеров в режиме автовыбора.
    pub fn summarizer(&self) -> crate::memory::Summarizer {
        let (host, store, tools, gemini_pool) = (
            self.host.clone(),
            self.store.clone(),
            self.tools.clone(),
            self.gemini_pool.clone(),
        );
        Arc::new(move |instructions: String, text: String| {
            let (host, store, tools, gemini_pool) = (
                host.clone(),
                store.clone(),
                tools.clone(),
                gemini_pool.clone(),
            );
            Box::pin(async move {
                let s = store.get();
                let mut error = crate::memory::NO_SUMMARIZER.to_string();
                for provider in crate::llm::memory_candidates(&host, &s) {
                    let profile = Profile {
                        engine: provider.into(), backend_model: crate::llm::memory_model(&s, provider).into(),
                        effort: "low".into(), web_search: false, ..Default::default()
                    };
                    match crate::llm::ask_with_pool(&host, &s, &profile, &instructions, &text, &tools, vec![], Some(&gemini_pool)).await {
                        Ok(summary) => return Ok(summary),
                        Err(why) => error = why,
                    }
                }
                Err(error)
            })
        })
    }

    fn start_voice(&self, sess: &dyn Session) -> Result<(), String> {
        self.player.start(sess.rate()).map_err(|e| e.to_string())?;
        self.player.set_boost(sess.out_boost());
        Ok(())
    }

    fn earcon(&self, ok: bool) {
        if self.player.start(RATE).is_ok() {
            let _ = self.player.write(&fastcmd::earcon(ok));
            self.player.finish(Duration::from_secs(2));
        }
    }

    fn should_announce_failure(&self, failure: &crate::voice_error::VoiceFailure, now: Instant) -> bool {
        if failure.expected {
            return false;
        }
        let key = failure.cooldown_key();
        let mut last = self.last_error_notice.lock().unwrap();
        if let Some((prev_key, at)) = last.as_ref()
            && *prev_key == key
            && now.saturating_duration_since(*at) < Duration::from_secs(20)
        {
            return false;
        }
        *last = Some((key, now));
        true
    }

    /// Единый обработчик финального отказа реального голосового диалога (issue #34):
    /// короткий диагноз через штатный `START_VOCALIZE` Алисы без повторного вызова сломанного AI.
    fn announce_voice_failure(self: &Arc<Self>, failure: crate::voice_error::VoiceFailure) {
        if failure.expected {
            return;
        }
        log::warn!("{}", failure.log_line());
        if !self.should_announce_failure(&failure, Instant::now()) {
            return;
        }
        if !self.store.with(|s| s.voice_error_alice) {
            return;
        }
        if self.store.is_night() {
            log::info!("диагностика голоса: ночной режим — короткий сигнал вместо речи Алисы");
            self.earcon(false);
            return;
        }
        if self
            .error_speaking
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        let tz = self.store.timezone();
        let phrase = failure.alice_phrase(&tz);
        let epoch = self.speech_epoch.load(Ordering::SeqCst);
        let engine = self.clone();
        let _ = std::thread::Builder::new()
            .name("alice-diag".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                struct SpeakingGuard<'a>(&'a Engine);
                impl Drop for SpeakingGuard<'_> {
                    fn drop(&mut self) {
                        self.0.error_speaking.store(false, Ordering::SeqCst);
                    }
                }
                let _guard = SpeakingGuard(&engine);
                if engine.speech_epoch.load(Ordering::SeqCst) != epoch || engine.talking() {
                    return;
                }
                log::info!("диагностика Алисой: «{phrase}»");
                if let Err(err) = alice::vocalize(&phrase, Duration::from_secs(15), || {
                    engine.speech_epoch.load(Ordering::SeqCst) != epoch || engine.talking()
                }) {
                    log::info!("диагностика Алисой недоступна ({err}) — локальный сигнал");
                    if engine.speech_epoch.load(Ordering::SeqCst) == epoch && !engine.talking() {
                        engine.earcon(false);
                    }
                }
            });
    }

    /// Один разговор от слова (или кнопки) до тишины.
    /// Возвращает `Some(VoiceFailure)` только при реальном сбое после всех автопереключений.
    fn converse(
        self: &Arc<Self>,
        by_word: bool,
        sure: bool,
        preroll: Vec<i16>,
        clip: Vec<i16>,
        wake_at: Instant,
        wake_prob: f32,
        wake_id: Option<String>,
    ) -> Option<crate::voice_error::VoiceFailure> {
        let s = self.store.get();
        let p = s.active_profile();
        let provider_id = if p.is_classic() {
            "classic"
        } else {
            match p.engine.as_str() {
                "" | "live" => "openai",
                "eleven_realtime" => "elevenlabs",
                id => id,
            }
        };
        let missing = profile_missing(&self.host, &s, &p);
        if !missing.is_empty() {
            self.journal.fail(&missing);
            return Some(crate::voice_error::classify(
                provider_id,
                crate::voice_error::Stage::Config,
                &missing,
                None,
                false,
            ));
        }
        if !p.is_classic() && p.engine == "gemini" {
            let all_keys = crate::gemini_pool::effective_keys(&s.gemini_keys, &s.gemini_key);
            let models = crate::gemini_pool::selected_live_models(&p);
            let now = crate::gemini_pool::now_unix();
            if self.gemini_pool.candidates(&all_keys, &s.gemini_key, &models, now).is_empty() {
                let tz = self.store.timezone();
                let failure = self.gemini_pool.all_exhausted_failure(&all_keys, &models, now, &tz);
                log::warn!("Gemini: {} | Что делать: {}", failure.detail, failure.action);
                self.journal.fail(&failure.detail);
                return Some(failure);
            }
        }
        {
            let t = self.tools.clone();
            self.rt.spawn(async move { t.prewarm().await });
        }
        let idle = if p.idle_s < 2.0 { 8.0 } else { p.idle_s };
        let mut sess: Option<Arc<dyn Session>> = None;
        let mut out = None;
        let mut speaking = false;
        let mut answered = false;
        let mut user_spoke = !by_word;
        let mut checked = 0i64;
        let mut checked_text = String::new();
        let opening_at = wake_at;
        let mut ready_at = Instant::now();
        let mut last_voice: Option<Instant> = None;
        let mut close_armed_at: Option<Instant> = None;
        let close_after_flag = Arc::new(AtomicBool::new(false));
        let wake_ctx = (by_word
            && self.store.train_voice_review()
            && self.host.provider("training").is_ok()
            && self.listener.is_default_phrase()
            && !std::path::Path::new("/data/opt/jarvisd/no-mic").exists()
            && !clip.is_empty())
        .then(|| {
            let wid = wake_id.unwrap_or_else(|| format!("w{}", crate::settings::rand_hex(6)));
            crate::tools::ActiveWake {
                wake_id: wid,
                clip: clip.clone(),
                prob: wake_prob,
            }
        });
        let _session_guard = self.tools.enter_voice_session(crate::tools::VoiceSessionContext {
            close_after_reply: {
                let flag = close_after_flag.clone();
                Arc::new(move || flag.store(true, Ordering::SeqCst))
            },
            wake: wake_ctx,
        });
        let _ = sure;

        // открыть разговор с моделью и отдать ему весь звук с момента слова
        let open = |sess: &mut Option<Arc<dyn Session>>, out: &mut Option<std::sync::mpsc::Receiver<Vec<i16>>>, ready_at: &mut Instant| -> Result<(), Option<crate::voice_error::VoiceFailure>> {
            if by_word {
                self.leds.set(Mode::Thinking); // соединяюсь с моделью — бегущий огонёк
            }
            let x = self.new_session(&s, &p);
            let reply = if by_word && sure && s.wake_reply_enabled { self.cached_wake_reply(&p) } else { None };
            if reply.is_some() {
                // Network setup is already in progress while giving a joined phrase a head start.
                std::thread::sleep(wake_reply::FOLLOW_ON_GAP);
            }
            let (play_reply, init_stats) = {
                let mut st = self.st.lock().unwrap();
                handoff_initial_audio(
                    &mut st,
                    &p.engine,
                    &preroll,
                    opening_at,
                    s.mic_boost_db,
                    reply.is_some(),
                    &x,
                )
            };
            self.initial_audio.lock().unwrap().record(init_stats);
            log::debug!(
                "начальный буфер: preroll={}ms (в feed {}ms), hold={}ms, передано={}ms, отброшено preroll={}ms hold={}ms, отклик={}, речь={}",
                samples_to_ms(init_stats.preroll_samples as u64),
                samples_to_ms(init_stats.preroll_fed_samples as u64),
                samples_to_ms(init_stats.hold_samples as u64),
                samples_to_ms(init_stats.fed_samples as u64),
                samples_to_ms(init_stats.dropped_preroll_samples as u64),
                samples_to_ms(init_stats.dropped_hold_samples as u64),
                init_stats.has_wake_reply,
                init_stats.heard_input,
            );
            *sess = Some(x.clone());
            if init_stats.aborted {
                return Err(None);
            }
            if play_reply && let Some(sample) = reply {
                self.play_wake_reply(&sample, &p.engine, opening_at);
            }
            if self.st.lock().unwrap().aborted {
                return Err(None);
            }
            let ready_timeout = Duration::from_secs(if p.engine == "chatgpt" && self.store.with(|s| s.chatgpt_auto_switch) { 45 } else if p.engine == "gemini" { 22 } else { 12 });
            if let Err(e) = x.ready(ready_timeout) {
                if self.st.lock().unwrap().aborted {
                    return Err(None);
                }
                log::info!("разговор не открылся: {e}");
                x.dialog().finish_turn(&format!("разговор не открылся: {}", short(&e)));
                let vpn_blocked = matches!(self.proxy.route(provider_id), crate::vpn::Route::Blocked(_));
                let failure = x.last_failure().unwrap_or_else(|| {
                    crate::voice_error::classify(provider_id, crate::voice_error::Stage::Connect, &e, None, vpn_blocked)
                });
                return Err(Some(failure));
            }
            if let Err(e) = self.start_voice(&*x) {
                x.dialog().finish_turn(&format!("динамик: {e}"));
                return Err(Some(crate::voice_error::classify(
                    provider_id,
                    crate::voice_error::Stage::Audio,
                    &e,
                    None,
                    false,
                )));
            }
            *out = x.take_audio();
            *ready_at = Instant::now();
            if p.engine == "gemini" {
                log::info!("Gemini: разговор готов за {:.2} с", opening_at.elapsed().as_secs_f64());
            }
            x.dialog().touch();
            self.leds.set(Mode::Listening);
            Ok(())
        };
        let close = |sess: &Option<Arc<dyn Session>>| {
            if let Some(x) = sess {
                x.close();
            }
        };
        if let Err(failure) = open(&mut sess, &mut out, &mut ready_at) {
            self.player.stop(true);
            close(&sess);
            return failure;
        }
        let x = sess.clone().unwrap();
        let out = out.unwrap();
        let mut tick = Instant::now();
        loop {
            match out.recv_timeout(Duration::from_millis(20)) {
                Err(RecvTimeoutError::Disconnected) => {
                    self.player.stop(true);
                    let aborted = self.st.lock().unwrap().aborted || x.dialog().close_requested();
                    if aborted {
                        x.dialog().finish_turn("");
                        close(&sess);
                        return None;
                    }
                    if let Some(failure) = x.last_failure() {
                        x.dialog().finish_turn(&failure.detail);
                        close(&sess);
                        return Some(failure);
                    }
                    if !answered && (user_spoke || x.dialog().heard_input_since(opening_at) || x.dialog().user_turns() > 0) {
                        let failure = crate::voice_error::classify(
                            provider_id,
                            crate::voice_error::Stage::Stream,
                            "Голосовое соединение неожиданно закрылось",
                            None,
                            false,
                        );
                        x.dialog().finish_turn(&failure.detail);
                        close(&sess);
                        return Some(failure);
                    }
                    x.dialog().finish_turn("");
                    close(&sess);
                    return None;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Ok(pcm) => {
                    if !x.dialog().muted() {
                        let is_loud = loud(&pcm);
                        if !self.player.playing() && is_loud {
                            let _ = self.start_voice(&*x); // после перебивания плеер был остановлен
                        }
                        let wrote = self.player.write(&pcm).is_ok();
                        if wrote {
                            let now = Instant::now();
                            if p.engine == "gemini" {
                                self.st.lock().unwrap().echo.played(now, &pcm, x.rate());
                            }
                            if is_loud {
                                let mut st = self.st.lock().unwrap();
                                let onset = !speaking || st.playback_started_at.is_none();
                                if onset {
                                    st.playback_started_at = Some(now);
                                    if p.is_classic() {
                                        // Даём VQE AEC ~350 мс сойтись на старте TTS, чтобы Classic VAD не прерывал сам себя.
                                        st.echo.played_with_tail(now, &pcm, x.rate(), Duration::from_millis(350));
                                    }
                                }
                            }
                        }
                        if is_loud {
                            answered = true;
                            if p.engine == "gemini" && last_voice.is_none() {
                                log::info!("Gemini: первый голос через {:.2} с от открытия разговора", opening_at.elapsed().as_secs_f64());
                            }
                            last_voice = Some(Instant::now());
                        }
                        self.leds.level(peak(&pcm) / 9000.0);
                        if is_loud != speaking {
                            speaking = !speaking;
                            if !speaking {
                                self.st.lock().unwrap().playback_started_at = None;
                            }
                            self.leds.set(if speaking { Mode::Speaking } else { Mode::Listening });
                        }
                    }
                }
            }
            if x.interrupted() {
                while out.try_recv().is_ok(){}
                self.player.stop(true);
                let mut st = self.st.lock().unwrap();
                st.echo.stopped(Instant::now());
                st.playback_started_at = None;
                drop(st);
                speaking = false;
                self.leds.set(Mode::Listening);
            }
            if x.dialog().close_requested() {
                log::info!("разговор: музыка включена — закрываю разговор");
                self.player.stop(true);
                x.dialog().finish_turn("");
                close(&sess);
                return None;
            }
            if close_after_flag.load(Ordering::SeqCst) || x.dialog().close_after_reply_requested() {
                let armed = *close_armed_at.get_or_insert_with(Instant::now);
                let wait = if x.dialog().is_turn_complete() {
                    Duration::from_millis(300)
                } else {
                    Duration::from_millis(800)
                };
                let done_speaking = last_voice.is_some_and(|t| t >= armed && t.elapsed() >= wait);
                if done_speaking || armed.elapsed() >= Duration::from_secs(8) {
                    log::info!("разговор: завершён после подтверждения инструмента");
                    self.player.finish(Duration::from_secs(3));
                    x.dialog().finish_turn("");
                    close(&sess);
                    return None;
                }
            }
            if tick.elapsed() < Duration::from_millis(100) {
                continue;
            }
            tick = Instant::now();
            if x.dialog().heard_input_since(opening_at) || x.dialog().user_turns() > 0 {
                user_spoke = true;
            }
            let stopped = {
                let st = self.st.lock().unwrap();
                st.aborted || !st.live.as_ref().is_some_and(|l| same(l, &x))
            };
            if stopped {
                self.player.stop(true);
                x.dialog().finish_turn("");
                close(&sess);
                return None;
            }
            // разговор открыт сразу по слову, а человек так ничего и не сказал — слово сработало зря
            if by_word && checked == 0 && x.dialog().user_turns() == 0
                && ready_at.elapsed() > Duration::from_secs(5)
                && !last_voice.is_some_and(|t| t > ready_at)
                && !x.dialog().heard_input_since(opening_at)
            {
                log::info!("Джарвис: ложное срабатывание (никто ничего не сказал) — молчу");
                self.player.stop(true);
                x.dialog().annotate("ложное срабатывание", "");
                x.dialog().finish_turn("");
                close(&sess);
                return None;
            }
            let (id, text, since) = x.dialog().utterance();
            if id != 0 && (id != checked || text != checked_text) && !x.handles_commands() {
                let command = fastcmd::command_for_phrase(&text, &self.listener.phrase());
                let is_stop = fastcmd::is_stop(&command);
                let question = fastcmd::clarification(&command);
                let schedule = if question.is_some() { fastcmd::AliceSchedule::Pending } else { fastcmd::alice_schedule(&command) };
                let quiet_for = self.st.lock().unwrap().last_loud.map(|time| time.elapsed()).unwrap_or(Duration::MAX);
                if command_settled(schedule, is_stop, since, quiet_for) {
                    checked = id;
                    checked_text.clone_from(&text);
                    if let Some(question) = question {
                        x.dialog().annotate("уточнение", question);
                        x.dialog().finish_turn("");
                        x.dialog().unmute();
                        if let Err(error) = x.say(question) {
                            log::info!("уточнение команды: {error}");
                        }
                    } else if self.handle(&x, &text, "модель") {
                        close(&sess);
                        return None;
                    }
                }
            }
            if x.dialog().idle(idle) {
                log::info!("разговор: тишина {idle:.0} с — окончен");
                self.player.finish(Duration::from_secs(3));
                x.dialog().finish_turn("");
                close(&sess);
                return None;
            }
        }
    }

    /// Законченная фраза человека: «стоп», команды плагинов, Алисе и умному дому — сами, без модели.
    /// true — разговор закончен.
    fn handle(&self, x: &Arc<dyn Session>, text: &str, from: &str) -> bool {
        let cmd = fastcmd::command_for_phrase(text, &self.listener.phrase());
        let quiet = || {
            x.dialog().mute();
            self.player.stop(true);
        };
        let note = |route: &str, answer: &str| {
            x.dialog().annotate(route, answer);
            if x.dialog().turn_text().is_empty() {
                x.dialog().set_text(text);
            }
            x.dialog().finish_turn("");
        };
        if fastcmd::is_stop(&cmd) {
            quiet();
            log::info!("Джарвис: «{text}» — разговор закрыт, музыка не тронута ({from})");
            note("стоп", "");
            return true;
        }
        if self.store.with(|s| s.profiles.len() > 1)
            && let Some(target) = fastcmd::profile_switch_target(&cmd)
        {
            let answer = match self.tools.switch_profile(&target) {
                Ok(answer) | Err(answer) => answer,
            };
            quiet();
            note("профиль", &answer);
            (self.say_hook())(answer);
            return true;
        }
        if fastcmd::youtube_request(&cmd)
            || (crate::music::parse_play_intent(&cmd).is_some()
                && self.host.provider("youtube").is_ok())
        {
            return false;
        }
        let rt = &self.rt;
        if let Some(answer) = rt.block_on(self.music.control(&self.host, &cmd)) {
            quiet();
            note("музыка", &answer);
            (self.say_hook())(answer);
            return true;
        }
        if let Some(answer) = rt.block_on(async { tokio::time::timeout(Duration::from_secs(20), self.host.command(&cmd)).await.ok().flatten() }) {
            quiet();
            log::info!("Джарвис: «{text}» → плагин ({from})");
            note("плагин", &answer);
            if !answer.trim().is_empty() {
                (self.say_hook())(answer);
            } else {
                self.earcon(true);
            }
            return true;
        }
        if let Some(answer) = rt.block_on(self.tools.ha_vacuum(&cmd)) {
            quiet();
            note("Home Assistant", &answer);
            (self.say_hook())(answer);
            return true;
        }
        let t0 = Instant::now();
        if let Some(answer) = rt.block_on(self.tools.smartlife_fast(&cmd)) {
            quiet();
            log::info!("Джарвис: «{text}» → Smart Life ({:.2} с, {from})", t0.elapsed().as_secs_f64());
            note("Smart Life", &answer);
            self.earcon(answer == "Готово.");
            return true;
        }
        // таймеры, будильники, напоминания, музыка — умеет только штатная Алиса; команды умного дома
        // отдаём ей, если так выбрано в настройках
        let schedule = fastcmd::alice_schedule(&cmd);
        let mut to_alice = fastcmd::alice_skill_for(&cmd, self.store.with(|s| s.use_home("yandexmusic")));
        if to_alice.is_none() && self.store.with(|s| s.use_home("alice")) {
            to_alice = rt.block_on(self.tools.home_command(&cmd));
        }
        if let Some(a) = to_alice {
            quiet();
            match alice::text(&a, false) {
                Ok(()) => {
                    log::info!("Джарвис: «{a}» → Алисе ({from}, {:.2} с)", t0.elapsed().as_secs_f64());
                    note("умный дом → Алиса", &format!("передал Алисе: «{a}»"));
                    return true;
                }
                Err(e) if schedule == fastcmd::AliceSchedule::Ready => {
                    note("уточнение", &format!("Не удалось передать Алисе: {e}"));
                    x.dialog().unmute();
                    let _ = x.say("Не удалось передать команду Алисе.");
                    return false;
                }
                Err(e) => log::info!("Алиса: {e} — выполняю сам"),
            }
        }
        if self.store.with(|s| s.use_home("yandex"))
            && let Some(answer) = rt.block_on(self.tools.fast(&cmd))
        {
            quiet();
            log::info!("Джарвис: быстрая команда «{text}» → {answer} (Яндекс {:.2} с, {from})", t0.elapsed().as_secs_f64());
            note("умный дом (быстро)", &answer);
            self.earcon(answer == "Готово.");
            return true;
        }
        false
    }

    /// Сказать вслух просьбу плагина. Пока идёт разговор — не перебиваем его.
    pub fn say_text(&self, text: &str) {
        if self.talking() {
            log::info!("плагин просит сказать «{}» — идёт разговор, пропускаю", crate::plugins::clip(text, 120));
            return;
        }
        (self.say_hook())(text.to_string());
    }

    pub async fn offer_next(self: Arc<Self>, id: String) {
        if self.talking() || self.music.playing() { return; }
        let epoch = self.speech_epoch.load(Ordering::SeqCst);
        let result = self.music.call(&self.host, "related", serde_json::json!({"expected":id})).await;
        let Ok(serde_json::Value::String(answer)) = result else { return };
        if answer.is_empty() || self.talking() || self.music.playing()
            || self.speech_epoch.load(Ordering::SeqCst) != epoch { return; }
        self.journal.note("следующий ролик", "ролик закончился", &answer);
        let _ = tokio::task::spawn_blocking(move || self.say_text(&answer)).await;
    }

    /// Озвучка короткого ответа (плагины, команды, музыка); не вышло — звуком «готово».
    fn say_hook(&self) -> impl Fn(String) + '_ {
        move |answer: String| {
            log::info!("Джарвис: ответ «{}»", crate::plugins::clip(&answer, 200));
            if let Err(e) = self.say_aloud(&answer, Duration::from_secs(25)) {
                log::info!("озвучка: {e}");
                if e != INTERRUPTED {
                    self.earcon(true);
                }
            }
        }
    }

    /// Произнести готовый текст голосом активного ассистента: отдельный короткий звонок, где модель
    /// только читает текст. Блокирует до конца фразы (не дольше limit). Вызывать не из потоков tokio.
    pub fn say_aloud(&self, text: &str, limit: Duration) -> Result<(), String> {
        if self.saying.compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed).is_err() {
            return Err(INTERRUPTED.into());
        }
        struct Speaking<'a>(&'a AtomicBool);
        impl Drop for Speaking<'_> { fn drop(&mut self) { self.0.store(false, Ordering::SeqCst); } }
        let _speaking = Speaking(&self.saying);
        let epoch = self.speech_epoch.load(Ordering::SeqCst);
        let s = self.store.get();
        let p = s.active_profile();
        let missing = tts_missing(&self.host, &s, &p);
        if !missing.is_empty() {
            return Err(missing);
        }
        let t0 = Instant::now();
        const READ_ALOUD: &str = "Ты озвучиваешь готовый текст. Произнеси ровно то, что тебе дали, ничего не добавляя, и замолчи.";
        let sess: Arc<dyn Session> = if p.is_classic() {
            crate::classic_voice::Classic::open_with(
                &self.rt,
                self.host.clone(),
                self.tools.clone(),
                Arc::new(Journal::default()),
                s.clone(),
                p.clone(),
                READ_ALOUD.into(),
                self.listener.phrase(),
                Some(self.gemini_pool.clone()),
                Some(self.proxy.clone()),
            )
        } else if ["", "live", "openai", "gemini", "grok", "eleven_realtime"].contains(&p.engine.as_str()) {
            crate::cloud_voice::Voice::open_with(
                &self.rt,
                self.host.clone(),
                None,
                Arc::new(Journal::default()),
                s.clone(),
                p.clone(),
                READ_ALOUD.into(),
                crate::cloud_voice::VoiceOptions {
                    gemini_pool: Some(self.gemini_pool.clone()),
                    store: Some(self.store.clone()),
                    proxy: Some(self.proxy.clone()),
                    allow_failover: true,
                },
            )
        } else {
            chatgpt::ChatGptSession::open(
                &self.rt,
                chatgpt::Open {
                    host: self.host.clone(),
                    voice: p.voice.clone(),
                    thread: self.chat_thread().into(),
                    instructions: READ_ALOUD.into(),
                    brain: None,
                    journal: Arc::new(Journal::default()),
                    memory: None,
                    proxy: Some(self.proxy.clone()),
                },
            )
        };
        let res = self.play_said(&*sess, text, limit, t0, epoch);
        sess.close();
        res
    }

    fn clear_say_leds(&self, active: bool) {
        if active && !self.st.lock().unwrap_or_else(|p| p.into_inner()).busy {
            self.leds.set(Mode::Off);
        }
    }

    fn play_said(&self, sess: &dyn Session, text: &str, limit: Duration, t0: Instant, epoch: u64) -> Result<(), String> {
        struct SayLedGuard<'a> {
            engine: &'a Engine,
            active: bool,
        }
        impl Drop for SayLedGuard<'_> {
            fn drop(&mut self) {
                self.engine.clear_say_leds(self.active);
            }
        }
        let mut led_guard = SayLedGuard { engine: self, active: false };
        sess.ready(Duration::from_secs(10).min(limit))?;
        if self.speech_epoch.load(Ordering::SeqCst) != epoch { return Err(INTERRUPTED.into()); }
        sess.say(text)?;
        let out = sess.take_audio().ok_or("нет звука")?;
        let (mut started, mut heard) = (false, false);
        let mut quiet_since = Instant::now();
        while quiet_since.elapsed() < Duration::from_millis(1200) || !heard {
            if self.speech_epoch.load(Ordering::SeqCst) != epoch {
                // «Стоп» из настроек или позвали Алису
                if started {
                    self.player.stop(true);
                }
                return Err(INTERRUPTED.into());
            }
            if t0.elapsed() > limit {
                if started {
                    self.player.stop(true);
                }
                return Err("голос не успел прозвучать".into());
            }
            match out.recv_timeout(Duration::from_millis(100)) {
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    if started {
                        self.player.finish(Duration::from_secs(3));
                    }
                    return if heard { Ok(()) } else { Err("голос не прозвучал".into()) };
                }
                Ok(pcm) => {
                    if !started {
                        // плеер — только с первым звуком, а не пока модель думает
                        self.start_voice(sess)?;
                        self.leds.set(Mode::Speaking);
                        led_guard.active = true;
                        let _ = self.player.write(&vec![0; sess.rate() / 4]);
                        started = true;
                    }
                    if self.player.write(&pcm).is_err() && !self.player.playing() {
                        return Err(INTERRUPTED.into()); // плеер забрали — нас перебили
                    }
                    self.leds.level(peak(&pcm) / 9000.0);
                    if loud(&pcm) {
                        heard = true;
                        quiet_since = Instant::now();
                    }
                }
            }
        }
        self.player.finish(Duration::from_secs(3));
        Ok(())
    }

    /// «Джарвис» посреди ответа: замолчать сразу, слушать новую фразу.
    fn interrupt(&self, sess: &Arc<dyn Session>, prob: f32) {
        log::info!("Джарвис: перебили словом (уверенность {prob:.2}) — замолкаю");
        sess.dialog().mute();
        self.player.stop(true);
        let mut st = self.st.lock().unwrap();
        st.echo.stopped(Instant::now());
        st.playback_started_at = None;
        drop(st);
        self.leds.set(Mode::Listening);
    }

    /// Прервать разговор и музыку (Алиса, кнопка «Стоп» в настройках).
    pub fn stop(&self, reason: &str) {
        self.end_chat(reason);
        self.rt.block_on(self.music.stop_all(&self.host));
    }

    /// Закончить только разговор: музыка, если играет, играет дальше. Блокирует до 3 с.
    pub fn end_chat(&self, reason: &str) {
        self.speech_epoch.fetch_add(1, Ordering::SeqCst);
        self.speech_notify.notify_waiters();
        if self.error_speaking.load(Ordering::Relaxed) {
            std::thread::spawn(alice::cancel_vocalize);
        }
        let sess = {
            let mut st = self.st.lock().unwrap();
            let sess = st.live.take();
            if st.busy || sess.is_some() {
                st.aborted = true;
            }
            if sess.is_none() && st.busy {
                log::info!("Джарвис: остановлен до начала разговора ({reason})");
            }
            sess
        };
        if let Some(sess) = sess {
            log::info!("Джарвис: разговор прерван ({reason})");
            self.player.stop(true);
            sess.close();
        }
    }

    /// Журнал maind: штатный споттер «Алиса» — Джарвис уступает.
    fn watch_alice(self: &Arc<Self>) {
        let w1 = Arc::downgrade(self);
        let w2 = Arc::downgrade(self);
        alice::follow_logs(vec![
            (alice::MAIND_LOG, Box::new(move |line| {
                let Some(e) = w1.upgrade() else { return };
                if line.contains("onPhraseSpotted") && line.contains("alice_capability") && e.talking() {
                    e.end_chat("позвали Алису");
                    let _ = e.rt.block_on(e.music.call(&e.host, "stop", serde_json::Value::Null));
                }
                if line.contains("Toggle play/pause") {
                    e.rt.block_on(e.music.pause_button(&e.host));
                }
                let (started, id) = e.music.alice.lock().unwrap().main_log(line);
                if started || id.is_some() {
                    e.rt.clone().spawn(async move {
                        if started { e.music.alice_started(&e.host).await; }
                        if let Some(id) = id && let Some(track) = crate::alice_music::lookup(&id).await {
                            e.music.alice.lock().unwrap().set_track(&id, track);
                        }
                    });
                }
            })),
            ("/data/quasar/daemons_logs/mediad.log", Box::new(move |line| {
                let Some(e) = w2.upgrade() else { return };
                let started = e.music.alice.lock().unwrap().media_log(line);
                if started { e.rt.block_on(e.music.alice_started(&e.host)); }
            })),
        ]);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn waits_for_the_duration_after_a_transcript_pause() {
        use super::{Duration, command_settled, fastcmd::AliceSchedule::{None as SchedNone, Pending, Ready}};
        let pause = Duration::from_millis;
        assert!(command_settled(SchedNone, true, pause(350), pause(0)));
        assert!(!command_settled(SchedNone, false, pause(400), pause(400)));
        assert!(!command_settled(SchedNone, false, pause(500), pause(100)));
        assert!(command_settled(SchedNone, false, pause(500), pause(500)));
        assert!(command_settled(SchedNone, false, pause(1650), pause(100)));
        assert!(!command_settled(Pending, false, pause(400), pause(400)));
        assert!(!command_settled(Pending, false, pause(1800), pause(1100)));
        assert!(!command_settled(Ready, false, pause(750), pause(100)));
        assert!(command_settled(Ready, false, pause(800), pause(650)));
        assert!(command_settled(Pending, false, pause(2300), pause(800)));
    }

    #[test]
    fn helpers() {
        assert!(super::loud(&[0, 700]) && !super::loud(&[600, -600]));
        assert_eq!(super::boost(&[1000], 6)[0], 1995);
        assert_eq!(super::peak(&[-32768, 5]), 32768.0);
    }

    #[test]
    fn input_activity_detects_quiet_speech_and_ignores_ambient_noise_and_spikes() {
        let mut detector = super::InputActivity::default();
        let background: Vec<i16> = (0..320).map(|i| if i % 2 == 0 { 36 } else { -36 }).collect();
        for _ in 0..100 {
            assert!(!detector.observe(&background, false, 0));
        }
        let mut spike = background.clone();
        spike[80] = i16::MAX;
        assert!(!detector.observe(&spike, true, 0));
        let quiet_speech: Vec<i16> = (0..320).map(|i| {
            ((i as f32 * std::f32::consts::TAU * 180.0 / 16_000.0).sin() * 180.0) as i16
        }).collect();
        assert!(!super::loud(&quiet_speech));
        for _ in 0..500 {
            assert!(detector.observe(&quiet_speech, true, 0));
        }
        assert!(!detector.observe(&background, true, 0));
    }

    #[test]
    fn input_activity_hears_quiet_speech_right_after_loud_wake_word_and_respects_boost() {
        let mut detector = super::InputActivity::default();
        let quiet_room: Vec<i16> = (0..320).map(|i| if i % 2 == 0 { 15 } else { -15 }).collect();
        for _ in 0..120 {
            assert!(!detector.observe(&quiet_room, false, 0));
        }
        // Громкое слово «Джарвис» (~0.8 с) пока listening == false не должно задирать фоновый порог.
        let wake_word: Vec<i16> = (0..320).map(|i| {
            ((i as f32 * std::f32::consts::TAU * 220.0 / 16_000.0).sin() * 2500.0) as i16
        }).collect();
        for _ in 0..40 {
            assert!(detector.observe(&wake_word, false, 0));
        }
        let distant_speech: Vec<i16> = (0..320).map(|i| {
            ((i as f32 * std::f32::consts::TAU * 180.0 / 16_000.0).sin() * 100.0) as i16
        }).collect();
        assert!(detector.observe(&distant_speech, true, 0));
        assert!(!detector.observe(&quiet_room, true, 0));

        // С усилением mic_boost_db = 12 ещё более тихая речь слышна, а ровный фон комнаты по-прежнему тихий.
        let whisper: Vec<i16> = (0..320).map(|i| {
            ((i as f32 * std::f32::consts::TAU * 180.0 / 16_000.0).sin() * 55.0) as i16
        }).collect();
        let very_quiet_room: Vec<i16> = (0..320).map(|i| if i % 2 == 0 { 10 } else { -10 }).collect();
        for _ in 0..80 {
            assert!(!detector.observe(&very_quiet_room, false, 12));
        }
        assert!(detector.observe(&whisper, true, 12));
        assert!(!detector.observe(&very_quiet_room, true, 12));

        // Постоянный фоновый шум (>4 с без пауз в простое) постепенно адаптирует фон.
        let steady_fan: Vec<i16> = (0..320).map(|i| if i % 2 == 0 { 120 } else { -120 }).collect();
        for _ in 0..350 {
            detector.observe(&steady_fan, false, 0);
        }
        assert!(!detector.observe(&steady_fan, true, 0));
    }

    #[test]
    fn echo_guard_covers_queued_audio_and_reopens_after_explicit_interrupt() {
        let now = std::time::Instant::now();
        let ms = std::time::Duration::from_millis;
        let mut echo = super::EchoWindow::default();
        assert!(!echo.muted(now));
        let half_second = vec![1500; 12_000];
        echo.played(now, &half_second, 24_000);
        echo.played(now + ms(10), &half_second, 24_000);
        assert!(echo.muted(now + ms(1200)));
        assert!(!echo.muted(now + ms(1451)));
        echo.stopped(now + ms(300));
        assert!(echo.muted(now + ms(749)));
        assert!(!echo.muted(now + ms(751)));
    }

    #[cfg(feature = "lua")]
    #[tokio::test]
    async fn profile_missing_checks_classic_stages_independently() {
        let dir = std::env::temp_dir().join(format!("jarvis-pm-{}", crate::settings::rand_hex(6)));
        for (id, provides, src) in [
            ("stt-p", "stt", "return {transcribe=function() return {text=''} end}"),
            ("llm-p", "llm", "return {chat=function() return 'ok' end}"),
            ("tts-p", "tts", "return {speak=function() return {audio='',rate=24000} end}"),
        ] {
            let p = dir.join("plugins").join(id);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("plugin.json"), serde_json::json!({"id":id,"name":id,"version":"1","kind":"lua","category":"assistant","main":"main.lua","provides":[provides]}).to_string()).unwrap();
            std::fs::write(p.join("main.lua"), src).unwrap();
        }
        let host = super::Host::new(dir.clone(), crate::plugins::Hooks::default());
        host.load().await;
        let s = super::Settings::default();
        let mut p = super::Profile {
            mode: "classic".into(),
            engine: "classic".into(),
            stt_engine: "stt-p".into(),
            llm_engine: "llm-p".into(),
            tts_engine: "tts-p".into(),
            ..Default::default()
        };
        assert_eq!(super::profile_missing(&host, &s, &p), "");
        p.tts_engine = "missing-tts".into();
        assert!(super::profile_missing(&host, &s, &p).contains("missing-tts"));
        p.tts_engine = "tts-p".into();
        p.llm_engine = "missing-llm".into();
        assert!(super::profile_missing(&host, &s, &p).contains("missing-llm"));
        p.llm_engine = "llm-p".into();
        p.stt_engine = "missing-stt".into();
        assert!(super::profile_missing(&host, &s, &p).contains("missing-stt"));
        host.stop_all().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn voice_failure_cooldown_deduplicates_and_skips_expected_stops() {
        let dir = std::env::temp_dir().join(format!("jarvis-vf-{}", crate::settings::rand_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let store = super::Store::open(dir.join("settings.json")).unwrap();
        let host = super::Host::new(dir.clone(), crate::plugins::Hooks::default());
        let proxy = crate::vpn::Proxy::new(store.clone());
        let mic = super::Mic::new(dir.join("mic.sock"), Box::new(|| 0));
        let player = super::Player::new(Box::new(|| 40), Box::new(|| 1.0));
        let leds = super::Leds::new(Box::new(crate::settings::default_led));
        let journal = std::sync::Arc::new(super::Journal::default());
        let tools = std::sync::Arc::new(super::Tools::new(host.clone(), store.clone()));
        let (listener, _) = super::Listener::new(&dir, &store, false);
        let music = std::sync::Arc::new(crate::music::Music::default());
        let engine = super::Engine::new(
            tokio::runtime::Handle::current(),
            store,
            host,
            proxy,
            mic,
            player,
            leds,
            journal,
            tools,
            listener,
            dir.clone(),
            music,
        );
        let t0 = std::time::Instant::now();
        let mut f = crate::voice_error::classify(
            "gemini",
            crate::voice_error::Stage::Connect,
            "gemini ответил 429 [retry=45s, quota=PerMinute]",
            None,
            false,
        );
        assert!(engine.should_announce_failure(&f, t0));
        assert!(!engine.should_announce_failure(&f, t0 + std::time::Duration::from_secs(5)));
        assert!(engine.should_announce_failure(&f, t0 + std::time::Duration::from_secs(21)));
        f.expected = true;
        assert!(!engine.should_announce_failure(&f, t0 + std::time::Duration::from_secs(50)));
        let _ = std::fs::remove_dir_all(dir);
    }

    struct SpySession {
        dialog: crate::session::Dialog,
        fed: std::sync::Mutex<Vec<Vec<i16>>>,
        heard_before_first_feed: std::sync::Mutex<Option<bool>>,
        opened_at: std::time::Instant,
    }

    impl SpySession {
        fn new(opened_at: std::time::Instant) -> std::sync::Arc<Self> {
            let journal = std::sync::Arc::new(super::Journal::default());
            std::sync::Arc::new(Self {
                dialog: crate::session::Dialog::new(journal, "chatgpt", "gpt-live"),
                fed: std::sync::Mutex::default(),
                heard_before_first_feed: std::sync::Mutex::new(None),
                opened_at,
            })
        }

        fn all_samples(&self) -> Vec<i16> {
            self.fed.lock().unwrap().iter().flatten().copied().collect()
        }
    }

    impl crate::session::Session for SpySession {
        fn feed(&self, pcm: &[i16]) {
            let mut flag = self.heard_before_first_feed.lock().unwrap();
            if flag.is_none() {
                *flag = Some(self.dialog.heard_input_since(self.opened_at));
            }
            self.fed.lock().unwrap().push(pcm.to_vec());
        }
        fn take_audio(&self) -> Option<std::sync::mpsc::Receiver<Vec<i16>>> { None }
        fn rate(&self) -> usize { super::RATE }
        fn ready(&self, _timeout: std::time::Duration) -> Result<(), String> { Ok(()) }
        fn say(&self, _text: &str) -> Result<(), String> { Ok(()) }
        fn close(&self) {}
        fn dialog(&self) -> &crate::session::Dialog { &self.dialog }
    }

    #[test]
    fn cached_wake_reply_preserves_hold_without_resending_wake_preroll() {
        let opening_at = std::time::Instant::now();
        let preroll: Vec<i16> = (0..super::RATE * 3).map(|i| (i % 97) as i16 + 1000).collect();
        let quiet_hold: Vec<i16> = (0..1280).map(|i| (i % 50) as i16 + 20).collect();

        let mut st = super::St {
            holding: true,
            hold: quiet_hold.clone(),
            input_at: Some(opening_at - std::time::Duration::from_millis(20)),
            ..Default::default()
        };
        let spy = SpySession::new(opening_at);
        let sess: std::sync::Arc<dyn crate::session::Session> = spy.clone();

        let (play_reply, stats) = super::handoff_initial_audio(
            &mut st,
            "chatgpt",
            &preroll,
            opening_at,
            6,
            true,
            &sess,
        );

        assert!(play_reply);
        assert!(!stats.aborted);
        assert!(!stats.heard_input);
        assert!(stats.has_wake_reply);
        assert_eq!(stats.preroll_samples, preroll.len());
        assert_eq!(stats.preroll_fed_samples, 0);
        assert_eq!(stats.hold_samples, quiet_hold.len());
        assert_eq!(stats.fed_samples, quiet_hold.len());
        assert_eq!(stats.dropped_preroll_samples, preroll.len());
        assert_eq!(stats.dropped_hold_samples, 0);
        assert_eq!(spy.all_samples(), super::boost(&quiet_hold, 6));
        assert!(st.hold.is_empty());
        assert!(!st.holding);
        assert!(st.live.is_some());
        // Ложное wake / тихий буфер без срабатывания детектора не подменяет heard_input_since.
        assert!(!spy.dialog.heard_input_since(opening_at));
    }

    #[test]
    fn no_wake_reply_feeds_engine_preroll_tail_and_full_hold_in_order() {
        let opening_at = std::time::Instant::now();
        let preroll: Vec<i16> = (0..super::RATE * 2).map(|i| (i % 1000) as i16).collect();
        let hold: Vec<i16> = (0..960).map(|i| 5000 + (i as i16)).collect();

        for (engine, expected_preroll) in [
            ("chatgpt", super::RATE / 4),
            ("gemini", super::RATE * 4 / 5),
            ("openai", preroll.len()),
        ] {
            let mut st = super::St {
                holding: true,
                hold: hold.clone(),
                ..Default::default()
            };
            let spy = SpySession::new(opening_at);
            let sess: std::sync::Arc<dyn crate::session::Session> = spy.clone();

            let (play_reply, stats) = super::handoff_initial_audio(
                &mut st,
                engine,
                &preroll,
                opening_at,
                0,
                false,
                &sess,
            );

            assert!(!play_reply);
            assert_eq!(stats.preroll_fed_samples, expected_preroll);
            assert_eq!(stats.hold_samples, hold.len());
            assert_eq!(stats.fed_samples, expected_preroll + hold.len());
            assert_eq!(stats.dropped_hold_samples, 0);
            let mut expected = preroll[preroll.len() - expected_preroll..].to_vec();
            expected.extend_from_slice(&hold);
            assert_eq!(spy.all_samples(), expected);
        }
    }

    #[test]
    fn joined_phrase_during_gap_cancels_wake_reply_and_marks_input_before_feed() {
        let opening_at = std::time::Instant::now();
        let preroll: Vec<i16> = (0..super::RATE).map(|i| (i % 500) as i16).collect();
        let hold: Vec<i16> = (0..1120).map(|i| 3000 + i as i16).collect();

        let mut st = super::St {
            holding: true,
            hold: hold.clone(),
            input_at: Some(opening_at + std::time::Duration::from_millis(15)),
            ..Default::default()
        };
        let spy = SpySession::new(opening_at);
        let sess: std::sync::Arc<dyn crate::session::Session> = spy.clone();

        let (play_reply, stats) = super::handoff_initial_audio(
            &mut st,
            "chatgpt",
            &preroll,
            opening_at,
            0,
            true,
            &sess,
        );

        assert!(!play_reply, "слитная фраза сразу после слова должна отменять кешированный отклик");
        assert!(stats.heard_input);
        assert!(!stats.has_wake_reply);
        assert_eq!(*spy.heard_before_first_feed.lock().unwrap(), Some(true));
        assert_eq!(stats.preroll_fed_samples, super::RATE / 4);
        assert_eq!(stats.dropped_hold_samples, 0);
        let mut expected = preroll[preroll.len() - super::RATE / 4..].to_vec();
        expected.extend_from_slice(&hold);
        assert_eq!(spy.all_samples(), expected);
    }

    #[test]
    fn speech_over_wake_reply_and_slow_connect_keeps_exact_stream_without_gaps_or_duplicates() {
        let opening_at = std::time::Instant::now();
        let frames: Vec<Vec<i16>> = (0..5)
            .map(|f| (0..320).map(|i| (f * 1000 + i) as i16).collect())
            .collect();
        let preroll = vec![77i16; super::RATE];

        let mut st = super::St {
            holding: true,
            ..Default::default()
        };
        // Первые 2 кадра пришли до handoff (во время FOLLOW_ON_GAP) -> копятся в st.hold.
        for frame in &frames[..2] {
            if st.holding && st.live.is_none() {
                st.hold.extend_from_slice(frame);
            }
        }

        let spy = SpySession::new(opening_at);
        let sess: std::sync::Arc<dyn crate::session::Session> = spy.clone();
        let (play_reply, stats) = super::handoff_initial_audio(
            &mut st,
            "chatgpt",
            &preroll,
            opening_at,
            0,
            true,
            &sess,
        );
        assert!(play_reply);
        assert_eq!(stats.hold_samples, 640);
        assert_eq!(stats.fed_samples, 640);
        assert_eq!(stats.dropped_hold_samples, 0);

        // Оставшиеся 3 кадра пришли поверх кешированного отклика и медленного подключения -> сразу в st.live.
        for frame in &frames[2..] {
            assert!(!st.holding);
            st.live.as_ref().unwrap().feed(frame);
        }

        let expected: Vec<i16> = frames.into_iter().flatten().collect();
        assert_eq!(spy.all_samples(), expected);
    }

    #[test]
    fn aborted_handoff_drops_cleanly_without_feeding_or_publishing_live_session() {
        let opening_at = std::time::Instant::now();
        let preroll = vec![10i16; 8000];
        let hold = vec![20i16; 960];

        let mut st = super::St {
            holding: true,
            hold: hold.clone(),
            aborted: true,
            ..Default::default()
        };
        let spy = SpySession::new(opening_at);
        let sess: std::sync::Arc<dyn crate::session::Session> = spy.clone();

        let (play_reply, stats) = super::handoff_initial_audio(
            &mut st,
            "chatgpt",
            &preroll,
            opening_at,
            0,
            true,
            &sess,
        );

        assert!(!play_reply);
        assert!(stats.aborted);
        assert_eq!(stats.fed_samples, 0);
        assert_eq!(stats.dropped_hold_samples, hold.len());
        assert_eq!(stats.dropped_preroll_samples, preroll.len());
        assert!(spy.all_samples().is_empty());
        assert!(st.live.is_none());
        assert!(st.hold.is_empty());
        assert!(!st.holding);

        let mut diag = super::InitialAudioDiag::default();
        diag.record(stats);
        let j = diag.to_json();
        assert_eq!(j["activations"], 1);
        assert_eq!(j["aborted"], 1);
        assert_eq!(j["total_dropped_hold_ms"], 60);
        assert_eq!(j["last"]["aborted"], true);
    }

    #[test]
    fn wake_interrupt_blocks_silent_onset_and_low_confidence_echo() {
        let now = std::time::Instant::now();
        let ms = std::time::Duration::from_millis;
        // Выключено настройкой — никогда не перебиваем.
        assert!(!super::should_wake_interrupt(false, true, false, Some(now - ms(1000)), 0.99, now));
        // Плеер молчит (например, LLM ещё думает) — не глушим будущий ответ.
        assert!(!super::should_wake_interrupt(true, false, false, None, 0.99, now));
        // Недавнее пробуждение (<2.5 с) — игнорируем.
        assert!(!super::should_wake_interrupt(true, true, true, Some(now - ms(1000)), 0.99, now));
        // Первые 550 мс воспроизведения (сходимость VQE AEC на первом слове TTS) — игнорируем.
        assert!(!super::should_wake_interrupt(true, true, false, Some(now - ms(250)), 0.99, now));
        // Пограничная уверенность на фоне речи колонки (эхо) — игнорируем.
        assert!(!super::should_wake_interrupt(true, true, false, Some(now - ms(800)), 0.72, now));
        // Уверенный оклик после окна сходимости — перебиваем.
        assert!(super::should_wake_interrupt(true, true, false, Some(now - ms(800)), 0.92, now));
    }

    #[tokio::test]
    async fn play_said_clears_leds_after_finishing_when_idle() {
        let dir = std::env::temp_dir().join(format!("jarvis-say-led-{}", crate::settings::rand_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let store = super::Store::open(dir.join("settings.json")).unwrap();
        let host = super::Host::new(dir.clone(), crate::plugins::Hooks::default());
        let proxy = crate::vpn::Proxy::new(store.clone());
        let mic = super::Mic::new(dir.join("mic.sock"), Box::new(|| 0));
        let player = super::Player::new(Box::new(|| 40), Box::new(|| 1.0));
        let leds = super::Leds::new(Box::new(crate::settings::default_led));
        let journal = std::sync::Arc::new(super::Journal::default());
        let tools = std::sync::Arc::new(super::Tools::new(host.clone(), store.clone()));
        let (listener, _) = super::Listener::new(&dir, &store, false);
        let music = std::sync::Arc::new(crate::music::Music::default());
        let engine = super::Engine::new(
            tokio::runtime::Handle::current(),
            store,
            host,
            proxy,
            mic,
            player,
            leds.clone(),
            journal,
            tools,
            listener,
            dir.clone(),
            music,
        );

        // После озвучки из панели (st.busy == false) подсветка должна вернуться в Mode::Off.
        leds.set(super::Mode::Speaking);
        assert_eq!(leds.phase(), super::Mode::Speaking);
        engine.clear_say_leds(true);
        assert_eq!(leds.phase(), super::Mode::Off);

        // Если озвучку перебила новая активация разговора (st.busy == true), чужой Mode::Listening не сбрасываем.
        engine.st.lock().unwrap().busy = true;
        leds.set(super::Mode::Listening);
        engine.clear_say_leds(true);
        assert_eq!(leds.phase(), super::Mode::Listening);
        let _ = std::fs::remove_dir_all(dir);
    }
}
