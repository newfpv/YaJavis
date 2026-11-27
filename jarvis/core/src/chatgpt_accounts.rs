//! Account credentials remain in the atomic private store; web gets summaries only.
use crate::settings::{Settings, rand_hex};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_ACCOUNTS: usize = 8;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub auth: Value,
    #[serde(default)]
    pub live: crate::live_stats::Stats,
    #[serde(default)]
    pub status: String,
    // Local retry backoff, never a quota reset timestamp.
    #[serde(default)]
    pub retry_after: i64,
    #[serde(default)]
    pub services: std::collections::BTreeMap<String, Restriction>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Restriction {
    pub status: String,
    pub retry_after: i64,
}
fn scope(p: &Value) -> &str {
    match p["scope"].as_str() {
        Some("voice") => "voice",
        Some("codex") => "codex",
        _ => "",
    }
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
pub fn ensure(s: &mut Settings) -> bool {
    if s.chatgpt_accounts.is_empty() && crate::chatgpt::signed_in(&s.chatgpt) {
        let id = rand_hex(12);
        s.chatgpt["_local_id"] = id.clone().into();
        s.chatgpt["_revision"] = rand_hex(8).into();
        s.chatgpt_accounts.push(Account {
            id: id.clone(),
            auth: s.chatgpt.clone(),
            live: Default::default(),
            status: String::new(),
            retry_after: 0,
            services: Default::default(),
        });
        s.chatgpt_active = id;
        return true;
    }
    // Stock builds preserve our account list but may rotate the active OAuth token.
    // Import their current credentials before an account selection can restore a stale copy.
    if crate::chatgpt::signed_in(&s.chatgpt) {
        if let Some(a) = s
            .chatgpt_accounts
            .iter_mut()
            .find(|a| a.auth["account"] == s.chatgpt["account"])
        {
            let mut current = s.chatgpt.clone();
            current["_local_id"] = a.id.clone().into();
            if current["_revision"].as_str().is_none_or(str::is_empty) {
                current["_revision"] = rand_hex(8).into();
            }
            if a.auth != current || s.chatgpt != current || s.chatgpt_active != a.id {
                a.auth = current.clone();
                s.chatgpt = current;
                s.chatgpt_active = a.id.clone();
                return true;
            }
        }
    }
    false
}
pub fn summary(s: &Settings) -> Value {
    json!({"active":s.chatgpt_active,"auto_switch":s.chatgpt_auto_switch,"accounts":s.chatgpt_accounts.iter().map(|a|json!({
        "id":a.id,"email":a.auth["email"],"plan":a.auth["plan"],"active":a.id==s.chatgpt_active,
        "status":if a.status.is_empty(){"ready"}else{&a.status},
        "retry_after":a.retry_after,"services":a.services,"live_stats":a.live.summary()})).collect::<Vec<_>>()})
}
fn available(a: &Account, service: &str) -> bool {
    crate::chatgpt::signed_in(&a.auth)
        && a.services.get(service).is_none_or(|r| r.status == "limit" && r.retry_after <= now())
        && (a.status.is_empty() || (a.status == "limit" && a.retry_after <= now()))
}
pub fn select(s: &mut Settings, id: &str) -> Result<(), String> {
    let a = s
        .chatgpt_accounts
        .iter_mut()
        .find(|a| a.id == id)
        .ok_or("Аккаунт не найден")?;
    a.auth["_revision"] = rand_hex(8).into();
    s.chatgpt = a.auth.clone();
    s.chatgpt_active = id.into();
    Ok(())
}
pub fn remove(s: &mut Settings, id: &str) -> Result<(), String> {
    if !s.chatgpt_accounts.iter().any(|a| a.id == id) {
        return Err("Аккаунт не найден".into());
    }
    s.chatgpt_accounts.retain(|a| a.id != id);
    if s.chatgpt_active == id {
        s.chatgpt = json!({});
        s.chatgpt_active.clear();
        if let Some(id) = s.chatgpt_accounts.first().map(|a| a.id.clone()) {
            select(s, &id)?;
        }
    }
    Ok(())
}
pub fn add(s: &mut Settings, mut auth: Value) -> Result<(), String> {
    if !crate::chatgpt::signed_in(&auth) {
        return Err("Неполные данные входа".into());
    }
    ensure(s);
    let index = s
        .chatgpt_accounts
        .iter()
        .position(|a| a.auth["account"] == auth["account"]);
    if index.is_none() && s.chatgpt_accounts.len() >= MAX_ACCOUNTS {
        return Err("Можно сохранить не больше 8 аккаунтов".into());
    }
    let id = index
        .map(|i| s.chatgpt_accounts[i].id.clone())
        .unwrap_or_else(|| rand_hex(12));
    auth["_local_id"] = id.clone().into();
    auth["_revision"] = rand_hex(8).into();
    let live = index
        .map(|i| s.chatgpt_accounts[i].live.clone())
        .unwrap_or_default();
    let a = Account {
        id: id.clone(),
        auth,
        live,
        status: String::new(),
        retry_after: 0,
        services: Default::default(),
    };
    if let Some(i) = index {
        s.chatgpt_accounts[i] = a;
    } else {
        s.chatgpt_accounts.push(a);
    }
    if s.chatgpt_active.is_empty() || s.chatgpt_active == id {
        select(s, &id)?;
    }
    s.auto_select_engine("chatgpt");
    Ok(())
}
pub fn refreshed(s: &mut Settings, after: Value) {
    s.chatgpt = after.clone();
    if let Some(a) = s
        .chatgpt_accounts
        .iter_mut()
        .find(|a| a.id == s.chatgpt_active)
    {
        a.auth = after;
    }
}
pub fn prepared(s: &Settings, p: &Value) -> Option<Value> {
    let active = s.chatgpt_accounts.iter().find(|a| a.id == s.chatgpt_active)?;
    (available(active, scope(p)) && active.auth == s.chatgpt
        && active.auth["_local_id"] == active.id && active.auth["_revision"].as_str().is_some_and(|v| !v.is_empty()))
        .then(|| active.auth.clone())
}

pub fn prepare(s: &mut Settings, p: &Value) -> Result<Value, String> {
    ensure(s);
    if let Some(a) = s.chatgpt_accounts.iter().find(|a| a.id == s.chatgpt_active) {
        if available(a, scope(p)) {
            return Ok(s.chatgpt.clone());
        }
    }
    if s.chatgpt_auto_switch {
        if let Some(id) = s
            .chatgpt_accounts
            .iter()
            .find(|a| available(a, scope(p)))
            .map(|a| a.id.clone())
        {
            select(s, &id)?;
            return Ok(s.chatgpt.clone());
        }
    }
    Err("Нет доступного аккаунта ChatGPT: проверьте вход или дождитесь окончания ограничения в «Подключениях»".into())
}
pub fn failover(s: &mut Settings, p: &Value) -> Result<Value, String> {
    if s.chatgpt["_local_id"] != p["before"]["_local_id"]
        || s.chatgpt["_revision"] != p["before"]["_revision"]
    {
        return Err("Аккаунт изменён — повторите обращение".into());
    }
    let reason = p["reason"].as_str().unwrap_or("");
    if !matches!(reason, "auth" | "limit" | "access") {
        return Err("Недопустимая причина переключения".into());
    }
    if let Some(a) = s
        .chatgpt_accounts
        .iter_mut()
        .find(|a| a.id == s.chatgpt_active)
    {
        let retry_after = if reason == "limit" {
            now() + p["retry_s"].as_i64().unwrap_or(60).clamp(1, 86400)
        } else {
            0
        };
        if reason == "auth" || scope(p).is_empty() {
            a.status = reason.into();
            a.retry_after = retry_after;
        } else {
            a.services.insert(scope(p).into(), Restriction { status: reason.into(), retry_after });
        }
    }
    if s.chatgpt_auto_switch {
        let tried = p["tried"].as_array().cloned().unwrap_or_default();
        if let Some(id) = s
            .chatgpt_accounts
            .iter()
            .find(|a| available(a, scope(p)) && !tried.iter().any(|v| v == &a.id))
            .map(|a| a.id.clone())
        {
            select(s, &id)?;
            log::info!("ChatGPT: автоматически выбран другой сохранённый аккаунт ({reason})");
            return Ok(json!({"switched":true}));
        }
    }
    Ok(json!({"switched":false}))
}
pub fn mark_ok(s: &mut Settings, id: &str, service: &str) -> bool {
    let Some(a) = s.chatgpt_accounts.iter_mut().find(|a| a.id == id) else {
        return false;
    };
    let cleared = a.services.remove(service).is_some();
    if a.status.is_empty() && a.retry_after == 0 {
        return cleared;
    }
    a.status.clear();
    a.retry_after = 0;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    fn auth(id: &str) -> Value {
        json!({"account":id,"refresh":format!("secret-refresh-{id}"),"access":format!("secret-access-{id}"),"email":format!("{id}@example.org")})
    }
    #[test]
    fn restrictions_apply_only_to_the_failed_service_until_auth_is_revoked() {
        for (service, other) in [("voice", "codex"), ("codex", "voice")] {
            let mut s = Settings::default();
            add(&mut s, auth("a")).unwrap();
            add(&mut s, auth("b")).unwrap();
            let before = s.chatgpt.clone();
            let id = s.chatgpt_active.clone();
            failover(&mut s, &json!({"before":before,"reason":"limit","scope":service})).unwrap();
            assert!(!mark_ok(&mut s, &id, other));
            assert_eq!(prepare(&mut s, &json!({"scope":other})).unwrap(), before);
            assert!(prepare(&mut s, &json!({"scope":service})).is_err());
            s.chatgpt_auto_switch = true;
            assert_ne!(prepare(&mut s, &json!({"scope":service})).unwrap()["_local_id"], id);
            select(&mut s, &id).unwrap();
            s.chatgpt_auto_switch = false;
            let before = s.chatgpt.clone();
            failover(&mut s, &json!({"before":before,"reason":"auth","scope":other})).unwrap();
            for scope in [service, other] {
                assert!(prepare(&mut s, &json!({"scope":scope})).is_err());
            }
        }
    }

    #[test]
    fn migration_refresh_add_and_summary() {
        let mut s = Settings::default();
        s.chatgpt = auth("a");
        assert!(ensure(&mut s));
        assert!(!ensure(&mut s));
        let first = s.chatgpt_active.clone();
        add(&mut s, auth("b")).unwrap();
        assert_eq!(s.chatgpt_active, first);
        assert_eq!(s.chatgpt_accounts.len(), 2);
        let mut after = s.chatgpt.clone();
        after["refresh"] = "rotated".into();
        refreshed(&mut s, after);
        select(&mut s, &first).unwrap();
        assert_eq!(s.chatgpt["refresh"], "rotated");
        let public = summary(&s).to_string();
        assert!(!public.contains("secret"));
        assert!(!public.contains("rotated"));
        add(&mut s, auth("a")).unwrap();
        assert_eq!(s.chatgpt_accounts.len(), 2);
    }
    #[test]
    fn stock_update_preserves_rotated_credentials_and_saved_accounts() {
        let mut s = Settings::default();
        add(&mut s, auth("a")).unwrap();
        add(&mut s, auth("b")).unwrap();
        s.chatgpt_auto_switch = true;
        let id = s.chatgpt_active.clone();
        s.chatgpt = auth("a");
        s.chatgpt["refresh"] = "new-stock-refresh".into();
        assert!(ensure(&mut s));
        assert!(!ensure(&mut s));
        assert_eq!(s.chatgpt_accounts.len(), 2);
        assert_eq!(s.chatgpt_active, id);
        assert!(s.chatgpt_auto_switch);
        select(&mut s, &id).unwrap();
        assert_eq!(s.chatgpt["refresh"], "new-stock-refresh");
        assert_eq!(s.chatgpt["_local_id"], id);
    }
    #[test]
    fn full_list_allows_reauthentication_but_rejects_a_ninth_account() {
        let mut s = Settings::default();
        for n in 0..MAX_ACCOUNTS {
            add(&mut s, auth(&n.to_string())).unwrap();
        }
        let active = s.chatgpt_active.clone();
        assert!(add(&mut s, auth("extra")).is_err());
        let mut fresh = auth("0");
        fresh["refresh"] = "reauthenticated".into();
        add(&mut s, fresh).unwrap();
        assert_eq!(s.chatgpt_accounts.len(), MAX_ACCOUNTS);
        assert_eq!(s.chatgpt_active, active);
        assert_eq!(s.chatgpt["refresh"], "reauthenticated");
    }
    #[test]
    fn failover_is_opt_in_and_stops_when_exhausted() {
        let mut s = Settings::default();
        add(&mut s, auth("a")).unwrap();
        add(&mut s, auth("b")).unwrap();
        let first = s.chatgpt_active.clone();
        let p = json!({"before":s.chatgpt,"reason":"limit","retry_s":90,"tried":[first]});
        assert_eq!(failover(&mut s, &p).unwrap()["switched"], false);
        assert_eq!(s.chatgpt_active, first);
        s.chatgpt_auto_switch = true;
        prepare(&mut s, &Value::Null).unwrap();
        assert_ne!(s.chatgpt_active, first);
        let p = json!({"before":s.chatgpt,"reason":"auth","tried":[]});
        assert_eq!(failover(&mut s, &p).unwrap()["switched"], false);
        assert!(prepare(&mut s, &Value::Null).is_err());
        add(&mut s, auth("a")).unwrap();
        assert!(prepare(&mut s, &Value::Null).is_ok());
    }
    #[test]
    fn stale_failure_and_unknown_errors_cannot_rotate() {
        let mut s = Settings::default();
        add(&mut s, auth("a")).unwrap();
        let before = s.chatgpt.clone();
        let id = s.chatgpt_active.clone();
        select(&mut s, &id).unwrap();
        assert!(failover(&mut s, &json!({"before":before,"reason":"auth"})).is_err());
        let p = json!({"before":s.chatgpt,"reason":"network"});
        assert!(failover(&mut s, &p).is_err());
        assert!(s.chatgpt_accounts[0].status.is_empty());
    }
    #[test]
    fn delete_active_preserves_other_accounts() {
        let mut s = Settings::default();
        add(&mut s, auth("a")).unwrap();
        let id = s.chatgpt_active.clone();
        add(&mut s, auth("b")).unwrap();
        remove(&mut s, &id).unwrap();
        assert_eq!(s.chatgpt["account"], "b");
        let id = s.chatgpt_active.clone();
        remove(&mut s, &id).unwrap();
        assert!(!crate::chatgpt::signed_in(&s.chatgpt));
    }
    #[test]
    fn retry_backoff_expiry_does_not_claim_quota_was_reset() {
        let mut s = Settings::default();
        add(&mut s, auth("a")).unwrap();
        s.chatgpt_accounts[0].status = "limit".into();
        s.chatgpt_accounts[0].retry_after = now() - 1;
        assert!(prepare(&mut s, &Value::Null).is_ok());
        assert_eq!(summary(&s)["accounts"][0]["status"], "limit");
    }
    #[test]
    fn mark_ok_clears_limit_status_after_confirmed_success() {
        let mut s = Settings::default();
        add(&mut s, auth("a")).unwrap();
        let id = s.chatgpt_active.clone();
        assert!(!mark_ok(&mut s, &id, ""));
        s.chatgpt_accounts[0].status = "limit".into();
        s.chatgpt_accounts[0].retry_after = now() + 60;
        assert!(mark_ok(&mut s, &id, ""));
        assert_eq!(summary(&s)["accounts"][0]["status"], "ready");
        assert!(!mark_ok(&mut s, &id, ""));
    }
}
