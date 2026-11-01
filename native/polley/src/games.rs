//! "Jogando agora": acha o jogo aberto no PC pelo nome do programa (.exe).
//!
//! Só olhamos os NOMES dos programas abertos (a mesma lista do Gerenciador de Tarefas), nada
//! de dentro deles: é leve (uma olhada a cada 15 s) e não mexe no jogo — nenhum anticheat
//! reclama disso. Quem quiser pode esconder o que está jogando nas configurações.

use crate::settings::CustomGame;
use std::collections::{HashMap, HashSet};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindow, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, GW_OWNER,
};

/// Jogos conhecidos: nome do .exe (minúsculo) → nome bonito.
pub const KNOWN: &[(&str, &str)] = &[
    ("valorant-win64-shipping.exe", "VALORANT"),
    ("league of legends.exe", "League of Legends"),
    ("cs2.exe", "Counter-Strike 2"),
    ("csgo.exe", "Counter-Strike: Global Offensive"),
    ("fortniteclient-win64-shipping.exe", "Fortnite"),
    ("robloxplayerbeta.exe", "Roblox"),
    ("minecraft.windows.exe", "Minecraft"),
    ("gta5.exe", "Grand Theft Auto V"),
    ("gta5_enhanced.exe", "Grand Theft Auto V"),
    ("rocketleague.exe", "Rocket League"),
    ("dota2.exe", "Dota 2"),
    ("r5apex.exe", "Apex Legends"),
    ("r5apex_dx12.exe", "Apex Legends"),
    ("overwatch.exe", "Overwatch 2"),
    ("escapefromtarkov.exe", "Escape from Tarkov"),
    ("rainbowsix.exe", "Rainbow Six Siege"),
    ("rainbowsix_vulkan.exe", "Rainbow Six Siege"),
    ("tslgame.exe", "PUBG: BATTLEGROUNDS"),
    ("cod.exe", "Call of Duty"),
    ("fc24.exe", "EA SPORTS FC 24"),
    ("fc25.exe", "EA SPORTS FC 25"),
    ("fc26.exe", "EA SPORTS FC 26"),
    ("rustclient.exe", "Rust"),
    ("deadbydaylight-win64-shipping.exe", "Dead by Daylight"),
    ("palworld-win64-shipping.exe", "Palworld"),
    ("eldenring.exe", "ELDEN RING"),
    ("cyberpunk2077.exe", "Cyberpunk 2077"),
    ("rdr2.exe", "Red Dead Redemption 2"),
    ("witcher3.exe", "The Witcher 3"),
    ("bg3.exe", "Baldur's Gate 3"),
    ("bg3_dx11.exe", "Baldur's Gate 3"),
    ("terraria.exe", "Terraria"),
    ("stardew valley.exe", "Stardew Valley"),
    ("stardewvalley.exe", "Stardew Valley"),
    ("amongus.exe", "Among Us"),
    ("fallguys_client_game.exe", "Fall Guys"),
    ("destiny2.exe", "Destiny 2"),
    ("warframe.x64.exe", "Warframe"),
    ("monsterhunterwilds.exe", "Monster Hunter Wilds"),
    ("monsterhunterworld.exe", "Monster Hunter: World"),
    ("discovery.exe", "THE FINALS"),
    ("marvel-win64-shipping.exe", "Marvel Rivals"),
    ("deadlock.exe", "Deadlock"),
    ("lethal company.exe", "Lethal Company"),
    ("phasmophobia.exe", "Phasmophobia"),
    ("repo.exe", "R.E.P.O."),
    ("pathofexile.exe", "Path of Exile"),
    ("pathofexile_x64.exe", "Path of Exile"),
    ("pathofexile_x64steam.exe", "Path of Exile"),
    ("pathofexilesteam.exe", "Path of Exile 2"),
    ("hearthstone.exe", "Hearthstone"),
    ("diablo iv.exe", "Diablo IV"),
    ("wow.exe", "World of Warcraft"),
    ("brawlhalla.exe", "Brawlhalla"),
    ("sotgame.exe", "Sea of Thieves"),
    ("forzahorizon5.exe", "Forza Horizon 5"),
    ("genshinimpact.exe", "Genshin Impact"),
    ("yuanshen.exe", "Genshin Impact"),
    ("starrail.exe", "Honkai: Star Rail"),
    ("zenlesszonezero.exe", "Zenless Zone Zero"),
    ("helldivers2.exe", "HELLDIVERS 2"),
    ("sonsoftheforest.exe", "Sons Of The Forest"),
    ("valheim.exe", "Valheim"),
    ("rimworldwin64.exe", "RimWorld"),
    ("hollow knight.exe", "Hollow Knight"),
    ("cuphead.exe", "Cuphead"),
    ("geometrydash.exe", "Geometry Dash"),
    ("osu!.exe", "osu!"),
    ("pointblank.exe", "Point Blank"),
    ("crossfire.exe", "CrossFire"),
    ("albion-online.exe", "Albion Online"),
    ("lostark.exe", "Lost Ark"),
    ("ts4_x64.exe", "The Sims 4"),
    ("eurotrucks2.exe", "Euro Truck Simulator 2"),
    ("amtrucks.exe", "American Truck Simulator"),
    ("efootball.exe", "eFootball"),
    ("streetfighter6.exe", "Street Fighter 6"),
    ("mk12.exe", "Mortal Kombat 1"),
    ("polaris-win64-shipping.exe", "TEKKEN 8"),
    ("hogwartslegacy.exe", "Hogwarts Legacy"),
    ("sekiro.exe", "Sekiro"),
    ("darksoulsiii.exe", "Dark Souls III"),
    ("re4.exe", "Resident Evil 4"),
    ("left4dead2.exe", "Left 4 Dead 2"),
    ("tf_win64.exe", "Team Fortress 2"),
    ("gmod.exe", "Garry's Mod"),
    ("unturned.exe", "Unturned"),
    ("dyinglightgame_x64_rwdi.exe", "Dying Light 2"),
    ("ittakestwo.exe", "It Takes Two"),
    ("fsd-win64-shipping.exe", "Deep Rock Galactic"),
    ("readyornot-win64-shipping.exe", "Ready or Not"),
    ("squadgame.exe", "Squad"),
    ("bf2042.exe", "Battlefield 2042"),
    ("bf6.exe", "Battlefield 6"),
    ("aces.exe", "War Thunder"),
    ("worldoftanks.exe", "World of Tanks"),
];

/// Nomes (minúsculos) dos programas abertos agora.
pub fn running() -> HashSet<String> {
    processes().into_values().collect()
}

/// pid → nome do .exe (minúsculo).
fn processes() -> HashMap<u32, String> {
    let mut out = HashMap::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut entry = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
                out.insert(entry.th32ProcessID, String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase());
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// O jogo aberto agora (os que a pessoa adicionou têm prioridade).
pub fn detect(custom: &[CustomGame]) -> Option<String> {
    let open = running();
    custom
        .iter()
        .find(|g| open.contains(&g.exe.to_lowercase()))
        .map(|g| g.name.clone())
        .or_else(|| KNOWN.iter().find(|(exe, _)| open.contains(*exe)).map(|(_, name)| name.to_string()))
}

/// Programas com janela aberta (para "Adicionar jogo"): (nome do .exe, título da janela).
pub fn windowed_apps() -> Vec<(String, String)> {
    unsafe extern "system" fn each(w: HWND, data: LPARAM) -> windows::core::BOOL {
        let found = &mut *(data.0 as *mut Vec<(u32, String)>);
        let owner = GetWindow(w, GW_OWNER).unwrap_or_default();
        let len = GetWindowTextLengthW(w);
        if IsWindowVisible(w).as_bool() && owner.is_invalid() && len > 0 {
            let mut buf = vec![0u16; len as usize + 1];
            let n = GetWindowTextW(w, &mut buf);
            let mut pid = 0;
            GetWindowThreadProcessId(w, Some(&mut pid));
            found.push((pid, String::from_utf16_lossy(&buf[..n as usize])));
        }
        true.into()
    }
    let mut windows: Vec<(u32, String)> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut windows as *mut _ as isize));
    }
    let names = processes();
    let me = std::process::id();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (pid, title) in windows {
        let Some(exe) = names.get(&pid) else { continue };
        // Programas do Windows e o próprio Polley não são jogos.
        if pid == me || matches!(exe.as_str(), "explorer.exe" | "applicationframehost.exe" | "textinputhost.exe" | "systemsettings.exe") {
            continue;
        }
        if seen.insert(exe.clone()) {
            out.push((exe.clone(), title.chars().take(40).collect::<String>()));
        }
    }
    out.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
    out
}
