//! Lógica do app: liga a interface (Slint) ao Supabase e ao Realtime.
//! Tudo que é rede roda em tarefas tokio; a interface só é tocada via `self.ui(...)`.

use anyhow::Result;
use chrono::{DateTime, Local};
use serde::Deserialize;
use serde_json::{json, Value};
use slint::{Color, ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::images::{self, Kind, Loader};
use crate::realtime::Realtime;
use crate::supabase::Supabase;
use crate::voice::Call;
use livekit::prelude::RoomEvent;
use crate::{AppWindow, ChannelItem, MessageItem, ServerItem, VoiceMember};

mod account;
mod calls;
mod camera;
mod chat;
mod devices;
mod keys;
mod golive;
mod manage;
mod members;
mod profile;
mod record;
mod social;
mod unread;
mod update;
mod usermenu;
mod voicemod;
mod reactions;
mod serveredit;
mod diag;
mod prefs;
pub(crate) mod video;

/// Cor padrão de quem ainda não escolheu uma (o "blurple").
const DEFAULT_COLOR: Color = Color::from_rgb_u8(0x58, 0x65, 0xf2);

#[derive(Clone, Deserialize)]
pub struct Profile {
    pub id: String,
    pub username: String,
    pub display_name: Option<String>,
    #[serde(default)]
    pub theme: Value,
    #[serde(default)]
    pub avatar_url: Option<String>,
    #[serde(default)]
    pub banner_url: Option<String>,
    #[serde(default)]
    pub bio: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    /// Desativou a conta (volta ao entrar de novo).
    #[serde(default)]
    pub deactivated: bool,
}

impl Profile {
    fn name(&self) -> String {
        self.display_name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| self.username.clone())
    }
    fn color(&self) -> Color {
        parse_color(self.theme["primary"].as_str().unwrap_or("#5865f2"))
    }
}

#[derive(Clone, Deserialize)]
struct Server {
    id: String,
    name: String,
    invite_code: String,
    #[serde(default)]
    icon_url: Option<String>,
}

#[derive(Clone, Deserialize)]
struct Channel {
    id: String,
    name: String,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Clone, Deserialize)]
struct Message {
    id: String,
    author_id: String,
    content: String,
    created_at: String,
    #[serde(default)]
    attachments: Vec<chat::Attachment>,
    #[serde(default)]
    edited_at: Option<String>,
    /// Resposta a outra mensagem (da mesma conversa).
    #[serde(default)]
    reply_to: Option<String>,
}

/// Onde a lista de mensagens fica depois de redesenhar.
#[derive(Clone, Copy, PartialEq)]
enum Scroll {
    /// Não mexe.
    Stay,
    /// Vai para a mensagem mais nova.
    Bottom,
    /// Entraram mensagens antigas em cima: mantém na tela o que o usuário estava lendo.
    KeepFromBottom,
}

#[derive(Default)]
struct State {
    servers: Vec<Server>,
    current_server: Option<String>,
    current_channel: Option<String>,
    profiles: HashMap<String, Profile>,
    messages: Vec<Message>,
    chat_topic: Option<String>,
    /// Coluna da conversa aberta: "channel_id" (canal de servidor) ou "dm_id" (mensagem direta).
    chat_column: &'static str,
    voice_channels: Vec<Channel>,
    /// Quantos canais o servidor aberto tem (posição do próximo canal criado).
    channel_count: usize,
    /// Meu papel no servidor aberto: "owner", "admin" ou "member".
    my_role: String,
    /// Assinatura ao vivo dos canais do servidor aberto.
    server_topic: Option<String>,
    /// Anexos, "digitando...", mensagens antigas.
    chat: chat::ChatExtra,
    /// Ligação de DM chegando.
    ringing: calls::Ringing,
    /// Mensagens não lidas (bolinhas) e avisos.
    unread: unread::Unread,
    /// Canais de texto do servidor aberto.
    text_channels: Vec<Channel>,
    /// Membros do servidor aberto (e a assinatura ao vivo deles).
    members: Vec<members::Member>,
    members_topic: Option<String>,
    // amigos / DMs / presença
    friendships: Vec<social::Friendship>,
    dms: Vec<social::Dm>,
    online: HashSet<String>,
    /// canal de voz → quem está nele (pela presença, sem precisar entrar na call)
    voice_presence: HashMap<String, Vec<String>>,
    /// pessoa → jogo que está jogando agora (pela presença)
    activities: HashMap<String, String>,
}

// A lista de mensagens vive na thread da interface (os modelos do Slint não são Send).
thread_local! {
    static MESSAGES: RefCell<Option<Rc<VecModel<MessageItem>>>> = const { RefCell::new(None) };
}

pub fn install_models(ui: &AppWindow) {
    let model = Rc::new(VecModel::<MessageItem>::default());
    ui.set_messages(ModelRc::from(model.clone()));
    MESSAGES.with(|m| *m.borrow_mut() = Some(model));
    images::install(ui);
    profile::install(ui);
    camera::install(ui);
}

/// Foto de perfil (URL) para a interface; "" = mostra as iniciais.
fn avatar_of(p: Option<&Profile>) -> SharedString {
    p.and_then(|p| p.avatar_url.clone()).unwrap_or_default().into()
}

fn parse_color(hex: &str) -> Color {
    let h = hex.trim_start_matches('#');
    let v = u32::from_str_radix(h, 16).unwrap_or(0x5865f2);
    Color::from_rgb_u8((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

fn initials(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    if words.len() > 1 {
        words.iter().take(3).filter_map(|w| w.chars().next()).collect::<String>().to_uppercase()
    } else {
        name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default()
    }
}

fn format_time(iso: &str) -> String {
    let Ok(t) = DateTime::parse_from_rfc3339(iso) else { return String::new() };
    let t = t.with_timezone(&Local);
    let today = Local::now().date_naive();
    // Relógio de 24 h ("15:05") ou de 12 h ("3:05 PM"), escolhido em Idioma e horário.
    let time = if crate::settings::get().clock_24h {
        t.format("%H:%M").to_string()
    } else {
        t.format("%-I:%M %p").to_string()
    };
    if t.date_naive() == today {
        format!("Hoje às {time}")
    } else if Some(t.date_naive()) == today.pred_opt() {
        format!("Ontem às {time}")
    } else {
        format!("{} {time}", t.format("%d/%m/%Y"))
    }
}

pub struct App {
    ui: slint::Weak<AppWindow>,
    sb: Supabase,
    rt: Mutex<Option<Realtime>>,
    state: Mutex<State>,
    // chamada de voz
    call: Mutex<Option<Arc<Call>>>,
    /// Muda a cada entrar/sair: descarta conexões que terminaram depois de o usuário desistir.
    call_gen: AtomicU64,
    muted: AtomicBool,
    deafened: AtomicBool,
    // silenciado / sem áudio por um moderador do servidor da chamada
    srv_muted: AtomicBool,
    srv_deafened: AtomicBool,
    voice_mod: Mutex<voicemod::VoiceMod>,
    // ícone escolhido na janela "Configurações do servidor" (Some("") = remover)
    server_icon_draft: Mutex<Option<String>>,
    // "Testar microfone" nas configurações
    mic_test: Mutex<Option<polley_media::mictest::MicTest>>,
    // perfil
    images: Arc<Loader>,
    draft: Mutex<Option<profile::Draft>>,
    // escolher o que transmitir
    golive_gen: AtomicU64,
    // visualizador de imagem/vídeo do chat
    viewer_gen: AtomicU64,
    // janela "Voz e vídeo"
    device_lists: Mutex<devices::DeviceLists>,
    // atualização baixada, esperando "Reiniciar"
    update: Mutex<Option<update::Ready>>,
    // "Esqueci minha senha": email cujo código já foi aceito (falta só a senha nova)
    recovery: Mutex<Option<String>>,
    // apertar para falar e atalhos globais
    ptt_down: AtomicBool,
    capturing: std::sync::atomic::AtomicU8,
    capture_armed: AtomicBool,
    keys_started: AtomicBool,
    // verificação em 2 etapas: fator sendo ativado ou pedido no login
    mfa_factor: Mutex<Option<String>>,
    // presença: canal de voz e jogo aberto (vão juntos para os amigos)
    presence_voice: Mutex<Option<String>>,
    game: Mutex<Option<String>>,
    games_started: AtomicBool,
    // "Adicionar jogo": programas com janela aberta (exe, título)
    running_apps: Mutex<Vec<(String, String)>>,
}

impl App {
    pub fn new(ui: slint::Weak<AppWindow>) -> Arc<Self> {
        Arc::new(Self {
            ui: ui.clone(),
            sb: Supabase::new(),
            rt: Mutex::new(None),
            state: Mutex::default(),
            call: Mutex::new(None),
            call_gen: AtomicU64::new(0),
            golive_gen: AtomicU64::new(0),
            viewer_gen: AtomicU64::new(0),
            device_lists: Mutex::default(),
            update: Mutex::new(None),
            recovery: Mutex::new(None),
            ptt_down: AtomicBool::new(false),
            capturing: std::sync::atomic::AtomicU8::new(keys::CAPTURE_NONE),
            capture_armed: AtomicBool::new(false),
            keys_started: AtomicBool::new(false),
            mfa_factor: Mutex::new(None),
            presence_voice: Mutex::new(None),
            game: Mutex::new(None),
            games_started: AtomicBool::new(false),
            running_apps: Mutex::default(),
            muted: AtomicBool::new(false),
            deafened: AtomicBool::new(false),
            srv_muted: AtomicBool::new(false),
            srv_deafened: AtomicBool::new(false),
            voice_mod: Mutex::default(),
            server_icon_draft: Mutex::new(None),
            mic_test: Mutex::new(None),
            images: Loader::new(ui.clone()),
            draft: Mutex::new(None),
        })
    }

    /// Executa `f` na thread da interface.
    fn ui(&self, f: impl FnOnce(AppWindow) + Send + 'static) {
        let _ = self.ui.upgrade_in_event_loop(f);
    }

    // ------------------------------------------------------------ sessão

    pub async fn restore(self: Arc<Self>) {
        match self.sb.restore().await {
            Ok(true) => self.after_login().await,
            _ => self.ui(|ui| ui.set_ready(true)),
        }
    }

    pub async fn login(self: Arc<Self>, email: String, password: String) {
        self.ui(|ui| {
            ui.set_auth_busy(true);
            ui.set_auth_error("".into());
        });
        match self.sb.sign_in(&email, &password).await {
            Ok(_) => self.after_login().await,
            // Cadastrou mas não confirmou: manda um código novo e mostra a tela do código.
            Err(e) if e.to_string() == crate::supabase::NOT_CONFIRMED => match self.sb.resend_signup(&email).await {
                Ok(()) => self.ask_signup_code(crate::supabase::NOT_CONFIRMED),
                Err(_) => self.ask_signup_code("Confirme seu email com o código que enviamos."),
            },
            Err(e) => self.auth_failed(e),
        }
    }

    pub async fn register(self: Arc<Self>, email: String, password: String, username: String) {
        self.ui(|ui| {
            ui.set_auth_busy(true);
            ui.set_auth_error("".into());
            ui.set_auth_info("".into());
        });
        match self.sb.sign_up(&email, &password, &username).await {
            Ok(Some(_)) => self.after_login().await,
            Ok(None) => self.ask_signup_code("Conta criada! Enviamos um código para o seu email (confira também o spam)."),
            Err(e) => self.auth_failed(e),
        }
    }

    fn auth_failed(&self, e: anyhow::Error) {
        let msg = e.to_string();
        self.ui(move |ui| {
            ui.set_auth_busy(false);
            ui.set_auth_error(msg.into());
        });
    }

    async fn after_login(self: &Arc<Self>) {
        // Conta com verificação em 2 etapas e só a senha até agora: pede o código do app.
        if self.needs_mfa().await {
            self.ui(|ui| {
                ui.set_auth_busy(false);
                ui.set_auth_error("".into());
                ui.set_auth_code("".into());
                ui.set_auth_mfa_mode(true);
                ui.set_ready(true);
            });
            return;
        }
        let uid = self.sb.user_id().unwrap_or_default();
        let me = self
            .sb
            .select::<Vec<Profile>>(&format!("profiles?id=eq.{uid}&select=*"))
            .await
            .ok()
            .and_then(|mut v| v.pop());
        let Some(me) = me else {
            self.sb.clear();
            return self.auth_failed(anyhow::anyhow!("Não achei seu perfil. Tente entrar de novo."));
        };
        self.reactivate_if_needed(&me).await;
        self.images.want(me.avatar_url.as_deref(), Kind::Avatar);
        self.state.lock().unwrap().profiles.insert(me.id.clone(), me.clone());
        self.show_me(&me);
        self.ui(move |ui| {
            ui.set_auth_busy(false);
            ui.set_logged_in(true);
            ui.set_ready(true);
        });

        // Tempo real + renovação do token a cada 45 min (ele expira em 60).
        let rt = Realtime::connect(self.sb.access_token().unwrap_or_default());
        let mut membership = rt.subscribe("me:servers", "server_members", &format!("user_id=eq.{uid}"));
        *self.rt.lock().unwrap() = Some(rt.clone());
        self.start_social(&rt, &uid);
        {
            let app = self.clone();
            tokio::spawn(async move { app.start_unread().await });
        }
        self.start_profiles(&rt);
        self.start_keys();
        self.start_games();
        {
            let app = self.clone();
            tokio::spawn(async move {
                while membership.recv().await.is_some() {
                    let _ = app.clone().load_servers().await;
                }
            });
        }
        {
            let app = self.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(45 * 60)).await;
                    let Ok(token) = app.sb.refresh().await else { continue };
                    let rt = app.rt.lock().unwrap().clone();
                    match rt {
                        Some(rt) => rt.set_token(token),
                        None => break, // saiu da conta
                    }
                }
            });
        }

        let _ = self.clone().load_servers().await;
    }

    pub fn logout(self: &Arc<Self>) {
        self.leave_voice();
        *self.game.lock().unwrap() = None;
        *self.presence_voice.lock().unwrap() = None;
        self.ui(|ui| {
            ui.set_auth_mfa_mode(false);
            crate::tray::set_badge(false);
        });
        self.sb.clear();
        *self.rt.lock().unwrap() = None;
        *self.state.lock().unwrap() = State::default();
        *self.draft.lock().unwrap() = None;
        self.ui(|ui| {
            ui.set_logged_in(false);
            // A senha nunca fica nos campos depois de sair (o email fica, para facilitar).
            ui.set_auth_password("".into());
            ui.set_legal_accepted(false);
            ui.set_profile_open(false);
            ui.set_card_open(false);
            ui.set_me_avatar("".into());
            ui.set_servers(ModelRc::default());
            ui.set_current_server("".into());
            ui.set_members(ModelRc::default());
            ui.set_text_channels(ModelRc::default());
            ui.set_voice_channels(ModelRc::default());
            ui.set_current_channel("".into());
            ui.set_channel_name("".into());
            ui.set_friends(ModelRc::default());
            ui.set_dms(ModelRc::default());
            ui.set_pending_count(0);
            ui.set_chat_is_dm(false);
            MESSAGES.with(|m| m.borrow().as_ref().map(|m| m.set_vec(vec![])));
        });
    }

    fn realtime(&self) -> Option<Realtime> {
        self.rt.lock().unwrap().clone()
    }

    // ------------------------------------------------------------ servidores

    async fn load_servers(self: Arc<Self>) -> Result<()> {
        let servers: Vec<Server> = self.sb.select("servers?select=id,name,invite_code,icon_url&order=created_at").await?;
        let current = self.state.lock().unwrap().current_server.clone();
        let still_member = current.as_ref().is_none_or(|c| servers.iter().any(|s| &s.id == c));
        self.state.lock().unwrap().servers = servers;
        self.render_servers();
        if !still_member {
            self.go_home();
        }
        Ok(())
    }

    /// Barra de servidores (com a bolinha de não lidas de cada um).
    fn render_servers(&self) {
        let items: Vec<ServerItem> = {
            let st = self.state.lock().unwrap();
            st.servers
                .iter()
                .map(|s| {
                    let (unread, mentions) = st.unread.server(&s.id);
                    self.images.want(s.icon_url.as_deref(), Kind::Avatar);
                    ServerItem {
                        icon: s.icon_url.clone().unwrap_or_default().into(),
                        id: s.id.clone().into(),
                        name: s.name.clone().into(),
                        initials: initials(&s.name).into(),
                        unread,
                        mentions: mentions as i32,
                    }
                })
                .collect()
        };
        self.ui(move |ui| ui.set_servers(ModelRc::new(VecModel::from(items))));
    }

    pub fn go_home(self: &Arc<Self>) {
        self.leave_chat();
        self.leave_server_topic();
        {
            let mut st = self.state.lock().unwrap();
            st.members.clear();
            st.current_server = None;
            st.current_channel = None;
            st.voice_channels.clear();
            st.messages.clear();
        }
        self.ui(|ui| {
            ui.set_current_server("".into());
            ui.set_members(ModelRc::default());
            ui.set_text_channels(ModelRc::default());
            ui.set_voice_channels(ModelRc::default());
            ui.set_current_channel("".into());
            ui.set_channel_name("".into());
            ui.set_channel_is_voice(false);
            ui.set_chat_is_dm(false);
            MESSAGES.with(|m| m.borrow().as_ref().map(|m| m.set_vec(vec![])));
        });
    }

    pub async fn select_server(self: Arc<Self>, id: String) {
        self.leave_chat();
        self.leave_server_topic();
        let server = {
            let mut st = self.state.lock().unwrap();
            st.current_server = Some(id.clone());
            st.current_channel = None;
            st.messages.clear();
            st.servers.iter().find(|s| s.id == id).cloned()
        };
        let (name, invite) = server.map(|s| (s.name, s.invite_code)).unwrap_or_default();
        let sid = id.clone();
        self.ui(move |ui| {
            ui.set_current_server(sid.into());
            ui.set_server_name(name.into());
            ui.set_invite_code(invite.into());
            ui.set_current_channel("".into());
            ui.set_channel_name("".into());
            MESSAGES.with(|m| m.borrow().as_ref().map(|m| m.set_vec(vec![])));
        });

        let channels = self.load_channels(&id).await;
        let first = channels.iter().find(|c| c.kind == "text").cloned();
        self.watch_channels(&id);
        self.load_members(&id).await;
        self.watch_members(&id);
        if let Some(first) = first {
            self.select_channel(first.id, false).await;
        }
    }

    pub async fn create_server(self: Arc<Self>, name: String) {
        let name = name.trim().to_string();
        if name.is_empty() {
            return self.add_error("Digite um nome para o servidor.");
        }
        match self.sb.rpc::<String>("create_server", json!({ "p_name": name })).await {
            Ok(id) => self.after_add(id).await,
            Err(e) => self.add_error(&e.to_string()),
        }
    }

    pub async fn join_server(self: Arc<Self>, code: String) {
        let code = code.trim().rsplit('/').next().unwrap_or_default().to_lowercase();
        if code.is_empty() {
            return self.add_error("Cole o código de convite.");
        }
        match self.sb.rpc::<String>("join_server", json!({ "p_code": code })).await {
            Ok(id) => self.after_add(id).await,
            Err(e) if e.to_string().contains("convite") => self.add_error("Convite inválido."),
            Err(e) => self.add_error(&e.to_string()),
        }
    }

    async fn after_add(self: Arc<Self>, id: String) {
        self.ui(|ui| {
            ui.set_add_open(false);
            ui.set_add_error("".into());
        });
        let _ = self.clone().load_servers().await;
        self.select_server(id).await;
    }

    fn add_error(&self, msg: &str) {
        let msg = msg.to_string();
        self.ui(move |ui| ui.set_add_error(msg.into()));
    }

    // ------------------------------------------------------------ chat

    fn leave_chat(&self) {
        let (topic, react) = {
            let mut st = self.state.lock().unwrap();
            (st.chat_topic.take(), st.chat.react_topic.take())
        };
        if let Some(rt) = self.realtime() {
            for t in [topic, react].into_iter().flatten() {
                rt.unsubscribe(&t);
            }
        }
    }

    pub async fn select_channel(self: Arc<Self>, id: String, voice: bool) {
        self.leave_chat();
        {
            let mut st = self.state.lock().unwrap();
            st.current_channel = Some(id.clone());
            st.messages.clear();
        }
        let channel_name = self.channel_name(&id).await;
        let cid = id.clone();
        self.ui(move |ui| {
            ui.set_current_channel(cid.into());
            ui.set_channel_name(channel_name.into());
            ui.set_channel_is_voice(voice);
            ui.set_chat_is_dm(false);
            ui.set_chat_error("".into());
            MESSAGES.with(|m| m.borrow().as_ref().map(|m| m.set_vec(vec![])));
        });
        if voice {
            return self.join_voice(id).await;
        }
        self.load_chat(id, "channel_id").await;
    }

    /// Carrega as últimas 50 mensagens de um canal ou DM e assina as novas ao vivo.
    async fn load_chat(self: Arc<Self>, id: String, column: &'static str) {
        self.mark_read(&id);
        {
            let mut st = self.state.lock().unwrap();
            st.chat_column = column;
            st.chat = chat::ChatExtra::default();
        }
        self.render_pending();
        self.ui(|ui| {
            ui.set_typing_text("".into());
            ui.set_chat_has_more(false);
            ui.set_editing_id("".into());
        });
        let mut list: Vec<Message> = self
            .sb
            .select(&format!("messages?{column}=eq.{id}&select={}&order=created_at.desc&limit={}", chat::COLUMNS, chat::PAGE))
            .await
            .unwrap_or_default();
        let more = list.len() == chat::PAGE;
        list.reverse();
        let authors: Vec<String> = list.iter().map(|m| m.author_id.clone()).collect();
        self.ensure_profiles(&authors).await;
        self.load_reactions(list.iter().map(|m| m.id.clone()).collect()).await;
        {
            let mut st = self.state.lock().unwrap();
            if st.current_channel.as_deref() != Some(&id) {
                return; // usuário já trocou de canal
            }
            st.messages = list;
            st.chat.has_more = more;
        }
        self.ui(move |ui| ui.set_chat_has_more(more));
        self.render_messages(true);
        self.start_typing_clock(id.clone());

        // Mensagens novas / editadas / apagadas ao vivo (e o "digitando...").
        // Mesmo tópico do app Electron, para os dois se verem digitando.
        let Some(rt) = self.realtime() else { return };
        let topic = format!("chat:{column}:{id}");
        let mut rx = rt.subscribe(&topic, "messages", &format!("{column}=eq.{id}"));
        self.state.lock().unwrap().chat_topic = Some(topic);
        // Reações ao vivo (tópico separado: outra tabela).
        let react_topic = format!("react:{column}:{id}");
        let mut react_rx = rt.subscribe(&react_topic, "message_reactions", &format!("{column}=eq.{id}"));
        self.state.lock().unwrap().chat.react_topic = Some(react_topic);
        {
            let app = self.clone();
            tokio::spawn(async move {
                while let Some(change) = react_rx.recv().await {
                    app.clone().on_reaction_change(change).await;
                }
            });
        }
        let app = self.clone();
        tokio::spawn(async move {
            while let Some(change) = rx.recv().await {
                app.clone().on_message_change(&id, change).await;
            }
        });
    }

    async fn channel_name(&self, id: &str) -> String {
        self.sb
            .select::<Vec<Value>>(&format!("channels?id=eq.{id}&select=name"))
            .await
            .ok()
            .and_then(|v| v.first().and_then(|c| c["name"].as_str().map(str::to_string)))
            .unwrap_or_default()
    }

    async fn on_message_change(self: Arc<Self>, channel: &str, change: Value) {
        let kind = change["type"].as_str().unwrap_or_default();
        if self.state.lock().unwrap().current_channel.as_deref() != Some(channel) {
            return;
        }
        match kind {
            "BROADCAST" if change["event"] == "typing" => {
                let user = change["payload"]["user_id"].as_str().unwrap_or_default().to_string();
                self.someone_typing(user).await;
            }
            "INSERT" => {
                let Ok(msg) = serde_json::from_value::<Message>(change["record"].clone()) else { return };
                self.stopped_typing(&msg.author_id);
                self.ensure_profiles(std::slice::from_ref(&msg.author_id)).await;
                if self.push_message(msg) {
                    self.render_messages(true);
                }
            }
            "UPDATE" => {
                let Ok(msg) = serde_json::from_value::<Message>(change["record"].clone()) else { return };
                let mut st = self.state.lock().unwrap();
                if let Some(m) = st.messages.iter_mut().find(|m| m.id == msg.id) {
                    *m = msg;
                }
                drop(st);
                self.render_messages(false);
            }
            "DELETE" => {
                let id = change["old_record"]["id"].as_str().unwrap_or_default().to_string();
                self.state.lock().unwrap().messages.retain(|m| m.id != id);
                self.render_messages(false);
            }
            _ => {}
        }
    }

    /// Adiciona sem duplicar (a mensagem que EU mando chega pela resposta e pelo Realtime).
    fn push_message(&self, msg: Message) -> bool {
        let mut st = self.state.lock().unwrap();
        if st.messages.iter().any(|m| m.id == msg.id) {
            return false;
        }
        st.messages.push(msg);
        true
    }

    pub async fn send_message(self: Arc<Self>, text: String, reply: String) {
        let (Some(target), column) = ({
            let st = self.state.lock().unwrap();
            (st.current_channel.clone(), st.chat_column)
        }) else {
            return;
        };
        let text = text.trim().to_string();
        let files = {
            let mut st = self.state.lock().unwrap();
            if st.chat.sending {
                return;
            }
            let files = std::mem::take(&mut st.chat.pending);
            if (text.is_empty() && files.is_empty()) || column.is_empty() {
                st.chat.pending = files;
                return;
            }
            st.chat.sending = !files.is_empty();
            st.chat.typing_sent = None;
            files
        };
        self.render_pending();
        self.ui(|ui| ui.set_chat_error("".into()));

        let attachments = if files.is_empty() {
            Ok(vec![])
        } else {
            self.upload_pending(files.clone()).await
        };
        let result = match attachments {
            Ok(attachments) => {
                let mut row = json!({ "content": text, "attachments": attachments });
                row[column] = target.into();
                if !reply.is_empty() {
                    row["reply_to"] = reply.into();
                }
                self.sb.insert::<Message>("messages", row).await
            }
            Err(e) => Err(e),
        };
        let failed = result.is_err();
        match result {
            Ok(msg) => {
                if self.push_message(msg) {
                    self.render_messages(true);
                }
            }
            Err(e) => {
                let e = e.to_string();
                // Erros do banco em português (o anti-spam já vem em português do próprio banco).
                let msg = if e.contains("row-level security") {
                    "Você não pode mandar mensagens aqui (a pessoa te bloqueou, você bloqueou ela, ou você saiu do servidor).".to_string()
                } else if e.starts_with("Calma") {
                    e
                } else {
                    format!("Não foi possível enviar: {e}")
                };
                self.ui(move |ui| ui.set_chat_error(msg.into()));
            }
        }
        {
            let mut st = self.state.lock().unwrap();
            st.chat.sending = false;
            if failed && st.chat.pending.is_empty() {
                st.chat.pending = files; // deixa os arquivos para tentar de novo
            }
        }
        self.render_pending();
    }

    async fn ensure_profiles(&self, ids: &[String]) {
        let missing: Vec<String> = {
            let st = self.state.lock().unwrap();
            ids.iter().filter(|id| !st.profiles.contains_key(*id)).cloned().collect::<HashSet<_>>().into_iter().collect()
        };
        if missing.is_empty() {
            return;
        }
        let query = format!("profiles?id=in.({})&select=*", missing.join(","));
        if let Ok(list) = self.sb.select::<Vec<Profile>>(&query).await {
            let mut st = self.state.lock().unwrap();
            for p in list {
                self.images.want(p.avatar_url.as_deref(), Kind::Avatar);
                st.profiles.insert(p.id.clone(), p);
            }
        }
    }

    /// Converte as mensagens em linhas da interface (agrupando mensagens seguidas do mesmo autor).
    fn render_messages(&self, scroll: bool) {
        self.render(if scroll { Scroll::Bottom } else { Scroll::Stay });
    }

    fn render(&self, scroll: Scroll) {
        let me = self.sb.user_id().unwrap_or_default();
        let rows: Vec<chat::MsgRow> = {
            let st = self.state.lock().unwrap();
            let by_id: HashMap<&str, &Message> = st.messages.iter().map(|m| (m.id.as_str(), m)).collect();
            // Dono/admin apagam mensagens de qualquer um no servidor (não em DMs).
            let moderator = st.chat_column == "channel_id" && matches!(st.my_role.as_str(), "owner" | "admin");
            let mut prev: Option<(&str, DateTime<chrono::FixedOffset>)> = None;
            st.messages
                .iter()
                .map(|m| {
                    let when = DateTime::parse_from_rfc3339(&m.created_at).ok();
                    let grouped = match (prev, when) {
                        (Some((author, t0)), Some(t)) => author == m.author_id && (t - t0).num_minutes() < 5,
                        _ => false,
                    };
                    if let Some(t) = when {
                        prev = Some((&m.author_id, t));
                    }
                    let p = st.profiles.get(&m.author_id);
                    let name = p.map(|p| p.name()).unwrap_or_else(|| "…".into());
                    for a in m.attachments.iter().filter(|a| a.is_image()) {
                        self.images.want(Some(&a.url), Kind::Attachment);
                    }
                    let mine = m.author_id == me;
                    // Resposta: "↱ Fulano  começo da mensagem original".
                    let (reply_author, reply_text, reply_color) = match m.reply_to.as_deref().map(|r| by_id.get(r)) {
                        Some(Some(orig)) => {
                            let op = st.profiles.get(&orig.author_id);
                            let text: String = orig.content.replace('\n', " ").chars().take(120).collect();
                            let text = if text.is_empty() && !orig.attachments.is_empty() { "📎 Anexo".to_string() } else { text };
                            (op.map(|p| p.name()).unwrap_or_default(), text, op.map(|p| p.color()).unwrap_or(DEFAULT_COLOR))
                        }
                        Some(None) => (String::new(), "Mensagem mais antiga".to_string(), DEFAULT_COLOR),
                        None => (String::new(), String::new(), DEFAULT_COLOR),
                    };
                    let jumbo = crate::emoji::only_emojis(&m.content).unwrap_or_default();
                    chat::MsgRow {
                        reply_id: m.reply_to.clone().unwrap_or_default().into(),
                        reply_author: reply_author.into(),
                        reply_text: reply_text.into(),
                        reply_color,
                        reactions: self.reaction_items(st.chat.reactions.get(&m.id), &me),
                        rich: reactions::rich_text(&m.content, m.edited_at.is_some()),
                        jumbo: jumbo.into_iter().map(SharedString::from).collect(),
                        id: m.id.clone().into(),
                        mine,
                        can_delete: mine || moderator,
                        edited: m.edited_at.is_some(),
                        attachments: chat::attachment_items(&m.attachments),
                        initials: initials(&name).into(),
                        author: SharedString::from(name),
                        author_id: m.author_id.clone().into(),
                        avatar: avatar_of(p),
                        color: p.map(|p| p.color()).unwrap_or(DEFAULT_COLOR),
                        time: format_time(&m.created_at).into(),
                        content: m.content.clone().into(),
                        grouped,
                    }
                })
                .collect()
        };
        self.ui(move |ui| {
            let items: Vec<MessageItem> = rows.into_iter().map(chat::MsgRow::into_item).collect();
            MESSAGES.with(|m| {
                if let Some(model) = m.borrow().as_ref() {
                    // Só troca o que mudou: mantém a rolagem estável.
                    if model.row_count() <= items.len()
                        && (0..model.row_count()).all(|i| model.row_data(i).is_some_and(|r| r.id == items[i].id))
                    {
                        for (i, item) in items.iter().enumerate().take(model.row_count()) {
                            model.set_row_data(i, item.clone());
                        }
                        for item in items.iter().skip(model.row_count()) {
                            model.push(item.clone());
                        }
                    } else {
                        model.set_vec(items);
                    }
                }
            });
            if scroll != Scroll::Stay {
                // Espera o layout recalcular a altura antes de rolar.
                let weak = ui.as_weak();
                slint::Timer::single_shot(Duration::from_millis(40), move || {
                    if let Some(ui) = weak.upgrade() {
                        if scroll == Scroll::Bottom {
                            ui.invoke_scroll_to_bottom();
                        } else {
                            ui.invoke_chat_restore();
                        }
                    }
                });
            }
        });
    }

    // ------------------------------------------------------------ voz

    fn current_call(&self) -> Option<Arc<Call>> {
        self.call.lock().unwrap().clone()
    }

    /// Mede o ping da chamada a cada 2 s (o sinal verde/amarelo/vermelho do painel "Voz conectada").
    pub async fn watch_ping(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let ping = match self.current_call() {
                Some(call) => call.ping_ms().await,
                None => None,
            };
            self.ui(move |ui| match ping {
                Some(ms) => {
                    ui.set_voice_ping(format!("{ms} ms").into());
                    ui.set_voice_ping_level(if ms < 100 { 0 } else if ms < 250 { 1 } else { 2 });
                }
                None => ui.set_voice_ping("".into()),
            });
        }
    }

    /// Entra no canal de voz (sai do anterior, se estiver em outro).
    pub async fn join_voice(self: Arc<Self>, channel_id: String) {
        self.join_call(channel_id, false).await;
    }

    /// Entra na chamada de um canal de voz ou de uma DM (`dm = true`, aí `channel_id` é o id da DM).
    async fn join_call(self: Arc<Self>, channel_id: String, dm: bool) {
        if self.current_call().is_some_and(|c| c.channel_id == channel_id) {
            if dm {
                self.ui(|ui| ui.set_show_call(true));
            }
            return;
        }
        let gen = self.call_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let old = self.call.lock().unwrap().take();
        if let Some(old) = old {
            old.leave().await;
        }
        self.stop_voice_mod();
        let call_server = if dm { None } else { self.state.lock().unwrap().current_server.clone() };

        let (server_name, channel_name) = if dm {
            let st = self.state.lock().unwrap();
            let other = st.dms.iter().find(|d| d.id == channel_id).map(|d| d.other.clone());
            let name = other.and_then(|o| st.profiles.get(&o).map(|p| p.name())).unwrap_or_default();
            ("Mensagem direta".to_string(), name)
        } else {
            let server = {
                let st = self.state.lock().unwrap();
                st.servers.iter().find(|s| Some(&s.id) == st.current_server.as_ref()).map(|s| s.name.clone())
            }
            .unwrap_or_default();
            (server, self.channel_name(&channel_id).await)
        };
        self.ui(move |ui| {
            ui.set_show_call(true);
            ui.set_voice_channel("".into());
            ui.set_voice_connecting(true);
            ui.set_voice_error("".into());
            ui.set_voice_members(ModelRc::default());
            ui.set_voice_channel_name(channel_name.into());
            ui.set_voice_server_name(server_name.into());
        });

        // Mutado, ensurdecido ou "apertar para falar" sem a tecla apertada: entra com o mic fechado.
        let start_muted = !self.mic_should_be_open();
        let deafened = self.is_deafened();
        let result = async {
            let body = if dm { json!({ "dmId": channel_id }) } else { json!({ "channelId": channel_id }) };
            let resp = self.sb.function("livekit-token", body).await?;
            let token = resp["token"].as_str().ok_or_else(|| anyhow::anyhow!("o servidor não mandou o token"))?;
            Call::connect(token, channel_id.clone(), start_muted, deafened).await
        }
        .await;

        let (call, mut events) = match result {
            Ok(v) => v,
            Err(e) => {
                if self.call_gen.load(Ordering::SeqCst) == gen {
                    let msg = e.to_string();
                    self.ui(move |ui| {
                        ui.set_voice_connecting(false);
                        ui.set_voice_error(msg.into());
                    });
                }
                return;
            }
        };
        if self.call_gen.load(Ordering::SeqCst) != gen {
            call.leave().await; // o usuário saiu/trocou enquanto conectava
            return;
        }
        let call = Arc::new(call);
        call.set_camera_sink(camera::sink(self.ui.clone(), call.cam_view.clone()));
        *self.call.lock().unwrap() = Some(call.clone());
        let cid = channel_id.clone();
        // Chamada de DM não aparece na presença (que todo mundo vê): o amigo é chamado pelo
        // canal privado de toques.
        if dm {
            self.ring_friend(channel_id.clone());
        } else {
            self.set_presence_voice(Some(&channel_id));
        }
        if let Some(server) = call_server {
            self.watch_voice_mod(server, gen);
        }
        self.ui(move |ui| {
            ui.set_voice_connecting(false);
            ui.set_voice_channel(cid.into());
        });
        crate::sounds::cue(crate::sounds::Cue::Join);
        self.refresh_voice().await;

        // Eventos da sala: alguém entrou/saiu, falou, mutou, abriu live...
        let weak = Arc::downgrade(&call);
        drop(call);
        let app = self.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if app.call_gen.load(Ordering::SeqCst) != gen {
                    break;
                }
                match event {
                    RoomEvent::Disconnected { .. } => {
                        app.voice_dropped(gen, "A conexão com a chamada caiu.");
                        break;
                    }
                    RoomEvent::ParticipantConnected(_) => crate::sounds::cue(crate::sounds::Cue::PeerJoin),
                    RoomEvent::ParticipantDisconnected(p) => {
                        crate::sounds::cue(crate::sounds::Cue::PeerLeave);
                        if let Some(call) = weak.upgrade() {
                            call.forget_player(p.identity().as_str());
                        }
                    }
                    RoomEvent::TrackSubscribed { track, publication, participant } => {
                        if let Some(call) = weak.upgrade() {
                            // Quem chega enquanto estou ensurdecido já entra sem som.
                            call.set_deafened(app.is_deafened());
                            call.on_track_subscribed(track, &publication, &participant);
                        }
                    }
                    // Algo novo publicado (microfone de quem chegou, uma live...): decide se baixa.
                    RoomEvent::TrackPublished { .. } => {
                        if let Some(call) = weak.upgrade() {
                            call.sync_subscriptions();
                        }
                    }
                    _ => {}
                }
                app.refresh_voice().await;
            }
        });
    }

    pub fn leave_voice(self: &Arc<Self>) {
        self.call_gen.fetch_add(1, Ordering::SeqCst);
        let call = self.call.lock().unwrap().take();
        if call.is_some() {
            self.set_presence_voice(None);
            crate::sounds::cue(crate::sounds::Cue::Leave);
        }
        self.stop_voice_mod();
        self.apply_audio_state();
        self.reset_screen_ui();
        self.ui(|ui| {
            ui.set_voice_channel("".into());
            ui.set_voice_connecting(false);
            ui.set_voice_members(ModelRc::default());
        });
        if let Some(call) = call {
            let ui = self.ui.clone();
            tokio::spawn(async move { record::show_saved(&ui, call.leave().await) });
        }
    }

    fn voice_dropped(&self, gen: u64, msg: &str) {
        if self.call_gen.load(Ordering::SeqCst) != gen {
            return;
        }
        if let Some(call) = self.call.lock().unwrap().take() {
            // Para a captura/encoder (e salva a gravação, se houver).
            let ui = self.ui.clone();
            tokio::spawn(async move { record::show_saved(&ui, call.leave().await) });
        }
        self.set_presence_voice(None);
        self.stop_voice_mod();
        self.reset_screen_ui();
        let msg = msg.to_string();
        self.ui(move |ui| {
            ui.set_voice_channel("".into());
            ui.set_voice_members(ModelRc::default());
            ui.set_voice_error(msg.into());
        });
    }

    /// Ensurdecido por mim ou por um moderador.
    pub(super) fn is_deafened(&self) -> bool {
        self.deafened.load(Ordering::SeqCst) || self.srv_deafened.load(Ordering::SeqCst)
    }

    /// Silenciado por um moderador: não adianta desmutar.
    fn blocked_by_server(&self) -> bool {
        let blocked = self.srv_muted.load(Ordering::SeqCst) || self.srv_deafened.load(Ordering::SeqCst);
        if blocked {
            self.ui(|ui| ui.set_notice("Um moderador silenciou você neste servidor.".into()));
        }
        blocked
    }

    pub fn toggle_mute(self: &Arc<Self>) {
        use crate::sounds::{cue, Cue};
        if self.blocked_by_server() {
            return;
        }
        if self.deafened.load(Ordering::SeqCst) {
            // Igual ao Discord: desmutar estando ensurdecido liga os dois de volta.
            self.deafened.store(false, Ordering::SeqCst);
            self.muted.store(false, Ordering::SeqCst);
            cue(Cue::Undeafen);
        } else {
            let was = self.muted.fetch_xor(true, Ordering::SeqCst);
            cue(if was { Cue::Unmute } else { Cue::Mute });
        }
        self.apply_audio_state();
    }

    pub fn toggle_deafen(self: &Arc<Self>) {
        use crate::sounds::{cue, Cue};
        if self.srv_deafened.load(Ordering::SeqCst) {
            self.blocked_by_server();
            return;
        }
        let was = self.deafened.fetch_xor(true, Ordering::SeqCst);
        cue(if was { Cue::Undeafen } else { Cue::Deafen });
        self.apply_audio_state();
    }

    fn apply_audio_state(self: &Arc<Self>) {
        let deafened = self.is_deafened();
        // Ensurdecido também não fala.
        let muted = self.muted.load(Ordering::SeqCst) || deafened || self.srv_muted.load(Ordering::SeqCst);
        if let Some(call) = self.current_call() {
            call.set_muted(!self.mic_should_be_open());
            call.set_deafened(deafened);
        }
        self.ui(move |ui| {
            ui.set_voice_muted(muted);
            ui.set_voice_deafened(deafened);
        });
        let app = self.clone();
        tokio::spawn(async move { app.refresh_voice().await });
    }

    /// Atualiza a lista de pessoas na chamada (barra lateral + grade de participantes).
    async fn refresh_voice(&self) {
        let Some(call) = self.current_call() else { return };
        let members = call.members();
        if let Some(watching) = call.watching() {
            if !members.iter().any(|m| m.id == watching && m.live) {
                call.stop_watching();
                self.ui(|ui| {
                    ui.set_watching("".into());
                    ui.set_stream_frame(slint::Image::default());
                    exit_theater(&ui);
                });
            }
        }
        let ids: Vec<String> = members.iter().map(|m| m.id.clone()).collect();
        self.ensure_profiles(&ids).await;
        let me = self.sb.user_id().unwrap_or_default();
        let deafened = self.deafened.load(Ordering::SeqCst);
        let s = crate::settings::get();
        let srv = self.voice_mod.lock().unwrap().flags_snapshot();
        let items: Vec<VoiceMember> = {
            let st = self.state.lock().unwrap();
            members
                .iter()
                .filter(|m| {
                    // "Mostrar participantes sem vídeo" desligado: só quem tem câmera ou live.
                    let cam = if m.is_me { m.camera && s.show_own_camera } else { m.camera && !s.hidden_cams.contains(&m.id) };
                    s.show_no_video || cam || m.live
                })
                .map(|m| {
                    let p = st.profiles.get(&m.id);
                    let name = p.map(|p| p.name()).unwrap_or_else(|| "…".into());
                    VoiceMember {
                        id: m.id.clone().into(),
                        initials: initials(&name).into(),
                        name: name.into(),
                        avatar: avatar_of(p),
                        color: p.map(|p| p.color()).unwrap_or(DEFAULT_COLOR),
                        speaking: m.speaking,
                        muted: m.muted,
                        deafened: m.id == me && deafened,
                        srv_muted: srv.get(&m.id).is_some_and(|f| f.0),
                        srv_deafened: srv.get(&m.id).is_some_and(|f| f.1),
                        live: m.live,
                        is_me: m.is_me,
                        camera: if m.is_me { m.camera && s.show_own_camera } else { m.camera && !s.hidden_cams.contains(&m.id) },
                        cam_hidden: m.is_me && m.camera && !s.show_own_camera,
                    }
                })
                .collect()
        };
        // Em destaque: a pessoa grande e o resto numa fileira embaixo. Se ela saiu da chamada
        // (ou sumiu da tela pelas opções), volta para a grade.
        let mut focus = call.cam_view.focus();
        let big = items.iter().find(|m| m.id.as_str() == focus).cloned();
        if big.is_none() && !focus.is_empty() {
            call.cam_view.set_focus("");
            focus.clear();
        }
        let others: Vec<VoiceMember> = items.iter().filter(|m| m.id.as_str() != focus).cloned().collect();
        self.ui(move |ui| {
            ui.set_voice_focus(focus.into());
            ui.set_voice_focus_member(big.unwrap_or_default());
            ui.set_voice_others(ModelRc::new(VecModel::from(others)));
            ui.set_voice_members(ModelRc::new(VecModel::from(items)));
        });
    }

    // ------------------------------------------------------------ transmitir / assistir

    pub async fn start_screen(self: Arc<Self>, source: String, height: u32, fps: u32, with_audio: bool) {
        let Some(call) = self.current_call() else { return };
        let target = polley_media::capture::Target::parse(&source);
        self.ui(|ui| {
            ui.set_screen_busy(true);
            ui.set_screen_error("".into());
        });
        match call.start_screen(target, height, fps, with_audio).await {
            Ok(info) => {
                let label = format!("{}x{} · {} fps", info.width, info.height, info.fps);
                let first = format!("{label} · {}", info.gpu);
                self.close_golive();
                self.ui(move |ui| {
                    ui.set_screen_busy(false);
                    ui.set_screen_live(true);
                    ui.set_screen_info(first.into());
                });
                self.refresh_voice().await;
                // Mostra fps/Mbps reais a cada 2 s enquanto a live durar.
                let app = self.clone();
                let weak = Arc::downgrade(&call);
                tokio::spawn(async move {
                    let mut last = (0u64, 0u64);
                    loop {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        let Some(call) = weak.upgrade() else { break };
                        // Parou sozinha (ex.: fecharam a janela transmitida): encerra a live e avisa.
                        if let Some(why) = call.screen_failure() {
                            app.clone().stop_screen().await;
                            let text = format!("A live parou: {why}.");
                            app.ui(move |ui| {
                                ui.set_last_recording("".into());
                                ui.set_record_message(text.into());
                            });
                            break;
                        }
                        let Some((frames, bytes)) = call.screen_counters() else { break };
                        let fps = (frames - last.0) as f64 / 2.0;
                        let mbps = (bytes - last.1) as f64 * 8.0 / 2.0 / 1e6;
                        last = (frames, bytes);
                        let text = format!("{label} · {fps:.0} fps reais · {mbps:.1} Mbps");
                        app.ui(move |ui| ui.set_screen_info(text.into()));
                    }
                });
            }
            Err(e) => {
                let msg = e.to_string();
                self.ui(move |ui| {
                    ui.set_screen_busy(false);
                    ui.set_screen_error(msg.into());
                });
            }
        }
    }

    pub async fn stop_screen(self: Arc<Self>) {
        let Some(call) = self.current_call() else { return };
        self.ui(|ui| ui.set_screen_busy(true));
        let recorded = call.stop_screen().await;
        record::show_saved(&self.ui, recorded);
        self.ui(|ui| {
            ui.set_screen_busy(false);
            ui.set_screen_live(false);
            ui.set_screen_info("".into());
        });
        self.refresh_voice().await;
    }

    pub fn watch(self: &Arc<Self>, identity: String) {
        let Some(call) = self.current_call() else { return };
        let name = self.state.lock().unwrap().profiles.get(&identity).map(|p| p.name()).unwrap_or_default();
        let id = identity.clone();
        self.ui(move |ui| {
            ui.set_watching(id.into());
            ui.set_watching_name(name.into());
            ui.set_stream_frame(slint::Image::default());
        });

        // Cada quadro chega de uma thread do tokio; a imagem é montada na thread da interface.
        let weak = self.ui.clone();
        let (viewport, pending, stats) = (call.viewport.clone(), call.frame_pending.clone(), call.watch_stats.clone());
        let sink: crate::voice::FrameSink = Arc::new(move |buffer| {
            let (viewport, pending, stats) = (viewport.clone(), pending.clone(), stats.clone());
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_stream_frame(slint::Image::from_rgba8(buffer));
                // Tamanho real (em pixels) da área da live, para o próximo quadro vir no tamanho certo.
                let scale = ui.window().scale_factor();
                viewport.0.store((ui.get_viewer_width() * scale) as u32, Ordering::Relaxed);
                viewport.1.store((ui.get_viewer_height() * scale) as u32, Ordering::Relaxed);
                stats.shown.fetch_add(1, Ordering::Relaxed);
                pending.store(false, Ordering::SeqCst);
            });
        });
        // Pela placa de vídeo (quase sem gastar processador); sem placa, pela interface.
        call.watch(identity.clone(), sink, video::sink());

        // Medidor: a cada 1 s mostra resolução, fps que chegam da rede e fps que aparecem na tela.
        let app = self.clone();
        let weak = Arc::downgrade(&call);
        tokio::spawn(async move {
            let mut last = (0u64, 0u64);
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Some(call) = weak.upgrade() else { break };
                if call.watching().as_deref() != Some(identity.as_str()) {
                    break;
                }
                let s = &call.watch_stats;
                let (rx, shown) = (s.received.load(Ordering::Relaxed), s.shown.load(Ordering::Relaxed));
                let text = if rx == last.0 {
                    "esperando vídeo...".to_string()
                } else {
                    format!(
                        "{}x{} · {} fps chegando · {} fps na tela",
                        s.width.load(Ordering::Relaxed),
                        s.height.load(Ordering::Relaxed),
                        rx - last.0,
                        shown - last.1
                    )
                };
                last = (rx, shown);
                app.ui(move |ui| ui.set_watch_info(text.into()));
            }
        });
    }

    pub fn stop_watching(self: &Arc<Self>) {
        if let Some(call) = self.current_call() {
            call.stop_watching();
        }
        self.ui(|ui| {
            ui.set_watching("".into());
            ui.set_stream_frame(slint::Image::default());
            exit_theater(&ui);
        });
    }

    /// Limpa a parte de live da interface (ao sair da call ou cair).
    fn reset_screen_ui(&self) {
        self.ui(|ui| {
            ui.set_screen_live(false);
            ui.set_screen_busy(false);
            ui.set_screen_info("".into());
            ui.set_golive_open(false);
            exit_theater(&ui);
            ui.set_watching("".into());
            ui.set_stream_frame(slint::Image::default());
            ui.set_recording(false);
            ui.set_record_time("".into());
            ui.set_camera_on(false);
            ui.set_camera_busy(false);
            camera::clear();
            exit_theater(&ui);
        });
    }
}

/// Sai da tela cheia da live (se estiver nela).
fn exit_theater(ui: &AppWindow) {
    if ui.get_theater() {
        ui.set_theater(false);
        ui.window().set_fullscreen(false);
    }
}
