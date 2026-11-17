//! Names each disk, network adapter and GPU of a sample the same way after a
//! restart, for its per-minute history. Disk numbers and GPU LUIDs can change
//! when Windows starts, so disks go by model and GPUs by name (each with an
//! ordinal among equals); network adapters by their NET_LUID, which Windows
//! keeps. A device whose model or name cannot be read is not recorded,
//! rather than under a name that could later mean another device.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use wtm_core::SystemStats;

/// Most devices recorded per sample; further ones are left out.
pub const MAX_DEVICES: usize = 32;
/// A disk number not seen for this long is read again when it returns: it
/// may be another disk by then.
const DISK_FORGET: Duration = Duration::from_secs(60);
/// A disk or GPU whose name could not be read is tried again after this.
const RETRY: Duration = Duration::from_secs(600);

/// What kind of device a history belongs to; it says what its three
/// measures are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Active time (%), read and write (bytes per second).
    Disk,
    /// Received and sent (bytes per second).
    Network,
    /// Load (%), dedicated and shared memory in use (bytes).
    Gpu,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disk => "disk",
            Self::Network => "network",
            Self::Gpu => "gpu",
        }
    }
}

/// One device in one sample.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    /// The name kept across restarts, such as `disk:Samsung SSD 990 PRO 2TB#0`.
    pub key: String,
    pub kind: Kind,
    /// What the device is called, for the history's caption.
    pub label: String,
    pub values: [Option<f64>; 3],
}

/// Reads device names from Windows; a seam so tests never touch hardware.
pub trait Names {
    /// The model of `\\.\PhysicalDrive{number}`, without reading SMART data
    /// or anything else that could wake the disk.
    fn disk_model(&mut self, number: u32) -> Option<String>;
    /// Every hardware GPU as `(luid, name)`, in DXGI order. Enumerates the
    /// adapters only; no Direct3D device is created.
    fn gpus(&mut self) -> Vec<(String, String)>;
}

/// The names Windows gives.
pub struct Windows;

impl Names for Windows {
    fn disk_model(&mut self, number: u32) -> Option<String> {
        wtm_sys::physical_disk::read(number)
            .ok()
            .map(|d| d.model.trim().to_owned())
            .filter(|m| !m.is_empty())
    }

    fn gpus(&mut self) -> Vec<(String, String)> {
        wtm_sys::gpu::adapters()
            .unwrap_or_default()
            .into_iter()
            .map(|a| (a.luid, a.name.trim().to_owned()))
            .collect()
    }
}

/// Remembers the names it read, so a device is looked up once, not on
/// every sample.
pub struct Keys<N> {
    names: N,
    /// Disk number to model (`None`: unreadable), when it was read and
    /// last seen.
    disks: HashMap<u32, (Option<String>, Instant, Instant)>,
    /// `(luid, name)` in DXGI order, and when they were read.
    gpus: Option<(Vec<(String, String)>, Instant)>,
}

impl<N: Names> Keys<N> {
    pub fn new(names: N) -> Self {
        Self {
            names,
            disks: HashMap::new(),
            gpus: None,
        }
    }

    /// The devices of `stats` with their lasting names, and for each the
    /// id the window knows it by now (`disk:0`, `net:…`, `gpu:…`) mapped to
    /// that name.
    pub fn readings(
        &mut self,
        stats: &SystemStats,
        now: Instant,
    ) -> (Vec<Reading>, HashMap<String, String>) {
        let mut readings = Vec::new();
        let mut ids = HashMap::new();
        let mut add = |id: String, reading: Reading| {
            if readings.len() < MAX_DEVICES {
                ids.insert(id, reading.key.clone());
                readings.push(reading);
            }
        };

        self.disks
            .retain(|_, (_, _, seen)| now.saturating_duration_since(*seen) < DISK_FORGET);
        let mut disks: Vec<_> = stats
            .disks
            .iter()
            .filter_map(|d| Some((d.number()?, d)))
            .collect();
        disks.sort_by_key(|(n, _)| *n);
        let mut models: Vec<String> = Vec::new();
        for (number, disk) in disks {
            let stale = |(model, read, _): &(Option<String>, Instant, Instant)| {
                model.is_none() && now.saturating_duration_since(*read) >= RETRY
            };
            if self.disks.get(&number).is_none_or(stale) {
                let model = self.names.disk_model(number);
                self.disks.insert(number, (model, now, now));
            }
            let Some(entry) = self.disks.get_mut(&number) else {
                continue;
            };
            entry.2 = now;
            let Some(model) = entry.0.clone() else {
                continue;
            };
            let nth = models.iter().filter(|m| **m == model).count();
            models.push(model.clone());
            add(
                format!("disk:{number}"),
                Reading {
                    key: format!("disk:{model}#{nth}"),
                    kind: Kind::Disk,
                    label: model,
                    values: [
                        Some(f64::from(disk.active_percent)),
                        Some(disk.read_rate),
                        Some(disk.write_rate),
                    ],
                },
            );
        }

        for a in &stats.net_adapters {
            add(
                format!("net:{:016x}", a.luid),
                Reading {
                    key: format!("net:{:016x}", a.luid),
                    kind: Kind::Network,
                    label: a.name.clone(),
                    values: [Some(a.rx_rate), Some(a.tx_rate), None],
                },
            );
        }

        let known = |gpus: &Option<(Vec<(String, String)>, Instant)>, luid: &str| {
            gpus.as_ref()
                .is_some_and(|(list, _)| list.iter().any(|(l, _)| l == luid))
        };
        let unknown = stats.gpus.iter().any(|g| !known(&self.gpus, &g.luid));
        let due = self
            .gpus
            .as_ref()
            .is_none_or(|(_, read)| now.saturating_duration_since(*read) >= RETRY);
        // The GPUs are listed when the service starts and, while one stays
        // unknown (added later, or not a hardware GPU), every ten minutes.
        if unknown && due {
            self.gpus = Some((self.names.gpus(), now));
        }
        if let Some((list, _)) = &self.gpus {
            let mut names: Vec<&str> = Vec::new();
            let mut ordinals: HashMap<&str, usize> = HashMap::new();
            for (luid, name) in list {
                let nth = names.iter().filter(|n| **n == name).count();
                names.push(name);
                ordinals.insert(luid, nth);
            }
            let mut gpus: Vec<_> = stats
                .gpus
                .iter()
                .filter_map(|g| Some((list.iter().position(|(l, _)| *l == g.luid)?, g)))
                .collect();
            gpus.sort_by_key(|(i, _)| *i);
            for (index, g) in gpus {
                let name = &list[index].1;
                if name.is_empty() {
                    continue;
                }
                let nth = ordinals.get(g.luid.as_str()).copied().unwrap_or(0);
                add(
                    format!("gpu:{}", g.luid),
                    Reading {
                        key: format!("gpu:{name}#{nth}"),
                        kind: Kind::Gpu,
                        label: name.clone(),
                        values: [
                            Some(f64::from(g.percent)),
                            g.dedicated_used.map(|b| b as f64),
                            g.shared_used.map(|b| b as f64),
                        ],
                    },
                );
            }
        }
        (readings, ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wtm_core::NetAdapterUsage;
    use wtm_sys::disk::DiskUsage;
    use wtm_sys::gpu::GpuAdapterUsage;

    #[derive(Default)]
    struct Fake {
        models: HashMap<u32, String>,
        gpus: Vec<(String, String)>,
        disk_reads: Vec<u32>,
        gpu_reads: usize,
    }

    impl Names for &mut Fake {
        fn disk_model(&mut self, number: u32) -> Option<String> {
            self.disk_reads.push(number);
            self.models.get(&number).cloned()
        }
        fn gpus(&mut self) -> Vec<(String, String)> {
            self.gpu_reads += 1;
            self.gpus.clone()
        }
    }

    fn disk(name: &str, active: f32) -> DiskUsage {
        DiskUsage {
            name: name.into(),
            active_percent: active,
            read_rate: 1000.0,
            write_rate: 2000.0,
            ..DiskUsage::default()
        }
    }

    fn gpu(luid: &str, percent: f32) -> GpuAdapterUsage {
        GpuAdapterUsage {
            luid: luid.into(),
            percent,
            dedicated_used: Some(512),
            ..GpuAdapterUsage::default()
        }
    }

    fn stats(disks: Vec<DiskUsage>, gpus: Vec<GpuAdapterUsage>) -> SystemStats {
        SystemStats {
            disks,
            gpus,
            net_adapters: vec![NetAdapterUsage {
                luid: 0x0006_0000_0100_0000,
                name: "Ethernet".into(),
                rx_rate: 10.0,
                tx_rate: 20.0,
                ..NetAdapterUsage::default()
            }],
            ..SystemStats::default()
        }
    }

    #[test]
    fn devices_get_names_that_survive_a_restart() {
        let mut fake = Fake {
            models: HashMap::from([
                (0, "SSD A".into()),
                (1, "HDD B".into()),
                (2, "SSD A".into()),
            ]),
            gpus: vec![
                ("0000_0001".into(), "GPU X".into()),
                ("0000_0002".into(), "GPU X".into()),
            ],
            ..Fake::default()
        };
        let t = Instant::now();
        let s = stats(
            vec![disk("2 E:", 3.0), disk("0 C:", 1.0), disk("1 D:", 2.0)],
            vec![gpu("0000_0002", 7.0), gpu("0000_0001", 5.0)],
        );
        let (readings, ids) = Keys::new(&mut fake).readings(&s, t);
        let keys: Vec<_> = readings.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "disk:SSD A#0",
                "disk:HDD B#0",
                "disk:SSD A#1",
                "net:0006000001000000",
                "gpu:GPU X#0",
                "gpu:GPU X#1",
            ]
        );
        assert_eq!(ids["disk:2"], "disk:SSD A#1");
        assert_eq!(ids["gpu:0000_0002"], "gpu:GPU X#1");
        assert_eq!(ids["net:0006000001000000"], "net:0006000001000000");
        assert_eq!(readings[0].values, [Some(1.0), Some(1000.0), Some(2000.0)]);
        assert_eq!(readings[4].values, [Some(5.0), Some(512.0), None]);

        // After a restart the GPUs have other LUIDs: same names.
        fake.gpus = vec![
            ("0000_0009".into(), "GPU X".into()),
            ("0000_0008".into(), "GPU X".into()),
        ];
        let s = stats(vec![], vec![gpu("0000_0008", 1.0)]);
        let (readings, ids) = Keys::new(&mut fake).readings(&s, t);
        assert_eq!(readings[1].key, "gpu:GPU X#1");
        assert_eq!(ids["gpu:0000_0008"], "gpu:GPU X#1");
    }

    #[test]
    fn names_are_read_once_and_unreadable_devices_are_left_out() {
        let mut fake = Fake {
            models: HashMap::from([(0, "SSD A".into())]),
            gpus: vec![("0000_0001".into(), "GPU X".into())],
            ..Fake::default()
        };
        let t = Instant::now();
        let mut keys = Keys::new(&mut fake);
        // Disk 1 has no readable model; GPU 0000_0005 is not a hardware GPU.
        let s = stats(
            vec![disk("0 C:", 1.0), disk("1 D:", 1.0)],
            vec![gpu("0000_0001", 1.0), gpu("0000_0005", 1.0)],
        );
        for second in 0..30 {
            let (readings, _) = keys.readings(&s, t + Duration::from_secs(second));
            assert_eq!(readings.len(), 3, "disk 0, the adapter, GPU X");
        }
        // A disk missing from a few samples keeps its name.
        let only_c = stats(vec![disk("0 C:", 1.0)], vec![]);
        keys.readings(&only_c, t + Duration::from_secs(40));
        // Ten minutes on, the unreadable ones are tried again, once.
        for second in (50..=700).step_by(30) {
            keys.readings(&s, t + Duration::from_secs(second));
        }
        drop(keys);
        assert_eq!(fake.disk_reads, [0, 1, 1]);
        assert_eq!(fake.gpu_reads, 2);
    }

    #[test]
    fn a_disk_number_gone_for_a_minute_is_read_again() {
        let mut fake = Fake {
            models: HashMap::from([(1, "USB A".into())]),
            ..Fake::default()
        };
        let t = Instant::now();
        let mut keys = Keys::new(&mut fake);
        keys.readings(&stats(vec![disk("1 E:", 1.0)], vec![]), t);
        keys.readings(&stats(vec![], vec![]), t + Duration::from_secs(30));
        keys.readings(&stats(vec![], vec![]), t + Duration::from_secs(90));
        let (readings, _) = keys.readings(
            &stats(vec![disk("1 E:", 1.0)], vec![]),
            t + Duration::from_secs(91),
        );
        assert_eq!(readings[0].key, "disk:USB A#0");
        drop(keys);
        assert_eq!(fake.disk_reads, [1, 1]);
    }

    #[test]
    fn at_most_the_device_limit_is_recorded() {
        let mut fake = Fake::default();
        let mut s = stats(vec![], vec![]);
        s.net_adapters = (0..MAX_DEVICES as u64 + 5)
            .map(|luid| NetAdapterUsage {
                luid,
                ..NetAdapterUsage::default()
            })
            .collect();
        let (readings, ids) = Keys::new(&mut fake).readings(&s, Instant::now());
        assert_eq!(readings.len(), MAX_DEVICES);
        assert_eq!(ids.len(), MAX_DEVICES);
    }
}
