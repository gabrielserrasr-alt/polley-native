//! Mede a RAM do próprio processo.

use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX2};
use windows::Win32::System::Threading::GetCurrentProcess;

pub struct Mem {
    /// "Conjunto de trabalho privado": é o número da coluna "Memória" do Gerenciador de Tarefas.
    pub task_manager_mb: f64,
    /// Memória privada confirmada (inclui o que foi reservado mas está parado/paginado).
    pub commit_mb: f64,
}

pub fn current() -> Mem {
    let mut c = PROCESS_MEMORY_COUNTERS_EX2 { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32, ..Default::default() };
    unsafe {
        let _ = GetProcessMemoryInfo(GetCurrentProcess(), &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS, c.cb);
    }
    let mb = |b: usize| b as f64 / 1024.0 / 1024.0;
    Mem { task_manager_mb: mb(c.PrivateWorkingSetSize), commit_mb: mb(c.PrivateUsage) }
}
