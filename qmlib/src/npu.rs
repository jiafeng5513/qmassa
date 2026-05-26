use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use log::{debug, info, warn};


/// Intel NPU device utilization reader.
/// Reads npu_busy_time_us from sysfs to calculate utilization percentage.
#[derive(Debug)]
pub struct NpuDevice
{
    pub pci_dev: String,
    busy_time_path: PathBuf,
    last_busy_us: u64,
    last_time: Instant,
    pub utilization: f64,
}

impl NpuDevice
{
    pub fn refresh(&mut self)
    {
        let busy_us = match fs::read_to_string(&self.busy_time_path) {
            Ok(s) => match s.trim().parse::<u64>() {
                Ok(v) => v,
                Err(e) => {
                    warn!("NPU {}: failed to parse npu_busy_time_us: {}",
                        &self.pci_dev, e);
                    return;
                }
            },
            Err(e) => {
                warn!("NPU {}: failed to read npu_busy_time_us: {}",
                    &self.pci_dev, e);
                return;
            }
        };

        let now = Instant::now();
        let elapsed_us = now.duration_since(self.last_time).as_micros() as f64;

        if elapsed_us > 0.0 {
            let delta_busy = busy_us.saturating_sub(self.last_busy_us) as f64;
            self.utilization = (delta_busy / elapsed_us) * 100.0;
            if self.utilization > 100.0 {
                self.utilization = 100.0;
            }
        }

        self.last_busy_us = busy_us;
        self.last_time = now;
    }

    fn from(pci_dev: String, busy_time_path: PathBuf) -> Option<NpuDevice>
    {
        let busy_us = match fs::read_to_string(&busy_time_path) {
            Ok(s) => match s.trim().parse::<u64>() {
                Ok(v) => v,
                Err(_) => return None,
            },
            Err(_) => return None,
        };

        Some(NpuDevice {
            pci_dev,
            busy_time_path,
            last_busy_us: busy_us,
            last_time: Instant::now(),
            utilization: 0.0,
        })
    }
}

/// Discover Intel NPU devices via the accel subsystem.
/// Returns the first found NPU device (typically only one per system).
pub fn find_npu_device() -> Option<NpuDevice>
{
    let accel_class = Path::new("/sys/class/accel");
    if !accel_class.is_dir() {
        debug!("No /sys/class/accel directory, NPU not available");
        return None;
    }

    let entries = match fs::read_dir(accel_class) {
        Ok(e) => e,
        Err(_) => return None,
    };

    for entry in entries.flatten() {
        let accel_path = entry.path();
        let device_path = accel_path.join("device");

        if !device_path.is_symlink() {
            continue;
        }

        // Resolve device path to get PCI slot
        let real_dev = match fs::canonicalize(&device_path) {
            Ok(p) => p,
            Err(_) => continue,
        };

        // Check driver is intel_vpu (ivpu)
        let driver_path = real_dev.join("driver");
        if let Ok(drv_link) = fs::read_link(&driver_path) {
            let drv_name = drv_link.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            if drv_name != "intel_vpu" && drv_name != "ivpu" {
                continue;
            }
        } else {
            continue;
        }

        // Check npu_busy_time_us exists
        let busy_time_path = real_dev.join("npu_busy_time_us");
        if !busy_time_path.exists() {
            debug!("NPU device found but npu_busy_time_us not available \
                (kernel >= 6.11 required)");
            continue;
        }

        // Get PCI slot name from the resolved path
        let pci_dev = real_dev.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        if let Some(npu) = NpuDevice::from(pci_dev.clone(), busy_time_path) {
            info!("Found Intel NPU at PCI slot {}", &npu.pci_dev);
            return Some(npu);
        }
    }

    None
}
