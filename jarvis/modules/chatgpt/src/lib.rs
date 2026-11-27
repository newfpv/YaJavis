//! Subscription credentials and Codex requests. Credentials live in the host's atomic store.
use serde_json::{Value,json};
use std::{future::Future,pin::Pin,sync::{Arc,Mutex},time::{Duration,SystemTime,UNIX_EPOCH},net::SocketAddr};
use base64::{Engine,engine::general_purpose::URL_SAFE_NO_PAD};
pub type Callback=Arc<dyn Fn(String,Value)->Pin<Box<dyn Future<Output=Result<Value,String>>+Send>>+Send+Sync>;
const ISSUER:&str="https://auth.openai.com";
const CLIENT:&str="app_EMoamEEZ73f0CkXaXp7hrann";
mod opus;
mod mic;
mod rtc_runtime;
mod rtc_udp;
mod rtc_voice;
mod account_error;
mod usage;
mod live_stats;
fn now()->i64{SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64}
pub struct ChatGpt {
    services:Callback,account_admin:Arc<tokio::sync::Mutex<()>>,refresh:tokio::sync::Mutex<()>,login:Arc<Mutex<Value>>,
    generation:Arc<std::sync::atomic::AtomicU64>,poller:Mutex<Option<tokio::task::JoinHandle<()>>>,
    issuer:String,codex:String,call_url:String,usage_url:String,usage_cache:usage::Cache,
    client_cache:Mutex<Option<(Value,reqwest::Client)>>,
    standby:Mutex<Option<rtc_voice::StandbyPeer>>,standby_lock:tokio::sync::Mutex<()>,
    voices:Mutex<std::collections::BTreeMap<String,Arc<rtc_voice::RtcVoice>>>,voice_lock:tokio::sync::Mutex<()>,
}
impl ChatGpt {
    pub fn new(services:Callback)->Self{Self{services,account_admin:Arc::new(tokio::sync::Mutex::new(())),refresh:tokio::sync::Mutex::new(()),login:Arc::new(Mutex::new(Value::Null)),generation:Arc::new(std::sync::atomic::AtomicU64::new(0)),poller:Mutex::new(None),usage_url:"https://chatgpt.com/backend-api/wham/usage".into(),usage_cache:usage::Cache::default(),issuer:ISSUER.into(),codex:"https://chatgpt.com/backend-api/codex/responses".into(),call_url:"https://chatgpt.com/backend-api/codex/realtime/calls?intent=quicksilver&architecture=avas".into(),client_cache:Mutex::default(),standby:Mutex::default(),standby_lock:tokio::sync::Mutex::new(()),voices:Mutex::default(),voice_lock:tokio::sync::Mutex::new(())}}
    async fn client(&self)->Result<reqwest::Client,String>{self.client_for(&(self.services)("network".into(),json!({})).await?)}
    fn client_for(&self,network:&Value)->Result<reqwest::Client,String>{
        if let Some(error)=network["error"].as_str(){return Err(error.into());}
        let mut cached=self.client_cache.lock().unwrap();if let Some((route,client))=cached.as_ref(){if route==network{return Ok(client.clone());}}
        let client=client(network)?;*cached=Some((network.clone(),client.clone()));Ok(client)
    }
    fn take_standby(&self,network:&Value)->Option<rtc_voice::StandbyPeer>{
        let peer=self.standby.lock().unwrap().take()?;
        peer.matches(network).then_some(peer)
    }
    #[cfg(test)]
    fn has_standby(&self)->bool{self.standby.lock().unwrap().is_some()}
    async fn ensure_standby(&self,network:&Value){
        let _lock=self.standby_lock.lock().await;
        let generation=self.generation.load(std::sync::atomic::Ordering::SeqCst);
        let stale={
            let mut slot=self.standby.lock().unwrap();
            if slot.as_ref().is_some_and(|peer|peer.fresh_for_warm(network)){return;}
            slot.take()
        };
        if let Some(peer)=stale{peer.close().await;}
        if let Ok(peer)=rtc_voice::StandbyPeer::build(&self.services,network.clone()).await{
            if generation!=self.generation.load(std::sync::atomic::Ordering::SeqCst){
                peer.close().await;
                return;
            }
            let extra = {
                let mut slot=self.standby.lock().unwrap();
                if slot.is_none(){
                    *slot=Some(peer);
                    None
                }else{
                    Some(peer)
                }
            };
            if let Some(peer)=extra{
                peer.close().await;
            }
        }
    }
    pub fn login_state(&self)->Value{self.login.lock().unwrap().clone()}
    pub fn stop(&self){self.generation.fetch_add(1,std::sync::atomic::Ordering::SeqCst);if let Some(task)=self.poller.lock().unwrap().take(){task.abort();}*self.login.lock().unwrap()=Value::Null;self.standby.lock().unwrap().take();self.voices.lock().unwrap().clear();self.client_cache.lock().unwrap().take();}
    pub async fn shutdown(&self){let voices=std::mem::take(&mut*self.voices.lock().unwrap());let standby=self.standby.lock().unwrap().take();self.stop();for (_,voice) in voices{voice.close().await;}if let Some(peer)=standby{peer.close().await;}}
    pub async fn request(&self,method:&str,params:Value)->Result<Value,String>{
        match method{
            "auth.token"=>Box::pin(self.token_available()).await,
            "accounts.usage"=>Box::pin(self.account_usage(&params)).await,
            "login.status"=>Ok(self.login_state()),
            "login.start"=>{let _lock=self.voice_lock.lock().await;let _admin=self.account_admin.lock().await;Box::pin(self.start()).await},
            "accounts.select"|"accounts.remove"=>{let _lock=self.voice_lock.lock().await;let _admin=self.account_admin.lock().await;self.shutdown().await;(self.services)(method.into(),params).await},
            "auth.logout"=>{let _lock=self.voice_lock.lock().await;let _admin=self.account_admin.lock().await;self.shutdown().await;let before=(self.services)("auth.get".into(),Value::Null).await?;(self.services)("accounts.remove".into(),json!({"id":before["_local_id"]})).await?;Ok(json!({}))},
            "codex.request"=>Box::pin(self.codex(params)).await,
            "voice.warm"=>Box::pin(async move {
                let auth=(self.services)("auth.get".into(),Value::Null).await?;
                if auth["refresh"].as_str().is_some_and(|value| !value.is_empty()) {
                    let network=(self.services)("network".into(),json!({})).await?;
                    if network["error"].as_str().is_none() {
                        let client=self.client_for(&network)?;
                        let url=reqwest::Url::parse(&self.call_url).map_err(|_|"Неверный адрес звонка ChatGPT")?;
                        // Reuse DNS, SOCKS, H2 TLS, access token and a local WebRTC PeerConnection without opening an OpenAI call.
                        let _=tokio::join!(
                            self.token_with_margin(900),
                            async{if let Ok(response)=client.head(url).timeout(Duration::from_secs(4)).send().await{let _=response.bytes().await;}},
                            self.ensure_standby(&network),
                        );
                    }
                }
                Ok::<Value,String>(Value::Null)
            }).await,
            "voice.open"=>Box::pin(async move {
                let _lock=self.voice_lock.lock().await;let id=params["id"].as_str().ok_or("Нужен ID звонка")?;
                if self.voices.lock().unwrap().contains_key(id){return Ok(json!({"rate":48000}));}
                if self.voices.lock().unwrap().len()>=2{return Err("Лимит: два звонка ChatGPT".into());}
                let generation=self.generation.load(std::sync::atomic::Ordering::SeqCst);let voice=Box::pin(self.open_with_failover(&params,generation)).await?;
                if generation!=self.generation.load(std::sync::atomic::Ordering::SeqCst){voice.close().await;return Err("Звонок отменён".into());}
                let model=voice.requested_model;
                self.voices.lock().unwrap().insert(voice.id.clone(),voice);Ok::<Value,String>(json!({"rate":48000,"requested_model":model}))
            }).await,
            "voice.feed"=>{let voice=self.voice(&params)?;voice.feed(params["pcm"].as_str().ok_or("Нет PCM")?)?;Ok(Value::Null)},
            "voice.say"=>{self.voice(&params)?.say(params["text"].as_str().unwrap_or("")).await?;Ok(Value::Null)},
            "voice.close"=>{let voice=self.voices.lock().unwrap().remove(params["id"].as_str().unwrap_or(""));if let Some(voice)=voice{voice.close().await;}Ok(Value::Null)},
            _=>Err(format!("ChatGPT: метод {method} не поддерживается")),
        }
    }
    fn voice(&self,params:&Value)->Result<Arc<rtc_voice::RtcVoice>,String>{self.voices.lock().unwrap().get(params["id"].as_str().unwrap_or("")).cloned().ok_or("Звонок ChatGPT закрыт".into())}
    async fn account_failure(&self,before:Value,error:&str,tried:&[Value],scope:&str)->Result<bool,String>{
        let Some(f)=account_error::Failure::parse(error)else{return Err(error.into())};
        let result=(self.services)("accounts.failover".into(),json!({"before":before,"reason":f.reason,"retry_s":f.retry,"tried":tried,"scope":scope})).await?;
        if result["switched"]==true{Ok(true)}else{Err(f.message())}
    }
    async fn open_with_failover(&self,params:&Value,generation:u64)->Result<Arc<rtc_voice::RtcVoice>,String>{
        (self.services)("accounts.prepare".into(),json!({"scope":"voice"})).await?;
        let mut tried=Vec::new();
        for _ in 0..8{
            if generation!=self.generation.load(std::sync::atomic::Ordering::SeqCst){return Err("Разговор отменён".into())}
            let before=(self.services)("auth.get".into(),Value::Null).await?;
            tried.push(before["_local_id"].clone());
            let auth=match self.token().await{Ok(a)=>a,Err(e)=>{self.account_failure(before,&e,&tried,"").await?;continue}};
            let mut result=rtc_voice::RtcVoice::open_auth(self,params,auth.clone()).await;
            let mut used=auth;
            if result.as_ref().err().and_then(|e|account_error::Failure::parse(e)).is_some_and(|f|f.reason=="auth"){
                match self.token_with_margin(i64::MAX/2).await{
                    Ok(fresh)=>{used=fresh;result=rtc_voice::RtcVoice::open_auth(self,params,used.clone()).await;},
                    Err(e)=>result=Err(e),
                }
            }
            match result{
                Ok(voice)=>{
                    let _=(self.services)("accounts.ok".into(),json!({"id":used["_local_id"],"scope":"voice"})).await;
                    return Ok(voice);
                }
                Err(e)=>{self.account_failure(used,&e,&tried,"voice").await?;}
            }
        }
        Err("Все сохранённые аккаунты ChatGPT проверены; доступных нет".into())
    }
    async fn token_available(&self)->Result<Value,String>{
        (self.services)("accounts.prepare".into(),Value::Null).await?;
        let mut tried=Vec::new();
        for _ in 0..8 {
            let before=(self.services)("auth.get".into(),Value::Null).await?;
            tried.push(before["_local_id"].clone());
            match self.token().await { Ok(a)=>return Ok(a), Err(e)=>{self.account_failure(before,&e,&tried,"").await?;} }
        }
        Err("Нет доступного входа ChatGPT".into())
    }
    async fn token(&self)->Result<Value,String>{self.token_with_margin(300).await}
    async fn token_with_margin(&self,margin_secs:i64)->Result<Value,String>{self.token_for(None,margin_secs).await}
    async fn token_for(&self,id:Option<&str>,margin_secs:i64)->Result<Value,String>{
        let _lock=self.refresh.lock().await;
        let _admin=self.account_admin.lock().await;
        let before=(self.services)("auth.get".into(),id.map(|id|json!({"id":id})).unwrap_or(Value::Null)).await?;
        let get=|k:&str|before[k].as_str().unwrap_or("");
        if get("refresh").is_empty() || get("account").is_empty(){return Err("Не выполнен вход в ChatGPT".into());}
        if !get("access").is_empty() && before["expires"].as_i64().unwrap_or(0)>now()+margin_secs{return Ok(before);}
        let client=self.client().await?;
        let response=client.post(format!("{}/oauth/token",self.issuer)).timeout(Duration::from_secs(10)).form(&[("grant_type","refresh_token"),("refresh_token",get("refresh")),("client_id",CLIENT),("scope","openid profile email")]).send().await.map_err(|e|net_error("Не продлить вход в ChatGPT",&e))?;
        if !response.status().is_success(){return Err(account_error::rejected(response).await);}
        let tokens=json_response(response).await?;let after=tokens_to_auth(&tokens,&before)?;
        (self.services)("auth.set".into(),json!({"id":id,"before":before,"after":after})).await?;Ok(after)
    }
    async fn start(&self)->Result<Value,String>{
        self.stop();let http=self.client().await?;
        let response=http.post(format!("{}/api/accounts/deviceauth/usercode",self.issuer)).json(&json!({"client_id":CLIENT})).send().await.map_err(|e|net_error("Не начать вход в ChatGPT",&e))?;
        let device=json_response(response).await?;let id=device["device_auth_id"].as_str().ok_or("OpenAI не выдал device ID")?.to_string();
        let code=device["user_code"].as_str().or_else(||device["usercode"].as_str()).ok_or("OpenAI не выдал код")?.to_string();
        let interval=device["interval"].as_u64().or_else(||device["interval"].as_str().and_then(|v|v.parse().ok())).unwrap_or(5).clamp(1,30);
        let state=json!({"code":code,"url":format!("{}/codex/device",self.issuer),"until":now()+900});*self.login.lock().unwrap()=state.clone();
        let generation=self.generation.load(std::sync::atomic::Ordering::SeqCst);let serial=self.generation.clone();let services=self.services.clone();let login=self.login.clone();let issuer=self.issuer.clone();let admin=self.account_admin.clone();
        let task=tokio::spawn(async move{
            let result=async{
                let until=tokio::time::Instant::now()+Duration::from_secs(900);
                while tokio::time::Instant::now()<until{
                    tokio::time::sleep(Duration::from_secs(interval)).await;
                    if serial.load(std::sync::atomic::Ordering::SeqCst)!=generation{return Ok(());}
                    let http=client(&(services)("network".into(),json!({})).await?)?;
                    let response=match http.post(format!("{issuer}/api/accounts/deviceauth/token")).json(&json!({"device_auth_id":id,"user_code":code})).send().await{Ok(r)=>r,Err(_)=>continue};
                    if [403,404,429,500,502,503,504].contains(&response.status().as_u16()){continue;}
                    let grant=json_response(response).await?;let Some(code)=grant["authorization_code"].as_str().filter(|s|!s.is_empty())else{continue;};
                    let verifier=grant["code_verifier"].as_str().ok_or("OpenAI не выдал verifier")?;
                    let response=http.post(format!("{issuer}/oauth/token")).form(&[("grant_type","authorization_code"),("code",code),("client_id",CLIENT),("redirect_uri",&format!("{issuer}/deviceauth/callback")),("code_verifier",verifier)]).send().await.map_err(|e|net_error("Не обменять код ChatGPT",&e))?;
                    let after=tokens_to_auth(&json_response(response).await?,&json!({}))?;
                    let _admin=admin.lock().await;
                    if serial.load(std::sync::atomic::Ordering::SeqCst)!=generation{return Ok(());}
                    (services)("auth.add".into(),json!({"after":after})).await?;*login.lock().unwrap()=Value::Null;return Ok(());
                }
                Err::<(),String>("Код ChatGPT устарел; начните вход заново".into())
            }.await;
            if serial.load(std::sync::atomic::Ordering::SeqCst)==generation{if let Err(error)=result{*login.lock().unwrap()=json!({"error":error});}}
        });*self.poller.lock().unwrap()=Some(task);Ok(state)
    }
    async fn codex(&self,params:Value)->Result<Value,String>{
        let generation=self.generation.load(std::sync::atomic::Ordering::SeqCst);
        (self.services)("accounts.prepare".into(),json!({"scope":"codex"})).await?;
        let mut tried=Vec::new();
        for _ in 0..8 {
            if generation!=self.generation.load(std::sync::atomic::Ordering::SeqCst){return Err("Запрос отменён при смене аккаунта".into())}
            let before=(self.services)("auth.get".into(),Value::Null).await?;
            tried.push(before["_local_id"].clone());
            let mut auth=match self.token().await{Ok(a)=>a,Err(e)=>{self.account_failure(before,&e,&tried,"").await?;continue}};
            for attempt in 0..2 {
                let access=auth["access"].as_str().ok_or("Нет токена ChatGPT")?;
                let account=auth["account"].as_str().ok_or("Нет аккаунта ChatGPT")?;
                let session=format!("{}-{}",auth["_local_id"].as_str().unwrap_or("legacy"),params["session"].as_str().unwrap_or("").chars().take(90).collect::<String>());
                let client=self.client().await?;
                let response=client.post(&self.codex).bearer_auth(access).header("chatgpt-account-id",account).header("Accept","text/event-stream").header("OpenAI-Beta","responses=experimental").header("originator","codex_cli_rs").header("session_id",session).json(&params["body"]).send().await.map_err(|e|net_error("Нет соединения с ChatGPT",&e))?;
                if response.status().is_success(){
                    let _=(self.services)("accounts.ok".into(),json!({"id":auth["_local_id"],"scope":"codex"})).await;
                    // Never replay a request after streamed output/tool calls have started.
                    return sse(response).await;
                }
                let mut error=account_error::rejected(response).await;
                if attempt==0 && account_error::Failure::parse(&error).is_some_and(|f|f.reason=="auth") {
                    match self.token_with_margin(i64::MAX/2).await {Ok(fresh)=>{auth=fresh;continue},Err(e)=>error=e}
                }
                self.account_failure(auth,&error,&tried,"codex").await?;
                break;
            }
        }
        Err("Все сохранённые аккаунты ChatGPT недоступны".into())
    }

}
impl Drop for ChatGpt{fn drop(&mut self){self.stop();}}
fn classify_net_error(is_timeout:bool,is_connect:bool,detail:&str)->&'static str{
    if is_timeout{"сервер не ответил вовремя"}
    else if detail.contains("socks")||detail.contains("proxy"){"ошибка VPN-прокси"}
    else if detail.contains("dns")||detail.contains("lookup")||detail.contains("resolve"){"не удалось определить адрес сервера"}
    else if detail.contains("tls")||detail.contains("handshake")||detail.contains("certificate")||detail.contains("ssl"){"ошибка TLS-соединения"}
    else if is_connect{"не удалось подключиться"}
    else{"сетевая ошибка"}
}
fn net_error(prefix:&str,err:&reqwest::Error)->String{
    use std::error::Error as _;
    let mut detail=err.to_string().to_ascii_lowercase();
    let mut src=err.source();
    while let Some(s)=src{detail.push(' ');detail.push_str(&s.to_string().to_ascii_lowercase());src=s.source();}
    let reason=classify_net_error(err.is_timeout(),err.is_connect(),&detail);
    format!("{prefix}: {reason}. Проверьте сеть и настройки VPN")
}
fn client(network:&Value)->Result<reqwest::Client,String>{
    if let Some(error)=network["error"].as_str(){return Err(error.into());}if !network.is_object(){return Err("Нет маршрута ChatGPT".into());}
    let mut builder=reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .tcp_keepalive(Some(Duration::from_secs(20)))
        .pool_idle_timeout(Some(Duration::from_secs(120)))
        .http2_keep_alive_interval(Some(Duration::from_secs(20)))
        .http2_keep_alive_while_idle(true);
    if let Some(socks)=network["socks"].as_str(){let addr:SocketAddr=socks.parse().map_err(|_|"Неверный SOCKS VPN")?;let user=network["user"].as_str().unwrap_or("");let pass=network["pass"].as_str().unwrap_or("");if !addr.ip().is_loopback()||addr.port()==0||user.is_empty()||pass.is_empty(){return Err("Неверный SOCKS VPN".into());}builder=builder.proxy(reqwest::Proxy::all(format!("socks5h://{socks}")).map_err(|_|"Неверный SOCKS VPN")?.basic_auth(user,pass));}
    builder.build().map_err(|_|"Не создать клиент ChatGPT".into())
}
async fn json_response(mut response:reqwest::Response)->Result<Value,String>{
    let status=response.status();let mut bytes=Vec::new();while let Some(chunk)=response.chunk().await.map_err(|_|"Обрыв ответа OpenAI")?{if bytes.len()+chunk.len()>1024*1024{return Err("Слишком большой ответ OpenAI".into());}bytes.extend(chunk);}
    if !status.is_success(){return Err(format!("OpenAI ответил {}",status.as_u16()));}serde_json::from_slice(&bytes).map_err(|_|"Неверный JSON OpenAI".into())
}
fn claims(token:&str)->Value{token.split('.').nth(1).and_then(|v|URL_SAFE_NO_PAD.decode(v.trim_end_matches('=')).ok()).and_then(|b|serde_json::from_slice(&b).ok()).unwrap_or(Value::Null)}
fn tokens_to_auth(tokens:&Value,before:&Value)->Result<Value,String>{
    let access=tokens["access_token"].as_str().filter(|s|!s.is_empty()).ok_or("OpenAI не выдал access token")?;let mut after=if before.is_object(){before.clone()}else{json!({})};after["access"]=access.into();
    if let Some(refresh)=tokens["refresh_token"].as_str().filter(|s|!s.is_empty()){after["refresh"]=refresh.into();}
    for token in [tokens["id_token"].as_str().unwrap_or(""),access]{let claim=claims(token);let data=&claim["https://api.openai.com/auth"];for (to,from) in [("account","chatgpt_account_id"),("plan","chatgpt_plan_type")]{if let Some(value)=data[from].as_str().filter(|s|!s.is_empty()){after[to]=value.into();}}if let Some(email)=claim["email"].as_str(){after["email"]=email.into();}}
    after["expires"]=claims(access)["exp"].as_i64().unwrap_or(now()+tokens["expires_in"].as_i64().unwrap_or(3600)).into();
    if after["account"].as_str().unwrap_or("").is_empty() || after["refresh"].as_str().unwrap_or("").is_empty(){return Err("OpenAI не выдал аккаунт или refresh token".into());}Ok(after)
}
async fn sse(mut response:reqwest::Response)->Result<Value,String>{
    let mut pending=Vec::new();let mut output=Vec::new();let mut complete=false;let mut total=0;
    'stream: while let Some(chunk)=response.chunk().await.map_err(|_|"Обрыв ответа ChatGPT")?{
        total+=chunk.len();if total>16*1024*1024{return Err("Слишком большой ответ ChatGPT".into());}pending.extend(chunk);
        while let Some(end)=pending.iter().position(|b|*b==b'\n'){
            let line:Vec<_>=pending.drain(..=end).collect();let text=std::str::from_utf8(&line).map_err(|_|"Неверная кодировка ChatGPT")?.trim_end();
            let Some(data)=text.strip_prefix("data:").map(str::trim_start)else{continue;};if data=="[DONE]"{continue;}
            let event:Value=serde_json::from_str(data).map_err(|_|"Неверное SSE-событие ChatGPT")?;
            match event["type"].as_str(){Some("response.output_item.done")=>output.push(event["item"].clone()),Some("response.completed")=>{
                complete=true;
                // Codex can send a compact terminal event with output:[] after complete
                // output_item.done events, notably when web_search is enabled.
                // An empty summary must not erase the answer or pending tool calls.
                if let Some(items)=event["response"]["output"].as_array().filter(|items|!items.is_empty()){output=items.clone();}
            },Some("response.failed"|"response.incomplete"|"error")=>return Err("ChatGPT не завершил запрос".into()),_=>{}}
            if output.len()>512{return Err("Слишком много элементов ответа ChatGPT".into());}if complete{break 'stream;}
        }
    }
    if !complete{return Err("Ответ ChatGPT оборван до завершения".into());}
    let mut answer=String::new();let mut calls=vec![];
    for item in &output{if item["type"]=="function_call"{calls.push(item.clone());}if item["type"]=="message"{for part in item["content"].as_array().into_iter().flatten(){if part["type"]=="output_text"{answer.push_str(part["text"].as_str().unwrap_or(""));}}}}
    Ok(json!({"answer":answer,"calls":calls,"output":output}))
}

#[cfg(test)]mod tests{
    use super::*;use tokio::io::{AsyncReadExt,AsyncWriteExt};
    fn jwt(claim:Value)->String{format!("e30.{}.signature",URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claim).unwrap()))}
    fn services(auth:Arc<Mutex<Value>>)->Callback{Arc::new(move|method,params|{let auth=auth.clone();Box::pin(async move{match method.as_str(){"network"=>Ok(json!({})),"auth.get"|"accounts.prepare"=>Ok(auth.lock().unwrap().clone()),"accounts.ok"=>Ok(Value::Null),"auth.set"=>{let mut current=auth.lock().unwrap();if *current!=params["before"]{return Err("account changed".into());}*current=params["after"].clone();Ok(json!(true))},_=>Err("unknown service".into())}})})}
    #[tokio::test]
    async fn refresh_serializes_and_commits_rotated_tokens_atomically(){
        let auth=Arc::new(Mutex::new(json!({"refresh":"old-refresh","account":"account","access":"expired","expires":0})));
        let mut provider=ChatGpt::new(services(auth.clone()));let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();provider.issuer=format!("http://{}",listener.local_addr().unwrap());
        let access=jwt(json!({"exp":now()+3600,"email":"test@example.org","https://api.openai.com/auth":{"chatgpt_account_id":"account","chatgpt_plan_type":"plus"}}));let expected=access.clone();
        let server=tokio::spawn(async move{let (mut socket,_)=listener.accept().await.unwrap();let mut request=[0;8192];let n=socket.read(&mut request).await.unwrap();let request=String::from_utf8_lossy(&request[..n]);assert!(request.starts_with("POST /oauth/token "));let data=json!({"access_token":access,"refresh_token":"rotated-refresh"}).to_string();socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",data.len(),data).as_bytes()).await.unwrap();});
        let (first,second)=tokio::join!(provider.token(),provider.token());assert_eq!(first.unwrap()["access"],expected);assert_eq!(second.unwrap()["refresh"],"rotated-refresh");assert_eq!(auth.lock().unwrap()["plan"],"plus");server.await.unwrap();
    }
    #[tokio::test]
    async fn compact_completed_event_preserves_streamed_text_and_function_calls(){
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let url=format!("http://{}/",listener.local_addr().unwrap());
        let events=[json!({"type":"response.output_item.done","item":{"type":"function_call","name":"weather","call_id":"call-1","arguments":"{}"}}),json!({"type":"response.output_item.done","item":{"type":"message","content":[{"type":"output_text","text":"Ответ"}]}}),json!({"type":"response.completed","response":{"status":"completed","output":[]}})].iter().map(|v|format!("data: {v}\r\n\r\n")).collect::<String>();
        let server=tokio::spawn(async move{let (mut io,_)=listener.accept().await.unwrap();io.read(&mut [0;1024]).await.unwrap();io.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{events}",events.len()).as_bytes()).await.unwrap();});
        let result=sse(client(&json!({})).unwrap().get(url).send().await.unwrap()).await.unwrap();
        assert_eq!(result["answer"],"Ответ");assert_eq!(result["calls"][0]["call_id"],"call-1");assert_eq!(result["output"].as_array().unwrap().len(),2);server.await.unwrap();
    }
    #[tokio::test]
    async fn sse_preserves_tool_calls_and_rejects_an_incomplete_response(){
        for complete in [true,false]{
            let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let url=format!("http://{}/",listener.local_addr().unwrap());
            let mut events=String::from("data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"Привет\"}]}}\n\n");
            if complete{events.push_str("data: {\"type\":\"response.completed\"}\n\n");}
            let server=tokio::spawn(async move{let (mut socket,_)=listener.accept().await.unwrap();let mut request=[0;1024];socket.read(&mut request).await.unwrap();socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",events.len()).as_bytes()).await.unwrap();for part in events.as_bytes().chunks(3){if socket.write_all(part).await.is_err(){break;}tokio::task::yield_now().await;}});
            let response=client(&json!({})).unwrap().get(url).send().await.unwrap();let result=sse(response).await;if complete{assert_eq!(result.unwrap()["answer"],"Привет");}else{assert!(result.unwrap_err().contains("оборван"));}server.await.unwrap();
        }
    }
    #[test]
    fn net_error_classification_covers_common_failures(){
        assert_eq!(classify_net_error(true,false,""),"сервер не ответил вовремя");
        assert_eq!(classify_net_error(false,true,"socks5 error"),"ошибка VPN-прокси");
        assert_eq!(classify_net_error(false,true,"dns lookup failed"),"не удалось определить адрес сервера");
        assert_eq!(classify_net_error(false,false,"tls handshake failed"),"ошибка TLS-соединения");
        assert_eq!(classify_net_error(false,true,"connection refused"),"не удалось подключиться");
        assert_eq!(classify_net_error(false,false,"unexpected eof"),"сетевая ошибка");
    }
}

#[cfg(test)] mod accounts_tests;
