use log::{debug, info};
use std::sync::OnceLock;

/// Hardware capabilities detected at startup
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HardwareCapabilities {
    pub has_gpu: bool,
    pub cpu_cores: usize,
    pub recommended_threads: usize,
}

static HARDWARE_CAPS: OnceLock<HardwareCapabilities> = OnceLock::new();

/// Detect if GPU acceleration is available for transcription
fn detect_gpu() -> bool {
    // Check for CUDA (NVIDIA)
    #[cfg(target_os = "windows")]
    {
        if std::process::Command::new("nvidia-smi")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            debug!("NVIDIA GPU detected via nvidia-smi");
            return true;
        }
    }

    #[cfg(target_os = "linux")]
    {
        // Check for NVIDIA GPU
        if std::path::Path::new("/proc/driver/nvidia/version").exists() {
            debug!("NVIDIA GPU detected via /proc/driver/nvidia");
            return true;
        }

        // Check for AMD GPU
        if std::path::Path::new("/sys/class/drm/card0/device/vendor").exists() {
            if let Ok(vendor) = std::fs::read_to_string("/sys/class/drm/card0/device/vendor") {
                if vendor.trim() == "0x1002" {
                    debug!("AMD GPU detected");
                    return true;
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        // On macOS, check for Metal support (all modern Macs have it)
        // M1/M2/M3 Macs have excellent GPU acceleration
        use std::process::Command;
        if let Ok(output) = Command::new("system_profiler")
            .args(["SPDisplaysDataType", "-json"])
            .output()
        {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if stdout.contains("Metal") || stdout.contains("sppci_model") {
                    debug!("macOS GPU detected (Metal support)");
                    return true;
                }
            }
        }

        // Fallback: assume Metal support on modern macOS
        debug!("Assuming Metal GPU support on macOS");
        return true;
    }

    debug!("No GPU detected");
    false
}

/// Get the number of CPU cores available
fn get_cpu_cores() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// Calculate recommended number of threads for transcription
fn calculate_recommended_threads(cpu_cores: usize, has_gpu: bool) -> usize {
    if has_gpu {
        // With GPU, use fewer CPU threads (GPU does the heavy lifting)
        (cpu_cores / 2).max(2)
    } else {
        // Without GPU, use more CPU threads but leave some for system
        // Use 75% of available cores, minimum 2, maximum 8
        let threads = (cpu_cores * 3 / 4).max(2).min(8);
        info!(
            "CPU-only mode: using {} threads (out of {} cores)",
            threads, cpu_cores
        );
        threads
    }
}

/// Detect hardware capabilities at startup
pub fn detect_hardware() -> HardwareCapabilities {
    let has_gpu = detect_gpu();
    let cpu_cores = get_cpu_cores();
    let recommended_threads = calculate_recommended_threads(cpu_cores, has_gpu);

    let caps = HardwareCapabilities {
        has_gpu,
        cpu_cores,
        recommended_threads,
    };

    info!(
        "Hardware detected: GPU={}, CPU cores={}, recommended threads={}",
        caps.has_gpu, caps.cpu_cores, caps.recommended_threads
    );

    caps
}

/// Get cached hardware capabilities (detect once at startup)
pub fn get_hardware_capabilities() -> HardwareCapabilities {
    *HARDWARE_CAPS.get_or_init(detect_hardware)
}

/// Force re-detection of hardware capabilities (useful for testing)
#[cfg(test)]
pub fn reset_hardware_detection() {
    // Can't reset OnceLock in stable Rust, but this is only for tests
    // In practice, detection happens once at startup
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cpu_cores_detection() {
        let cores = get_cpu_cores();
        assert!(cores >= 1, "Should detect at least 1 CPU core");
        assert!(cores <= 256, "Sanity check: unrealistic core count");
    }

    #[test]
    fn test_thread_calculation_with_gpu() {
        let threads = calculate_recommended_threads(8, true);
        assert!(threads >= 2 && threads <= 4);
    }

    #[test]
    fn test_thread_calculation_without_gpu() {
        let threads = calculate_recommended_threads(8, false);
        assert!(threads >= 4 && threads <= 8);
    }

    #[test]
    fn test_thread_calculation_low_cores() {
        let threads = calculate_recommended_threads(2, false);
        assert_eq!(threads, 2, "Should use minimum 2 threads");
    }

    #[test]
    fn test_hardware_detection() {
        let caps = detect_hardware();
        assert!(caps.cpu_cores >= 1);
        assert!(caps.recommended_threads >= 2);
        assert!(caps.recommended_threads <= caps.cpu_cores);
    }
}
