//! Machine description and load measurements. Measurements only: nothing here limits a runner.

use std::ffi::CString;
use std::path::Path;

use sysinfo::{Disks, System};

use crate::model::{LoadSample, MachineInfo, MemoryPressure};

pub struct LoadSampler {
    system: System,
    disks: Disks,
}

impl Default for LoadSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl LoadSampler {
    pub fn new() -> Self {
        let mut system = System::new();
        system.refresh_cpu_usage();
        system.refresh_memory();
        LoadSampler { system, disks: Disks::new_with_refreshed_list() }
    }

    /// CPU usage is measured since the previous call; call it every few seconds.
    pub fn sample(&mut self, now_ms: u64) -> LoadSample {
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        self.disks.refresh(true);
        LoadSample {
            sampled_at_ms: now_ms,
            measured_cpu_percent: f64::from(self.system.global_cpu_usage()),
            measured_memory_used_bytes: self.system.used_memory(),
            measured_memory_total_bytes: self.system.total_memory(),
            measured_memory_pressure: memory_pressure(),
            measured_disk_free_bytes: self.disk_free(),
        }
    }

    fn disk_free(&self) -> u64 {
        self.disks
            .list()
            .iter()
            .filter(|d| d.mount_point() == Path::new("/") || d.mount_point() == Path::new("/System/Volumes/Data"))
            .map(|d| d.available_space())
            .max()
            .unwrap_or(0)
    }

    pub fn machine_info(&mut self) -> MachineInfo {
        self.system.refresh_memory();
        self.disks.refresh(true);
        MachineInfo {
            name: System::host_name().unwrap_or_else(|| "This machine".into()),
            model: sysctl_string("hw.model").unwrap_or_else(|| "Unknown model".into()),
            // Apple's name for the architecture, as `uname -m` prints it.
            architecture: match std::env::consts::ARCH {
                "aarch64" if cfg!(target_os = "macos") => "arm64".to_string(),
                other => other.to_string(),
            },
            operating_system: System::long_os_version().unwrap_or_else(|| std::env::consts::OS.into()),
            cpu_cores: std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1),
            memory_bytes: self.system.total_memory(),
            disk_free_bytes: self.disk_free(),
        }
    }
}

/// macOS memory pressure level (`kern.memorystatus_vm_pressure_level`: 1 normal, 2 warning,
/// 4 critical). `None` elsewhere or if it cannot be read.
pub fn memory_pressure() -> Option<MemoryPressure> {
    match sysctl_int("kern.memorystatus_vm_pressure_level")? {
        1 => Some(MemoryPressure::Normal),
        2 => Some(MemoryPressure::Warning),
        4 => Some(MemoryPressure::Critical),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn sysctl_int(name: &str) -> Option<i32> {
    let name = CString::new(name).ok()?;
    let mut value: i32 = 0;
    let mut size = std::mem::size_of::<i32>();
    let result = unsafe {
        libc::sysctlbyname(name.as_ptr(), (&mut value as *mut i32).cast(), &mut size, std::ptr::null_mut(), 0)
    };
    (result == 0).then_some(value)
}

#[cfg(target_os = "macos")]
fn sysctl_string(name: &str) -> Option<String> {
    let name = CString::new(name).ok()?;
    let mut size = 0usize;
    unsafe {
        if libc::sysctlbyname(name.as_ptr(), std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0) != 0 || size == 0
        {
            return None;
        }
        let mut buffer = vec![0u8; size];
        if libc::sysctlbyname(name.as_ptr(), buffer.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) != 0 {
            return None;
        }
        buffer.truncate(size);
        while buffer.last() == Some(&0) {
            buffer.pop();
        }
        String::from_utf8(buffer).ok()
    }
}

#[cfg(not(target_os = "macos"))]
fn sysctl_int(_name: &str) -> Option<i32> {
    None
}

#[cfg(not(target_os = "macos"))]
fn sysctl_string(_name: &str) -> Option<String> {
    let _ = CString::new("");
    None
}
