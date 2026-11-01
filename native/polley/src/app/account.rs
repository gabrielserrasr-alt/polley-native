//! Conta: esqueci minha senha (código por email), trocar senha e excluir conta (LGPD).

use super::*;

const MIN_PASSWORD: usize = 8;

impl App {
    // ------------------------------------------------------------ esqueci minha senha (tela de login)

    /// Passo 1: manda o código para o email.
    pub async fn send_reset(self: Arc<Self>, email: String) {
        if !email.contains('@') {
            return self.auth_failed(anyhow::anyhow!("Digite o email da sua conta."));
        }
        self.ui(|ui| {
            ui.set_auth_busy(true);
            ui.set_auth_error("".into());
            ui.set_auth_info("".into());
        });
        // Código novo: esquece um código antigo que já tinha sido aceito.
        self.forget_recovery();
        match self.sb.recover(&email).await {
            Ok(()) => self.ui(|ui| {
                ui.set_auth_busy(false);
                ui.set_auth_code_sent(true);
                ui.set_auth_info("Se existir uma conta com esse email, enviamos um código. Confira também o spam.".into());
            }),
            Err(e) => self.auth_failed(e),
        }
    }

    /// Passo 2: código + senha nova → já entra na conta.
    pub async fn reset_password(self: Arc<Self>, email: String, code: String, password: String) {
        let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        if code.len() < 6 || !code.chars().all(|c| c.is_ascii_digit()) {
            return self.auth_failed(anyhow::anyhow!("Digite o código de números que chegou no email."));
        }
        if password.chars().count() < MIN_PASSWORD {
            return self.auth_failed(anyhow::anyhow!("A senha precisa ter pelo menos 8 caracteres."));
        }
        self.ui(|ui| {
            ui.set_auth_busy(true);
            ui.set_auth_error("".into());
            ui.set_auth_info("".into());
        });
        // Cada código só vale UMA vez. Se ele já foi aceito (e só a senha foi recusada),
        // não confere de novo: a pessoa só escolhe outra senha.
        let email = email.trim().to_lowercase();
        let verified = self.recovery.lock().unwrap().as_deref() == Some(email.as_str());
        if !verified {
            if let Err(e) = self.sb.verify_code("recovery", &email, &code).await {
                return self.auth_failed(e);
            }
            *self.recovery.lock().unwrap() = Some(email);
        }
        if let Err(e) = self.sb.update_password(&password).await {
            return self.auth_failed(anyhow::anyhow!("{e} (o código continua valendo: é só escolher outra senha)"));
        }
        *self.recovery.lock().unwrap() = None;
        self.ui(|ui| ui.set_auth_code_sent(false));
        self.after_login().await;
    }

    /// Saiu da tela "Esqueci minha senha" sem terminar: descarta o login de recuperação.
    pub fn forget_recovery(&self) {
        if self.recovery.lock().unwrap().take().is_some() {
            self.sb.clear();
        }
    }

    // ------------------------------------------------------------ confirmar o cadastro (código no email)

    /// Mostra a tela "Confirme seu email".
    pub(super) fn ask_signup_code(&self, info: &str) {
        let info = info.to_string();
        self.ui(move |ui| {
            ui.set_auth_busy(false);
            ui.set_auth_error("".into());
            ui.set_auth_info(info.into());
            ui.set_auth_confirm(true);
        });
    }

    pub async fn confirm_signup(self: Arc<Self>, email: String, code: String) {
        let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        if code.len() < 6 || !code.chars().all(|c| c.is_ascii_digit()) {
            return self.auth_failed(anyhow::anyhow!("Digite o código de números que chegou no email."));
        }
        self.ui(|ui| {
            ui.set_auth_busy(true);
            ui.set_auth_error("".into());
            ui.set_auth_info("".into());
        });
        match self.sb.verify_code("email", &email, &code).await {
            Ok(_) => {
                self.ui(|ui| ui.set_auth_confirm(false));
                self.after_login().await;
            }
            Err(e) => self.auth_failed(e),
        }
    }

    pub async fn resend_signup(self: Arc<Self>, email: String) {
        self.ui(|ui| {
            ui.set_auth_busy(true);
            ui.set_auth_error("".into());
        });
        match self.sb.resend_signup(&email).await {
            Ok(()) => self.ask_signup_code("Enviamos um código novo. Confira também o spam."),
            Err(e) => self.auth_failed(e),
        }
    }

    // ------------------------------------------------------------ Configurações → Minha conta

    fn account_result(&self, error: Option<String>, info: &str) {
        let info = info.to_string();
        self.ui(move |ui| {
            ui.set_account_busy(false);
            match error {
                Some(e) => {
                    ui.set_account_error(e.into());
                    ui.set_account_info("".into());
                }
                None => {
                    ui.set_account_error("".into());
                    ui.set_account_info(info.into());
                }
            }
        });
    }

    pub async fn change_password(self: Arc<Self>, current: String, new: String, confirm: String) {
        if new != confirm {
            return self.account_result(Some("As duas senhas novas não são iguais.".into()), "");
        }
        if new.chars().count() < MIN_PASSWORD {
            return self.account_result(Some("A senha nova precisa ter pelo menos 8 caracteres.".into()), "");
        }
        self.ui(|ui| ui.set_account_busy(true));
        let result = async {
            let email = self.sb.email().await?;
            // Confere a senha atual antes de trocar (alguém com o PC aberto não troca a sua senha).
            self.sb.sign_in(&email, &current).await.map_err(|_| anyhow::anyhow!("A senha atual está errada."))?;
            self.sb.update_password(&new).await
        }
        .await;
        match result {
            Ok(()) => {
                self.account_result(None, "Senha trocada!");
                self.ui(|ui| ui.invoke_clear_password_fields());
            }
            Err(e) => self.account_result(Some(e.to_string()), ""),
        }
    }

    pub async fn delete_account(self: Arc<Self>, password: String) {
        if password.is_empty() {
            return self.account_result(Some("Digite sua senha para confirmar.".into()), "");
        }
        self.ui(|ui| ui.set_account_busy(true));
        match self.sb.function("delete-account", json!({ "password": password })).await {
            Ok(_) => {
                self.logout();
                self.ui(|ui| {
                    ui.set_account_busy(false);
                    ui.set_settings_open(false);
                    ui.invoke_clear_password_fields();
                    ui.set_auth_email("".into()); // conta excluída: não sobra nada dela na tela
                    ui.set_auth_info("Sua conta e seus dados foram excluídos.".into());
                });
            }
            Err(e) => self.account_result(Some(e.to_string()), ""),
        }
    }
}
