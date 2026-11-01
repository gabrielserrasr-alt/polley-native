//! "Configurações do servidor": nome, ícone e novo código de convite.
//!
//! Nome e ícone só o dono muda (regra do banco). Código de convite novo: dono e admins, por uma
//! função do banco (o convite não muda por UPDATE solto). O convite antigo para de funcionar.

use super::*;
use anyhow::{anyhow, bail};
use image::DynamicImage;

const MAX_FILE: u64 = 25 * 1024 * 1024;

impl App {
    pub fn open_server_settings(&self) {
        let (name, icon, invite) = {
            let st = self.state.lock().unwrap();
            let Some(s) = st.servers.iter().find(|s| Some(&s.id) == st.current_server.as_ref()) else { return };
            (s.name.clone(), s.icon_url.clone().unwrap_or_default(), s.invite_code.clone())
        };
        *self.server_icon_draft.lock().unwrap() = Some(icon.clone());
        self.ui(move |ui| {
            ui.set_srv_edit_initials(initials(&name).into());
            ui.set_srv_edit_name(name.into());
            ui.set_srv_edit_icon(icon.into());
            ui.set_invite_code(invite.into());
            ui.set_srv_edit_error("".into());
            ui.set_srv_edit_busy(false);
            ui.set_srv_edit_open(true);
        });
    }

    fn server_busy(&self, busy: bool, error: &str) {
        let error = error.to_string();
        self.ui(move |ui| {
            ui.set_srv_edit_busy(busy);
            ui.set_srv_edit_error(error.into());
        });
    }

    /// "Trocar ícone": escolhe a imagem, reduz e envia (só é salvo no servidor ao clicar em Salvar).
    pub async fn pick_server_icon(self: Arc<Self>) {
        self.server_busy(true, "");
        let picked = tokio::task::spawn_blocking(|| -> Option<Result<DynamicImage>> {
            let path = rfd::FileDialog::new()
                .set_title("Escolher ícone do servidor")
                .add_filter("Imagens", &["png", "jpg", "jpeg", "webp", "gif"])
                .pick_file()?;
            Some((|| {
                if std::fs::metadata(&path)?.len() > MAX_FILE {
                    bail!("Imagem grande demais (máximo 25 MB).");
                }
                images::decode(&std::fs::read(&path)?).map_err(|_| anyhow!("Não consegui abrir essa imagem."))
            })())
        })
        .await
        .ok()
        .flatten();
        let img = match picked {
            None => return self.server_busy(false, ""),
            Some(Err(e)) => return self.server_busy(false, &e.to_string()),
            Some(Ok(img)) => img,
        };
        let result = async {
            let uid = self.sb.user_id().ok_or_else(|| anyhow!("sem sessão"))?;
            let ((bytes, content_type, ext), display) = tokio::task::spawn_blocking(move || -> Result<_> {
                Ok((images::for_upload(&img, Kind::Avatar)?, images::for_display(&img, Kind::Avatar)))
            })
            .await??;
            // Fica na pasta do dono (a regra do banco só aceita ícone dali).
            let path = format!("{uid}/servidor-{}.{ext}", chrono::Utc::now().timestamp_millis());
            let url = self.sb.upload("avatars", &path, bytes, content_type).await?;
            self.images.put(&url, Kind::Avatar, display);
            anyhow::Ok(url)
        }
        .await;
        match result {
            Ok(url) => {
                *self.server_icon_draft.lock().unwrap() = Some(url.clone());
                self.ui(move |ui| ui.set_srv_edit_icon(url.into()));
                self.server_busy(false, "");
            }
            Err(e) => self.server_busy(false, &format!("Não deu para enviar a imagem: {e}")),
        }
    }

    pub fn remove_server_icon(&self) {
        *self.server_icon_draft.lock().unwrap() = Some(String::new());
        self.ui(|ui| ui.set_srv_edit_icon("".into()));
    }

    pub async fn save_server(self: Arc<Self>, name: String) {
        let name = name.trim().to_string();
        if name.is_empty() {
            return self.server_busy(false, "O servidor precisa de um nome.");
        }
        if name.chars().count() > 100 {
            return self.server_busy(false, "O nome pode ter no máximo 100 caracteres.");
        }
        let Some(server) = self.state.lock().unwrap().current_server.clone() else { return };
        let icon = self.server_icon_draft.lock().unwrap().clone().unwrap_or_default();
        self.server_busy(true, "");
        let changes = json!({ "name": name, "icon_url": (!icon.is_empty()).then_some(icon) });
        if let Err(e) = self.sb.update("servers", &format!("id=eq.{server}"), changes).await {
            return self.server_busy(false, &format!("Não deu para salvar: {e}"));
        }
        let _ = self.clone().load_servers().await;
        let name2 = name.clone();
        self.ui(move |ui| {
            ui.set_server_name(name2.into());
            ui.set_srv_edit_busy(false);
            ui.set_srv_edit_open(false);
        });
    }

    /// "Gerar novo convite": o código antigo para de funcionar na hora.
    pub async fn new_invite(self: Arc<Self>) {
        let Some(server) = self.state.lock().unwrap().current_server.clone() else { return };
        match self.sb.rpc::<String>("new_invite", json!({ "p_server": server })).await {
            Ok(code) => {
                {
                    let mut st = self.state.lock().unwrap();
                    if let Some(s) = st.servers.iter_mut().find(|s| s.id == server) {
                        s.invite_code = code.clone();
                    }
                }
                self.ui(move |ui| {
                    ui.set_invite_code(code.into());
                    ui.set_notice("Convite novo criado. O código antigo não funciona mais.".into());
                });
            }
            Err(e) => {
                let msg = if e.to_string().contains("permissão") {
                    "Só o dono e os admins podem trocar o convite.".to_string()
                } else {
                    format!("Não deu para criar o convite: {e}")
                };
                self.ui(move |ui| ui.set_notice(msg.into()));
            }
        }
    }
}
