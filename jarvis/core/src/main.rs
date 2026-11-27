//! jarvis — голосовой ассистент «Джарвис» целиком на Яндекс Станции: без своего сервера.
//! Голосовые движки, YouTube и подключения устанавливаются отдельными пакетами.
#![recursion_limit = "256"]

mod alice;
mod alice_music;
mod launchpatch;
mod audio;
mod chatgpt;
mod chatgpt_accounts;
mod live_stats;
mod buttons;
mod speakervol;
mod cmdspot;
mod counter;
mod memory;
mod music;
mod engine;
mod fastcmd;
mod hardware;
mod jlog;
mod journal;
mod led;
mod llm;
mod gemini;
mod gemini_pool;
mod voice_error;
mod cloud_voice;
mod classic_voice;
mod net;
mod plugins;
mod rules;
mod session;
mod settings;
mod tools;
mod updates;
mod telemetry;
mod vpn;
mod wake;
mod wake_profiles;
mod training_io;
mod web;

use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};

pub const DEFAULT_REPO: &str = match option_env!("JARVIS_REPO") {
    Some(r) => r,
    None => "newfpv/YaJarvis",
};

pub fn version() -> &'static str {
    option_env!("JARVIS_VERSION").unwrap_or("dev")
}

/// Флаги как у Go-версии: -data, -mic, -listen (jarvisd.sh запускает с ними).
struct Flags {
    data: PathBuf,
    mic: PathBuf,
    listen: String,
    listen_explicit: bool,
}

fn flags() -> Flags {
    let mut f = Flags {
        data: "/data/opt/jarvisd".into(),
        mic: "/tmp/jarvisd/mic.sock".into(),
        listen: ":8080".into(),
        listen_explicit: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let a = a.trim_start_matches('-');
        let (k, v) = match a.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (a.to_string(), None),
        };
        let mut val = || v.clone().or_else(|| args.next()).unwrap_or_default();
        match k.as_str() {
            "data" => f.data = val().into(),
            "mic" => f.mic = val().into(),
            "listen" => {
                f.listen = val();
                f.listen_explicit = true;
            }
            _ => {}
        }
    }
    if !f.listen_explicit
        && let Ok(custom) = std::fs::read_to_string(f.data.join("listen"))
        && !custom.trim().is_empty()
    {
        f.listen = custom.trim().to_string();
        f.listen_explicit = true;
    }
    f
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("module-manifest") {
        let id = std::env::args().nth(2).unwrap_or_default();
        if let Some(m) = plugins::modules::manifests().into_iter().find(|m| m.id == id) {
            println!("{}", serde_json::to_string(&m).expect("манифест модуля"));
            return;
        }
        eprintln!("Модуля нет в сборке");
        std::process::exit(1);
    }
    if std::env::args().nth(1).as_deref() == Some("core-info") {
        let image = jarvis_station::Image { format: jarvis_station::IDENTITY.into(), version: version().into(),
            arch: jarvis_station::arch().into(), modules: plugins::compiled_modules() };
        println!("{}", serde_json::to_string(&image).unwrap());
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("patch-launch") {
        let args: Vec<_> = std::env::args().skip(2).collect();
        let result = if args.len() == 4 {
            launchpatch::patch(&args[0], &args[1], &args[2], &args[3])
        } else {
            Err("usage: jarvis patch-launch <launch.json> <out.json> <vqe_tap.so> <mic.sock>".into())
        };
        if let Err(error) = result { eprintln!("{error}"); std::process::exit(1); }
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("modules") {
        println!("{}", serde_json::to_string(&plugins::compiled_modules()).expect("список модулей"));
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("version") {
        println!("{}", version());
        return;
    }
    jlog::init();
    let f = flags();
    // проверка слова на колонке без микрофона: jarvis wake-check -data /data/opt/jarvisd файл.wav…
    if std::env::args().nth(1).as_deref() == Some("wake-check") {
        let store = settings::Store::open(f.data.join("settings.json")).expect("настройки");
        for a in std::env::args().skip(2).filter(|a| a.ends_with(".wav") || a.ends_with(".raw")) {
            print!("{a}: ");
            if let Err(e) = wake::check_file(&f.data, std::path::Path::new(&a), &store) {
                println!("{e}");
            }
        }
        return;
    }
    // проверка разговора без живого микрофона: jarvis mic-send сокет файл.wav — звук в сокет в реальном времени
    #[cfg(unix)]
    if std::env::args().nth(1).as_deref() == Some("mic-send") {
        let a: Vec<String> = std::env::args().skip(2).collect();
        if let Err(e) = audio::send_wav(std::path::Path::new(&a[0]), std::path::Path::new(&a[1])) {
            println!("{e}");
        }
        return;
    }
    log::info!("jarvis {} (ядро на Rust)", version());
    if let Err(e) = web::backup::recover(&f.data) {
        log::error!("Ошибка восстановления резервной копии: {e}");
        std::process::exit(1);
    }
    let store = match settings::Store::open(f.data.join("settings.json")) {
        Ok(s) => s,
        Err(e) => {
            log::info!("настройки: {e}");
            std::process::exit(1);
        }
    };
    jlog::set_timezone(&store.with(|s| s.timezone.clone()));
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(8)
        .thread_stack_size(if cfg!(debug_assertions) { 2 * 1024 * 1024 } else { 1024 * 1024 })
        .enable_all()
        .build()
        .expect("tokio");
    rt.block_on(run(f, store));
}

/// Город, комната, язык, модель колонки — плагинам при запуске.
fn info(store: &settings::Store) -> Value {
    store.with(|s| {
        let p = s.active_profile();
        json!({"city": s.city, "timezone": s.timezone, "room": s.speaker_room, "language": s.language,
               "model": hardware::station_model().id, "assistant": p.name})
    })
}

async fn run(f: Flags, store: settings::Store) {
    let proxy = vpn::Proxy::new(store.clone());
    let music = Arc::new(music::Music::default());
    let host_cell: Arc<std::sync::OnceLock<std::sync::Weak<plugins::Host>>> = Arc::default();
    let engine_cell: Arc<std::sync::OnceLock<std::sync::Weak<engine::Engine>>> = Arc::default();
    let svc = {
        let (store, proxy, data, cell) = (store.clone(), proxy.clone(), f.data.clone(), host_cell.clone());
        let st2 = store.clone();
        Arc::new(plugins::coresvc::CoreServices {
            store,
            proxy,
            data_dir: data,
            info: Box::new(move || info(&st2)),
            official: Box::new(move |id| cell.get().and_then(|w| w.upgrade()).is_some_and(|h| h.official(id))),
        })
    };
    let hooks = {
        let st = store.clone();
        let svc = svc.clone();
        plugins::Hooks {
            network: {let proxy=proxy.clone();Box::new(move ||proxy.network_reply("plugins"))},
            request: Box::new(move |id, method, params| {
                let svc = svc.clone();
                Box::pin(async move {
                    if id == "chatgpt" && method == "voice.brain" {
                        if !(svc.official)(&id) {
                            return Err("official plugin required".into());
                        }
                        return chatgpt::brain_request(params).await;
                    }
                    tokio::task::spawn_blocking(move || svc.call(&id, &method, &params)).await.unwrap_or_else(|e| Err(e.to_string()))
                })
            }),
            alice: Box::new(|text, silent| Box::pin(async move { tokio::task::spawn_blocking(move || alice::text(&text, silent).is_ok()).await.unwrap_or(false) })),
            say: {
                let cell = engine_cell.clone();
                Box::new(move |text| {
                    // плагин просит сказать вслух (напоминание, ответ на событие): голосом ассистента
                    if let Some(e) = cell.get().and_then(std::sync::Weak::upgrade) {
                        let _ = std::thread::Builder::new().name("say".into()).stack_size(512 * 1024).spawn(move || e.say_text(&text));
                    }
                })
            },
            info: Box::new(move || info(&st)),
            event: {
                let music = music.clone();
                let engine = engine_cell.clone();
                Box::new(move |manifest, name, data| {
                    if manifest.id=="chatgpt" && name=="voice.event"{chatgpt::module_voice_event(data.clone());}
                    if manifest.provides("youtube") {
                        let was_playing = music.state().playing;
                        let next = (name == "music.related_end").then(|| data.as_str().map(str::to_owned)).flatten();
                        music.event(name, data);
                        if let Some(e) = engine.get().and_then(std::sync::Weak::upgrade) {
                            if let Some(id) = next {
                                tokio::spawn(e.offer_next(id));
                            } else if !was_playing && music.state().playing && music.alice.lock().unwrap().playing() {
                                let music = music.clone();
                                tokio::spawn(async move { let _ = music.alice_control("pause", &Value::Null).await; });
                            }
                        }
                    }
                })
            },
            ..Default::default()
        }
    };
    let host = plugins::Host::new(f.data.clone(), hooks);
    let _ = host_cell.set(Arc::downgrade(&host));
    {
        let (host, proxy) = (host.clone(), proxy.clone());
        let installing = std::env::var_os("JARVIS_INSTALL_MODULE").is_some();
        if installing {
            host.load().await;
        }
        tokio::spawn(async move {
            if !installing {
                host.load().await;
            }
            tokio::spawn(proxy.run(host.clone()));
            host.clone().restore_pending_plugins().await;
        });
    }
    {
        let host = host.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            host.stop_all().await;
            std::process::exit(0);
        });
    }
    #[cfg(unix)]
    {
        let host = host.clone();
        tokio::spawn(async move {
            if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                s.recv().await;
                log::info!("завершаюсь: останавливаю плагины");
                host.stop_all().await;
                std::process::exit(0);
            }
        });
    }
    // микрофон и слово «Джарвис»
    let mic = {
        let st = store.clone();
        audio::Mic::new(f.mic.clone(), Box::new(move || st.mic_gain_db()))
    };
    if cfg!(unix) {
        mic.clone().run();
    }
    #[cfg(unix)]
    alice::watch_mic(mic.clone());
    let (listener, det) = wake::Listener::new(&f.data, &store, host.wake_training());
    let player = {
        let st = store.clone();
        audio::Player::new(Box::new(move || st.voice_volume()), Box::new(|| speakervol::voice_trim(speakervol::speaker_level())))
    };
    let leds = {
        let st = store.clone();
        led::Leds::new(Box::new(move || st.led_for()))
    };
    tokio::spawn(chatgpt::warm_loop(host.clone(), store.clone()));
    let journal = Arc::new(journal::Journal::default());
    let memory = {
        let (a, b) = (store.clone(), store.clone());
        memory::Memory::load(f.data.join("chatgpt-chat.json"), Box::new(move || !a.with(|s| s.memory.off)), Box::new(move || b.now()))
    };
    {
        let m = memory.clone();
        *journal.on_remember.lock().unwrap() = Some(Box::new(move |t: journal::Turn| {
            if !t.model.is_empty() {
                m.add(&t.text, &t.answer); // разговор с моделью — в общую память, какой бы ассистент ни отвечал
            } else if !t.answer.is_empty() && memory::worthy(&t.answer) {
                m.add(&t.text, &format!("[{}]", t.answer)); // выполнено без модели — модель тоже должна знать
            }
        }));
    }
    let mut tools = tools::Tools::new(host.clone(), store.clone());
    tools.on_profile_switch = {
        let (st, l) = (store.clone(), leds.clone());
        Some(Arc::new(move || l.configure(&st.with(|s| s.led_for()))))
    };
    tools.memory = Some(memory.clone());
    for (target, queue) in [(&mut tools.music, false), (&mut tools.music_queue, true)] {
        let (music, host) = (music.clone(), host.clone());
        *target = Some(Arc::new(move |q| {
            let (music, host) = (music.clone(), host.clone());
            Box::pin(async move { music.play(&host, &q, queue).await })
        }));
    }
    tools.music_ctl = Some({
        let (music, host) = (music.clone(), host.clone());
        Arc::new(move |text| {
            let (music, host) = (music.clone(), host.clone());
            Box::pin(async move { music.control(&host, &text).await.unwrap_or_else(|| "Сейчас музыка не играет или команда не распознана.".into()) })
        })
    });
    tools.music_fast=Some({let (music,host)=(music.clone(),host.clone());Arc::new(move|text|{let (music,host)=(music.clone(),host.clone());Box::pin(async move{music.control(&host,&text).await})})});
    tools.music_cancel = Some({
        let (music, host) = (music.clone(), host.clone());
        Arc::new(move || {
            let (music, host) = (music.clone(), host.clone());
            Box::pin(async move { music.cancel_loading(&host).await })
        })
    });
    let tools = Arc::new(tools);
    let engine = engine::Engine::new(
        tokio::runtime::Handle::current(),
        store.clone(),
        host.clone(),
        proxy.clone(),
        mic,
        player,
        leds,
        journal,
        tools,
        listener,
        f.data.clone(),
        music.clone(),
    );
    #[cfg(unix)]
    speakervol::watch_night(store.clone(), engine.leds.clone());
    let _ = engine_cell.set(Arc::downgrade(&engine));
    {
        let e = Arc::downgrade(&engine);
        music::volume_loop(music, host.clone(), move || e.upgrade().is_some_and(|e| e.talking() || e.player.playing()));
    }
    memory.set_summarizer(Some(engine.summarizer()));
    engine.run(det);
    let app = web::Web::new(store.clone(), host.clone(), proxy, f.data.clone(), engine);
    #[cfg(unix)]
    {
        let app = app.clone();
        buttons::watch(move || web::buttons_reset(&app));
    }
    tokio::spawn(counter::run(store.clone(), f.data.clone()));
    if let Err(e) = web::serve(app, &f.listen, f.listen_explicit).await {
        log::info!("веб-настройки: {e}");
        host.stop_all().await;
        std::process::exit(1);
    }
}
