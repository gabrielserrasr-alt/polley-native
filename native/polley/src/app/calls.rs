//! Ligar pela DM.
//!   - a chamada é a sala `dm:<id>` do LiveKit (a Edge Function já confere se sou da DM);
//!   - quem liga manda um "toque" a cada 3 s, por até 30 s, no canal PRIVADO `ring:<id do amigo>`
//!     do Realtime (regras em supabase/migrations/0003_ring_channel.sql: só o dono escuta o dele,
//!     e só quem tem DM com ele consegue mandar). A presença pública não fica sabendo de nada;
//!   - o app do amigo toca e mostra "Fulano está te ligando" (Atender / Recusar). Se os toques
//!     param de chegar (desistiu, ou alguém atendeu), para de tocar.

use super::*;
use std::time::Instant;

/// Quanto tempo quem liga continua chamando.
const RING_FOR: Duration = Duration::from_secs(30);
/// Intervalo entre os toques.
const RING_EVERY: Duration = Duration::from_secs(3);
/// Sem toque novo nesse tempo = a pessoa desistiu.
const RING_SILENCE: Duration = Duration::from_secs(7);

#[derive(Default)]
pub(super) struct Ringing {
    /// DM que está me ligando agora → último toque recebido.
    pub current: Option<(String, Instant)>,
    /// Ligações que recusei → último toque recebido (sai da lista quando os toques param).
    pub dismissed: HashMap<String, Instant>,
}

fn ring_topic(user: &str) -> String {
    format!("ring:{user}")
}

impl App {
    /// Escuta o meu canal de toques (chamado depois do login).
    pub(super) fn listen_rings(self: &Arc<Self>, rt: &Realtime, me: &str) {
        let mut rx = rt.listen(&ring_topic(me), true);
        let app = self.clone();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                // Fui expulso/banido de um servidor (quem manda é o banco, em kick_member/ban_member).
                if msg["event"] == "removed" {
                    let server = msg["payload"]["server_id"].as_str().unwrap_or_default().to_string();
                    let how = msg["payload"]["how"].as_str().unwrap_or_default().to_string();
                    app.on_removed(server, how);
                    continue;
                }
                if msg["event"] != "ring" {
                    continue;
                }
                let dm = msg["payload"]["dm"].as_str().unwrap_or_default().to_string();
                let from = msg["payload"]["from"].as_str().unwrap_or_default().to_string();
                app.on_ring(dm, from);
            }
        });
    }

    /// Botão de telefone na DM.
    pub async fn call_dm(self: Arc<Self>, dm: String) {
        self.join_call(dm, true).await;
    }

    /// Já entrei na chamada da DM: fica chamando o amigo até ele entrar (ou 30 s).
    pub(super) fn ring_friend(self: &Arc<Self>, dm: String) {
        let (Some(me), Some(other)) = (
            self.sb.user_id(),
            self.state.lock().unwrap().dms.iter().find(|d| d.id == dm).map(|d| d.other.clone()),
        ) else {
            return;
        };
        let app = self.clone();
        tokio::spawn(async move {
            let start = Instant::now();
            while start.elapsed() < RING_FOR {
                let Some(call) = app.current_call().filter(|c| c.channel_id == dm) else { break };
                if call.members().iter().any(|m| !m.is_me) {
                    break; // atendeu
                }
                drop(call);
                let _ = app.sb.broadcast(&ring_topic(&other), "ring", json!({ "dm": dm, "from": me })).await;
                tokio::time::sleep(RING_EVERY).await;
            }
        });
    }

    fn on_ring(self: &Arc<Self>, dm: String, from: String) {
        let now = Instant::now();
        {
            let mut st = self.state.lock().unwrap();
            // Só toca se for mesmo o outro membro de uma DM minha.
            if !st.dms.iter().any(|d| d.id == dm && d.other == from) {
                return;
            }
            if let Some(t) = st.ringing.dismissed.get_mut(&dm) {
                *t = now;
                return;
            }
            if self.current_call().is_some_and(|c| c.channel_id == dm) {
                return;
            }
            if let Some((cur, t)) = st.ringing.current.as_mut() {
                if *cur == dm {
                    *t = now;
                }
                return; // já tocando (esta ou outra ligação)
            }
            st.ringing.current = Some((dm.clone(), now));
        }
        self.show_incoming(&dm, &from);

        // Vigia: para de tocar quando os toques param; esquece recusas antigas.
        let app = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let silent = {
                    let mut st = app.state.lock().unwrap();
                    st.ringing.dismissed.retain(|_, t| t.elapsed() < RING_SILENCE);
                    match &st.ringing.current {
                        Some((cur, t)) if *cur == dm => t.elapsed() > RING_SILENCE,
                        _ => break, // atendeu, recusou ou outra ligação
                    }
                };
                if silent {
                    app.stop_ringing(false);
                    break;
                }
            }
        });
    }

    fn show_incoming(&self, dm: &str, caller: &str) {
        let (name, avatar, initial, color) = {
            let st = self.state.lock().unwrap();
            let p = st.profiles.get(caller);
            let name = p.map(|p| p.name()).unwrap_or_else(|| "Alguém".into());
            (name.clone(), avatar_of(p), initials(&name), p.map(|p| p.color()).unwrap_or(DEFAULT_COLOR))
        };
        crate::sounds::ring_start();
        let dm = dm.to_string();
        self.ui(move |ui| {
            ui.set_incoming_name(name.into());
            ui.set_incoming_avatar(avatar);
            ui.set_incoming_initials(initial.into());
            ui.set_incoming_color(color);
            ui.set_incoming_dm(dm.into());
        });
    }

    pub async fn answer_call(self: Arc<Self>, dm: String) {
        self.stop_ringing(false);
        self.clone().open_dm(dm.clone()).await;
        self.join_call(dm, true).await;
    }

    pub fn decline_call(&self) {
        self.stop_ringing(true);
    }

    /// Para de tocar. `dismiss` = não tocar de novo enquanto essa pessoa continuar chamando.
    fn stop_ringing(&self, dismiss: bool) {
        let was = {
            let mut st = self.state.lock().unwrap();
            let was = st.ringing.current.take();
            if let (true, Some((dm, _))) = (dismiss, &was) {
                st.ringing.dismissed.insert(dm.clone(), Instant::now());
            }
            was
        };
        if was.is_some() {
            crate::sounds::ring_stop();
            self.ui(|ui| ui.set_incoming_dm("".into()));
        }
    }
}
