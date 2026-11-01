//! Gerenciar o servidor aberto: criar/apagar canais, sair do servidor e excluir o servidor.
//! Quem pode o quê é decidido pelo banco (RLS): dono e admins mexem em canais; o dono não pode
//! sair (só excluir), e só o dono exclui.

use super::*;

impl App {
    /// Busca os canais do servidor e atualiza as listas de texto e voz.
    pub(super) async fn load_channels(&self, server: &str) -> Vec<Channel> {
        let channels: Vec<Channel> = self
            .sb
            .select(&format!("channels?server_id=eq.{server}&select=id,name,type&order=position,created_at"))
            .await
            .unwrap_or_default();
        {
            let mut st = self.state.lock().unwrap();
            if st.current_server.as_deref() != Some(server) {
                return channels; // usuário já trocou de servidor
            }
            st.voice_channels = channels.iter().filter(|c| c.kind == "voice").cloned().collect();
            st.text_channels = channels.iter().filter(|c| c.kind == "text").cloned().collect();
            st.channel_count = channels.len();
            for c in &channels {
                st.unread.note_channel(&c.id, server);
            }
        }
        self.render_text_channels();
        self.render_voice_channels();
        channels
    }

    /// Canais de texto do servidor aberto (em negrito os que têm não lidas).
    pub(super) fn render_text_channels(&self) {
        let text: Vec<(SharedString, SharedString, bool)> = {
            let st = self.state.lock().unwrap();
            if st.current_server.is_none() {
                return;
            }
            st.text_channels.iter().map(|c| (c.id.clone().into(), c.name.clone().into(), st.unread.count(&c.id) > 0)).collect()
        };
        self.ui(move |ui| {
            let text: Vec<ChannelItem> = text
                .into_iter()
                .map(|(id, name, unread)| ChannelItem { id, name, unread, people: ModelRc::default() })
                .collect();
            ui.set_text_channels(ModelRc::new(VecModel::from(text)));
        });
    }

    /// Canal criado/apagado por outra pessoa aparece/some na hora.
    pub(super) fn watch_channels(self: &Arc<Self>, server: &str) {
        let Some(rt) = self.realtime() else { return };
        let topic = format!("server:{server}");
        let mut rx = rt.subscribe(&topic, "channels", &format!("server_id=eq.{server}"));
        self.state.lock().unwrap().server_topic = Some(topic);
        let (app, server) = (self.clone(), server.to_string());
        tokio::spawn(async move {
            while let Some(change) = rx.recv().await {
                let channels = app.load_channels(&server).await;
                // O canal aberto foi apagado: volta para o primeiro de texto.
                let deleted = change["old_record"]["id"].as_str().map(str::to_string);
                let current = app.state.lock().unwrap().current_channel.clone();
                if change["type"] == "DELETE" && deleted.is_some() && deleted == current {
                    if let Some(first) = channels.iter().find(|c| c.kind == "text") {
                        app.clone().select_channel(first.id.clone(), false).await;
                    }
                }
            }
        });
    }

    pub(super) fn leave_server_topic(&self) {
        self.leave_members_topic();
        let topic = self.state.lock().unwrap().server_topic.take();
        if let (Some(topic), Some(rt)) = (topic, self.realtime()) {
            rt.unsubscribe(&topic);
        }
    }

    // ------------------------------------------------------------ canais

    pub async fn create_channel(self: Arc<Self>, name: String, voice: bool) {
        let Some(server) = self.state.lock().unwrap().current_server.clone() else { return };
        // Canal de texto no estilo do Discord: minúsculo e com hífens ("Bate Papo" → "bate-papo").
        let name = if voice {
            name.trim().to_string()
        } else {
            name.trim().to_lowercase().split_whitespace().collect::<Vec<_>>().join("-")
        };
        if name.is_empty() {
            return self.ui(|ui| ui.set_channel_error("Digite um nome para o canal.".into()));
        }
        if name.chars().count() > 100 {
            return self.ui(|ui| ui.set_channel_error("O nome pode ter no máximo 100 caracteres.".into()));
        }
        let position = self.state.lock().unwrap().channel_count;
        let row = json!({ "server_id": server, "name": name, "type": if voice { "voice" } else { "text" }, "position": position });
        match self.sb.insert::<Channel>("channels", row).await {
            Ok(channel) => {
                self.ui(|ui| {
                    ui.set_channel_modal_open(false);
                    ui.set_channel_error("".into());
                });
                self.load_channels(&server).await;
                if !voice {
                    self.select_channel(channel.id, false).await;
                }
            }
            Err(e) => {
                let msg = if e.to_string().contains("row-level security") {
                    "Só o dono e os admins do servidor podem criar canais.".to_string()
                } else {
                    format!("Não deu para criar o canal: {e}")
                };
                self.ui(move |ui| ui.set_channel_error(msg.into()));
            }
        }
    }

    pub async fn delete_channel(self: Arc<Self>, id: String) {
        let Some(server) = self.state.lock().unwrap().current_server.clone() else { return };
        // Estou na call desse canal: sai antes de apagar.
        if self.current_call().is_some_and(|c| c.channel_id == id) {
            self.leave_voice();
        }
        if let Err(e) = self.sb.delete("channels", &format!("id=eq.{id}")).await {
            let msg = format!("Não deu para apagar o canal: {e}");
            return self.ui(move |ui| ui.set_chat_error(msg.into()));
        }
        let channels = self.load_channels(&server).await;
        let current = self.state.lock().unwrap().current_channel.clone();
        if current.as_deref() == Some(&id) {
            if let Some(first) = channels.iter().find(|c| c.kind == "text") {
                self.select_channel(first.id.clone(), false).await;
            }
        }
    }

    // ------------------------------------------------------------ servidor

    /// Sai da call se ela for de um canal deste servidor.
    pub(super) fn leave_call_in_server(self: &Arc<Self>) {
        let voice_ids: Vec<String> = self.state.lock().unwrap().voice_channels.iter().map(|c| c.id.clone()).collect();
        if self.current_call().is_some_and(|c| voice_ids.contains(&c.channel_id)) {
            self.leave_voice();
        }
    }

    pub async fn leave_server(self: Arc<Self>) {
        let (Some(server), Some(me)) = (self.state.lock().unwrap().current_server.clone(), self.sb.user_id()) else {
            return;
        };
        self.leave_call_in_server();
        match self.sb.delete("server_members", &format!("server_id=eq.{server}&user_id=eq.{me}")).await {
            Ok(()) => {
                self.go_home();
                let _ = self.clone().load_servers().await;
            }
            Err(e) => {
                let msg = format!("Não deu para sair do servidor: {e}");
                self.ui(move |ui| ui.set_chat_error(msg.into()));
            }
        }
    }

    pub async fn delete_server(self: Arc<Self>) {
        let Some(server) = self.state.lock().unwrap().current_server.clone() else { return };
        self.leave_call_in_server();
        match self.sb.delete("servers", &format!("id=eq.{server}")).await {
            Ok(()) => {
                self.go_home();
                let _ = self.clone().load_servers().await;
            }
            Err(e) => {
                let msg = format!("Não deu para excluir o servidor: {e}");
                self.ui(move |ui| ui.set_chat_error(msg.into()));
            }
        }
    }
}
