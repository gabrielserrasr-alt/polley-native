//! Chat: anexos (imagens e arquivos), editar/apagar mensagens, "digitando..." e mensagens antigas.
//! O básico (abrir conversa, receber ao vivo, enviar texto) fica em `app.rs`.

use super::*;
use crate::AttachmentItem;
use serde::Serialize;
use std::path::PathBuf;
use std::time::Instant;

/// Mensagens por página (a primeira carga e cada "carregar antigas").
pub(super) const PAGE: usize = 50;
/// Colunas lidas de cada mensagem.
pub(super) const COLUMNS: &str = "id,author_id,content,created_at,attachments,edited_at,reply_to";
/// Tamanho máximo de cada arquivo enviado (o plano grátis do Supabase aceita até 50 MB).
const MAX_UPLOAD: u64 = 25 * 1024 * 1024;
/// Máximo de anexos por mensagem (o banco também confere).
const MAX_FILES: usize = 10;
/// Tamanho máximo de um anexo baixado com "Baixar".
const MAX_SAVE: u64 = 200 * 1024 * 1024;
/// Quanto tempo o "digitando..." dura sem novo aviso.
const TYPING_TTL: Duration = Duration::from_millis(3500);
/// De quanto em quanto tempo eu aviso que estou digitando.
const TYPING_EVERY: Duration = Duration::from_millis(2500);

/// Mesmo formato do app Electron: `{ url, name, size, type }`.
#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Attachment {
    pub url: String,
    pub name: String,
    #[serde(default)]
    pub size: u64,
    #[serde(rename = "type", default)]
    pub kind: String,
}

impl Attachment {
    pub fn is_image(&self) -> bool {
        self.kind.starts_with("image/")
    }

    pub fn is_video(&self) -> bool {
        video_ext(&self.name).is_some()
    }
}

/// Extensão de vídeo que o app aceita abrir no player do Windows (nunca outra coisa).
fn video_ext(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    ["mp4", "webm", "mov", "mkv", "m4v", "avi", "wmv"].into_iter().find(|e| *e == ext)
}

/// Maior imagem aberta no visualizador e maior vídeo baixado para assistir.
const MAX_VIEW: u64 = 30 * 1024 * 1024;
const MAX_VIDEO: u64 = 200 * 1024 * 1024;

/// Baixa um anexo do nosso Storage (sem seguir redirecionamentos, com limite de tamanho).
async fn fetch(url: &str, max: u64) -> Result<Vec<u8>> {
    if !images::allowed(url) {
        anyhow::bail!("endereço não permitido");
    }
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build()?;
    let mut resp = crate::supabase::storage_get(&http, url).send().await?.error_for_status()?;
    if resp.content_length().is_some_and(|n| n > max) {
        anyhow::bail!("arquivo grande demais");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() as u64 > max {
            anyhow::bail!("arquivo grande demais");
        }
    }
    Ok(bytes)
}

/// Estado do chat que não cabe no `State` principal.
#[derive(Default)]
pub(super) struct ChatExtra {
    /// Ainda existem mensagens mais antigas no servidor.
    pub has_more: bool,
    pub loading_older: bool,
    /// Quem está digitando → até quando.
    pub typers: HashMap<String, Instant>,
    pub typing_sent: Option<Instant>,
    /// Arquivos escolhidos esperando o "Enter".
    pub pending: Vec<PathBuf>,
    pub sending: bool,
    /// Reações de cada mensagem carregada: id → [(pessoa, emoji)].
    pub reactions: HashMap<String, Vec<(String, String)>>,
    pub react_topic: Option<String>,
}

pub(super) fn format_size(bytes: u64) -> String {
    match bytes {
        b if b >= 1024 * 1024 => format!("{:.1} MB", b as f64 / 1024.0 / 1024.0),
        b if b >= 1024 => format!("{} KB", b / 1024),
        b => format!("{b} bytes"),
    }
}

pub(super) fn attachment_items(list: &[Attachment]) -> Vec<AttachmentItem> {
    list.iter()
        .filter(|a| images::allowed(&a.url)) // anexos de fora do nosso Storage nem aparecem
        .map(|a| AttachmentItem {
            url: a.url.clone().into(),
            name: a.name.clone().into(),
            size: format_size(a.size).into(),
            image: a.is_image(),
            video: a.is_video(),
        })
        .collect()
}

/// Nome seguro para o Storage e para o disco: só letras, números, ponto, hífen e sublinhado.
/// 128 bits aleatórios em hexadecimal.
fn random_id() -> String {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 16];
    let _ = ring::rand::SystemRandom::new().fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Marca o arquivo como "veio da internet" (igual aos navegadores). Assim o Windows avisa
/// antes de abrir um .exe/.bat baixado do chat e o Office abre documentos em modo protegido.
fn mark_from_internet(path: &std::path::Path, url: &str) {
    let stream = format!("{}:Zone.Identifier", path.display());
    let _ = std::fs::write(stream, format!("[ZoneTransfer]\r\nZoneId=3\r\nHostUrl={url}\r\n"));
}

fn safe_name(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect();
    while s.contains("..") {
        s = s.replace("..", "_");
    }
    let s = s.trim_matches('.').to_string();
    let s: String = s.chars().rev().take(80).collect::<Vec<_>>().into_iter().rev().collect(); // guarda a extensão
    if s.is_empty() { "arquivo".into() } else { s }
}

fn content_type(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "txt" | "log" => "text/plain",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

/// Pasta Downloads do usuário (segue o Windows, inclusive se tiver sido movida).
fn downloads_dir() -> PathBuf {
    use windows::Win32::UI::Shell::{FOLDERID_Downloads, SHGetKnownFolderPath, KNOWN_FOLDER_FLAG};
    unsafe { SHGetKnownFolderPath(&FOLDERID_Downloads, KNOWN_FOLDER_FLAG(0), None) }
        .ok()
        .and_then(|p| {
            let path = unsafe { p.to_string() }.ok();
            unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _)) };
            path
        })
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default()).join("Downloads"))
}

/// "foto.png" já existe → "foto (1).png", "foto (2).png"...
fn unique_path(dir: &std::path::Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    if !path.exists() {
        return path;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{e}")),
        None => (name.to_string(), String::new()),
    };
    (1..).map(|i| dir.join(format!("{stem} ({i}){ext}"))).find(|p| !p.exists()).unwrap()
}


impl App {
    fn chat_key(&self) -> Option<(String, &'static str)> {
        let st = self.state.lock().unwrap();
        Some((st.current_channel.clone()?, st.chat_column)).filter(|(_, c)| !c.is_empty())
    }

    // ------------------------------------------------------------ anexos

    pub async fn pick_files(self: Arc<Self>) {
        let picked = tokio::task::spawn_blocking(|| rfd::FileDialog::new().set_title("Enviar arquivos").pick_files())
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        if picked.is_empty() {
            return;
        }
        let mut error = String::new();
        {
            let mut st = self.state.lock().unwrap();
            for path in picked {
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                if size > MAX_UPLOAD {
                    error = format!("{} é grande demais (máximo 25 MB).", path.file_name().unwrap_or_default().to_string_lossy());
                } else if st.chat.pending.len() >= MAX_FILES {
                    error = "No máximo 10 arquivos por mensagem.".into();
                } else if !st.chat.pending.contains(&path) {
                    st.chat.pending.push(path);
                }
            }
        }
        self.render_pending();
        self.ui(move |ui| ui.set_chat_error(error.into()));
    }

    pub fn remove_pending(&self, index: usize) {
        {
            let mut st = self.state.lock().unwrap();
            if index < st.chat.pending.len() {
                st.chat.pending.remove(index);
            }
        }
        self.render_pending();
    }

    pub(super) fn render_pending(&self) {
        let (names, sending): (Vec<SharedString>, bool) = {
            let st = self.state.lock().unwrap();
            let names = st
                .chat
                .pending
                .iter()
                .map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default().into())
                .collect();
            (names, st.chat.sending)
        };
        self.ui(move |ui| {
            ui.set_pending_files(ModelRc::new(VecModel::from(names)));
            ui.set_chat_sending(sending);
        });
    }

    /// Sobe os arquivos escolhidos. Fotos grandes viram JPEG de até 1920 px antes de subir.
    pub(super) async fn upload_pending(&self, files: Vec<PathBuf>) -> Result<Vec<Attachment>> {
        let me = self.sb.user_id().ok_or_else(|| anyhow::anyhow!("sessão expirada"))?;
        let mut out = Vec::new();
        for path in files {
            let original = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "arquivo".into());
            let prepared = tokio::task::spawn_blocking(move || -> Result<_> {
                let bytes = std::fs::read(&path)?;
                if bytes.len() as u64 > MAX_UPLOAD {
                    anyhow::bail!("arquivo grande demais (máximo 25 MB)");
                }
                let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
                let is_photo = matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp" | "bmp");
                // GIF fica como está (senão perde a animação).
                if let Some(img) = is_photo.then(|| images::decode(&bytes).ok()).flatten() {
                    let preview = images::for_display(&img, Kind::Attachment);
                    let big = bytes.len() > 1_500_000 || img.width() > 1920 || img.height() > 1920 || ext == "bmp";
                    if big {
                        let (data, mime, new_ext) = images::for_upload(&img, Kind::Attachment)?;
                        return Ok((data, mime, Some(new_ext), Some(preview)));
                    }
                    return Ok((bytes, content_type(&ext), None, Some(preview)));
                }
                Ok((bytes, content_type(&ext), None, None))
            })
            .await??;
            let (data, mime, new_ext, preview) = prepared;
            let name = match new_ext {
                Some(ext) => format!("{}.{ext}", original.rsplit_once('.').map(|(s, _)| s).unwrap_or(&original)),
                None => original,
            };
            let size = data.len() as u64;
            // Nome imprevisível: o bucket é público, então ninguém pode "adivinhar" o endereço
            // de um anexo de DM chutando horários.
            let id = random_id();
            let url = self.sb.upload("attachments", &format!("{me}/{id}-{}", safe_name(&name)), data, mime).await?;
            if let Some(pixels) = preview {
                self.images.put(&url, Kind::Attachment, pixels); // já temos a imagem: não precisa baixar
            }
            out.push(Attachment { url, name, size, kind: mime.to_string() });
        }
        Ok(out)
    }

    /// Clicar numa imagem: abre grande dentro do app. Só a imagem aberta fica na memória
    /// (reduzida para no máximo 2560x1440) e é solta ao fechar.
    pub async fn view_image(self: Arc<Self>, url: String, name: String) {
        let generation = self.viewer_gen.fetch_add(1, Ordering::Relaxed) + 1;
        {
            let (url, name) = (url.clone(), name.clone());
            self.ui(move |ui| {
                ui.set_viewer_url(url.into());
                ui.set_viewer_name(name.into());
                ui.set_viewer_status("Carregando...".into());
                ui.set_viewer_image(Default::default());
                ui.set_viewer_open(true);
            });
        }
        let result = async {
            let bytes = fetch(&url, MAX_VIEW).await?;
            tokio::task::spawn_blocking(move || -> Result<_> {
                let img = images::decode(&bytes)?;
                let img = if img.width() > 2560 || img.height() > 1440 {
                    img.resize(2560, 1440, image::imageops::FilterType::Triangle)
                } else {
                    img
                };
                let rgba = img.to_rgba8();
                let (w, h) = rgba.dimensions();
                Ok(slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba.as_raw(), w, h))
            })
            .await?
        }
        .await;
        let app = self.clone();
        self.ui(move |ui| {
            if app.viewer_gen.load(Ordering::Relaxed) != generation {
                return; // fechou ou abriu outra
            }
            match result {
                Ok(pixels) => {
                    ui.set_viewer_image(slint::Image::from_rgba8(pixels));
                    ui.set_viewer_status("".into());
                }
                Err(e) => ui.set_viewer_status(format!("Não deu para abrir a imagem: {e}").into()),
            }
        });
    }

    pub fn close_viewer(&self) {
        self.viewer_gen.fetch_add(1, Ordering::Relaxed);
        self.ui(|ui| {
            ui.set_viewer_open(false);
            ui.set_viewer_image(Default::default());
        });
    }

    /// "Assistir": baixa o vídeo para a pasta temporária e abre no player do Windows.
    /// A extensão é sempre a de vídeo que nós conferimos (um ".exe" disfarçado nunca é aberto).
    pub async fn play_video(self: Arc<Self>, url: String, name: String) {
        let Some(ext) = video_ext(&name) else { return };
        let generation = self.viewer_gen.fetch_add(1, Ordering::Relaxed) + 1;
        {
            let name = name.clone();
            self.ui(move |ui| {
                ui.set_viewer_url("".into());
                ui.set_viewer_name(name.into());
                ui.set_viewer_status("Baixando o vídeo...".into());
                ui.set_viewer_image(Default::default());
                ui.set_viewer_open(true);
            });
        }
        let dir = std::env::temp_dir().join("Polley");
        // Mesmo endereço → mesmo arquivo (não baixa de novo o que já assistiu).
        let key = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            url.hash(&mut h);
            h.finish()
        };
        let path = dir.join(format!("video-{key:016x}.{ext}"));
        let result: Result<()> = async {
            if !path.exists() {
                let bytes = fetch(&url, MAX_VIDEO).await?;
                std::fs::create_dir_all(&dir)?;
                std::fs::write(&path, bytes)?;
            }
            std::process::Command::new("explorer.exe").arg(&path).spawn()?;
            Ok(())
        }
        .await;
        let app = self.clone();
        self.ui(move |ui| {
            if app.viewer_gen.load(Ordering::Relaxed) != generation {
                return;
            }
            match result {
                Ok(()) => ui.set_viewer_open(false),
                Err(e) => ui.set_viewer_status(format!("Não deu para abrir o vídeo: {e}").into()),
            }
        });
    }

    /// "Abrir no navegador" (só endereços do nosso Storage). Anexos são privados: o navegador
    /// recebe um link temporário de 5 minutos, que só é gerado se a pessoa pode ver o arquivo.
    pub async fn open_attachment(self: Arc<Self>, url: String) {
        if !images::allowed(&url) {
            return;
        }
        match self.sb.signed_attachment_url(&url).await {
            Ok(link) => {
                let _ = std::process::Command::new("explorer.exe").arg(link).spawn();
            }
            Err(e) => {
                let msg = format!("Não deu para abrir: {e}");
                self.ui(move |ui| ui.set_chat_error(msg.into()));
            }
        }
    }

    /// "Baixar": salva em Downloads e mostra o arquivo na pasta (nunca abre/executa sozinho).
    pub async fn save_attachment(self: Arc<Self>, url: String, name: String) {
        if !images::allowed(&url) {
            return;
        }
        let result: Result<PathBuf> = async {
            let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build()?;
            let mut resp = crate::supabase::storage_get(&http, &url).send().await?.error_for_status()?;
            if resp.content_length().is_some_and(|n| n > MAX_SAVE) {
                anyhow::bail!("arquivo grande demais");
            }
            let path = unique_path(&downloads_dir(), &safe_name(&name));
            let mut file = std::fs::File::create(&path)?;
            let mut total = 0u64;
            while let Some(chunk) = resp.chunk().await? {
                total += chunk.len() as u64;
                if total > MAX_SAVE {
                    drop(file);
                    let _ = std::fs::remove_file(&path);
                    anyhow::bail!("arquivo grande demais");
                }
                std::io::Write::write_all(&mut file, &chunk)?;
            }
            drop(file);
            mark_from_internet(&path, &url);
            Ok(path)
        }
        .await;
        match result {
            Ok(path) => {
                let _ = std::process::Command::new("explorer.exe").arg(format!("/select,{}", path.display())).spawn();
            }
            Err(e) => {
                let msg = format!("Não deu para baixar: {e}");
                self.ui(move |ui| ui.set_chat_error(msg.into()));
            }
        }
    }

    // ------------------------------------------------------------ editar / apagar

    pub async fn edit_message(self: Arc<Self>, id: String, text: String) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        let changes = json!({ "content": text, "edited_at": chrono::Utc::now().to_rfc3339() });
        match self.sb.update("messages", &format!("id=eq.{id}"), changes).await {
            Ok(()) => {
                {
                    let mut st = self.state.lock().unwrap();
                    if let Some(m) = st.messages.iter_mut().find(|m| m.id == id) {
                        m.content = text;
                        m.edited_at = Some(chrono::Utc::now().to_rfc3339());
                    }
                }
                self.render_messages(false);
            }
            Err(e) => {
                let msg = format!("Não deu para editar: {e}");
                self.ui(move |ui| ui.set_chat_error(msg.into()));
            }
        }
    }

    pub async fn delete_message(self: Arc<Self>, id: String) {
        match self.sb.delete("messages", &format!("id=eq.{id}")).await {
            Ok(()) => {
                self.state.lock().unwrap().messages.retain(|m| m.id != id);
                self.render_messages(false);
            }
            Err(e) => {
                let msg = format!("Não deu para apagar: {e}");
                self.ui(move |ui| ui.set_chat_error(msg.into()));
            }
        }
    }

    // ------------------------------------------------------------ mensagens antigas

    /// Chegou no topo da conversa: busca as 50 anteriores e mantém a rolagem no lugar.
    pub async fn load_older(self: Arc<Self>) {
        let Some((id, column)) = self.chat_key() else { return };
        let before = {
            let mut st = self.state.lock().unwrap();
            if !st.chat.has_more || st.chat.loading_older {
                return;
            }
            let Some(first) = st.messages.first().map(|m| m.created_at.clone()) else { return };
            st.chat.loading_older = true;
            first
        };
        self.ui(|ui| ui.set_chat_loading_older(true));
        // O "+" do fuso horário precisa ir codificado na URL.
        let before = before.replace('+', "%2B");
        let mut older: Vec<Message> = self
            .sb
            .select(&format!("messages?{column}=eq.{id}&created_at=lt.{before}&select={COLUMNS}&order=created_at.desc&limit={PAGE}"))
            .await
            .unwrap_or_default();
        older.reverse();
        let authors: Vec<String> = older.iter().map(|m| m.author_id.clone()).collect();
        self.ensure_profiles(&authors).await;
        self.load_reactions(older.iter().map(|m| m.id.clone()).collect()).await;
        {
            let mut st = self.state.lock().unwrap();
            st.chat.loading_older = false;
            if st.current_channel.as_deref() != Some(&id) {
                return;
            }
            st.chat.has_more = older.len() == PAGE;
            let known: HashSet<String> = st.messages.iter().map(|m| m.id.clone()).collect();
            older.retain(|m| !known.contains(&m.id));
            older.append(&mut st.messages);
            st.messages = older;
        }
        let more = self.state.lock().unwrap().chat.has_more;
        self.ui(move |ui| {
            ui.set_chat_loading_older(false);
            ui.set_chat_has_more(more);
            ui.invoke_chat_remember();
        });
        self.render(Scroll::KeepFromBottom);
    }

    // ------------------------------------------------------------ digitando...

    /// Eu digitei algo: avisa os outros (no máximo a cada 2,5 s).
    pub fn typing(&self) {
        let Some((id, column)) = self.chat_key() else { return };
        let Some(me) = self.sb.user_id() else { return };
        {
            let mut st = self.state.lock().unwrap();
            if st.chat.typing_sent.is_some_and(|t| t.elapsed() < TYPING_EVERY) {
                return;
            }
            st.chat.typing_sent = Some(Instant::now());
        }
        if let Some(rt) = self.realtime() {
            rt.broadcast(&format!("chat:{column}:{id}"), "typing", json!({ "user_id": me }));
        }
    }

    /// Alguém avisou que está digitando.
    pub(super) async fn someone_typing(&self, user: String) {
        if Some(&user) == self.sb.user_id().as_ref() || user.is_empty() {
            return;
        }
        self.ensure_profiles(std::slice::from_ref(&user)).await;
        self.state.lock().unwrap().chat.typers.insert(user, Instant::now() + TYPING_TTL);
        self.render_typing();
    }

    /// Mensagem de alguém chegou: essa pessoa parou de digitar.
    pub(super) fn stopped_typing(&self, user: &str) {
        if self.state.lock().unwrap().chat.typers.remove(user).is_some() {
            self.render_typing();
        }
    }

    /// Tira quem parou de digitar há mais de 3,5 s. Roda enquanto a conversa estiver aberta.
    pub(super) fn start_typing_clock(self: &Arc<Self>, chat: String) {
        let app = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let changed = {
                    let mut st = app.state.lock().unwrap();
                    if st.current_channel.as_deref() != Some(&chat) {
                        break;
                    }
                    let before = st.chat.typers.len();
                    let now = Instant::now();
                    st.chat.typers.retain(|_, until| *until > now);
                    before != st.chat.typers.len()
                };
                if changed {
                    app.render_typing();
                }
            }
        });
    }

    pub(super) fn render_typing(&self) {
        let text = {
            let st = self.state.lock().unwrap();
            let mut names: Vec<String> =
                st.chat.typers.keys().map(|id| st.profiles.get(id).map(|p| p.name()).unwrap_or_else(|| "Alguém".into())).collect();
            names.sort();
            match names.as_slice() {
                [] => String::new(),
                [a] => format!("{a} está digitando..."),
                [a, b] => format!("{a} e {b} estão digitando..."),
                [a, b, c] => format!("{a}, {b} e {c} estão digitando..."),
                _ => "Várias pessoas estão digitando...".into(),
            }
        };
        self.ui(move |ui| ui.set_typing_text(text.into()));
    }
}

/// Uma linha da lista de mensagens montada fora da thread da interface. O `MessageItem` do Slint
/// tem uma lista de anexos, e listas do Slint só existem na thread da interface: por isso os campos
/// viajam soltos e viram `MessageItem` lá.
pub(super) struct MsgRow {
    pub id: SharedString,
    pub author: SharedString,
    pub author_id: SharedString,
    pub avatar: SharedString,
    pub initials: SharedString,
    pub color: Color,
    pub time: SharedString,
    pub content: SharedString,
    pub grouped: bool,
    pub mine: bool,
    pub can_delete: bool,
    pub edited: bool,
    pub attachments: Vec<AttachmentItem>,
    pub reply_id: SharedString,
    pub reply_author: SharedString,
    pub reply_text: SharedString,
    pub reply_color: Color,
    pub reactions: Vec<crate::ReactionItem>,
    pub rich: slint::StyledText,
    pub jumbo: Vec<SharedString>,
}

impl MsgRow {
    pub fn into_item(self) -> MessageItem {
        MessageItem {
            id: self.id,
            author: self.author,
            author_id: self.author_id,
            avatar: self.avatar,
            initials: self.initials,
            color: self.color,
            time: self.time,
            content: self.content,
            grouped: self.grouped,
            mine: self.mine,
            can_delete: self.can_delete,
            edited: self.edited,
            attachments: ModelRc::new(VecModel::from(self.attachments)),
            reply_id: self.reply_id,
            reply_author: self.reply_author,
            reply_text: self.reply_text,
            reply_color: self.reply_color,
            reactions: ModelRc::new(VecModel::from(self.reactions)),
            rich: self.rich,
            jumbo: ModelRc::new(VecModel::from(self.jumbo)),
        }
    }
}
