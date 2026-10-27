//! NVIDIA's management library (NVML), loaded at run time from the driver.
//! Without the driver `open` gives `None` and nothing is listed from it.
//! On Linux it is the source for NVIDIA cards; on Windows only its sensors
//! are laid over the counters' adapters.

use std::collections::HashMap;

use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};
use nvml_wrapper::enums::device::UsedGpuMemory;
use nvml_wrapper::error::NvmlError;
use nvml_wrapper::struct_wrappers::device::ProcessUtilizationSample;

use super::{Device, Fan, Field, GpuProc, Why};

pub struct Reader {
    nvml: Nvml,
    /// The newest process sample seen on each device (µs), so the next
    /// reading asks only for what came after.
    seen: HashMap<u32, u64>,
}

/// Each process's use since `since`: its busiest engine (SM, encoder or
/// decoder), averaged over the samples the driver took.
pub fn proc_usage(samples: &[ProcessUtilizationSample], since: u64) -> HashMap<u32, f32> {
    let mut sums: HashMap<u32, (u32, u32)> = HashMap::new();
    for s in samples.iter().filter(|s| s.timestamp > since) {
        let e = sums.entry(s.pid).or_default();
        e.0 += s.sm_util.max(s.enc_util).max(s.dec_util).min(100);
        e.1 += 1;
    }
    sums.into_iter().map(|(pid, (sum, n))| (pid, sum as f32 / n as f32)).collect()
}

impl Reader {
    pub fn open() -> Option<Self> {
        // The unversioned name is only there with the development package.
        #[cfg(target_os = "linux")]
        if let Ok(nvml) = Nvml::builder().lib_path(std::ffi::OsStr::new("libnvidia-ml.so.1")).init() {
            return Some(Self { nvml, seen: HashMap::new() });
        }
        Nvml::init().ok().map(|nvml| Self { nvml, seen: HashMap::new() })
    }

    /// Every device. `procs` also asks for each process's use (Linux; on
    /// Windows the counters give it).
    pub fn read(&mut self, procs: bool) -> Vec<Device> {
        let count = self.nvml.device_count().unwrap_or(0);
        let mut out = Vec::new();
        for i in 0..count {
            let Ok(dev) = self.nvml.device_by_index(i) else { continue };
            let mut d = Device {
                id: format!("nvml:{i}"),
                name: dev.name().unwrap_or_default(),
                vendor: Some("NVIDIA"),
                driver: self.nvml.sys_driver_version().ok().map(|v| format!("nvidia {v}")),
                source: "nvml",
                ..Device::default()
            };
            if let Ok(pci) = dev.pci_info() {
                let id = pci.pci_device_id;
                d.pci = Some(format!("{:04x}:{:04x}", id & 0xffff, id >> 16));
                d.bus = Some(super::bus_addr(&pci.bus_id));
            }
            match dev.utilization_rates() {
                Ok(u) => {
                    d.usage = Some(u.gpu.min(100) as f32);
                    d.engines.push(("Graphics".into(), u.gpu.min(100) as f32));
                    d.engines.push(("Memory".into(), u.memory.min(100) as f32));
                }
                Err(_) => d.why.push((Field::Usage, Why::NoSensor)),
            }
            if let Ok(e) = dev.encoder_utilization() {
                d.engines.push(("Encode".into(), e.utilization.min(100) as f32));
            }
            if let Ok(e) = dev.decoder_utilization() {
                d.engines.push(("Decode".into(), e.utilization.min(100) as f32));
            }
            match dev.memory_info() {
                Ok(m) => (d.vram_used, d.vram_total) = (Some(m.used), Some(m.total)),
                Err(_) => d.why.push((Field::Vram, Why::NoVram)),
            }
            d.temp = dev.temperature(TemperatureSensor::Gpu).ok().map(|t| t as f32);
            d.fan = dev.fan_speed(0).ok().map(Fan::Percent);
            d.power = dev.power_usage().ok().map(|mw| mw as f32 / 1000.0);
            d.clock = dev.clock_info(Clock::Graphics).ok();
            for (f, have) in [(Field::Temp, d.temp.is_some()), (Field::Power, d.power.is_some()), (Field::Clock, d.clock.is_some())] {
                if !have {
                    d.why.push((f, Why::NoSensor));
                }
            }
            let want = procs;
            let mut procs: Vec<GpuProc> = Vec::new();
            let lists = if want { vec![dev.running_graphics_processes(), dev.running_compute_processes()] } else { Vec::new() };
            for p in lists.into_iter().flatten().flatten() {
                let mem = match p.used_gpu_memory {
                    UsedGpuMemory::Used(b) => Some(b),
                    UsedGpuMemory::Unavailable => None,
                };
                match procs.iter_mut().find(|q| q.pid == p.pid) {
                    Some(q) => q.mem = q.mem.max(mem),
                    None => procs.push(GpuProc { pid: p.pid, usage: None, mem }),
                }
            }
            if want && !procs.is_empty() {
                let since = self.seen.get(&i).copied();
                // No samples since then means nobody was busy.
                let samples = match dev.process_utilization_stats(since) {
                    Err(NvmlError::NotFound) => Ok(Vec::new()),
                    r => r,
                };
                if let Ok(samples) = samples {
                    let usage = proc_usage(&samples, since.unwrap_or(0));
                    for p in &mut procs {
                        p.usage = Some(usage.get(&p.pid).copied().unwrap_or(0.0));
                    }
                    if let Some(t) = samples.iter().map(|s| s.timestamp).max() {
                        self.seen.insert(i, t.max(since.unwrap_or(0)));
                    }
                }
            }
            procs.sort_by(|a, b| b.usage.unwrap_or(0.0).total_cmp(&a.usage.unwrap_or(0.0)).then(b.mem.cmp(&a.mem)).then(a.pid.cmp(&b.pid)));
            d.procs = procs;
            out.push(d);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(pid: u32, timestamp: u64, sm: u32, enc: u32) -> ProcessUtilizationSample {
        ProcessUtilizationSample { pid, timestamp, sm_util: sm, mem_util: 0, enc_util: enc, dec_util: 0 }
    }

    #[test]
    fn process_samples_average_their_busiest_engine() {
        let s = [sample(10, 5, 90, 0), sample(10, 20, 30, 0), sample(10, 30, 50, 0), sample(20, 30, 0, 70), sample(30, 40, 255, 0)];
        let u = proc_usage(&s, 10);
        // The sample from before `since` is left out.
        assert_eq!(u.get(&10), Some(&40.0));
        assert_eq!(u.get(&20), Some(&70.0));
        assert_eq!(u.get(&30), Some(&100.0));
        assert!(proc_usage(&s, 40).is_empty());
    }
}
