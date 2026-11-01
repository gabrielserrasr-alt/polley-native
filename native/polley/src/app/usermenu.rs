//! Menu do botão direito numa pessoa da chamada (igual ao do Discord).
//!
//! Em mim: perfil, mutar/ensurdecer, editar perfil e o que aparece na tela da chamada.
//! Em outra pessoa: perfil, mensagem, ligar, volume, silenciar só para mim, esconder a câmera,
//! amizade, bloquear e (para quem modera o servidor) expulsar/banir.

use super::*;
use crate::settings;

impl App {
    /// Abre o menu: a interface só desenha; aqui decidimos o que aparece para essa pessoa.
    pub fn open_user_menu(self: &Arc<Self>, id: String, name: String, x: f32, y: f32) {
        let s = settings::get();
        let me = self.sb.user_id().unwrap_or_default();
        let is_me = id == me;
        let relation = self.relation_with(&id);
        let (my_role, their_role) = (self.role_in_current(&me), self.role_in_current(&id));
        // Dono modera todos; admin só membros comuns (o banco confere de novo).
        let can_mod = !is_me
            && !their_role.is_empty()
            && their_role != "owner"
            && (my_role == "owner" || (my_role == "admin" && their_role == "member"));
        let volume = s.volumes.get(&id).copied().unwrap_or(100);
        let muted = s.muted_users.contains(&id);
        let hide_cam = s.hidden_cams.contains(&id);
        let (no_video, own_cam) = (s.show_no_video, s.show_own_camera);
        let (srv_muted, srv_deafened) = self.server_flags(&id);
        self.ui(move |ui| {
            ui.set_um_srv_muted(srv_muted);
            ui.set_um_srv_deafened(srv_deafened);
            ui.set_um_id(id.into());
            ui.set_um_name(name.into());
            ui.set_um_me(is_me);
            ui.set_um_relation(relation.into());
            ui.set_um_can_mod(can_mod);
            ui.set_um_muted(muted);
            ui.set_um_hide_cam(hide_cam);
            ui.set_show_no_video(no_video);
            ui.set_show_own_camera(own_cam);
            ui.set_vol_value(volume as f32);
            ui.set_um_x(x);
            ui.set_um_y(y);
            ui.set_um_open(true);
        });
    }

    /// Barrinha de volume (0 a 200%): salva e vale na hora.
    pub fn set_user_volume(self: &Arc<Self>, id: String, percent: u32) {
        let mut s = settings::get();
        match percent.min(200) {
            100 => s.volumes.remove(&id),
            p => s.volumes.insert(id.clone(), p),
        };
        settings::set(s);
        self.volume_changed(id);
    }

    fn volume_changed(self: &Arc<Self>, id: String) {
        if let Some(call) = self.current_call() {
            // Abrir o tocador mexe com o áudio do Windows: fora da thread da interface.
            tokio::spawn(async move { call.volume_changed(&id) });
        }
    }

    /// Itens do menu que não precisam de confirmação.
    pub async fn user_menu(self: Arc<Self>, action: String, id: String) {
        match action.as_str() {
            "message" => self.message_user(id).await,
            "call" => {
                // Ligar = abrir (ou criar) a DM e já chamar a pessoa.
                match self.sb.rpc::<String>("get_or_create_dm", json!({ "p_other": id })).await {
                    Ok(dm) => {
                        self.load_dms().await;
                        self.clone().open_dm(dm.clone()).await;
                        self.call_dm(dm).await;
                    }
                    Err(e) => {
                        let msg = format!("Não deu para ligar: {e}");
                        self.ui(move |ui| ui.set_notice(msg.into()));
                    }
                }
            }
            "add-friend" => {
                let username = self.state.lock().unwrap().profiles.get(&id).map(|p| p.username.clone());
                let msg = match username {
                    Some(u) => match self.try_add_friend(&u).await {
                        Ok(text) => {
                            self.load_friends().await;
                            text
                        }
                        Err(e) => e.to_string(),
                    },
                    None => "Não achei essa pessoa.".into(),
                };
                self.ui(move |ui| ui.set_notice(msg.into()));
            }
            "accept-friend" => {
                if let Some(f) = self.friendship_with(&id) {
                    self.accept_friend(f).await;
                }
            }
            // Desbloquear = apagar a linha de bloqueio.
            "unblock" => {
                if let Some(f) = self.friendship_with(&id) {
                    self.remove_friend(f).await;
                }
            }
            "srv-mute" | "srv-deafen" => {
                let (m, d) = self.server_flags(&id);
                let (m, d) = if action == "srv-mute" { (!m, d) } else { (m, !d) };
                self.set_server_voice(id, m, d).await;
            }
            "mute" => {
                let mut s = settings::get();
                if !s.muted_users.remove(&id) {
                    s.muted_users.insert(id.clone());
                }
                settings::set(s);
                self.volume_changed(id);
            }
            "hide-cam" => {
                let mut s = settings::get();
                if !s.hidden_cams.remove(&id) {
                    s.hidden_cams.insert(id);
                }
                settings::set(s);
                if let Some(call) = self.current_call() {
                    call.sync_subscriptions();
                }
                self.refresh_voice().await;
            }
            "show-no-video" | "show-own-camera" => {
                let mut s = settings::get();
                if action == "show-no-video" {
                    s.show_no_video = !s.show_no_video;
                } else {
                    s.show_own_camera = !s.show_own_camera;
                }
                let (no_video, own_cam) = (s.show_no_video, s.show_own_camera);
                settings::set(s);
                // A caixinha do menu (que continua aberto) mostra o valor novo.
                self.ui(move |ui| {
                    ui.set_show_no_video(no_video);
                    ui.set_show_own_camera(own_cam);
                });
                self.refresh_voice().await;
            }
            "camera-hq" => {
                let mut s = settings::get();
                s.camera_hq = !s.camera_hq;
                let hq = s.camera_hq;
                settings::set(s);
                self.ui(move |ui| ui.set_camera_hq(hq));
                // Câmera ligada: religa para já valer a qualidade nova.
                if self.current_call().is_some_and(|c| c.camera_on()) {
                    self.clone().toggle_camera().await;
                    self.clone().toggle_camera().await;
                }
            }
            _ => {}
        }
    }
}
