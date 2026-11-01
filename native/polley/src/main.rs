//! Polley nativo — Rust + Slint.
//!
//! Estrutura:
//!   ui/app.slint     → a interface (desenho, layout, cores)
//!   src/app.rs       → lógica: login, servidores, canais, chat
//!   src/supabase.rs  → HTTP com o Supabase (mesmo backend do app Electron)
//!   src/realtime.rs  → WebSocket do Supabase Realtime (mensagens ao vivo)
//!   src/voice.rs     → chamada de voz (LiveKit: microfone, fone, quem está falando)
//!   ../media         → motor de vídeo/áudio (captura + encode na GPU), já testado no protótipo

// No executável final, sem janela preta de console.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod clip;
mod games;
mod images;
mod legal;
mod palette;
mod settings;
mod sounds;
mod realtime;
mod supabase;
mod emoji;
mod tray;
mod voice;

use std::time::Duration;

slint::include_modules!();

// Notebooks com 2 placas: pede a placa dedicada (usada depois pelo encoder de vídeo).
#[no_mangle]
pub static NvOptimusEnablement: u32 = 1;
#[no_mangle]
pub static AmdPowerXpressRequestHighPerformance: u32 = 1;

/// Dispara uma tarefa assíncrona a partir de um callback da interface.
macro_rules! on {
    ($ui:ident . $setter:ident, $app:ident, | $($arg:ident),* | $body:expr) => {{
        let $app = $app.clone();
        $ui.$setter(move |$($arg),*| {
            let $app = $app.clone();
            $(let $arg = $arg.to_string();)*
            tokio::spawn(async move { $body.await });
        });
    }};
}

fn main() -> anyhow::Result<()> {
    // Já tem um Polley aberto (talvez na bandeja)? Ele aparece e este fecha.
    if !tray::single_instance() {
        return Ok(());
    }
    let _ = rustls::crypto::ring::default_provider().install_default();

    // 2 threads bastam: o app espera rede quase o tempo todo.
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let _guard = runtime.enter();

    let ui = AppWindow::new()?;
    app::install_models(&ui);
    let app = app::App::new(ui.as_weak());

    on!(ui.on_login, app, |email, password| app.login(email, password));
    on!(ui.on_register, app, |email, password, username| app.register(email, password, username));
    // senha e conta
    on!(ui.on_send_reset, app, |email| app.send_reset(email));
    on!(ui.on_reset_password, app, |email, code, password| app.reset_password(email, code, password));
    {
        let app = app.clone();
        ui.on_leave_forgot(move || app.forget_recovery());
    }
    on!(ui.on_confirm_signup, app, |email, code| app.confirm_signup(email, code));
    {
        let weak = ui.as_weak();
        ui.on_open_legal(move |which| {
            if let Some(ui) = weak.upgrade() {
                legal::show(&ui, &which);
            }
        });
    }
    on!(ui.on_resend_signup, app, |email| app.resend_signup(email));
    on!(ui.on_change_password, app, |current, new, confirm| app.change_password(current, new, confirm));
    on!(ui.on_delete_account, app, |password| app.delete_account(password));
    on!(ui.on_select_server, app, |id| app.select_server(id));
    on!(ui.on_create_server, app, |name| app.create_server(name));
    on!(ui.on_join_server, app, |code| app.join_server(code));
    on!(ui.on_leave_server, app, | | app.leave_server());
    on!(ui.on_delete_server, app, | | app.delete_server());
    // A confirmação manda "delete-channel:<id>".
    on!(ui.on_delete_channel, app, |action| app.delete_channel(action.trim_start_matches("delete-channel:").to_string()));
    // Avisos confirmados ("Tem certeza?"): canal ou mensagem.
    {
        let app = app.clone();
        ui.on_confirmed(move |action| {
            let app = app.clone();
            tokio::spawn(async move {
                if let Some(id) = action.strip_prefix("delete-message:") {
                    app.delete_message(id.to_string()).await;
                } else if let Some(id) = action.strip_prefix("delete-channel:") {
                    app.delete_channel(id.to_string()).await;
                } else if action == "new-invite" {
                    app.new_invite().await;
                } else if let Some(url) = action.strip_prefix("open-link:") {
                    app.open_link(url);
                } else if let Some(id) = action.strip_prefix("block:") {
                    app.block_user(id.to_string()).await;
                } else if ["kick:", "ban:", "unban:", "promote:", "demote:"].iter().any(|p| action.starts_with(p)) {
                    app.moderate(action.to_string()).await;
                }
            });
        });
    }
    // chat
    on!(ui.on_pick_files, app, | | app.pick_files());
    on!(ui.on_edit_message, app, |id, text| app.edit_message(id, text));
    on!(ui.on_save_attachment, app, |url, name| app.save_attachment(url, name));
    on!(ui.on_load_older, app, | | app.load_older());
    on!(ui.on_open_bans, app, | | app.open_bans());
    // apertar para falar e atalhos
    {
        let app = app.clone();
        ui.on_capture_key(move |which| app.capture_key(&which));
    }
    {
        let app = app.clone();
        ui.on_clear_key(move |which| app.clear_key(&which));
    }
    {
        let app = app.clone();
        ui.on_set_push_to_talk(move |on| app.set_push_to_talk(on));
    }
    {
        let app = app.clone();
        ui.on_set_call_sounds(move |on| app.set_call_sounds(on));
    }
    on!(ui.on_view_image, app, |url, name| app.view_image(url, name));
    on!(ui.on_toggle_camera, app, | | app.toggle_camera());
    on!(ui.on_open_settings, app, | | app.open_settings());
    on!(ui.on_set_pref, app, |name, value| app.set_pref(name, value));
    on!(ui.on_set_privacy, app, |which, on| app.set_privacy(which, on));
    on!(ui.on_mfa_start, app, | | app.mfa_start());
    on!(ui.on_mfa_confirm, app, |code| app.mfa_confirm(code));
    on!(ui.on_mfa_disable, app, |code| app.mfa_disable(code));
    on!(ui.on_verify_login_mfa, app, |code| app.verify_login_mfa(code));
    on!(ui.on_logout_all, app, | | app.logout_all());
    on!(ui.on_deactivate_account, app, | | app.deactivate_account());
    on!(ui.on_add_game, app, |index, name| app.add_game(index, name));
    on!(ui.on_remove_game, app, |exe| app.remove_game(exe));
    {
        let app = app.clone();
        ui.on_copy_id(move |id| app.copy_id(id.to_string()));
    }
    {
        let app = app.clone();
        ui.on_refresh_games(move || app.show_games());
    }
    ui.global::<Prefs>().on_matches(|label, query| {
        let q = query.trim().to_lowercase();
        q.is_empty() || label.to_lowercase().contains(&q)
    });
    ui.set_app_version(env!("CARGO_PKG_VERSION").into());
    {
        let app = app.clone();
        ui.on_save_settings(move |mic, speaker, camera, noise| {
            let app = app.clone();
            tokio::spawn(async move {
                app.save_settings(mic.max(0) as usize, speaker.max(0) as usize, camera.max(0) as usize, noise).await
            });
        });
    }
    // ligar pela DM
    on!(ui.on_call_dm, app, |dm| app.call_dm(dm));
    on!(ui.on_answer_call, app, |dm| app.answer_call(dm));
    {
        let app = app.clone();
        ui.on_decline_call(move || app.decline_call());
    }
    on!(ui.on_play_video, app, |url, name| app.play_video(url, name));
    {
        let app = app.clone();
        ui.on_close_viewer(move || app.close_viewer());
    }
    {
        let app = app.clone();
        ui.on_remove_pending(move |i| app.remove_pending(i as usize));
    }
    {
        let app = app.clone();
        ui.on_open_attachment(move |url| {
            tokio::spawn(app.clone().open_attachment(url.into()));
        });
    }
    {
        let app = app.clone();
        ui.on_typing(move || app.typing());
    }
    {
        let app = app.clone();
        ui.on_create_channel(move |name, voice| {
            let app = app.clone();
            let name = name.to_string();
            tokio::spawn(async move { app.create_channel(name, voice).await });
        });
    }
    on!(ui.on_send_message, app, |text, reply| app.send_message(text, reply));
    on!(ui.on_react, app, |message, emoji| app.react(message, emoji));
    // teste de microfone
    {
        let app = app.clone();
        ui.on_start_mic_test(move |mic, speaker| app.start_mic_test(mic.max(0) as usize, speaker.max(0) as usize));
    }
    {
        let app = app.clone();
        ui.on_stop_mic_test(move || app.stop_mic_test());
    }
    {
        let app = app.clone();
        ui.on_set_mic_monitor(move |on| app.set_mic_monitor(on));
    }
    // configurações do servidor
    {
        let app = app.clone();
        ui.on_open_server_settings(move || app.open_server_settings());
    }
    {
        let app = app.clone();
        ui.on_remove_server_icon(move || app.remove_server_icon());
    }
    on!(ui.on_pick_server_icon, app, | | app.pick_server_icon());
    on!(ui.on_save_server, app, |name| app.save_server(name));
    on!(ui.on_new_invite, app, | | app.new_invite());
    // emojis coloridos (desenhados pelo Windows) e a lista do seletor
    ui.global::<Emoji>().on_image(|e| emoji::image(e.as_str()));
    ui.set_emoji_list(slint::ModelRc::new(slint::VecModel::from(
        emoji::PICKER.iter().map(|e| slint::SharedString::from(*e)).collect::<Vec<_>>(),
    )));
    {
        let app = app.clone();
        ui.on_select_channel(move |id, voice| {
            let app = app.clone();
            let id = id.to_string();
            tokio::spawn(async move { app.select_channel(id, voice).await });
        });
    }
    {
        let app = app.clone();
        ui.on_go_home(move || app.go_home());
    }
    {
        let app = app.clone();
        ui.on_logout(move || app.logout());
    }
    on!(ui.on_join_voice, app, |id| app.join_voice(id));
    on!(ui.on_add_friend, app, |name| app.add_friend(name));
    on!(ui.on_accept_friend, app, |id| app.accept_friend(id));
    on!(ui.on_remove_friend, app, |id| app.remove_friend(id));
    on!(ui.on_block_user, app, |id| app.block_user(id));
    on!(ui.on_message_user, app, |id| app.message_user(id));
    on!(ui.on_open_dm, app, |id| app.open_dm(id));

    // perfil
    on!(ui.on_show_profile, app, |id| app.show_profile(id));
    on!(ui.on_save_profile, app, |name, bio| app.save_profile(name, bio));
    on!(ui.on_card_add_friend, app, |id, username| app.card_add_friend(id, username));
    {
        let app = app.clone();
        ui.on_pick_image(move |target, also_banner| {
            let app = app.clone();
            let target = target.to_string();
            tokio::spawn(async move { app.pick_image(target, also_banner).await });
        });
    }
    {
        let app = app.clone();
        ui.on_open_profile(move || app.open_profile_editor());
    }
    {
        let app = app.clone();
        ui.on_apply_preset(move |i| app.apply_preset(i as usize));
    }
    {
        let app = app.clone();
        ui.on_set_color(move |i, hex| app.set_color(i as usize, hex.to_string()));
    }
    {
        let app = app.clone();
        ui.on_remove_images(move || app.remove_images());
    }
    {
        let app = app.clone();
        ui.on_leave_voice(move || app.leave_voice());
    }
    {
        let app = app.clone();
        ui.on_toggle_mute(move || app.toggle_mute());
    }
    {
        let app = app.clone();
        ui.on_toggle_deafen(move || app.toggle_deafen());
    }
    {
        let app = app.clone();
        ui.on_start_screen(move |source, height, fps, audio| {
            let app = app.clone();
            tokio::spawn(async move { app.start_screen(source.into(), height as u32, fps as u32, audio).await });
        });
    }
    {
        let app = app.clone();
        ui.on_open_golive(move || {
            let app = app.clone();
            tokio::spawn(async move { app.open_golive().await });
        });
    }
    {
        let app = app.clone();
        ui.on_close_golive(move || app.close_golive());
    }
    {
        let app = app.clone();
        ui.on_stop_screen(move || {
            let app = app.clone();
            tokio::spawn(async move { app.stop_screen().await });
        });
    }
    {
        let app = app.clone();
        ui.on_toggle_record(move || {
            let app = app.clone();
            tokio::spawn(async move { app.toggle_record().await });
        });
    }
    {
        let app = app.clone();
        ui.on_show_recording(move |path| app.show_recording(path.to_string()));
    }
    {
        let app = app.clone();
        ui.on_watch(move |id| app.watch(id.to_string()));
    }
    {
        let app = app.clone();
        ui.on_stop_watching(move || app.stop_watching());
    }
    {
        let app = app.clone();
        ui.on_focus_member(move |id| {
            let app = app.clone();
            tokio::spawn(app.focus_member(id.to_string()));
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_set_fullscreen(move |on| {
            if let Some(ui) = weak.upgrade() {
                ui.window().set_fullscreen(on);
            }
        });
    }

    // Mostra a RAM do processo na barra do usuário (a mesma métrica do Gerenciador de Tarefas).
    let ram_timer = slint::Timer::default();
    {
        let weak = ui.as_weak();
        let update = move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_ram(format!("{:.0} MB", polley_media::mem::current().task_manager_mb).into());
            }
        };
        update();
        ram_timer.start(slint::TimerMode::Repeated, Duration::from_secs(2), update);
    }

    // Live pela placa de vídeo: a janelinha acompanha a área da live (janela mudou de tamanho etc.).
    let video_timer = slint::Timer::default();
    {
        let weak = ui.as_weak();
        video_timer.start(slint::TimerMode::Repeated, Duration::from_millis(50), move || {
            if let Some(ui) = weak.upgrade() {
                app::video::tick(&ui);
            }
        });
    }

    // Chamada: se o mouse fica parado 3 s, os botões somem (igual ao Discord).
    let idle_timer = slint::Timer::default();
    {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
        let weak = ui.as_weak();
        let mut last = (POINT::default(), std::time::Instant::now());
        idle_timer.start(slint::TimerMode::Repeated, Duration::from_millis(250), move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut p = POINT::default();
            let _ = unsafe { GetCursorPos(&mut p) };
            if p != last.0 {
                last = (p, std::time::Instant::now());
            }
            let idle = last.1.elapsed() >= Duration::from_secs(3);
            if ui.get_call_idle() != idle {
                ui.set_call_idle(idle);
            }
        });
    }

    // atualização automática
    {
        let app = app.clone();
        ui.on_install_update(move || app.install_update());
    }
    tokio::spawn(app.clone().watch_updates());
    tokio::spawn(app.clone().watch_ping());

    // Coluna de canais: largura salva (arrastando a borda).
    if let Some(w) = settings::get().side_width {
        ui.set_side_width(w.clamp(200, 420) as f32);
    }
    // menu do botão direito numa pessoa da chamada
    {
        let app = app.clone();
        ui.on_open_user_menu(move |id, name, x, y| app.open_user_menu(id.to_string(), name.to_string(), x, y));
    }
    {
        let app = app.clone();
        ui.on_set_user_volume(move |id, v| app.set_user_volume(id.to_string(), v.round().max(0.0) as u32));
    }
    on!(ui.on_user_menu, app, |action, id| app.user_menu(action.to_string(), id.to_string()));
    ui.set_show_own_camera(settings::get().show_own_camera);
    ui.set_camera_hq(settings::get().camera_hq);
    ui.set_show_no_video(settings::get().show_no_video);
    ui.on_save_side_width(|w| {
        let mut s = settings::get();
        s.side_width = Some(w.clamp(200, 420) as u32);
        settings::set(s);
    });

    // bandeja e "iniciar com o Windows"
    {
        let mut s = settings::get();
        if !s.autostart_done && tray::can_autostart() {
            // Igual ao Discord: liga na primeira vez; dá para desligar em Configurações → Windows.
            tray::set_autostart(true);
            s.autostart_done = true;
            settings::set(s);
        }
    }
    {
        let weak = ui.as_weak();
        tray::install(move |action| {
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || match action {
                tray::Action::Open => {
                    if let Some(ui) = weak.upgrade() {
                        let _ = ui.show();
                        tray::bring_to_front();
                    }
                }
                tray::Action::Quit => {
                    let _ = slint::quit_event_loop();
                }
            });
        });
    }
    ui.window().on_close_requested(|| {
        if !settings::get().close_to_tray {
            let _ = slint::quit_event_loop();
        }
        slint::CloseRequestResponse::HideWindow
    });
    {
        let app = app.clone();
        ui.on_set_windows_option(move |which, on| app.set_windows_option(&which, on));
    }

    app.apply_appearance();
    tokio::spawn(app.clone().restore());
    // Diagnóstico de memória (só para desenvolvimento: POLLEY_DIAG=camera ou tela).
    if let Ok(mode) = std::env::var("POLLEY_DIAG") {
        tokio::spawn(app.clone().run_diag(mode));
    }
    // Aberto pelo Windows ao ligar o PC com "começar na bandeja": não mostra a janela.
    let auto = std::env::args().any(|a| a == "--auto");
    if !(auto && settings::get().start_minimized) {
        ui.show()?;
    }
    // Esconder a janela (X) não encerra: só "Sair do Polley" (ícone da bandeja) encerra.
    slint::run_event_loop_until_quit()?;
    tray::remove();
    Ok(())
}
