//! Сервисы ядра для плагинов (сообщение «core»). Каждому плагину — только своё: явная карта
//! «плагин → сервис», весь settings.json плагину никогда не отдаётся. Правила те же, что у Go-версии.

use crate::{settings::Store, vpn::Proxy};
use serde_json::{Map, Value, json};
use std::{path::PathBuf, sync::Arc};

/// Убрать пустое (null, "", 0, false, пустые объекты) — так Go сравнивал структуры с omitempty.
fn norm(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let o: Map<String, Value> = m
                .iter()
                .map(|(k, v)| (k.clone(), norm(v)))
                .filter(|(_, v)| match v {
                    Value::Null => false,
                    Value::String(s) => !s.is_empty(),
                    Value::Bool(b) => *b,
                    Value::Number(n) => n.as_f64() != Some(0.0),
                    Value::Object(o) => !o.is_empty(),
                    Value::Array(a) => !a.is_empty(),
                })
                .collect();
            Value::Object(o)
        }
        Value::Array(a) => Value::Array(a.iter().map(norm).collect()),
        x => x.clone(),
    }
}

pub struct CoreServices {
    pub store: Store,
    pub proxy: Arc<Proxy>,
    pub data_dir: PathBuf,
    pub info: Box<dyn Fn() -> Value + Send + Sync>,
    pub official: Box<dyn Fn(&str) -> bool + Send + Sync>,
}

fn yt_auth(s: &crate::settings::Settings) -> Value {
    let g = |k: &str| s.extra.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    json!({
        "refresh": g("youtube_refresh"),
        "name": g("youtube_name"),
        "handle": g("youtube_handle"),
        "email": g("youtube_email")
    })
}

impl CoreServices {
    pub fn call(&self, id: &str, method: &str, params: &Value) -> Result<Value, String> {
        let official = (self.official)(id);
        if id == "chatgpt" && method=="voice.stats" {
            if !official{return Err("official plugin required".into())}
            self.store.update(|s|->Result<(),std::io::Error>{
                let a=s.chatgpt_accounts.iter_mut().find(|a|Some(a.id.as_str())==params["id"].as_str()).ok_or_else(||std::io::Error::other("Аккаунт удалён"))?;
                a.live.apply(&params["report"]).map_err(std::io::Error::other)
            }).map_err(|e|e.to_string())?;
            return Ok(Value::Null);
        }
        if id == "chatgpt" && method == "accounts.ok" {
            if !official {
                return Err("official plugin required".into());
            }
            let target_id = params["id"].as_str().unwrap_or("");
            let service = params["scope"].as_str().unwrap_or("");
            let needs_clear = self.store.with(|s| {
                s.chatgpt_accounts
                    .iter()
                    .find(|a| a.id == target_id)
                    .is_some_and(|a| !a.status.is_empty() || a.retry_after != 0 || a.services.contains_key(service))
            });
            if needs_clear {
                self.store
                    .update(|s| {
                        crate::chatgpt_accounts::mark_ok(s, target_id, service);
                        Ok::<_, std::io::Error>(())
                    })
                    .map_err(|e| e.to_string())?;
            }
            return Ok(Value::Null);
        }
        if id == "chatgpt" && matches!(method,"auth.add"|"accounts.prepare"|"accounts.failover"|"accounts.select"|"accounts.remove") {
            if !official { return Err("official plugin required".into()); }
            if method == "accounts.prepare" {
                if let Some(auth) = self.store.with(|s| crate::chatgpt_accounts::prepared(s, params)) {
                    return Ok(auth);
                }
            }
            let mut result=Value::Null;
            self.store.update(|s| -> Result<(),std::io::Error> {
                result=match method {
                    "auth.add"=>crate::chatgpt_accounts::add(s,params["after"].clone()).map(|_|Value::Null),
                    "accounts.prepare"=>crate::chatgpt_accounts::prepare(s,params),
                    "accounts.select"=>crate::chatgpt_accounts::select(s,params["id"].as_str().unwrap_or("")).map(|_|Value::Null),
                    "accounts.remove"=>crate::chatgpt_accounts::remove(s,params["id"].as_str().unwrap_or("")).map(|_|Value::Null),
                    _=>crate::chatgpt_accounts::failover(s,params),
                }.map_err(std::io::Error::other)?;
                Ok(())
            }).map_err(|e|e.to_string())?;
            return Ok(result);
        }
        if id == "chatgpt" && (method == "auth.get" || method == "auth.set") {
            if !official {
                return Err("official plugin required".into());
            }
            if method == "auth.get" {
                return self.store.with(|s| match params["id"].as_str() {
                    Some(id)=>s.chatgpt_accounts.iter().find(|a|a.id==id).map(|a|a.auth.clone()).ok_or("Аккаунт не найден".into()),
                    None=>Ok(s.chatgpt.clone()),
                });
            }
            let before = params.get("Before").or_else(|| params.get("before")).cloned().unwrap_or(Value::Null);
            let after = params.get("After").or_else(|| params.get("after")).cloned().ok_or("invalid auth update")?;
            self.store
                .update(|st| {
                    if let Some(id)=params["id"].as_str() {
                        let account=st.chatgpt_accounts.iter_mut().find(|a|a.id==id).ok_or_else(||std::io::Error::other("Аккаунт не найден"))?;
                        if norm(&account.auth)!=norm(&before){return Err(std::io::Error::other("account changed; retry login"));}
                        let after=norm(&after);
                        if after["_local_id"]!=account.auth["_local_id"] || after["account"]!=account.auth["account"] {return Err(std::io::Error::other("account identity changed"));}
                        account.auth=after.clone();
                        if st.chatgpt_active==id{st.chatgpt=after;}
                        return Ok(());
                    }
                    if norm(&st.chatgpt) != norm(&before) {
                        return Err(std::io::Error::other("account changed; retry login"));
                    }
                    crate::chatgpt_accounts::refreshed(st,norm(&after));
                    if crate::chatgpt::signed_in(&st.chatgpt) {
                        st.auto_select_engine("chatgpt");
                    }
                    Ok(())
                })
                .map_err(|e| e.to_string())?;
            return Ok(true.into());
        }
        if id == "youtube" && official {
            let scope = params.get("service").or_else(|| params.get("Service")).and_then(Value::as_str).unwrap_or("");
            match method {
                "network" if scope == "plugins" => return Ok(self.proxy.network_reply("plugins")),
                "info" => return Ok((self.info)()),
                _ => {}
            }
        }
        let service = match id {
            "yandexhome"=>"yandex",
            "homeassistant" | "xiaomi" | "smartlife" | "youtube" | "chatgpt" | "openai" | "gemini" | "grok" | "elevenlabs" => id,
            _ => "",
        };
        if service.is_empty() {
            return Err("у плагина нет доступа к этому сервису ядра".into());
        }
        if matches!(id,"xiaomi"|"smartlife"|"youtube") && !official && matches!(method,"config.get"|"config.set"|"legacy") {
            return Err("official plugin required".into());
        }
        let field = |svc: &str| match svc {
            "homeassistant" => Some("home_assistant"),
            "xiaomi" => Some("xiaomi"),
            "smartlife" => Some("tuya"),
            _ => None,
        };
        let get_field = |s: &crate::settings::Settings, f: &str| -> Value {
            match f {
                "home_assistant" => s.home_assistant.clone(),
                "xiaomi" => s.xiaomi.clone(),
                "tuya" => s.tuya.clone(),
                _ => Value::Null,
            }
        };
        match method {
            "network" => {
                if service == "homeassistant" {
                    return Ok(json!({}));
                }
                Ok(self.proxy.network_reply(service))
            }
            "config.get" => {
                let s = self.store.get();
                if service == "youtube" {
                    return Ok(json!({"auth": yt_auth(&s), "radio_off": s.radio_off}));
                }
                let f = field(service).ok_or("сервис ещё не поддерживает перенос настроек")?;
                Ok(json!({"auth": get_field(&s, f), "speaker_room": s.speaker_room, "enabled": s.use_home(service)}))
            }
            "config.set" => {
                let before = params.get("before").cloned().filter(|v| !v.is_null()).ok_or("неверное обновление настроек")?;
                let after = params.get("after").cloned().filter(|v| !v.is_null()).ok_or("неверное обновление настроек")?;
                let connected = params.get("connected").and_then(Value::as_bool).unwrap_or(false);
                self.store
                    .update(|st| {
                        let was = st.home_connected(service);
                        let old = if service == "youtube" {
                            yt_auth(st)
                        } else {
                            get_field(st, field(service).ok_or_else(|| std::io::Error::other("настройки сервиса недоступны"))?)
                        };
                        if norm(&old) != norm(&before) {
                            return Err(std::io::Error::other("settings changed; retry"));
                        }
                        match service {
                            "youtube" => {
                                for (k, from) in [
                                    ("youtube_refresh", "refresh"),
                                    ("youtube_name", "name"),
                                    ("youtube_handle", "handle"),
                                    ("youtube_email", "email"),
                                ] {
                                    st.extra.insert(k.into(), after.get(from).cloned().unwrap_or(Value::from("")));
                                }
                                return Ok(());
                            }
                            "homeassistant" => st.home_assistant = after.clone(),
                            "xiaomi" => st.xiaomi = norm(&after),
                            "smartlife" => st.tuya = norm(&after),
                            _ => return Err(std::io::Error::other("настройки сервиса недоступны")),
                        }
                        if connected && !was && st.home_connected(service) {
                            st.home_use.remove(service); // только что подключили — галочка включается сама
                        }
                        Ok(())
                    })
                    .map_err(|e| e.to_string())?;
                Ok(true.into())
            }
            "legacy" => {
                let mut name = match service {
                    "xiaomi" => "xiaomi-devices.json",
                    "smartlife" => "tuya-devices.json",
                    _ => "",
                };
                if service == "youtube" {
                    // до 0.9.71 куки и места остановки лежали в папке Джарвиса
                    let want = params.get("name").and_then(Value::as_str).unwrap_or("");
                    if want == "youtube_cookies.txt" || want == "youtube-progress.json" {
                        name = if want == "youtube_cookies.txt" { "youtube_cookies.txt" } else { "youtube-progress.json" };
                    }
                }
                if name.is_empty() {
                    return Err("у сервиса нет старого кеша".into());
                }
                let path = if matches!(id,"xiaomi"|"smartlife") {
                    let fresh=self.data_dir.join("plugins/home/data").join(name);
                    if fresh.exists() { fresh }
                    else if self.data_dir.join("plugins/home/data").join(format!("{name}.migrated")).exists() {
                        return Ok(Value::Null);
                    } else { self.data_dir.join(name) }
                } else { self.data_dir.join(name) };
                let read = || -> std::io::Result<Vec<u8>> {
                    use std::io::Read;
                    let mut bytes=Vec::new();
                    std::fs::File::open(path)?.take((4<<20)+1).read_to_end(&mut bytes)?;
                    Ok(bytes)
                };
                match read() {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Null),
                    Err(e) => Err(e.to_string()),
                    Ok(b) if b.len() > 4 << 20 => Err("старый кеш слишком большой".into()),
                    // Go отдавал []byte — в JSON это base64
                    Ok(b) => Ok(Value::String(b64(&b))),
                }
            }
            _ => Err("неизвестный сервис ядра".into()),
        }
    }
}

pub fn b64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_like_go() {
        assert_eq!(b64(b"hello"), "aGVsbG8=");
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"ab"), "YWI=");
    }

    #[test]
    fn norm_drops_empty() {
        assert_eq!(norm(&json!({"a": "", "b": 0, "c": {"d": null}, "e": "x"})), json!({"e": "x"}));
    }
    #[test]
    fn yandex_module_gets_only_its_own_network_policy() {
        let dir=std::env::temp_dir().join(format!("jarvis-coresvc-{}",crate::settings::rand_hex(6)));std::fs::create_dir_all(&dir).unwrap();
        let store=Store::open(dir.join("settings.json")).unwrap();
        store.update(|s|->Result<(),std::io::Error>{s.vless="vless://pending".into();s.vpn.insert("yandex".into(),true);s.vpn.insert("chatgpt".into(),false);Ok(())}).unwrap();
        let svc=CoreServices{store:store.clone(),proxy:Proxy::new(store),data_dir:dir.clone(),info:Box::new(||json!({})),official:Box::new(|_|true)};
        assert!(svc.call("yandexhome","network",&json!({"service":"chatgpt"})).unwrap()["error"].is_string());
        for method in ["config.get","auth.get","legacy"] {assert!(svc.call("yandexhome",method,&json!({"service":"chatgpt"})).is_err());}
        assert_eq!(dir.parent(),Some(std::env::temp_dir().as_path()));assert!(dir.file_name().unwrap().to_string_lossy().starts_with("jarvis-coresvc-"));std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn account_store_rejects_stale_refresh_and_untrusted_plugins() {
        let dir=std::env::temp_dir().join(format!("jarvis-coresvc-{}",crate::settings::rand_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();let store=Store::open(dir.join("settings.json")).unwrap();
        let mut svc=CoreServices{store:store.clone(),proxy:Proxy::new(store.clone()),data_dir:dir.clone(),info:Box::new(||json!({})),official:Box::new(|_|true)};
        for id in ["a","b"] {svc.call("chatgpt","auth.add",&json!({"after":{"account":id,"refresh":"secret","access":"old"}})).unwrap();}
        let before=store.get().chatgpt;let target=store.get().chatgpt_accounts[1].id.clone();
        svc.call("chatgpt","accounts.select",&json!({"id":target})).unwrap();
        assert!(svc.call("chatgpt","auth.set",&json!({"before":before,"after":{"account":"a","refresh":"rotated"}})).is_err());
        assert_eq!(store.get().chatgpt["account"],"b");
        svc.official=Box::new(|_|false);
        for method in ["auth.get","auth.add","accounts.prepare","accounts.select","accounts.remove","accounts.failover"] {assert!(svc.call("chatgpt",method,&json!({})).is_err());assert!(svc.call("weather",method,&json!({})).is_err());}
        let reloaded=Store::open(dir.join("settings.json")).unwrap();assert_eq!(reloaded.get().chatgpt_accounts.len(),2);assert_eq!(reloaded.get().chatgpt_active,target);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn active_account_prepare_works_without_writing_settings() {
        let dir = std::env::temp_dir().join(format!("jarvis-prepare-{}", crate::settings::rand_hex(6)));
        let store = Store::open(dir.join("settings.json")).unwrap();
        let svc = CoreServices {
            store: store.clone(), proxy: Proxy::new(store), data_dir: dir.clone(),
            info: Box::new(|| json!({})), official: Box::new(|_| true),
        };
        svc.call("chatgpt", "auth.add", &json!({"after":{"account":"a","refresh":"test"}})).unwrap();
        let active = svc.store.get().chatgpt_active;
        std::fs::create_dir(dir.join("settings.json.tmp")).unwrap();
        for _ in 0..100 {
            let auth = svc.call("chatgpt", "accounts.prepare", &json!({"scope":"voice"})).unwrap();
            assert_eq!(auth["_local_id"], active);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn inactive_account_refresh_preserves_selection_and_rejects_removed_account() {
        let dir=std::env::temp_dir().join(format!("jarvis-coresvc-{}",crate::settings::rand_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();let store=Store::open(dir.join("settings.json")).unwrap();
        let svc=CoreServices{store:store.clone(),proxy:Proxy::new(store.clone()),data_dir:dir.clone(),info:Box::new(||json!({})),official:Box::new(|_|true)};
        for id in ["a","b"]{svc.call("chatgpt","auth.add",&json!({"after":{"account":id,"refresh":"old","access":"old"}})).unwrap();}
        let active=store.get().chatgpt.clone();let id=store.get().chatgpt_accounts[1].id.clone();
        let before=svc.call("chatgpt","auth.get",&json!({"id":id})).unwrap();let mut after=before.clone();after["refresh"]="rotated".into();
        svc.call("chatgpt","auth.set",&json!({"id":id,"before":before,"after":after})).unwrap();
        assert_eq!(store.get().chatgpt,active);assert_eq!(store.get().chatgpt_accounts[1].auth["refresh"],"rotated");
        svc.call("chatgpt","accounts.remove",&json!({"id":id})).unwrap();
        assert!(svc.call("chatgpt","auth.set",&json!({"id":id,"before":after,"after":before})).is_err());
        assert!(svc.call("chatgpt","auth.get",&json!({"id":id})).is_err());assert_eq!(store.get().chatgpt,active);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn live_stats_follow_original_account_survive_reload_and_relogin_and_reject_deleted_ids(){
        let dir=std::env::temp_dir().join(format!("jarvis-coresvc-{}",crate::settings::rand_hex(6)));std::fs::create_dir_all(&dir).unwrap();let store=Store::open(dir.join("settings.json")).unwrap();
        let mut svc=CoreServices{store:store.clone(),proxy:Proxy::new(store.clone()),data_dir:dir.clone(),info:Box::new(||json!({})),official:Box::new(|_|true)};
        for id in ["a","b"]{svc.call("chatgpt","auth.add",&json!({"after":{"account":id,"refresh":"secret"}})).unwrap();}
        let old=store.get().chatgpt_active.clone();let other=store.get().chatgpt_accounts[1].id.clone();svc.call("chatgpt","accounts.select",&json!({"id":other})).unwrap();
        let p=json!({"id":old,"report":{"session":"s","started_at":100,"opened":true,"closed":true,"connected_ms":1500,"sent_ms":1400,"received_ms":400,"limit":true,"limit_at":101}});
        svc.call("chatgpt","voice.stats",&p).unwrap();svc.call("chatgpt","voice.stats",&p).unwrap();
        svc.call("chatgpt","auth.add",&json!({"after":{"account":"a","refresh":"fresh"}})).unwrap();
        let reload=Store::open(dir.join("settings.json")).unwrap();assert_eq!(reload.get().chatgpt_active,other);
        let summary=crate::chatgpt_accounts::summary(&reload.get());assert_eq!(summary["accounts"][0]["live_stats"]["sessions"],1);assert_eq!(summary["accounts"][1]["live_stats"]["sessions"],0);assert_eq!(summary["accounts"][0]["live_stats"]["limit_events"][0]["at"],101);
        assert!(!summary.to_string().contains("fresh"));assert!(!summary.to_string().contains("secret"));
        svc.call("chatgpt","accounts.remove",&json!({"id":old})).unwrap();assert!(svc.call("chatgpt","voice.stats",&p).is_err());
        let p_fail=json!({"before":store.get().chatgpt,"reason":"limit","retry_s":60,"tried":[]});
        svc.call("chatgpt","accounts.failover",&p_fail).unwrap();
        assert_eq!(store.get().chatgpt_accounts[0].status,"limit");
        svc.call("chatgpt","accounts.ok",&json!({"id":other})).unwrap();
        assert!(store.get().chatgpt_accounts[0].status.is_empty());
        svc.official=Box::new(|_|false);assert!(svc.call("chatgpt","voice.stats",&json!({"id":other,"report":p["report"]})).is_err());assert!(svc.call("chatgpt","accounts.ok",&json!({"id":other})).is_err());assert!(svc.call("weather","voice.stats",&p).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

}
