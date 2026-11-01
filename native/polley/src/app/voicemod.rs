//! "Silenciar voz no servidor" e "Desativar áudio no servidor" (moderadores).
//!
//! Quem manda é o banco (server_members.voice_muted / voice_deafened, só moderador muda) e o
//! LiveKit: a Edge Function tira a permissão de publicar microfone da pessoa na hora, então nem
//! um app modificado consegue falar. Aqui o app só acompanha o estado para mostrar os ícones
//! vermelhos e fechar o próprio microfone/som.

use super::*;
use crate::sounds::{cue, Cue};

#[derive(Default)]
pub(super) struct VoiceMod {
    /// Servidor da chamada em que estou (None = fora de chamada ou numa DM).
    server: Option<String>,
    topic: Option<String>,
    /// Quem está silenciado/sem áudio pelo servidor: id → (voz, áudio).
    flags: HashMap<String, (bool, bool)>,
}

impl VoiceMod {
    pub(super) fn flags_snapshot(&self) -> HashMap<String, (bool, bool)> {
        self.flags.clone()
    }
}

#[derive(Deserialize)]
struct Flags {
    user_id: String,
    #[serde(default)]
    voice_muted: bool,
    #[serde(default)]
    voice_deafened: bool,
}

impl App {
    /// Entrei numa chamada de servidor: acompanha ao vivo quem os moderadores silenciaram.
    pub(super) fn watch_voice_mod(self: &Arc<Self>, server: String, gen: u64) {
        self.stop_voice_mod();
        self.voice_mod.lock().unwrap().server = Some(server.clone());
        let app = self.clone();
        tokio::spawn(async move {
            app.load_voice_mod(&server).await;
            let Some(rt) = app.realtime() else { return };
            let topic = format!("voicemod:{server}");
            let mut rx = rt.subscribe(&topic, "server_members", &format!("server_id=eq.{server}"));
            app.voice_mod.lock().unwrap().topic = Some(topic);
            while rx.recv().await.is_some() {
                if app.call_gen.load(Ordering::SeqCst) != gen {
                    break;
                }
                app.load_voice_mod(&server).await;
            }
        });
    }

    /// Saí da chamada: esquece tudo e devolve o microfone/som ao que a pessoa escolheu.
    pub(super) fn stop_voice_mod(&self) {
        let topic = {
            let mut vm = self.voice_mod.lock().unwrap();
            vm.server = None;
            vm.flags.clear();
            vm.topic.take()
        };
        if let (Some(topic), Some(rt)) = (topic, self.realtime()) {
            rt.unsubscribe(&topic);
        }
        self.srv_muted.store(false, Ordering::SeqCst);
        self.srv_deafened.store(false, Ordering::SeqCst);
    }

    async fn load_voice_mod(self: &Arc<Self>, server: &str) {
        let query = format!(
            "server_members?server_id=eq.{server}&or=(voice_muted.eq.true,voice_deafened.eq.true)&select=user_id,voice_muted,voice_deafened"
        );
        let Ok(rows) = self.sb.select::<Vec<Flags>>(&query).await else { return };
        let me = self.sb.user_id().unwrap_or_default();
        let (muted, deafened) =
            rows.iter().find(|r| r.user_id == me).map(|r| (r.voice_muted, r.voice_deafened)).unwrap_or_default();
        {
            let mut vm = self.voice_mod.lock().unwrap();
            if vm.server.as_deref() != Some(server) {
                return; // já saiu/trocou de chamada
            }
            vm.flags = rows.into_iter().map(|r| (r.user_id, (r.voice_muted, r.voice_deafened))).collect();
        }
        let was_muted = self.srv_muted.swap(muted, Ordering::SeqCst);
        let was_deafened = self.srv_deafened.swap(deafened, Ordering::SeqCst);
        let msg = if deafened && !was_deafened {
            cue(Cue::Deafen);
            Some("Um moderador desativou seu áudio neste servidor.")
        } else if muted && !was_muted {
            cue(Cue::Mute);
            Some("Um moderador silenciou sua voz neste servidor.")
        } else if (was_muted || was_deafened) && !muted && !deafened {
            cue(Cue::Unmute);
            Some("Um moderador liberou sua voz de novo.")
        } else {
            None
        };
        if let Some(msg) = msg {
            self.ui(move |ui| ui.set_notice(msg.into()));
        }
        // Acabou de ser liberado (vale também para quem entrou já silenciado): microfone de volta.
        if (was_muted || was_deafened) && !muted && !deafened {
            if let Some(call) = self.current_call() {
                self.apply_audio_state();
                tokio::time::sleep(Duration::from_millis(600)).await;
                call.republish_mic().await;
            }
        }
        self.apply_audio_state();
    }

    /// (voz, áudio) de alguém na chamada atual.
    pub(super) fn server_flags(&self, user: &str) -> (bool, bool) {
        self.voice_mod.lock().unwrap().flags.get(user).copied().unwrap_or_default()
    }

    /// Um moderador marcou/desmarcou no menu do botão direito.
    pub(super) async fn set_server_voice(self: &Arc<Self>, user: String, muted: bool, deafened: bool) {
        let Some(server) = self.state.lock().unwrap().current_server.clone() else { return };
        let body = json!({ "action": "moderate", "serverId": server, "userId": user, "muted": muted, "deafened": deafened });
        match self.sb.function("livekit-token", body).await {
            Ok(v) if v["error"].is_null() => {
                // Mostra na hora (o Realtime confirma logo depois).
                if self.voice_mod.lock().unwrap().server.as_deref() == Some(server.as_str()) {
                    self.voice_mod.lock().unwrap().flags.insert(user, (muted, deafened));
                }
                self.refresh_voice().await;
            }
            Ok(v) => {
                let msg = format!("Não deu certo: {}", v["error"].as_str().unwrap_or("erro"));
                self.ui(move |ui| ui.set_notice(msg.into()));
            }
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

    /// Expulso/banido: tira a pessoa da chamada de voz também (senão ela ficaria ouvindo).
    pub(super) async fn kick_from_voice(&self, server: &str, user: &str) {
        let body = json!({ "action": "remove", "serverId": server, "userId": user });
        let _ = self.sb.function("livekit-token", body).await;
    }
}
