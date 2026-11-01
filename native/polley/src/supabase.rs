//! Cliente mínimo do Supabase: Auth (email/senha), PostgREST (tabelas/RPC) e sessão salva em disco.
//! Fala com o MESMO projeto do app Electron — contas, servidores e mensagens são os mesmos.

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::RwLock;

pub const URL: &str = env!("POLLEY_SUPABASE_URL");
pub const ANON_KEY: &str = env!("POLLEY_SUPABASE_ANON_KEY");

#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub access_token: String,
    pub refresh_token: String,
    pub user_id: String,
}

pub struct Supabase {
    http: reqwest::Client,
    session: RwLock<Option<Session>>,
}

/// Onde a sessão fica salva (igual ao "lembrar de mim" do Electron): %APPDATA%\Polley\session.json
/// Com `--perfil nome`, usa session-nome.json — dá pra abrir duas contas no mesmo PC (testes).
fn session_file() -> PathBuf {
    let base = std::env::var("APPDATA").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    let mut args = std::env::args().skip_while(|a| a != "--perfil").skip(1);
    let name = match args.next() {
        Some(p) => format!("session-{}.json", p.replace(['/', '\\', '.'], "")),
        None => "session.json".into(),
    };
    base.join("Polley").join(name)
}

/// Login de quem ainda não digitou o código de confirmação do cadastro.
pub const NOT_CONFIRMED: &str = "Confirme seu email: enviamos um código novo para você.";

/// A sessão (tokens de login) fica CRIPTOGRAFADA no disco com o DPAPI do Windows: só a sua
/// conta do Windows, neste PC, consegue abrir. Um programa que copie o arquivo para outro PC
/// (os famosos "token grabbers" que roubam contas do Discord) leva só lixo.
mod dpapi {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    fn run(data: &[u8], protect: bool) -> Option<Vec<u8>> {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB::default();
        unsafe {
            let result = if protect {
                CryptProtectData(&input, windows::core::w!("Polley"), None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            } else {
                CryptUnprotectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            };
            result.ok()?;
            let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            let _ = LocalFree(Some(HLOCAL(out.pbData as _)));
            Some(bytes)
        }
    }

    pub fn protect(data: &[u8]) -> Option<Vec<u8>> {
        run(data, true)
    }

    pub fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
        run(data, false)
    }
}

/// Traduz os erros do Supabase Auth para português.
fn auth_error(body: &Value) -> anyhow::Error {
    let msg = body["msg"]
        .as_str()
        .or(body["error_description"].as_str())
        .or(body["message"].as_str())
        .unwrap_or("erro desconhecido");
    let pt = match msg {
        "Invalid login credentials" => "Email ou senha incorretos.",
        "Email not confirmed" => NOT_CONFIRMED,
        "User already registered" => "Já existe uma conta com esse email.",
        m if m.contains("Database error") => "Esse nome de usuário já está em uso.",
        m if m.contains("Password should be") => "A senha precisa ter pelo menos 8 caracteres.",
        m if m.contains("rate limit") => "Muitas tentativas. Espere um pouco e tente de novo.",
        m if m.contains("security purposes") => "Espere um minuto antes de pedir outro código.",
        m if m.contains("expired or is invalid") => "Código errado, vencido ou já usado. Peça outro em \"Reenviar código\".",
        m if m.contains("should be different") => "A nova senha precisa ser diferente da atual.",
        m => m,
    };
    anyhow!(pt.to_string())
}

/// Decodifica base64url (o meio do token JWT).
fn base64url(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => break,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Cópia do token de login atual para baixar anexos (o bucket deles é privado).
static TOKEN: RwLock<Option<String>> = RwLock::new(None);

/// Pedido GET para um arquivo do nosso Storage. Anexos ficam num bucket privado: vão pelo
/// endereço "authenticated" com o login da pessoa, e o banco confere se ela pode ver a
/// mensagem em que o arquivo está. Fotos de perfil e banners continuam públicos.
pub fn storage_get(http: &reqwest::Client, url: &str) -> reqwest::RequestBuilder {
    let public = format!("{URL}/storage/v1/object/public/attachments/");
    match url.strip_prefix(&public) {
        Some(path) => {
            let token = TOKEN.read().unwrap().clone().unwrap_or_else(|| ANON_KEY.to_string());
            http.get(format!("{URL}/storage/v1/object/authenticated/attachments/{path}"))
                .header("apikey", ANON_KEY)
                .bearer_auth(token)
        }
        None => http.get(url),
    }
}

impl Supabase {
    pub fn new() -> Self {
        Self { http: reqwest::Client::new(), session: RwLock::new(None) }
    }

    pub fn user_id(&self) -> Option<String> {
        self.session.read().unwrap().as_ref().map(|s| s.user_id.clone())
    }

    pub fn access_token(&self) -> Option<String> {
        self.session.read().unwrap().as_ref().map(|s| s.access_token.clone())
    }

    fn store(&self, session: Session) {
        let path = session_file().with_extension("dat");
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Some(sealed) = serde_json::to_vec(&session).ok().and_then(|t| dpapi::protect(&t)) {
            let _ = std::fs::write(&path, sealed);
        }
        // Versões antigas guardavam em texto puro: apaga.
        let _ = std::fs::remove_file(session_file());
        *TOKEN.write().unwrap() = Some(session.access_token.clone());
        *self.session.write().unwrap() = Some(session);
    }

    /// Sessão salva: a criptografada (.dat) ou, uma última vez, a antiga em texto (.json).
    fn saved_session() -> Option<Session> {
        if let Some(plain) = std::fs::read(session_file().with_extension("dat")).ok().and_then(|b| dpapi::unprotect(&b)) {
            return serde_json::from_slice(&plain).ok();
        }
        serde_json::from_str(&std::fs::read_to_string(session_file()).ok()?).ok()
    }

    pub fn clear(&self) {
        let _ = std::fs::remove_file(session_file().with_extension("dat"));
        let _ = std::fs::remove_file(session_file());
        *TOKEN.write().unwrap() = None;
        *self.session.write().unwrap() = None;
    }

    async fn token_request(&self, grant: &str, body: Value) -> Result<Session> {
        let resp = self
            .http
            .post(format!("{URL}/auth/v1/token?grant_type={grant}"))
            .header("apikey", ANON_KEY)
            .json(&body)
            .send()
            .await
            .context("sem conexão com o servidor")?;
        let value: Value = resp.json().await?;
        self.session_from(&value)
    }

    /// Resposta do Auth com tokens → sessão salva.
    fn session_from(&self, value: &Value) -> Result<Session> {
        let (Some(access), Some(refresh), Some(user)) =
            (value["access_token"].as_str(), value["refresh_token"].as_str(), value["user"]["id"].as_str())
        else {
            return Err(auth_error(value));
        };
        let session = Session { access_token: access.into(), refresh_token: refresh.into(), user_id: user.into() };
        self.store(session.clone());
        Ok(session)
    }

    /// "Esqueci minha senha": o Supabase manda um email com o código.
    /// (Responde OK mesmo se o email não existir — assim ninguém descobre quem tem conta.)
    pub async fn recover(&self, email: &str) -> Result<()> {
        let resp = self
            .http
            .post(format!("{URL}/auth/v1/recover"))
            .header("apikey", ANON_KEY)
            .json(&json!({ "email": email.trim() }))
            .send()
            .await
            .context("sem conexão com o servidor")?;
        if resp.status().is_success() {
            return Ok(());
        }
        Err(auth_error(&resp.json().await.unwrap_or_default()))
    }

    /// Troca o código do email por uma sessão.
    /// `kind`: "recovery" (esqueci minha senha) ou "email" (confirmar o cadastro).
    pub async fn verify_code(&self, kind: &str, email: &str, code: &str) -> Result<Session> {
        let resp = self
            .http
            .post(format!("{URL}/auth/v1/verify"))
            .header("apikey", ANON_KEY)
            .json(&json!({ "type": kind, "email": email.trim(), "token": code.trim() }))
            .send()
            .await
            .context("sem conexão com o servidor")?;
        let value: Value = resp.json().await.unwrap_or_default();
        self.session_from(&value)
    }

    /// Manda de novo o código de confirmação do cadastro.
    pub async fn resend_signup(&self, email: &str) -> Result<()> {
        let resp = self
            .http
            .post(format!("{URL}/auth/v1/resend"))
            .header("apikey", ANON_KEY)
            .json(&json!({ "type": "signup", "email": email.trim() }))
            .send()
            .await
            .context("sem conexão com o servidor")?;
        if resp.status().is_success() {
            return Ok(());
        }
        Err(auth_error(&resp.json().await.unwrap_or_default()))
    }

    /// Define a senha de quem está logado.
    pub async fn update_password(&self, password: &str) -> Result<()> {
        let resp = self
            .authed(self.http.put(format!("{URL}/auth/v1/user")))
            .json(&json!({ "password": password }))
            .send()
            .await
            .context("sem conexão com o servidor")?;
        if resp.status().is_success() {
            return Ok(());
        }
        Err(auth_error(&resp.json().await.unwrap_or_default()))
    }

    /// Email da conta logada.
    pub async fn email(&self) -> Result<String> {
        let resp = self
            .authed(self.http.get(format!("{URL}/auth/v1/user")))
            .send()
            .await
            .context("sem conexão com o servidor")?;
        let value: Value = resp.json().await.unwrap_or_default();
        value["email"].as_str().map(String::from).ok_or_else(|| auth_error(&value))
    }

    pub async fn sign_in(&self, email: &str, password: &str) -> Result<Session> {
        self.token_request("password", json!({ "email": email.trim(), "password": password })).await
    }

    /// Cadastro. Devolve `None` se o projeto exige confirmar o email antes de entrar.
    pub async fn sign_up(&self, email: &str, password: &str, username: &str) -> Result<Option<Session>> {
        let name = username.trim().to_lowercase();
        let valid = (2..=32).contains(&name.len())
            && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.');
        if !valid {
            bail!("Nome de usuário: 2–32 caracteres, só letras minúsculas, números, _ e .");
        }
        let resp = self
            .http
            .post(format!("{URL}/auth/v1/signup"))
            .header("apikey", ANON_KEY)
            .json(&json!({
                "email": email.trim(),
                "password": password,
                // A versão dos Termos/Política aceita fica salva na conta (prova do aceite, LGPD).
                "data": { "username": name, "display_name": username.trim(), "terms_version": crate::legal::VERSION }
            }))
            .send()
            .await
            .context("sem conexão com o servidor")?;
        let status = resp.status();
        let value: Value = resp.json().await?;
        if !status.is_success() {
            return Err(auth_error(&value));
        }
        match (value["access_token"].as_str(), value["refresh_token"].as_str(), value["user"]["id"].as_str()) {
            (Some(a), Some(r), Some(u)) => {
                let session = Session { access_token: a.into(), refresh_token: r.into(), user_id: u.into() };
                self.store(session.clone());
                Ok(Some(session))
            }
            _ => Ok(None),
        }
    }

    /// Tenta voltar a sessão salva (renovando o token). `Ok(false)` = precisa logar.
    pub async fn restore(&self) -> Result<bool> {
        let Some(saved) = Self::saved_session() else { return Ok(false) };
        match self.token_request("refresh_token", json!({ "refresh_token": saved.refresh_token })).await {
            Ok(_) => Ok(true),
            Err(_) => {
                self.clear();
                Ok(false)
            }
        }
    }

    /// Renova o access token (ele expira em 1 hora).
    pub async fn refresh(&self) -> Result<String> {
        let refresh = self.session.read().unwrap().as_ref().map(|s| s.refresh_token.clone()).context("sem sessão")?;
        Ok(self.token_request("refresh_token", json!({ "refresh_token": refresh })).await?.access_token)
    }

    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let token = self.access_token().unwrap_or_else(|| ANON_KEY.to_string());
        req.header("apikey", ANON_KEY).bearer_auth(token)
    }

    async fn parse<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T> {
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["message"].as_str().map(str::to_string))
                .unwrap_or(text);
            bail!(msg);
        }
        Ok(serde_json::from_str(&text)?)
    }

    /// GET /rest/v1/<consulta>, ex.: `servers?select=*&order=created_at`
    pub async fn select<T: DeserializeOwned>(&self, query: &str) -> Result<T> {
        let resp = self.authed(self.http.get(format!("{URL}/rest/v1/{query}"))).send().await?;
        Self::parse(resp).await
    }

    /// INSERT devolvendo a linha criada.
    pub async fn insert<T: DeserializeOwned>(&self, table: &str, row: Value) -> Result<T> {
        let resp = self
            .authed(self.http.post(format!("{URL}/rest/v1/{table}")))
            .header("Prefer", "return=representation")
            .header("Accept", "application/vnd.pgrst.object+json")
            .json(&row)
            .send()
            .await?;
        Self::parse(resp).await
    }

    /// INSERT ou, se a linha já existe (mesma chave), atualiza só as colunas enviadas.
    pub async fn upsert(&self, table: &str, row: Value) -> Result<()> {
        let resp = self
            .authed(self.http.post(format!("{URL}/rest/v1/{table}")))
            .header("Prefer", "resolution=merge-duplicates,return=minimal")
            .json(&row)
            .send()
            .await?;
        Self::check(resp).await
    }

    /// UPDATE nas linhas que batem com o filtro (ex.: `id=eq.<id>`).
    pub async fn update(&self, table: &str, filter: &str, changes: Value) -> Result<()> {
        let resp = self.authed(self.http.patch(format!("{URL}/rest/v1/{table}?{filter}"))).json(&changes).send().await?;
        Self::check(resp).await
    }

    /// DELETE nas linhas que batem com o filtro.
    pub async fn delete(&self, table: &str, filter: &str) -> Result<()> {
        let resp = self.authed(self.http.delete(format!("{URL}/rest/v1/{table}?{filter}"))).send().await?;
        Self::check(resp).await
    }

    async fn check(resp: reqwest::Response) -> Result<()> {
        if resp.status().is_success() {
            return Ok(());
        }
        let text = resp.text().await.unwrap_or_default();
        let msg = serde_json::from_str::<Value>(&text).ok().and_then(|v| v["message"].as_str().map(str::to_string));
        bail!(msg.unwrap_or(text))
    }

    /// Chama uma função SQL (create_server, join_server...).
    pub async fn rpc<T: DeserializeOwned>(&self, function: &str, args: Value) -> Result<T> {
        let resp = self.authed(self.http.post(format!("{URL}/rest/v1/rpc/{function}"))).json(&args).send().await?;
        Self::parse(resp).await
    }

    /// RPC que não devolve nada (`returns void`).
    pub async fn rpc_void(&self, function: &str, args: Value) -> Result<()> {
        let resp = self.authed(self.http.post(format!("{URL}/rest/v1/rpc/{function}"))).json(&args).send().await?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let text = resp.text().await.unwrap_or_default();
        let msg = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v["message"].as_str().map(str::to_string))
            .unwrap_or(text);
        bail!(msg)
    }

    /// Envia um arquivo para o Storage e devolve a URL pública.
    /// `path` precisa começar com o id do usuário (a regra do bucket só deixa escrever na própria pasta).
    pub async fn upload(&self, bucket: &str, path: &str, bytes: Vec<u8>, content_type: &str) -> Result<String> {
        let resp = self
            .authed(self.http.post(format!("{URL}/storage/v1/object/{bucket}/{path}")))
            .header("Content-Type", content_type)
            .body(bytes)
            .send()
            .await
            .context("sem conexão com o servidor")?;
        Self::check(resp).await?;
        Ok(format!("{URL}/storage/v1/object/public/{bucket}/{path}"))
    }

    /// Dados da conta logada (email, fatores da verificação em 2 etapas...).
    pub async fn user(&self) -> Result<Value> {
        let resp = self
            .authed(self.http.get(format!("{URL}/auth/v1/user")))
            .send()
            .await
            .context("sem conexão com o servidor")?;
        Self::parse(resp).await
    }

    /// Nível da sessão atual: "aal1" (só senha) ou "aal2" (senha + código do app autenticador).
    pub fn aal(&self) -> String {
        let token = self.access_token().unwrap_or_default();
        let payload = token.split('.').nth(1).and_then(base64url).unwrap_or_default();
        serde_json::from_slice::<Value>(&payload)
            .ok()
            .and_then(|v| v["aal"].as_str().map(String::from))
            .unwrap_or_else(|| "aal1".into())
    }

    /// Id do fator TOTP (app autenticador) já confirmado, se a pessoa ativou as 2 etapas.
    pub async fn totp_factor(&self) -> Result<Option<String>> {
        let user = self.user().await?;
        Ok(user["factors"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|f| f["factor_type"] == "totp" && f["status"] == "verified")
            .and_then(|f| f["id"].as_str().map(String::from)))
    }

    /// Começa a ativar as 2 etapas: devolve (id do fator, endereço otpauth:// para o QR, segredo).
    pub async fn mfa_enroll(&self) -> Result<(String, String, String)> {
        // Tentativas antigas que não foram confirmadas atrapalham: apaga.
        let user = self.user().await?;
        for f in user["factors"].as_array().into_iter().flatten() {
            if f["status"] != "verified" {
                if let Some(id) = f["id"].as_str() {
                    let _ = self.mfa_unenroll(id).await;
                }
            }
        }
        let resp = self
            .authed(self.http.post(format!("{URL}/auth/v1/factors")))
            .json(&json!({ "factor_type": "totp", "friendly_name": format!("Polley {}", chrono::Utc::now().timestamp()) }))
            .send()
            .await
            .context("sem conexão com o servidor")?;
        let v: Value = Self::parse(resp).await?;
        let id = v["id"].as_str().context("resposta sem id")?.to_string();
        let uri = v["totp"]["uri"].as_str().context("resposta sem QR")?.to_string();
        let secret = v["totp"]["secret"].as_str().unwrap_or_default().to_string();
        Ok((id, uri, secret))
    }

    /// Confere o código de 6 dígitos. Deu certo = a sessão vira "aal2" (salva).
    pub async fn mfa_verify(&self, factor: &str, code: &str) -> Result<()> {
        let resp = self
            .authed(self.http.post(format!("{URL}/auth/v1/factors/{factor}/challenge")))
            .json(&json!({}))
            .send()
            .await
            .context("sem conexão com o servidor")?;
        let challenge: Value = Self::parse(resp).await?;
        let challenge = challenge["id"].as_str().context("resposta sem desafio")?;
        let resp = self
            .authed(self.http.post(format!("{URL}/auth/v1/factors/{factor}/verify")))
            .json(&json!({ "challenge_id": challenge, "code": code.trim() }))
            .send()
            .await
            .context("sem conexão com o servidor")?;
        let status = resp.status();
        let v: Value = resp.json().await.unwrap_or_default();
        if !status.is_success() {
            bail!("Código errado ou vencido. Confira o app e tente de novo.");
        }
        self.session_from(&v)?;
        Ok(())
    }

    /// Desliga as 2 etapas (precisa da sessão "aal2").
    pub async fn mfa_unenroll(&self, factor: &str) -> Result<()> {
        let resp = self
            .authed(self.http.delete(format!("{URL}/auth/v1/factors/{factor}")))
            .send()
            .await
            .context("sem conexão com o servidor")?;
        if resp.status().is_success() {
            return Ok(());
        }
        Err(auth_error(&resp.json().await.unwrap_or_default()))
    }

    /// "Sair de todos os aparelhos": invalida todas as sessões da conta (inclusive esta).
    pub async fn logout_all(&self) -> Result<()> {
        let resp = self
            .authed(self.http.post(format!("{URL}/auth/v1/logout?scope=global")))
            .send()
            .await
            .context("sem conexão com o servidor")?;
        if !resp.status().is_success() {
            return Err(auth_error(&resp.json().await.unwrap_or_default()));
        }
        self.clear();
        Ok(())
    }

    /// Link temporário (5 min) de um anexo, para abrir no navegador. `url` é o endereço gravado
    /// na mensagem; o link só sai se a pessoa puder ver o arquivo.
    pub async fn signed_attachment_url(&self, url: &str) -> Result<String> {
        let public = format!("{URL}/storage/v1/object/public/attachments/");
        let path = url.strip_prefix(&public).context("não é um anexo")?;
        let resp = self
            .authed(self.http.post(format!("{URL}/storage/v1/object/sign/attachments/{path}")))
            .json(&json!({ "expiresIn": 300 }))
            .send()
            .await
            .context("sem conexão com o servidor")?;
        let value: Value = Self::parse(resp).await?;
        let signed = value["signedURL"].as_str().context("resposta sem link")?;
        // Só aceita um link do próprio Storage, do mesmo arquivo.
        if !signed.starts_with(&format!("/object/sign/attachments/{path}?token=")) {
            bail!("link inesperado");
        }
        Ok(format!("{URL}/storage/v1{signed}"))
    }

    /// Manda um broadcast do Realtime pela API REST (sem precisar entrar no canal).
    /// Em canal privado, quem decide se pode é a regra de INSERT em realtime.messages.
    pub async fn broadcast(&self, topic: &str, event: &str, payload: Value) -> Result<()> {
        let body = serde_json::json!({ "messages": [{ "topic": topic, "event": event, "payload": payload, "private": true }] });
        let resp = self
            .authed(self.http.post(format!("{URL}/realtime/v1/api/broadcast")))
            .json(&body)
            .send()
            .await
            .context("sem conexão com o servidor")?;
        Self::check(resp).await
    }

    /// Chama uma Edge Function (ex.: `livekit-token`). Erros vêm como `{ "error": "..." }`.
    pub async fn function(&self, name: &str, body: Value) -> Result<Value> {
        let resp = self
            .authed(self.http.post(format!("{URL}/functions/v1/{name}")))
            .json(&body)
            .send()
            .await
            .context("sem conexão com o servidor")?;
        let status = resp.status();
        let value: Value = resp.json().await.unwrap_or_default();
        if !status.is_success() {
            bail!("{}", value["error"].as_str().unwrap_or("erro no servidor"));
        }
        Ok(value)
    }
}
