//! Lista de membros do servidor (online primeiro) e moderação: expulsar, banir/desbanir,
//! promover/remover admin. As regras de quem pode o quê ficam no banco (0005_moderation.sql);
//! aqui o app só esconde os botões que não fariam sentido e mostra os erros.

use super::*;
use crate::{BanItem, MemberItem};

#[derive(Clone, Deserialize)]
pub(super) struct Member {
    pub user_id: String,
    pub role: String,
}

fn role_rank(role: &str) -> u8 {
    match role {
        "owner" => 0,
        "admin" => 1,
        _ => 2,
    }
}

impl App {
    /// Busca os membros do servidor, descobre o meu papel e redesenha a lista.
    pub(super) async fn load_members(&self, server: &str) {
        let Ok(members) = self
            .sb
            .select::<Vec<Member>>(&format!("server_members?server_id=eq.{server}&select=user_id,role"))
            .await
        else {
            return;
        };
        let ids: Vec<String> = members.iter().map(|m| m.user_id.clone()).collect();
        self.ensure_profiles(&ids).await;
        let me = self.sb.user_id().unwrap_or_default();
        let role = members.iter().find(|m| m.user_id == me).map(|m| m.role.clone()).unwrap_or_else(|| "member".into());
        {
            let mut st = self.state.lock().unwrap();
            if st.current_server.as_deref() != Some(server) {
                return; // já trocou de servidor
            }
            st.my_role = role.clone();
            st.members = members;
        }
        self.ui(move |ui| ui.set_my_role(role.into()));
        self.render_members();
    }

    /// Alguém entrou, saiu ou virou admin: atualiza na hora.
    pub(super) fn watch_members(self: &Arc<Self>, server: &str) {
        let Some(rt) = self.realtime() else { return };
        let topic = format!("members:{server}");
        let mut rx = rt.subscribe(&topic, "server_members", &format!("server_id=eq.{server}"));
        self.state.lock().unwrap().members_topic = Some(topic);
        let (app, server) = (self.clone(), server.to_string());
        tokio::spawn(async move {
            while rx.recv().await.is_some() {
                app.load_members(&server).await;
            }
        });
    }

    pub(super) fn leave_members_topic(&self) {
        let topic = self.state.lock().unwrap().members_topic.take();
        if let (Some(topic), Some(rt)) = (topic, self.realtime()) {
            rt.unsubscribe(&topic);
        }
    }

    /// Coluna da direita: "ONLINE — n" e "OFFLINE — n", dono e admins primeiro.
    pub(super) fn render_members(&self) {
        let items: Vec<MemberItem> = {
            let st = self.state.lock().unwrap();
            if st.current_server.is_none() {
                return;
            }
            let mut rows: Vec<(bool, u8, String, MemberItem)> = st
                .members
                .iter()
                .filter_map(|m| {
                    let p = st.profiles.get(&m.user_id)?;
                    let name = p.name();
                    let online = st.online.contains(&m.user_id);
                    let item = MemberItem {
                        id: m.user_id.clone().into(),
                        initials: initials(&name).into(),
                        name: name.clone().into(),
                        avatar: avatar_of(Some(p)),
                        color: p.color(),
                        online,
                        activity: st.activities.get(&m.user_id).cloned().unwrap_or_default().into(),
                        role: m.role.clone().into(),
                        header: false,
                    };
                    Some((online, role_rank(&m.role), name.to_lowercase(), item))
                })
                .collect();
            rows.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
            let online = rows.iter().filter(|r| r.0).count();
            let offline = rows.len() - online;
            let header = |text: String| MemberItem { name: text.into(), header: true, ..Default::default() };
            let mut out = Vec::with_capacity(rows.len() + 2);
            if online > 0 {
                out.push(header(format!("ONLINE — {online}")));
            }
            let mut rows = rows.into_iter().peekable();
            while let Some(r) = rows.next_if(|r| r.0) {
                out.push(r.3);
            }
            if offline > 0 {
                out.push(header(format!("OFFLINE — {offline}")));
            }
            out.extend(rows.map(|r| r.3));
            out
        };
        self.ui(move |ui| ui.set_members(ModelRc::new(VecModel::from(items))));
    }

    /// Papel de alguém no servidor aberto ("" = não é membro), para os botões do card.
    pub(super) fn role_in_current(&self, user: &str) -> String {
        let st = self.state.lock().unwrap();
        if st.current_server.is_none() {
            return String::new();
        }
        st.members.iter().find(|m| m.user_id == user).map(|m| m.role.clone()).unwrap_or_default()
    }

    /// Ações confirmadas no aviso: "kick:<id>", "ban:<id>", "promote:<id>", "demote:<id>", "unban:<id>".
    pub async fn moderate(self: Arc<Self>, action: String) {
        let Some(server) = self.state.lock().unwrap().current_server.clone() else { return };
        let Some((what, user)) = action.split_once(':') else { return };
        let args = json!({ "p_server": server, "p_user": user });
        let result = match what {
            "kick" => self.sb.rpc_void("kick_member", args).await,
            "ban" => self.sb.rpc_void("ban_member", args).await,
            "unban" => self.sb.rpc_void("unban_member", args).await,
            "promote" | "demote" => {
                let role = if what == "promote" { "admin" } else { "member" };
                self.sb
                    .rpc_void("set_member_role", json!({ "p_server": server, "p_user": user, "p_role": role }))
                    .await
            }
            _ => return,
        };
        if result.is_ok() && matches!(what, "kick" | "ban") {
            self.kick_from_voice(&server, user).await;
        }
        match result {
            Ok(()) if what == "unban" => self.clone().open_bans().await,
            Ok(()) => self.load_members(&server).await,
            Err(e) => {
                let msg = if e.to_string().contains("permissão") {
                    "Você não tem permissão para fazer isso.".to_string()
                } else {
                    format!("Não deu certo: {e}")
                };
                self.ui(move |ui| ui.set_notice(msg.into()));
            }
        }
    }

    /// Menu do servidor → "Banidos".
    pub async fn open_bans(self: Arc<Self>) {
        let Some(server) = self.state.lock().unwrap().current_server.clone() else { return };
        let bans: Vec<Value> = self
            .sb
            .select(&format!("server_bans?server_id=eq.{server}&select=user_id&order=created_at.desc"))
            .await
            .unwrap_or_default();
        let ids: Vec<String> = bans.iter().filter_map(|b| b["user_id"].as_str().map(str::to_string)).collect();
        self.ensure_profiles(&ids).await;
        let items: Vec<BanItem> = {
            let st = self.state.lock().unwrap();
            ids.iter()
                .map(|id| {
                    let p = st.profiles.get(id);
                    let name = p.map(|p| p.name()).unwrap_or_else(|| "Usuário".into());
                    BanItem {
                        id: id.clone().into(),
                        initials: initials(&name).into(),
                        name: name.into(),
                        avatar: avatar_of(p),
                        color: p.map(|p| p.color()).unwrap_or(DEFAULT_COLOR),
                    }
                })
                .collect()
        };
        self.ui(move |ui| {
            ui.set_bans(ModelRc::new(VecModel::from(items)));
            ui.set_bans_open(true);
        });
    }

    /// Chegou no meu canal privado: fui expulso/banido de um servidor.
    pub(super) fn on_removed(self: &Arc<Self>, server: String, how: String) {
        let name = {
            let st = self.state.lock().unwrap();
            st.servers.iter().find(|s| s.id == server).map(|s| s.name.clone()).unwrap_or_default()
        };
        let here = self.state.lock().unwrap().current_server.as_deref() == Some(server.as_str());
        if here {
            self.leave_call_in_server();
            self.go_home();
        }
        let msg = if how == "ban" {
            format!("Você foi banido do servidor \"{name}\".")
        } else {
            format!("Você foi removido do servidor \"{name}\".")
        };
        self.ui(move |ui| ui.set_notice(msg.into()));
        let app = self.clone();
        tokio::spawn(async move {
            let _ = app.load_servers().await;
        });
    }
}
