use crate::hardware_detection::get_hardware_capabilities;
use serde::Serialize;
use specta::Type;

#[derive(Debug, Clone, Serialize, Type)]
pub struct HardwareInfo {
    pub has_gpu: bool,
    pub cpu_cores: usize,
    pub recommended_threads: usize,
}

#[tauri::command]
#[specta::specta]
pub fn get_hardware_info() -> HardwareInfo {
    let caps = get_hardware_capabilities();
    HardwareInfo {
        has_gpu: caps.has_gpu,
        cpu_cores: caps.cpu_cores,
        recommended_threads: caps.recommended_threads,
    }
}
