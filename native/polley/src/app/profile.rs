//! Perfil: editor (nome, bio, foto, banner, cores), "pegar cores da imagem", card de perfil
//! e atualização ao vivo quando alguém muda o próprio perfil.

use super::*;
use crate::palette::{self, Theme};
use crate::{ColorSlot, ProfileCardData, ThemeItem};
use anyhow::{anyhow, bail};
use image::DynamicImage;

/// O que está sendo editado e ainda não foi salvo (nome e bio ficam nos campos da interface).
pub(super) struct Draft {
    avatar_url: Option<String>,
    banner_url: Option<String>,
    theme: Theme,
}

/// Foto do celular pode ter 10+ MB; acima disso provavelmente não é uma foto.
const MAX_FILE: u64 = 25 * 1024 * 1024;

pub(super) fn install(ui: &AppWindow) {
    let presets: Vec<ThemeItem> = palette::PRESETS
        .iter()
        .map(|c| ThemeItem {
            primary: parse_color(c[0]),
            accent: parse_color(c[1]),
            background: parse_color(c[2]),
            text: parse_color(c[3]),
        })
        .collect();
    ui.set_presets(ModelRc::new(VecModel::from(presets)));
}

/// "2026-09-26T..." → "26/09/2026"
fn format_date(iso: &str) -> String {
    DateTime::parse_from_rfc3339(iso).map(|t| t.with_timezone(&Local).format("%d/%m/%Y").to_string()).unwrap_or_default()
}

impl App {
    fn my_profile(&self) -> Option<Profile> {
        let uid = self.sb.user_id()?;
        self.state.lock().unwrap().profiles.get(&uid).cloned()
    }

    /// Meu nome/foto/cor na barra do usuário.
    pub(super) fn show_me(&self, me: &Profile) {
        let (name, username, color, avatar) = (me.name(), me.username.clone(), me.color(), avatar_of(Some(me)));
        self.ui(move |ui| {
            ui.set_me_initials(initials(&name).into());
            ui.set_me_name(name.into());
            ui.set_me_username(username.into());
            ui.set_me_color(color);
            ui.set_me_avatar(avatar);
        });
    }

    // ------------------------------------------------------------ editor

    pub fn open_profile_editor(&self) {
        let Some(me) = self.my_profile() else { return };
        self.images.want(me.banner_url.as_deref(), Kind::Banner);
        *self.draft.lock().unwrap() = Some(Draft {
            avatar_url: me.avatar_url.clone(),
            banner_url: me.banner_url.clone(),
            theme: Theme::from_json(&me.theme),
        });
        let (name, bio) = (me.display_name.clone().unwrap_or_default(), me.bio.clone().unwrap_or_default());
        let (username, since) = (me.username.clone(), me.created_at.as_deref().map(format_date).unwrap_or_default());
        self.ui(move |ui| {
            ui.set_edit_name(name.into());
            ui.set_edit_bio(bio.into());
            ui.set_edit_username(username.into());
            ui.set_edit_since(since.into());
            ui.set_profile_error("".into());
            ui.set_profile_busy("".into());
            ui.set_card_open(false);
            ui.set_profile_open(true);
        });
        self.render_draft();
    }

    /// Manda o rascunho (fotos + cores) para a interface.
    fn render_draft(&self) {
        let Some((avatar, banner, theme)) = self
            .draft
            .lock()
            .unwrap()
            .as_ref()
            .map(|d| (d.avatar_url.clone().unwrap_or_default(), d.banner_url.clone().unwrap_or_default(), d.theme.clone()))
        else {
            return;
        };
        let slots: Vec<ColorSlot> = palette::SLOTS
            .iter()
            .zip(theme.as_array())
            .map(|((_, label), hex)| ColorSlot { label: (*label).into(), hex: hex.into(), color: parse_color(hex) })
            .collect();
        self.ui(move |ui| {
            ui.set_edit_avatar(avatar.into());
            ui.set_edit_banner(banner.into());
            ui.set_edit_slots(ModelRc::new(VecModel::from(slots)));
        });
    }

    fn with_draft(&self, f: impl FnOnce(&mut Draft)) {
        if let Some(d) = self.draft.lock().unwrap().as_mut() {
            f(d);
        }
        self.render_draft();
    }

    pub fn apply_preset(&self, index: usize) {
        let Some(colors) = palette::PRESETS.get(index) else { return };
        self.with_draft(|d| d.theme = Theme::from_array(*colors));
    }

    /// Cor digitada à mão. Inválida = volta a mostrar a anterior.
    pub fn set_color(&self, index: usize, hex: String) {
        let valid = palette::normalize_hex(&hex);
        self.with_draft(|d| {
            if let Some(hex) = valid {
                d.theme.set(index, hex);
            }
        });
    }

    pub fn remove_images(&self) {
        self.with_draft(|d| {
            d.avatar_url = None;
            d.banner_url = None;
        });
    }

    fn profile_busy(&self, what: &str, error: &str) {
        let (what, error) = (what.to_string(), error.to_string());
        self.ui(move |ui| {
            ui.set_profile_busy(what.into());
            ui.set_profile_error(error.into());
        });
    }

    /// Botões "Trocar foto", "Trocar banner" e "Pegar cores da imagem".
    /// `target`: "avatar" | "banner" | "palette". `also_banner`: no "palette", usa a imagem como banner também.
    pub async fn pick_image(self: Arc<Self>, target: String, also_banner: bool) {
        self.profile_busy("Escolhendo imagem...", "");
        // A janela de escolher arquivo trava a thread até fechar: fica fora do tokio e da interface.
        let picked = tokio::task::spawn_blocking(|| -> Option<Result<DynamicImage>> {
            let path = rfd::FileDialog::new()
                .set_title("Escolher imagem")
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
            None => return self.profile_busy("", ""), // cancelou
            Some(Err(e)) => return self.profile_busy("", &e.to_string()),
            Some(Ok(img)) => img,
        };

        let result = match target.as_str() {
            "palette" => {
                self.profile_busy("Tirando as cores...", "");
                let (theme, img) = tokio::task::spawn_blocking(move || (palette::extract(&img), img)).await.expect("palette");
                self.with_draft(|d| d.theme = theme);
                if also_banner {
                    self.upload_image(img, Kind::Banner).await
                } else {
                    Ok(())
                }
            }
            "banner" => self.upload_image(img, Kind::Banner).await,
            _ => self.upload_image(img, Kind::Avatar).await,
        };
        match result {
            Ok(()) => self.profile_busy("", ""),
            Err(e) => self.profile_busy("", &format!("Não deu para enviar a imagem: {e}")),
        }
    }

    /// Reduz, envia ao Storage e já coloca no cache (sem baixar de volta o que acabou de subir).
    async fn upload_image(&self, img: DynamicImage, kind: Kind) -> Result<()> {
        self.profile_busy("Enviando imagem...", "");
        let uid = self.sb.user_id().ok_or_else(|| anyhow!("sem sessão"))?;
        let ((bytes, content_type, ext), display) = tokio::task::spawn_blocking(move || -> Result<_> {
            Ok((images::for_upload(&img, kind)?, images::for_display(&img, kind)))
        })
        .await??;
        let bucket = match kind {
            Kind::Avatar => "avatars",
            Kind::Banner | Kind::Attachment => "banners",
        };
        // Nome novo a cada envio: quem tem a foto antiga em cache não fica preso nela.
        let path = format!("{uid}/{}.{ext}", chrono::Utc::now().timestamp_millis());
        let url = self.sb.upload(bucket, &path, bytes, content_type).await?;
        self.images.put(&url, kind, display);
        self.with_draft(|d| match kind {
            Kind::Avatar => d.avatar_url = Some(url),
            Kind::Banner | Kind::Attachment => d.banner_url = Some(url),
        });
        Ok(())
    }

    pub async fn save_profile(self: Arc<Self>, name: String, bio: String) {
        let (name, bio) = (name.trim().to_string(), bio.trim().to_string());
        if name.chars().count() > 32 {
            return self.profile_busy("", "O nome pode ter no máximo 32 caracteres.");
        }
        if bio.chars().count() > 190 {
            return self.profile_busy("", "A bio pode ter no máximo 190 caracteres.");
        }
        let (Some(uid), Some(mut me)) = (self.sb.user_id(), self.my_profile()) else { return };
        let Some((avatar, banner, theme)) =
            self.draft.lock().unwrap().as_ref().map(|d| (d.avatar_url.clone(), d.banner_url.clone(), d.theme.to_json()))
        else {
            return;
        };
        self.profile_busy("Salvando...", "");
        let opt = |s: &str| (!s.is_empty()).then(|| s.to_string());
        let changes = json!({
            "display_name": opt(&name),
            "bio": opt(&bio),
            "avatar_url": avatar,
            "banner_url": banner,
            "theme": theme,
        });
        if let Err(e) = self.sb.update("profiles", &format!("id=eq.{uid}"), changes).await {
            return self.profile_busy("", &format!("Não deu para salvar: {e}"));
        }
        (me.display_name, me.bio, me.avatar_url, me.banner_url, me.theme) = (opt(&name), opt(&bio), avatar, banner, theme);
        self.apply_profile(me);
        *self.draft.lock().unwrap() = None;
        self.ui(|ui| {
            ui.set_profile_busy("".into());
            ui.set_profile_open(false);
        });
    }

    // ------------------------------------------------------------ ao vivo

    /// Perfis mudando ao vivo (a RLS deixa qualquer logado ler perfis).
    pub(super) fn start_profiles(self: &Arc<Self>, rt: &Realtime) {
        let mut changes = rt.subscribe("profiles", "profiles", "");
        let app = self.clone();
        tokio::spawn(async move {
            while let Some(change) = changes.recv().await {
                if change["type"] != "UPDATE" {
                    continue;
                }
                let Ok(p) = serde_json::from_value::<Profile>(change["record"].clone()) else { continue };
                // Só interessa quem já aparece em algum lugar do app.
                let known = app.state.lock().unwrap().profiles.contains_key(&p.id);
                if known {
                    app.apply_profile(p);
                }
            }
        });
    }

    /// Guarda o perfil novo e redesenha tudo onde a pessoa aparece.
    fn apply_profile(self: &Arc<Self>, p: Profile) {
        self.images.want(p.avatar_url.as_deref(), Kind::Avatar);
        if self.sb.user_id().as_deref() == Some(&p.id) {
            self.show_me(&p);
        }
        let card_id = p.id.clone();
        self.state.lock().unwrap().profiles.insert(p.id.clone(), p);
        self.render_messages(false);
        self.render_friends();
        self.render_dms();
        self.render_voice_channels();
        let app = self.clone();
        tokio::spawn(async move { app.refresh_voice().await });
        // Card aberto dessa pessoa: atualiza também.
        let app = self.clone();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            if ui.get_card_open() && ui.get_card().id == card_id.as_str() {
                app.fill_card(&card_id);
            }
        });
    }

    // ------------------------------------------------------------ card

    /// Clicar na foto/nome de alguém: mostra o card de perfil.
    pub async fn show_profile(self: Arc<Self>, user: String) {
        let user = if user.is_empty() { self.sb.user_id().unwrap_or_default() } else { user };
        self.ensure_profiles(std::slice::from_ref(&user)).await;
        self.fill_card(&user);
        let role = self.role_in_current(&user);
        self.ui(move |ui| {
            ui.set_card_role(role.into());
            ui.set_card_message("".into());
            ui.set_card_open(true);
        });
    }

    fn fill_card(&self, user: &str) {
        let Some(p) = self.state.lock().unwrap().profiles.get(user).cloned() else { return };
        self.images.want(p.banner_url.as_deref(), Kind::Banner);
        let theme = Theme::from_json(&p.theme);
        let name = p.name();
        // Pega tudo do estado de uma vez só: travar o estado duas vezes na mesma linha congela o app.
        let (online, activity) = {
            let st = self.state.lock().unwrap();
            (st.online.contains(user), st.activities.get(user).cloned().unwrap_or_default())
        };
        let card = ProfileCardData {
            id: p.id.clone().into(),
            initials: initials(&name).into(),
            name: name.into(),
            username: p.username.clone().into(),
            avatar: avatar_of(Some(&p)),
            banner: p.banner_url.clone().unwrap_or_default().into(),
            bio: p.bio.clone().unwrap_or_default().into(),
            since: p.created_at.as_deref().map(format_date).unwrap_or_default().into(),
            primary: parse_color(&theme.primary),
            accent: parse_color(&theme.accent),
            background: parse_color(&theme.background),
            text: parse_color(&theme.text),
            relation: self.relation_with(user).into(),
            online,
            activity: activity.into(),
            deactivated: p.deactivated,
        };
        self.ui(move |ui| ui.set_card(card));
    }

    /// "Adicionar amigo" dentro do card.
    pub async fn card_add_friend(self: Arc<Self>, user: String, username: String) {
        let result = self.try_add_friend(&username).await;
        if result.is_ok() {
            self.load_friends().await;
        }
        let text = result.unwrap_or_else(|e| e.to_string());
        self.fill_card(&user);
        self.ui(move |ui| ui.set_card_message(text.into()));
    }
}
