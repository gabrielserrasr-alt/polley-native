//! Configurações "do Discord": conta, senha e 2 etapas, privacidade, notificações, aparência,
//! acessibilidade, idioma/horário, jogos e modo desenvolvedor.
//!
//! O que é só deste computador fica no settings.json; o que precisa valer para os outros
//! (privacidade de pedidos de amizade, conta desativada, 2 etapas) fica no banco, que confere.

use super::*;
use crate::settings::{self, CustomGame};
use crate::{GameItem, Motion};
use anyhow::Context as _;

/// Cores do tema "Meia-noite" (mais escuro, igual ao do Discord).
struct Palette {
    rail: u32,
    side: u32,
    main: u32,
    panel: u32,
    hover: u32,
    active: u32,
    input: u32,
}

const DARK: Palette = Palette {
    rail: 0x1e1f22,
    side: 0x2b2d31,
    main: 0x313338,
    panel: 0x232428,
    hover: 0x35373c,
    active: 0x404249,
    input: 0x383a40,
};
const MIDNIGHT: Palette = Palette {
    rail: 0x0b0b0d,
    side: 0x141417,
    main: 0x1a1a1e,
    panel: 0x101012,
    hover: 0x232327,
    active: 0x2c2c31,
    input: 0x1f1f23,
};

fn rgb(c: u32) -> slint::Color {
    slint::Color::from_rgb_u8((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

/// Esconde o meio do email: "g*******r@gmail.com".
fn mask_email(email: &str) -> String {
    let Some((user, domain)) = email.split_once('@') else { return email.to_string() };
    let chars: Vec<char> = user.chars().collect();
    let shown = match chars.len() {
        0 => String::new(),
        1 | 2 => format!("{}*", chars[0]),
        n => format!("{}{}{}", chars[0], "*".repeat(n - 2), chars[n - 1]),
    };
    format!("{shown}@{domain}")
}

/// QR code do endereço otpauth:// desenhado em pixels (preto no branco, com borda).
fn qr_image(uri: &str) -> Option<slint::SharedPixelBuffer<slint::Rgba8Pixel>> {
    let code = qrcode::QrCode::new(uri.as_bytes()).ok()?;
    let n = code.width();
    let colors = code.to_colors();
    let (scale, border) = (6usize, 4usize);
    let size = (n + border * 2) * scale;
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(size as u32, size as u32);
    let px = buf.make_mut_slice();
    for (i, p) in px.iter_mut().enumerate() {
        let (x, y) = ((i % size) / scale, (i / size) / scale);
        let dark = x >= border
            && y >= border
            && x < n + border
            && y < n + border
            && colors[(y - border) * n + (x - border)] == qrcode::Color::Dark;
        let v = if dark { 0 } else { 255 };
        *p = slint::Rgba8Pixel { r: v, g: v, b: v, a: 255 };
    }
    Some(buf)
}

impl App {
    /// Aplica tema, animações, fonte e modo compacto (ao abrir o app e ao mudar a opção).
    pub(crate) fn apply_appearance(&self) {
        let s = settings::get();
        self.ui(move |ui| {
            let p = if s.theme == "midnight" { &MIDNIGHT } else { &DARK };
            let t = ui.global::<crate::Theme>();
            t.set_rail(rgb(p.rail));
            t.set_side(rgb(p.side));
            t.set_main(rgb(p.main));
            t.set_panel(rgb(p.panel));
            t.set_hover(rgb(p.hover));
            t.set_active(rgb(p.active));
            t.set_input(rgb(p.input));
            ui.global::<Motion>().set_reduce(s.reduce_motion);
            ui.set_pref_theme(s.theme.clone().into());
            ui.set_pref_compact(s.compact);
            ui.set_pref_chat_font(s.chat_font as i32);
            ui.set_pref_reduce_motion(s.reduce_motion);
            ui.set_pref_clock24(s.clock_24h);
            ui.set_pref_dev(s.dev_mode);
            ui.set_pref_share_activity(s.share_activity);
            ui.set_pref_notif_sound(s.notif_sound);
            ui.set_pref_notif_flash(s.notif_flash);
            ui.set_pref_notif_popup(s.notif_popup);
            ui.set_pref_notif_badge(s.notif_badge);
        });
    }

    /// Abriu as configurações: busca o que vem do servidor (email, 2 etapas, privacidade).
    pub(super) async fn load_prefs(self: Arc<Self>) {
        self.apply_appearance();
        self.show_games();
        let me = self.sb.user_id().unwrap_or_default();
        let (username, display) = {
            let st = self.state.lock().unwrap();
            st.profiles.get(&me).map(|p| (p.username.clone(), p.name())).unwrap_or_default()
        };
        self.ui(move |ui| {
            ui.set_acc_username(username.into());
            ui.set_acc_display(display.into());
            ui.set_mfa_setup(false);
            ui.set_mfa_error("".into());
            ui.set_mfa_info("".into());
        });
        if let Ok(user) = self.sb.user().await {
            let email = user["email"].as_str().unwrap_or_default().to_string();
            let masked = mask_email(&email);
            let enabled = user["factors"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|f| f["factor_type"] == "totp" && f["status"] == "verified");
            self.ui(move |ui| {
                ui.set_acc_email(email.into());
                ui.set_acc_email_masked(masked.into());
                ui.set_mfa_enabled(enabled);
            });
        }
        let rows: Vec<Value> = self
            .sb
            .select(&format!("user_privacy?user_id=eq.{me}&select=fr_everyone,fr_friends_of_friends,fr_server_members"))
            .await
            .unwrap_or_default();
        let get = |k: &str| rows.first().and_then(|r| r[k].as_bool()).unwrap_or(true);
        let (all, fof, servers) = (get("fr_everyone"), get("fr_friends_of_friends"), get("fr_server_members"));
        self.ui(move |ui| {
            ui.set_priv_everyone(all);
            ui.set_priv_fof(fof);
            ui.set_priv_servers(servers);
        });
    }

    /// Opções que ficam neste computador.
    pub async fn set_pref(self: Arc<Self>, name: String, value: String) {
        let on = value == "true";
        let mut s = settings::get();
        match name.as_str() {
            "notif-sound" => s.notif_sound = on,
            "notif-flash" => s.notif_flash = on,
            "notif-popup" => s.notif_popup = on,
            "notif-badge" => s.notif_badge = on,
            "theme" => s.theme = if value == "midnight" { "midnight".into() } else { "dark".into() },
            "compact" => s.compact = on,
            "chat-font" => s.chat_font = value.parse::<u32>().unwrap_or(15).clamp(12, 22),
            "reduce-motion" => s.reduce_motion = on,
            "clock24" => s.clock_24h = on,
            "dev" => s.dev_mode = on,
            "share-activity" => s.share_activity = on,
            _ => return,
        }
        settings::set(s);
        self.apply_appearance();
        match name.as_str() {
            // A hora das mensagens já desenhadas muda junto.
            "clock24" => self.render_messages(false),
            "notif-badge" => self.render_unread(),
            "share-activity" => self.clone().check_game().await,
            _ => {}
        }
    }

    /// Quem pode me mandar pedido de amizade (vale no banco).
    pub async fn set_privacy(self: Arc<Self>, which: String, on: String) {
        let on = on == "true";
        let column = match which.as_str() {
            "everyone" => "fr_everyone",
            "fof" => "fr_friends_of_friends",
            "servers" => "fr_server_members",
            _ => return,
        };
        let me = self.sb.user_id().unwrap_or_default();
        let body = json!({ "user_id": me, column: on });
        if let Err(e) = self.sb.upsert("user_privacy", body).await {
            let msg = format!("Não deu para salvar: {e}");
            self.ui(move |ui| ui.set_notice(msg.into()));
        }
        self.load_prefs().await;
    }

    // ------------------------------------------------------------ 2 etapas

    /// "Ativar": gera o QR code para o app autenticador.
    pub async fn mfa_start(self: Arc<Self>) {
        self.ui(|ui| {
            ui.set_mfa_busy(true);
            ui.set_mfa_error("".into());
        });
        match self.sb.mfa_enroll().await {
            Ok((id, uri, secret)) => {
                *self.mfa_factor.lock().unwrap() = Some(id);
                let qr = qr_image(&uri);
                self.ui(move |ui| {
                    ui.set_mfa_busy(false);
                    if let Some(qr) = qr {
                        ui.set_mfa_qr(slint::Image::from_rgba8(qr));
                    }
                    ui.set_mfa_secret(secret.into());
                    ui.set_mfa_setup(true);
                });
            }
            Err(e) => {
                let msg = format!("Não deu para começar: {e}");
                self.ui(move |ui| {
                    ui.set_mfa_busy(false);
                    ui.set_mfa_error(msg.into());
                });
            }
        }
    }

    /// Confirma o código do app: as 2 etapas passam a valer.
    pub async fn mfa_confirm(self: Arc<Self>, code: String) {
        let Some(factor) = self.mfa_factor.lock().unwrap().clone() else { return };
        self.ui(|ui| ui.set_mfa_busy(true));
        let result = self.sb.mfa_verify(&factor, &code).await;
        if result.is_ok() {
            // A sessão virou "aal2": o tempo real precisa do token novo (o antigo passa a ser recusado).
            self.refresh_realtime_token();
        }
        self.ui(move |ui| {
            ui.set_mfa_busy(false);
            match result {
                Ok(()) => {
                    ui.set_mfa_setup(false);
                    ui.set_mfa_enabled(true);
                    ui.set_mfa_qr(slint::Image::default());
                    ui.set_mfa_secret("".into());
                    ui.set_mfa_error("".into());
                    ui.set_mfa_info("Verificação em 2 etapas ativada! Guarde bem o app autenticador.".into());
                }
                Err(e) => ui.set_mfa_error(e.to_string().into()),
            }
        });
    }

    fn refresh_realtime_token(&self) {
        if let (Some(rt), Some(token)) = (self.realtime(), self.sb.access_token()) {
            rt.set_token(token);
        }
    }

    /// Desliga as 2 etapas (pede o código de novo, igual ao Discord).
    pub async fn mfa_disable(self: Arc<Self>, code: String) {
        self.ui(|ui| {
            ui.set_mfa_busy(true);
            ui.set_mfa_error("".into());
        });
        let result = async {
            let factor = self.sb.totp_factor().await?.context("as 2 etapas já estão desligadas")?;
            self.sb.mfa_verify(&factor, &code).await?;
            self.sb.mfa_unenroll(&factor).await
        }
        .await;
        self.ui(move |ui| {
            ui.set_mfa_busy(false);
            match result {
                Ok(()) => {
                    ui.set_mfa_enabled(false);
                    ui.set_mfa_info("Verificação em 2 etapas desativada.".into());
                }
                Err(e) => ui.set_mfa_error(e.to_string().into()),
            }
        });
    }

    /// Login: a conta tem 2 etapas e a sessão ainda é só da senha? Então pede o código.
    pub(super) async fn needs_mfa(&self) -> bool {
        if self.sb.aal() == "aal2" {
            return false;
        }
        match self.sb.totp_factor().await {
            Ok(Some(factor)) => {
                *self.mfa_factor.lock().unwrap() = Some(factor);
                true
            }
            _ => false,
        }
    }

    /// Código digitado na tela de login.
    pub async fn verify_login_mfa(self: Arc<Self>, code: String) {
        let Some(factor) = self.mfa_factor.lock().unwrap().clone() else { return };
        self.ui(|ui| {
            ui.set_auth_busy(true);
            ui.set_auth_error("".into());
        });
        match self.sb.mfa_verify(&factor, &code).await {
            Ok(()) => {
                self.ui(|ui| {
                    ui.set_auth_mfa_mode(false);
                    ui.set_auth_code("".into());
                });
                self.after_login().await;
            }
            Err(e) => self.auth_failed(e),
        }
    }

    // ------------------------------------------------------------ conta

    /// "Sair de todos os aparelhos": derruba todas as sessões (inclusive esta).
    pub async fn logout_all(self: Arc<Self>) {
        match self.sb.logout_all().await {
            Ok(()) => {
                self.logout();
                self.ui(|ui| ui.set_auth_info("Você saiu de todos os aparelhos. Entre de novo.".into()));
            }
            Err(e) => {
                let msg = format!("Não deu para sair de todos: {e}");
                self.ui(move |ui| ui.set_account_error(msg.into()));
            }
        }
    }

    /// Desativa: ninguém consegue te mandar pedido novo até você entrar de novo.
    pub async fn deactivate_account(self: Arc<Self>) {
        let me = self.sb.user_id().unwrap_or_default();
        match self.sb.update("profiles", &format!("id=eq.{me}"), json!({ "deactivated": true })).await {
            Ok(()) => {
                self.logout();
                self.ui(|ui| ui.set_auth_info("Conta desativada. Quando quiser voltar, é só entrar de novo.".into()));
            }
            Err(e) => {
                let msg = format!("Não deu para desativar: {e}");
                self.ui(move |ui| ui.set_account_error(msg.into()));
            }
        }
    }

    /// Entrou numa conta desativada: reativa.
    pub(super) async fn reactivate_if_needed(&self, me: &Profile) {
        if me.deactivated && self.sb.update("profiles", &format!("id=eq.{}", me.id), json!({ "deactivated": false })).await.is_ok() {
            self.ui(|ui| ui.set_notice("Bem-vindo de volta! Sua conta foi reativada.".into()));
        }
    }

    /// Modo desenvolvedor: "Copiar ID".
    pub fn copy_id(&self, id: String) {
        let msg = if crate::clip::set_text(&id) { "Copiado!" } else { "Não deu para copiar." };
        self.ui(move |ui| ui.set_notice(msg.into()));
    }

    // ------------------------------------------------------------ jogos

    /// Lista da página "Jogos registrados" (os adicionados e os programas abertos agora).
    pub fn show_games(&self) {
        let s = settings::get();
        let added: Vec<GameItem> =
            s.games.iter().map(|g| GameItem { exe: g.exe.clone().into(), name: g.name.clone().into() }).collect();
        let apps = crate::games::windowed_apps();
        let labels: Vec<SharedString> = apps.iter().map(|(exe, title)| format!("{title}  ({exe})").into()).collect();
        *self.running_apps.lock().unwrap() = apps;
        let now = self.game.lock().unwrap().clone().unwrap_or_default();
        self.ui(move |ui| {
            ui.set_games_added(ModelRc::new(VecModel::from(added)));
            ui.set_games_running(ModelRc::new(VecModel::from(labels)));
            ui.set_game_now(now.into());
        });
    }

    /// "Adicionar jogo": o programa escolhido na lista, com o nome que a pessoa deu.
    pub async fn add_game(self: Arc<Self>, index: String, name: String) {
        let index = index.parse::<usize>().unwrap_or(0);
        let Some((exe, title)) = self.running_apps.lock().unwrap().get(index).cloned() else { return };
        let name = name.trim().chars().take(40).collect::<String>();
        let name = if name.is_empty() { title } else { name };
        let mut s = settings::get();
        s.games.retain(|g| g.exe != exe);
        s.games.push(CustomGame { exe, name });
        settings::set(s);
        self.show_games();
        self.clone().check_game().await;
    }

    pub async fn remove_game(self: Arc<Self>, exe: String) {
        let mut s = settings::get();
        s.games.retain(|g| g.exe != exe);
        settings::set(s);
        self.show_games();
        self.clone().check_game().await;
    }

    /// Olha qual jogo está aberto e conta para os amigos (pela presença) se mudou.
    pub(super) async fn check_game(self: Arc<Self>) {
        let s = settings::get();
        let found = if s.share_activity {
            tokio::task::spawn_blocking(move || crate::games::detect(&s.games)).await.ok().flatten()
        } else {
            None
        };
        let changed = {
            let mut game = self.game.lock().unwrap();
            let changed = *game != found;
            *game = found.clone();
            changed
        };
        if changed {
            self.track_presence();
            let now = found.unwrap_or_default();
            self.ui(move |ui| ui.set_game_now(now.into()));
        }
    }

    /// Depois do login: confere o jogo aberto a cada 15 s (uma tarefa só, mesmo trocando de conta).
    pub(super) fn start_games(self: &Arc<Self>) {
        if self.games_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let app = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let Some(a) = app.upgrade() else { break };
                if a.sb.user_id().is_some() {
                    a.clone().check_game().await;
                }
                drop(a);
                tokio::time::sleep(Duration::from_secs(15)).await;
            }
        });
    }
}
