//! Ícone na bandeja (perto do relógio), "só um Polley aberto" e "iniciar com o Windows".
//!
//! - Fechar a janela (X) deixa o Polley rodando na bandeja, igual ao Discord: continua
//!   recebendo mensagens e ligações. Clique no ícone = abre; botão direito = menu com "Sair".
//! - Abrir o Polley de novo (atalho, menu Iniciar) só traz a janela que já está aberta.
//! - "Iniciar com o Windows" = um valor em HKCU\...\Run (sem precisar de administrador).

use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::OnceLock;
use windows::Win32::Graphics::Gdi::CreateBitmap;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
use windows::core::{w, BOOL, HSTRING, PCWSTR};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ};
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcessId};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// O que a pessoa pediu pelo ícone.
pub enum Action {
    Open,
    Quit,
}

const WM_TRAY: u32 = WM_APP + 1;
/// Outro Polley foi aberto: ele manda isso e fecha; nós mostramos a janela.
const WM_SHOW: u32 = WM_APP + 2;
const ID_OPEN: usize = 1;
const ID_QUIT: usize = 2;

static HANDLER: OnceLock<Box<dyn Fn(Action) + Send + Sync>> = OnceLock::new();
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();
/// A janela invisível do ícone (para os avisos do Windows).
static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);

/// Nome do perfil (`--perfil 2`): cada perfil tem seu próprio "só um aberto".
fn profile() -> String {
    let mut args = std::env::args().skip_while(|a| a != "--perfil").skip(1);
    args.next().map(|p| p.replace(['/', '\\', '.'], "")).unwrap_or_default()
}

fn class_name() -> HSTRING {
    HSTRING::from(format!("PolleyTray{}", profile()))
}

/// Garante um Polley por perfil. Se já existe um aberto, pede para ele aparecer e devolve `false`
/// (este aqui deve fechar).
pub fn single_instance() -> bool {
    let name = HSTRING::from(format!("Local\\Polley-{}", profile()));
    unsafe {
        if CreateMutexW(None, false, &name).is_err() {
            return true;
        }
        if GetLastError() != ERROR_ALREADY_EXISTS {
            // A trava (o HANDLE) fica aberta até o processo acabar: é o que avisa "já tem um".
            return true;
        }
        if let Ok(hwnd) = FindWindowW(&class_name(), PCWSTR::null()) {
            // Deixa o outro Polley vir para a frente (o Windows bloqueia isso por padrão).
            let _ = AllowSetForegroundWindow(ASFW_ANY);
            let _ = PostMessageW(Some(hwnd), WM_SHOW, WPARAM(0), LPARAM(0));
        }
        false
    }
}

/// Cria o ícone. Precisa ser chamado na thread da interface (as mensagens chegam por ela).
pub fn install(handler: impl Fn(Action) + Send + Sync + 'static) {
    let _ = HANDLER.set(Box::new(handler));
    unsafe {
        let Ok(module) = GetModuleHandleW(PCWSTR::null()) else { return };
        let class = class_name();
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: module.into(), lpszClassName: PCWSTR(class.as_ptr()), ..Default::default() };
        RegisterClassW(&wc);
        // Janela invisível só para receber os cliques do ícone.
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            &class,
            w!("Polley"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(module.into()),
            None,
        ) else {
            return;
        };
        let _ = TASKBAR_CREATED.set(RegisterWindowMessageW(w!("TaskbarCreated")));
        TRAY_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
        add_icon(hwnd);
    }
}

unsafe fn add_icon(hwnd: HWND) {
    let module = GetModuleHandleW(PCWSTR::null()).ok();
    // Ícone 1 = o do .exe (embutido pelo build.rs).
    let icon = LoadImageW(
        module.map(Into::into),
        PCWSTR(1 as *const u16),
        IMAGE_ICON,
        GetSystemMetrics(SM_CXSMICON),
        GetSystemMetrics(SM_CYSMICON),
        LR_DEFAULTCOLOR,
    )
    .map(|h| HICON(h.0))
    .unwrap_or_else(|_| LoadIconW(None, IDI_APPLICATION).unwrap_or_default());
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: icon,
        ..Default::default()
    };
    for (i, c) in "Polley".encode_utf16().enumerate() {
        data.szTip[i] = c;
    }
    let _ = Shell_NotifyIconW(NIM_ADD, &data);
}

unsafe fn remove_icon(hwnd: HWND) {
    let data = NOTIFYICONDATAW { cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: 1, ..Default::default() };
    let _ = Shell_NotifyIconW(NIM_DELETE, &data);
}

fn fire(action: Action) {
    if let Some(h) = HANDLER.get() {
        h(action);
    }
}

unsafe fn show_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else { return };
    let _ = AppendMenuW(menu, MF_STRING, ID_OPEN, w!("Abrir o Polley"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, ID_QUIT, w!("Sair do Polley"));
    let _ = SetMenuDefaultItem(menu, ID_OPEN as u32, 0);
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // Sem isso o menu não fecha ao clicar fora (regra antiga do Windows).
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, None, hwnd, None);
    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);
    match cmd.0 as usize {
        ID_OPEN => fire(Action::Open),
        ID_QUIT => {
            remove_icon(hwnd);
            fire(Action::Quit);
        }
        _ => {}
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            match lparam.0 as u32 {
                // Clicar no ícone ou no aviso do Windows abre o Polley.
                WM_LBUTTONUP | WM_LBUTTONDBLCLK | NIN_BALLOONUSERCLICK => fire(Action::Open),
                WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        WM_SHOW => {
            fire(Action::Open);
            LRESULT(0)
        }
        WM_DESTROY => {
            remove_icon(hwnd);
            LRESULT(0)
        }
        // O Explorer reiniciou (travou, atualizou): o ícone some e precisa ser posto de novo.
        m if Some(&m) == TASKBAR_CREATED.get() => {
            add_icon(hwnd);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Tira o ícone ao sair (senão ele fica "fantasma" até passar o mouse).
pub fn remove() {
    unsafe {
        if let Ok(hwnd) = FindWindowW(&class_name(), PCWSTR::null()) {
            remove_icon(hwnd);
        }
    }
}

/// Aviso do Windows (aparece no canto da tela, como os do Discord). `text` = começo da mensagem.
pub fn popup(title: &str, text: &str) {
    let hwnd = TRAY_HWND.load(Ordering::Relaxed);
    if hwnd == 0 {
        return;
    }
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: HWND(hwnd as *mut _),
        uID: 1,
        uFlags: NIF_INFO,
        // Respeita o "Não perturbe" / jogo em tela cheia do Windows.
        dwInfoFlags: NIIF_NONE | NIIF_RESPECT_QUIET_TIME,
        ..Default::default()
    };
    for (i, c) in title.encode_utf16().take(data.szInfoTitle.len() - 1).enumerate() {
        data.szInfoTitle[i] = c;
    }
    let text = if text.trim().is_empty() { "📎 Anexo" } else { text };
    for (i, c) in text.encode_utf16().take(data.szInfo.len() - 1).enumerate() {
        data.szInfo[i] = c;
    }
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
    }
}

thread_local! {
    static TASKBAR: RefCell<Option<(ITaskbarList3, HICON)>> = const { RefCell::new(None) };
}

/// Bolinha vermelha em cima do ícone do Polley na barra de tarefas (mensagens não lidas).
/// Chamar na thread da interface.
pub fn set_badge(on: bool) {
    let Some(hwnd) = main_window() else { return };
    TASKBAR.with(|t| {
        let mut t = t.borrow_mut();
        if t.is_none() {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                if let Ok(list) = CoCreateInstance::<_, ITaskbarList3>(&TaskbarList, None, CLSCTX_INPROC_SERVER) {
                    if list.HrInit().is_ok() {
                        *t = Some((list, red_dot()));
                    }
                }
            }
        }
        let Some((list, dot)) = t.as_ref() else { return };
        unsafe {
            let _ = if on {
                list.SetOverlayIcon(hwnd, *dot, w!("Mensagens não lidas"))
            } else {
                list.SetOverlayIcon(hwnd, HICON::default(), PCWSTR::null())
            };
        }
    });
}

/// Ícone 16x16: bolinha vermelha com borda escura (desenhada aqui, sem arquivo).
unsafe fn red_dot() -> HICON {
    const S: i32 = 16;
    let mut px = vec![0u32; (S * S) as usize];
    for y in 0..S {
        for x in 0..S {
            let d = (((x as f32 + 0.5) - 8.0).powi(2) + ((y as f32 + 0.5) - 8.0).powi(2)).sqrt();
            let a = ((7.5 - d).clamp(0.0, 1.0) * 255.0) as u32;
            // Vermelho do Discord por dentro, borda escura para aparecer em qualquer fundo.
            let rgb = if d < 5.8 { 0xF2_3F_42 } else { 0x1E_1F_22 };
            px[(y * S + x) as usize] = (a << 24) | rgb;
        }
    }
    let color = CreateBitmap(S, S, 1, 32, Some(px.as_ptr() as *const _));
    let mask_bits = vec![0u8; (S * S / 8) as usize];
    let mask = CreateBitmap(S, S, 1, 1, Some(mask_bits.as_ptr() as *const _));
    let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
    CreateIconIndirect(&info).unwrap_or_default()
}

/// A janela principal do Polley (a visível deste processo).
pub fn main_window() -> Option<HWND> {
    unsafe extern "system" fn each(w: HWND, data: LPARAM) -> BOOL {
        let found = &mut *(data.0 as *mut Option<HWND>);
        let mut pid = 0;
        GetWindowThreadProcessId(w, Some(&mut pid));
        let owner = GetWindow(w, GW_OWNER).unwrap_or_default();
        if pid == GetCurrentProcessId() && IsWindowVisible(w).as_bool() && owner.is_invalid() {
            *found = Some(w);
            return false.into();
        }
        true.into()
    }
    let mut found: Option<HWND> = None;
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut found as *mut _ as isize));
    }
    found
}

/// Traz a janela do Polley para a frente (desminimiza se precisar).
pub fn bring_to_front() {
    unsafe {
        if let Some(w) = main_window() {
            if IsIconic(w).as_bool() {
                let _ = ShowWindow(w, SW_RESTORE);
            }
            let _ = SetForegroundWindow(w);
        }
    }
}

// ------------------------------------------------------------ iniciar com o Windows

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("Polley");

/// Só o Polley instalado entra no "iniciar com o Windows" (não a versão de teste nem outro perfil).
pub fn can_autostart() -> bool {
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().to_lowercase()).unwrap_or_default();
    profile().is_empty() && !exe.contains("\\target\\")
}

pub fn autostart_enabled() -> bool {
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None, None, None) == ERROR_SUCCESS }
}

pub fn set_autostart(on: bool) {
    if !can_autostart() {
        return;
    }
    unsafe {
        if on {
            let Ok(exe) = std::env::current_exe() else { return };
            // "--auto": sabemos que foi o Windows que abriu (para poder começar só na bandeja).
            let cmd: Vec<u16> = format!("\"{}\" --auto", exe.display()).encode_utf16().chain([0]).collect();
            let _ = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                RUN_VALUE,
                REG_SZ.0,
                Some(cmd.as_ptr() as *const _),
                (cmd.len() * 2) as u32,
            );
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
        }
    }
}
