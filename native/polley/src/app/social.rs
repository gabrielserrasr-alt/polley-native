//! Amigos, mensagens diretas (DMs) e presença (quem está online e em qual canal de voz).
//! Usa as mesmas tabelas e o mesmo canal de presença do app Electron, então os dois se enxergam.

use super::*;
use crate::realtime::PresenceState;
use crate::{DmItem, FriendItem};
use anyhow::{anyhow, bail};

/// Canal de presença compartilhado com o app Electron (hooks/useRealtime.ts).
const PRESENCE: &str = "polley-presence";

#[derive(Clone, Deserialize)]
pub(super) struct Friendship {
    id: String,
    requester_id: String,
    addressee_id: String,
    status: String,
}

impl Friendship {
    fn other(&self, me: &str) -> &str {
        if self.requester_id == me {
            &self.addressee_id
        } else {
            &self.requester_id
        }
    }
}

#[derive(Clone)]
pub(super) struct Dm {
    pub id: String,
    pub other: String,
}

impl App {
    fn me(&self) -> String {
        self.sb.user_id().unwrap_or_default()
    }

    /// Liga amigos, DMs e presença (chamado depois do login).
    pub(super) fn start_social(self: &Arc<Self>, rt: &Realtime, uid: &str) {
        // Pedidos de amizade chegando/aceitos/removidos (a RLS só entrega os meus).
        let mut friends = rt.subscribe("me:friends", "friendships", "");
        let app = self.clone();
        tokio::spawn(async move {
            while friends.recv().await.is_some() {
                app.load_friends().await;
                app.load_dms().await;
            }
        });

        // Conversa nova: chega mensagem de uma DM que ainda não está na lista.
        let mut messages = rt.subscribe("me:messages", "messages", "");
        let app = self.clone();
        tokio::spawn(async move {
            while let Some(change) = messages.recv().await {
                if change["type"] == "INSERT" {
                    app.on_incoming(&change["record"]).await;
                }
                let Some(dm) = change["record"]["dm_id"].as_str() else { continue };
                let known = app.state.lock().unwrap().dms.iter().any(|d| d.id == dm);
                if !known {
                    app.load_dms().await;
                }
            }
        });

        // Ligações pela DM chegando (canal privado, só eu escuto).
        self.listen_rings(rt, uid);

        // Presença: anuncio que estou online (e fora de call) e recebo quem mais está.
        let mut presence = rt.presence(PRESENCE, uid);
        rt.track(PRESENCE, json!({ "user_id": uid, "voice": null }));
        let app = self.clone();
        tokio::spawn(async move {
            while let Some(state) = presence.recv().await {
                app.on_presence(state).await;
            }
        });

        let app = self.clone();
        tokio::spawn(async move {
            app.load_friends().await;
            app.load_dms().await;
        });
    }

    // ------------------------------------------------------------ presença

    async fn on_presence(&self, state: PresenceState) {
        let online: HashSet<String> = state.keys().cloned().collect();
        let mut voice: HashMap<String, Vec<String>> = HashMap::new();
        let mut games: HashMap<String, String> = HashMap::new();
        for (user, metas) in &state {
            if let Some(channel) = metas.iter().find_map(|m| m["voice"].as_str()) {
                voice.entry(channel.to_string()).or_default().push(user.clone());
            }
            // Nome do jogo: curto e sem quebra de linha (vem de outro usuário).
            if let Some(game) = metas.iter().find_map(|m| m["game"].as_str()) {
                let game: String = game.chars().filter(|c| !c.is_control()).take(40).collect();
                if !game.trim().is_empty() {
                    games.insert(user.clone(), game);
                }
            }
        }
        let ids: Vec<String> = online.iter().cloned().collect();
        self.ensure_profiles(&ids).await;
        {
            let mut st = self.state.lock().unwrap();
            st.online = online;
            st.voice_presence = voice;
            st.activities = games;
        }
        self.render_friends();
        self.render_dms();
        self.render_voice_channels();
        self.render_members();
    }

    /// Conta para todo mundo em qual canal de voz estou (ou `None` = fora de call).
    pub(super) fn set_presence_voice(&self, channel: Option<&str>) {
        *self.presence_voice.lock().unwrap() = channel.map(String::from);
        self.track_presence();
    }

    /// Manda a minha presença: canal de voz e jogo aberto (se eu deixei mostrar).
    pub(super) fn track_presence(&self) {
        let Some(rt) = self.realtime() else { return };
        let voice = self.presence_voice.lock().unwrap().clone();
        let game = self.game.lock().unwrap().clone();
        rt.track(PRESENCE, json!({ "user_id": self.me(), "voice": voice, "game": game }));
    }

    /// Canais de voz do servidor aberto, com quem está em cada um (pela presença).
    pub(super) fn render_voice_channels(&self) {
        let list: Vec<(SharedString, SharedString, Vec<VoiceMember>)> = {
            let st = self.state.lock().unwrap();
            st.voice_channels
                .iter()
                .map(|c| {
                    let people = st
                        .voice_presence
                        .get(&c.id)
                        .map(|ids| {
                            ids.iter()
                                .map(|id| {
                                    let p = st.profiles.get(id);
                                    let name = p.map(|p| p.name()).unwrap_or_else(|| "…".into());
                                    VoiceMember {
                                        id: id.clone().into(),
                                        initials: initials(&name).into(),
                                        name: name.into(),
                                        avatar: avatar_of(p),
                                        color: p.map(|p| p.color()).unwrap_or(DEFAULT_COLOR),
                                        speaking: false,
                                        muted: false,
                                        deafened: false,
                                        live: false,
                                        is_me: false,
                                        camera: false,
                                        srv_muted: false,
                                        srv_deafened: false,
                                        cam_hidden: false,
                                    }
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    (c.id.clone().into(), c.name.clone().into(), people)
                })
                .collect()
        };
        self.ui(move |ui| {
            let items: Vec<ChannelItem> = list
                .into_iter()
                .map(|(id, name, people)| ChannelItem { id, name, unread: false, people: ModelRc::new(VecModel::from(people)) })
                .collect();
            ui.set_voice_channels(ModelRc::new(VecModel::from(items)));
        });
    }

    // ------------------------------------------------------------ amigos

    /// Minha relação com alguém: "me", "friend", "incoming", "outgoing", "blocked" ou "none".
    pub(super) fn relation_with(&self, user: &str) -> &'static str {
        let me = self.me();
        if user == me {
            return "me";
        }
        let st = self.state.lock().unwrap();
        match st.friendships.iter().find(|f| f.other(&me) == user) {
            Some(f) if f.status == "accepted" => "friend",
            Some(f) if f.status == "pending" && f.requester_id == me => "outgoing",
            Some(f) if f.status == "pending" => "incoming",
            Some(_) => "blocked",
            None => "none",
        }
    }

    /// Linha da amizade/pedido/bloqueio com alguém (para aceitar ou desfazer).
    pub(super) fn friendship_with(&self, user: &str) -> Option<String> {
        let me = self.me();
        self.state.lock().unwrap().friendships.iter().find(|f| f.other(&me) == user).map(|f| f.id.clone())
    }

    pub(super) async fn load_friends(&self) {
        let Ok(list) = self.sb.select::<Vec<Friendship>>("friendships?select=id,requester_id,addressee_id,status").await
        else {
            return;
        };
        let ids: Vec<String> = list.iter().flat_map(|f| [f.requester_id.clone(), f.addressee_id.clone()]).collect();
        self.ensure_profiles(&ids).await;
        self.state.lock().unwrap().friendships = list;
        self.render_friends();
    }

    pub(super) fn render_friends(&self) {
        let me = self.me();
        let (mut items, pending) = {
            let st = self.state.lock().unwrap();
            let mut pending = 0;
            let items: Vec<FriendItem> = st
                .friendships
                .iter()
                .filter_map(|f| {
                    let kind = match f.status.as_str() {
                        "accepted" => "friend",
                        "pending" if f.addressee_id == me => {
                            pending += 1;
                            "incoming"
                        }
                        "pending" => "outgoing",
                        "blocked" if f.requester_id == me => "blocked",
                        _ => return None, // quem me bloqueou não aparece
                    };
                    let other = f.other(&me);
                    let p = st.profiles.get(other);
                    let name = p.map(|p| p.name()).unwrap_or_else(|| "…".into());
                    Some(FriendItem {
                        id: f.id.clone().into(),
                        user_id: other.into(),
                        initials: initials(&name).into(),
                        name: name.into(),
                        username: p.map(|p| p.username.clone()).unwrap_or_default().into(),
                        avatar: avatar_of(p),
                        color: p.map(|p| p.color()).unwrap_or(DEFAULT_COLOR),
                        online: st.online.contains(other),
                        activity: st.activities.get(other).cloned().unwrap_or_default().into(),
                        kind: kind.into(),
                    })
                })
                .collect();
            (items, pending)
        };
        // Online primeiro, depois por nome.
        items.sort_by(|a, b| b.online.cmp(&a.online).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        let count = |f: &dyn Fn(&FriendItem) -> bool| items.iter().filter(|i| f(i)).count() as i32;
        let online = count(&|i| i.kind == "friend" && i.online);
        let all = count(&|i| i.kind == "friend");
        let pending_total = count(&|i| i.kind == "incoming" || i.kind == "outgoing");
        let blocked = count(&|i| i.kind == "blocked");
        self.ui(move |ui| {
            ui.set_friends(ModelRc::new(VecModel::from(items)));
            ui.set_pending_count(pending);
            ui.set_online_count(online);
            ui.set_friend_count(all);
            ui.set_pending_total(pending_total);
            ui.set_blocked_count(blocked);
        });
    }

    pub async fn add_friend(self: Arc<Self>, username: String) {
        let result = self.try_add_friend(&username).await;
        let ok = result.is_ok();
        let text = result.unwrap_or_else(|e| e.to_string());
        self.ui(move |ui| {
            ui.set_friend_ok(ok);
            ui.set_friend_result(text.into());
        });
        if ok {
            self.load_friends().await;
        }
    }

    pub(super) async fn try_add_friend(&self, username: &str) -> Result<String> {
        let name = username.trim().trim_start_matches('@').to_lowercase();
        if name.is_empty() {
            bail!("Digite o nome de usuário da pessoa.");
        }
        let not_found = || anyhow!("Não achamos ninguém com o nome de usuário \"{name}\".");
        // Nomes de usuário só têm a-z, 0-9, _ e . — qualquer outra coisa nem vai ao servidor.
        if !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.') {
            return Err(not_found());
        }
        let target = self
            .sb
            .select::<Vec<Profile>>(&format!("profiles?username=eq.{name}&select=*"))
            .await?
            .pop()
            .ok_or_else(not_found)?;
        let me = self.me();
        if target.id == me {
            bail!("Você não pode se adicionar :)");
        }
        self.state.lock().unwrap().profiles.insert(target.id.clone(), target.clone());

        let existing = self.state.lock().unwrap().friendships.iter().find(|f| f.other(&me) == target.id).cloned();
        if let Some(f) = existing {
            match f.status.as_str() {
                "accepted" => bail!("Vocês já são amigos."),
                "blocked" => bail!("Não é possível adicionar esse usuário."),
                // A pessoa já tinha me pedido: é só aceitar.
                _ if f.addressee_id == me => {
                    self.sb.update("friendships", &format!("id=eq.{}", f.id), json!({ "status": "accepted" })).await?;
                    return Ok(format!("Agora você e {} são amigos!", target.name()));
                }
                _ => bail!("Você já mandou um pedido para {}.", target.name()),
            }
        }
        self.sb
            .insert::<Value>("friendships", json!({ "requester_id": me, "addressee_id": target.id }))
            .await
            .map_err(|_| anyhow!("Não foi possível mandar o pedido para {}.", target.name()))?;
        Ok(format!("Pedido de amizade enviado para {}!", target.name()))
    }

    pub async fn accept_friend(self: Arc<Self>, friendship: String) {
        let _ = self.sb.update("friendships", &format!("id=eq.{friendship}"), json!({ "status": "accepted" })).await;
        self.load_friends().await;
    }

    /// Recusar pedido, cancelar pedido, desfazer amizade ou desbloquear: tudo é apagar a linha.
    pub async fn remove_friend(self: Arc<Self>, friendship: String) {
        let _ = self.sb.delete("friendships", &format!("id=eq.{friendship}")).await;
        self.load_friends().await;
    }

    pub async fn block_user(self: Arc<Self>, user: String) {
        let me = self.me();
        let existing = self.state.lock().unwrap().friendships.iter().find(|f| f.other(&me) == user).map(|f| f.id.clone());
        if let Some(id) = existing {
            let _ = self.sb.delete("friendships", &format!("id=eq.{id}")).await;
        }
        let _ = self
            .sb
            .insert::<Value>("friendships", json!({ "requester_id": me, "addressee_id": user, "status": "blocked" }))
            .await;
        self.load_friends().await;
    }

    // ------------------------------------------------------------ DMs

    pub(super) async fn load_dms(&self) {
        let me = self.me();
        let Ok(rows) = self.sb.select::<Vec<Value>>("dm_members?select=dm_id,user_id").await else { return };
        let dms: Vec<Dm> = rows
            .iter()
            .filter(|r| r["user_id"].as_str() != Some(&me))
            .filter_map(|r| Some(Dm { id: r["dm_id"].as_str()?.into(), other: r["user_id"].as_str()?.into() }))
            .collect();
        let ids: Vec<String> = dms.iter().map(|d| d.other.clone()).collect();
        self.ensure_profiles(&ids).await;
        self.state.lock().unwrap().dms = dms;
        self.render_dms();
    }

    pub(super) fn render_dms(&self) {
        let mut items: Vec<DmItem> = {
            let st = self.state.lock().unwrap();
            st.dms
                .iter()
                .map(|d| {
                    let p = st.profiles.get(&d.other);
                    let name = p.map(|p| p.name()).unwrap_or_else(|| "…".into());
                    DmItem {
                        id: d.id.clone().into(),
                        user_id: d.other.clone().into(),
                        initials: initials(&name).into(),
                        name: name.into(),
                        avatar: avatar_of(p),
                        color: p.map(|p| p.color()).unwrap_or(DEFAULT_COLOR),
                        online: st.online.contains(&d.other),
                        activity: st.activities.get(&d.other).cloned().unwrap_or_default().into(),
                        unread: st.unread.count(&d.id) as i32,
                    }
                })
                .collect()
        };
        items.sort_by_key(|d| d.name.to_lowercase());
        self.ui(move |ui| ui.set_dms(ModelRc::new(VecModel::from(items))));
    }

    /// Botão "Mensagem" de um amigo: abre (ou cria) a conversa.
    pub async fn message_user(self: Arc<Self>, user: String) {
        match self.sb.rpc::<String>("get_or_create_dm", json!({ "p_other": user })).await {
            Ok(dm) => {
                self.load_dms().await;
                self.open_dm(dm).await;
            }
            Err(e) => {
                let msg = format!("Não deu para abrir a conversa: {e}");
                self.ui(move |ui| {
                    ui.set_friend_ok(false);
                    ui.set_friend_result(msg.into());
                });
            }
        }
    }

    pub async fn open_dm(self: Arc<Self>, dm: String) {
        self.leave_chat();
        let other = {
            let mut st = self.state.lock().unwrap();
            st.current_server = None;
            st.current_channel = Some(dm.clone());
            st.voice_channels.clear();
            st.messages.clear();
            st.dms.iter().find(|d| d.id == dm).map(|d| d.other.clone())
        };
        let name = {
            let st = self.state.lock().unwrap();
            other.and_then(|o| st.profiles.get(&o).map(|p| p.name())).unwrap_or_default()
        };
        let id = dm.clone();
        self.ui(move |ui| {
            ui.set_current_server("".into());
            ui.set_current_channel(id.into());
            ui.set_channel_name(name.into());
            ui.set_channel_is_voice(false);
            ui.set_chat_is_dm(true);
            ui.set_chat_error("".into());
            ui.set_voice_channels(ModelRc::default());
            MESSAGES.with(|m| m.borrow().as_ref().map(|m| m.set_vec(vec![])));
        });
        self.load_chat(dm, "dm_id").await;
    }
}
