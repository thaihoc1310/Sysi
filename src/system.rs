use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Instant,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SystemReadOptions {
    pub processes: bool,
    pub cores: bool,
    pub gpus: bool,
    pub cpu_temp: bool,
    pub gpu_temp: bool,
    pub ssd_temp: bool,
    pub root_disk: bool,
    pub home_disk: bool,
    pub network: bool,
}

/// How much of something is in use, in KiB. Both halves are kept rather than
/// only the percentage they work out to, because "312G of 476G" is what tells
/// the user whether there is room for one more thing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub used_kib: u64,
    pub total_kib: u64,
}

impl Usage {
    pub fn percent(self) -> f64 {
        if self.total_kib == 0 {
            return 0.0;
        }
        self.used_kib.min(self.total_kib) as f64 * 100.0 / self.total_kib as f64
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct NetworkRates {
    pub down_bytes_per_sec: f64,
    pub up_bytes_per_sec: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GpuSnapshot {
    pub label: String,
    /// `None` on a card whose driver answers for its temperature but not its
    /// load, which is all a passthrough card or an older AMD driver offers.
    pub percent: Option<f64>,
    pub temperature: Option<f64>,
}

#[derive(Clone, Debug, Default)]
pub struct SystemSnapshot {
    pub cpu_percent: f64,
    pub memory_percent: f64,
    pub memory: Usage,
    /// `None` on a machine with no swap configured at all.
    pub swap: Option<Usage>,
    pub gpus: Vec<GpuSnapshot>,
    pub cpu_temperature: Option<f64>,
    /// Drive temperatures, labelled the way they are captioned: "SSD", or
    /// "SSD 1" and "SSD 2" once the machine has more than one.
    pub storage_temperatures: Vec<(String, f64)>,
    pub root_disk: Option<Usage>,
    pub home_disk: Option<Usage>,
    pub cores: Vec<f64>,
    pub processes: Vec<ProcessSnapshot>,
    /// `None` until a second sample exists, since a rate needs two counters.
    pub network: Option<NetworkRates>,
}

#[derive(Clone, Debug, Default)]
pub struct ProcessSnapshot {
    pub name: String,
    pub pid: u32,
    pub cpu_percent: f64,
    pub memory_kib: u64,
}

#[derive(Default)]
pub struct SystemReader {
    previous_total: u64,
    previous_idle: u64,
    previous_cores: Vec<(u64, u64)>,
    previous_process_ticks: HashMap<u32, u64>,
    /// Set once `nvidia-smi` turns out not to be installed. Without it a
    /// machine with no NVIDIA driver pays for spawning a missing process every
    /// two seconds for as long as the GPU meters are on.
    nvidia_missing: bool,
    /// The last interface counters and when they were read, which is what a
    /// throughput rate is measured against.
    previous_network: Option<(Instant, u64, u64)>,
    /// Where the CPU package temperature is read from, resolved on first use.
    /// The outer `None` means the search has not run yet; the inner one means
    /// it ran and this machine exposes no such sensor.
    cpu_temp_path: Option<Option<PathBuf>>,
    /// The drive sensors and the caption each one earned, resolved on first
    /// use. Kept for the same reason as `cpu_temp_path`, and for one more: a
    /// caption worked out afresh every two seconds would renumber a pair of
    /// same-vendor drives the moment one of their sensors missed a read.
    /// `None` means the search has not run, and an empty list means it ran and
    /// found nothing worth watching.
    drive_sensors: Option<Vec<DriveSensor>>,
}

/// One drive's temperature sensor: where to read it, and what to call it.
#[derive(Clone, Debug)]
struct DriveSensor {
    label: String,
    path: PathBuf,
}

impl SystemReader {
    pub fn read(&mut self, options: SystemReadOptions) -> SystemSnapshot {
        let cpu_lines = read_cpu_lines().unwrap_or_default();
        let (total, idle) = cpu_lines.first().copied().unwrap_or((0, 0));
        let delta_total = total.saturating_sub(self.previous_total);
        let delta_idle = idle.saturating_sub(self.previous_idle);
        let cpu_percent = if self.previous_total == 0 || delta_total == 0 {
            0.0
        } else {
            (delta_total.saturating_sub(delta_idle)) as f64 * 100.0 / delta_total as f64
        };
        self.previous_total = total;
        self.previous_idle = idle;

        let memory_info = read_memory().unwrap_or_default();
        let memory_percent = memory_info.memory.percent();
        let cores = if options.cores {
            let mut result = Vec::with_capacity(cpu_lines.len().saturating_sub(1));
            for (index, (core_total, core_idle)) in cpu_lines.iter().skip(1).copied().enumerate() {
                let (previous_total, previous_idle) = self
                    .previous_cores
                    .get(index)
                    .copied()
                    .unwrap_or((core_total, core_idle));
                let delta_total = core_total.saturating_sub(previous_total);
                let delta_idle = core_idle.saturating_sub(previous_idle);
                let percent = if delta_total == 0 {
                    0.0
                } else {
                    delta_total.saturating_sub(delta_idle) as f64 * 100.0 / delta_total as f64
                };
                result.push(percent);
            }
            self.previous_cores = cpu_lines.iter().skip(1).copied().collect();
            result
        } else {
            self.previous_cores.clear();
            Vec::new()
        };
        let processes = if options.processes {
            self.read_processes(delta_total)
        } else {
            self.previous_process_ticks.clear();
            Vec::new()
        };

        // The temperature of a card is read from the same place its load is,
        // so the GPU readers run for either meter.
        let gpus = if options.gpus || options.gpu_temp {
            self.read_gpus()
        } else {
            Vec::new()
        };
        let cpu_temperature = if options.cpu_temp {
            self.read_cpu_temperature()
        } else {
            None
        };
        let storage_temperatures = if options.ssd_temp {
            self.read_storage_temperatures()
        } else {
            Vec::new()
        };
        let network = if options.network {
            self.read_network()
        } else {
            // Stale counters would make the first rate after switching the row
            // back on cover the whole time it was off.
            self.previous_network = None;
            None
        };

        SystemSnapshot {
            cpu_percent,
            memory_percent,
            memory: memory_info.memory,
            swap: memory_info.swap,
            gpus,
            cpu_temperature,
            storage_temperatures,
            root_disk: options.root_disk.then(|| read_disk_usage("/")).flatten(),
            home_disk: options.home_disk.then(read_home_disk).flatten(),
            cores,
            processes,
            network,
        }
    }

    fn read_cpu_temperature(&mut self) -> Option<f64> {
        // Walking every hwmon chip to find the CPU is a directory scan and a
        // handful of reads, and the answer cannot change while the machine is
        // up. Pay for it once rather than every two seconds.
        let path = self
            .cpu_temp_path
            .get_or_insert_with(find_cpu_temperature_path)
            .clone()?;
        read_millidegrees(&path)
    }

    fn read_storage_temperatures(&mut self) -> Vec<(String, f64)> {
        // Cheap enough to retry while nothing has turned up: a machine with no
        // drive sensor at all is also a machine where this walk finds nothing
        // to open. Once a sensor exists it is remembered, so each sample after
        // that is one file read per drive.
        let sensors = match &self.drive_sensors {
            Some(sensors) if !sensors.is_empty() => sensors,
            _ => self.drive_sensors.insert(find_drive_sensors()),
        };
        sensors
            .iter()
            .filter_map(|sensor| {
                read_millidegrees(&sensor.path).map(|value| (sensor.label.clone(), value))
            })
            .collect()
    }

    fn read_network(&mut self) -> Option<NetworkRates> {
        let (received, transmitted) = read_network_counters()?;
        let now = Instant::now();
        let (then, previous_received, previous_transmitted) =
            self.previous_network
                .replace((now, received, transmitted))?;
        let elapsed = now.saturating_duration_since(then).as_secs_f64();
        // Two readings from the same instant say nothing about a rate.
        if elapsed <= 0.0 {
            return None;
        }
        Some(network_rates(
            (previous_received, previous_transmitted),
            (received, transmitted),
            elapsed,
        ))
    }

    fn read_processes(&mut self, total_delta: u64) -> Vec<ProcessSnapshot> {
        let mut next_ticks = HashMap::new();
        let mut processes = Vec::new();
        let Ok(entries) = fs::read_dir("/proc") else {
            return processes;
        };
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|value| value.parse::<u32>().ok())
            else {
                continue;
            };
            let Some((comm, ticks)) = read_process_stat(pid) else {
                continue;
            };
            let name = read_process_name(pid, comm);
            let memory_kib = read_process_memory(pid).unwrap_or(0);
            let previous = self
                .previous_process_ticks
                .get(&pid)
                .copied()
                .unwrap_or(ticks);
            let cpu_percent = if total_delta == 0 {
                0.0
            } else {
                ticks.saturating_sub(previous) as f64 * 100.0 / total_delta as f64
            };
            next_ticks.insert(pid, ticks);
            processes.push(ProcessSnapshot {
                name,
                pid,
                cpu_percent,
                memory_kib,
            });
        }
        self.previous_process_ticks = next_ticks;
        sort_processes(&mut processes);
        processes
    }
}

fn sort_processes(processes: &mut [ProcessSnapshot]) {
    // "TOP PROCESSES" means active now, not merely the five largest address
    // spaces. Memory remains the stable tie-breaker for the first sample, when
    // every CPU delta is necessarily zero.
    processes.sort_by(|left, right| {
        right
            .cpu_percent
            .total_cmp(&left.cpu_percent)
            .then_with(|| right.memory_kib.cmp(&left.memory_kib))
    });
}

impl SystemReader {
    fn read_gpus(&mut self) -> Vec<GpuSnapshot> {
        let mut gpus = self.read_nvidia_gpus();
        gpus.extend(read_amd_gpus());
        number_repeated_labels(&mut gpus, |gpu| &mut gpu.label);
        gpus
    }

    fn read_nvidia_gpus(&mut self) -> Vec<GpuSnapshot> {
        if self.nvidia_missing {
            return Vec::new();
        }
        let output = Command::new("nvidia-smi")
            .args([
                "--query-gpu=index,name,utilization.gpu,temperature.gpu",
                "--format=csv,noheader,nounits",
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        let output = match output {
            Ok(output) => output,
            Err(error) => {
                // Only a missing binary is permanent. A driver that is still
                // loading fails in other ways and deserves another try.
                self.nvidia_missing = error.kind() == io::ErrorKind::NotFound;
                return Vec::new();
            }
        };
        if !output.status.success() {
            return Vec::new();
        }
        parse_nvidia_gpus(&String::from_utf8_lossy(&output.stdout))
    }
}

fn parse_nvidia_gpus(raw: &str) -> Vec<GpuSnapshot> {
    raw.lines()
        .filter_map(|line| {
            // Counted from the right, because a card whose name has a comma in
            // it would otherwise shift every column along.
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            let _index = fields.first()?.parse::<usize>().ok()?;
            let (percent, temperature) = match fields.len() {
                0..=2 => return None,
                3 => (fields[2], None),
                length => (fields[length - 2], Some(fields[length - 1])),
            };
            // A driver that answers "[N/A]" for one column still means what it
            // says in the other, so neither reading is allowed to take the
            // card's whole row down with it.
            let percent = percent.parse::<f64>().ok().map(clamp_percent);
            let temperature = temperature.and_then(|value| value.parse::<f64>().ok());
            (percent.is_some() || temperature.is_some()).then(|| GpuSnapshot {
                label: "NVIDIA".into(),
                percent,
                temperature,
            })
        })
        .collect()
}

/// A caption has to fit in the gap at the bottom of its ring, so a spelt-out
/// "NVIDIA GeForce RTX 4060 Laptop GPU" is no use. The vendor is what tells the
/// two cards of a hybrid laptop apart; only a machine with two from the same
/// vendor needs them numbered.
fn number_repeated_labels<T>(items: &mut [T], label: fn(&mut T) -> &mut String) {
    let mut totals: HashMap<String, usize> = HashMap::new();
    for item in items.iter_mut() {
        *totals.entry(label(item).clone()).or_default() += 1;
    }
    let repeated: Vec<String> = totals
        .into_iter()
        .filter(|(_, total)| *total > 1)
        .map(|(name, _)| name)
        .collect();
    for name in repeated {
        let mut nth = 0;
        for item in items.iter_mut() {
            if *label(item) != name {
                continue;
            }
            nth += 1;
            *label(item) = format!("{name} {nth}");
        }
    }
}

fn clamp_percent(value: f64) -> f64 {
    value.clamp(0.0, 100.0)
}

fn read_amd_gpus() -> Vec<GpuSnapshot> {
    let Ok(entries) = fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
    let mut values = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(card) = name.to_str() else {
            continue;
        };
        let Some(number) = card.strip_prefix("card") else {
            continue;
        };
        if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let device = entry.path().join("device");
        if fs::read_to_string(device.join("vendor"))
            .ok()
            .is_none_or(|vendor| vendor.trim() != "0x1002")
        {
            continue;
        }
        // `gpu_busy_percent` is missing on the older drivers and on some
        // APUs; the card's temperature is still worth a ring on its own.
        let percent = fs::read_to_string(device.join("gpu_busy_percent"))
            .ok()
            .and_then(|raw| raw.trim().parse::<f64>().ok())
            .map(clamp_percent);
        let temperature = amd_gpu_temperature(&device);
        if percent.is_none() && temperature.is_none() {
            continue;
        }
        values.push(GpuSnapshot {
            label: "AMD".into(),
            percent,
            temperature,
        });
    }
    values
}

/// The temperature of an AMD card, read from the hwmon chip the driver hangs
/// off the same PCI device the load percentage comes from.
fn amd_gpu_temperature(device: &Path) -> Option<f64> {
    sorted_dirs(&device.join("hwmon"))
        .iter()
        // "edge" is the die's outside; "junction" is its hottest spot, which is
        // what a card without an edge sensor reports instead.
        .find_map(|dir| hwmon_temperature_path(dir, &["edge", "junction"]))
        .as_deref()
        .and_then(read_millidegrees)
}

fn read_disk_usage(path: impl AsRef<Path>) -> Option<Usage> {
    let path = path.as_ref();
    let total = fs2::total_space(path).ok()?;
    let available = fs2::available_space(path).ok()?;
    if total == 0 {
        return None;
    }
    Some(Usage {
        used_kib: total.saturating_sub(available) / 1024,
        total_kib: total / 1024,
    })
}

fn read_home_disk() -> Option<Usage> {
    home_disk_row(read_disk_usage("/"), read_disk_usage("/home")?)
}

/// Whether /home has earned a row of its own.
///
/// A single-partition install keeps /home inside /, where a second row would
/// repeat the first one byte for byte. The device number cannot answer this:
/// btrfs hands every subvolume its own, so the root and home subvolumes of a
/// stock Fedora or openSUSE install look like two filesystems while reporting
/// one pool of space. What they cannot disguise is being the same pool, so
/// that is what gets compared. Two genuinely separate mounts agreeing on both
/// their total and their free space down to the kibibyte is not a case worth
/// planning around.
fn home_disk_row(root: Option<Usage>, home: Usage) -> Option<Usage> {
    (root != Some(home)).then_some(home)
}

fn read_cpu_lines() -> Option<Vec<(u64, u64)>> {
    let raw = fs::read_to_string("/proc/stat").ok()?;
    let mut result = Vec::new();
    for line in raw.lines().take_while(|line| line.starts_with("cpu")) {
        let mut values = line.split_whitespace();
        let label = values.next()?;
        if label != "cpu" && label[3..].parse::<usize>().is_err() {
            continue;
        }
        let nums: Vec<u64> = values.filter_map(|v| v.parse().ok()).collect();
        if nums.len() < 5 {
            continue;
        }
        result.push((
            nums.iter().sum(),
            nums[3] + nums.get(4).copied().unwrap_or(0),
        ));
    }
    (!result.is_empty()).then_some(result)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MemoryInfo {
    memory: Usage,
    swap: Option<Usage>,
}

fn read_memory() -> Option<MemoryInfo> {
    parse_memory(&fs::read_to_string("/proc/meminfo").ok()?)
}

fn parse_memory(raw: &str) -> Option<MemoryInfo> {
    let mut total = None;
    let mut available = None;
    let mut swap_total: Option<u64> = None;
    let mut swap_free: Option<u64> = None;
    for line in raw.lines() {
        let mut bits = line.split_whitespace();
        let Some(key) = bits.next() else {
            continue;
        };
        match key {
            "MemTotal:" => total = bits.next().and_then(|v| v.parse().ok()),
            "MemAvailable:" => available = bits.next().and_then(|v| v.parse().ok()),
            "SwapTotal:" => swap_total = bits.next().and_then(|v| v.parse().ok()),
            "SwapFree:" => swap_free = bits.next().and_then(|v| v.parse().ok()),
            _ => {}
        }
    }
    let total: u64 = total?;
    let available: u64 = available?;
    Some(MemoryInfo {
        memory: Usage {
            used_kib: total.saturating_sub(available),
            total_kib: total,
        },
        // A machine with swap turned off reports a zero rather than nothing at
        // all, and "0% of nothing" is not a meter worth the room.
        swap: match (swap_total, swap_free) {
            (Some(swap_total), Some(swap_free)) if swap_total > 0 => Some(Usage {
                used_kib: swap_total.saturating_sub(swap_free),
                total_kib: swap_total,
            }),
            _ => None,
        },
    })
}

/// The chips that speak for the CPU package, in the order they are preferred,
/// with the sensor label to look for on each. Intel exposes `coretemp` and AMD
/// `k10temp`, so at most one of these is present on a given machine.
const CPU_TEMP_CHIPS: [(&str, &[&str]); 3] = [
    ("coretemp", &["Package id 0"]),
    ("k10temp", &["Tctl", "Tdie"]),
    ("zenpower", &["Tdie"]),
];

fn find_cpu_temperature_path() -> Option<PathBuf> {
    let chips = sorted_dirs(Path::new("/sys/class/hwmon"));
    for (name, labels) in CPU_TEMP_CHIPS {
        for chip in &chips {
            if hwmon_name(chip).as_deref() != Some(name) {
                continue;
            }
            if let Some(path) = hwmon_temperature_path(chip, labels) {
                return Some(path);
            }
        }
    }
    // No chip of the CPU's own: fall back to whichever ACPI thermal zone
    // speaks for the package, which is all a virtual machine tends to offer.
    thermal_zone_path(&["x86_pkg_temp", "acpitz"])
}

fn find_drive_sensors() -> Vec<DriveSensor> {
    let mut sensors = Vec::new();
    for chip in sorted_dirs(Path::new("/sys/class/hwmon")) {
        // `nvme` is the drive's own sensor; `drivetemp` is SATA SMART.
        if !matches!(hwmon_name(&chip).as_deref(), Some("nvme" | "drivetemp")) {
            continue;
        }
        let Some(path) = hwmon_temperature_path(&chip, &["Composite"]) else {
            continue;
        };
        // "SSD 1" and "SSD 2" say nothing about which drive is which. The
        // vendor is what the owner of the machine knows them by, and it fits
        // in a caption where a full model name never would. A drive whose
        // maker cannot be named still gets its size, which at least tells two
        // drives apart.
        let model = fs::read_to_string(chip.join("device/model")).unwrap_or_default();
        let label = drive_vendor(&model)
            .or_else(|| drive_capacity_bytes(&chip).map(format_drive_capacity))
            .unwrap_or_else(|| "SSD".to_owned());
        sensors.push(DriveSensor { label, path });
    }
    number_repeated_labels(&mut sensors, |sensor| &mut sensor.label);
    sensors
}

/// The maker of a drive, out of the free-form model string its firmware
/// reports. The name can sit anywhere in there — "UMIS RPJYJ512MKN1QWY" leads
/// with it, "PM981a NVMe Samsung 1024GB" buries it in the middle — so the
/// vendors worth naming are looked for wherever they appear. A drive from
/// anyone else falls back to the first word of its model that is not
/// boilerplate, which is usually its product line.
fn drive_vendor(model: &str) -> Option<String> {
    // Left of each pair is what firmware writes, right is what goes in a
    // caption 34 pixels wide.
    const VENDORS: &[(&str, &str)] = &[
        ("SAMSUNG", "SAMSUNG"),
        ("WESTERN DIGITAL", "WD"),
        ("WDC", "WD"),
        ("SANDISK", "SANDISK"),
        ("SEAGATE", "SEAGATE"),
        ("KINGSTON", "KINGSTON"),
        ("CRUCIAL", "CRUCIAL"),
        ("MICRON", "MICRON"),
        ("SOLIDIGM", "SOLIDIGM"),
        ("INTEL", "INTEL"),
        ("SK HYNIX", "HYNIX"),
        ("HYNIX", "HYNIX"),
        ("KIOXIA", "KIOXIA"),
        ("TOSHIBA", "TOSHIBA"),
        ("UMIS", "UMIS"),
        ("ADATA", "ADATA"),
        ("LEXAR", "LEXAR"),
        ("TRANSCEND", "TRANSCEND"),
        ("CORSAIR", "CORSAIR"),
        ("SABRENT", "SABRENT"),
        ("NETAC", "NETAC"),
        ("PATRIOT", "PATRIOT"),
        ("TEAMGROUP", "TEAM"),
        ("SILICON POWER", "SILICON"),
        ("APACER", "APACER"),
        ("KIMTIGO", "KIMTIGO"),
        ("HIKVISION", "HIKVISION"),
        ("PNY", "PNY"),
        ("HGST", "HGST"),
    ];
    let upper = model.trim().to_ascii_uppercase();
    if upper.is_empty() {
        return None;
    }
    if let Some((_, label)) = VENDORS.iter().find(|(pattern, _)| upper.contains(pattern)) {
        return Some((*label).to_owned());
    }
    // Words that describe the interface rather than the drive, and bare
    // capacities, are no use as a name.
    const BOILERPLATE: &[&str] = &[
        "NVME", "SSD", "HDD", "SATA", "PCIE", "DISK", "DRIVE", "SOLID", "STATE", "M.2",
    ];
    upper
        .split_whitespace()
        .map(|word| word.trim_matches(|character: char| !character.is_ascii_alphanumeric()))
        .find(|word| {
            word.len() >= 3
                && !BOILERPLATE.contains(word)
                && !word.bytes().all(|byte| byte.is_ascii_digit())
        })
        // A product line can run long enough to be unreadable once the caption
        // shrinks to fit; the first characters are the recognisable part.
        .map(|word| word.chars().take(8).collect())
}

/// How big the drive behind a temperature sensor is. SATA hangs its block
/// device off a `block` directory, while an NVMe namespace sits directly
/// inside the controller, so both places are searched.
fn drive_capacity_bytes(chip: &Path) -> Option<u64> {
    let device = chip.join("device");
    let namespace = sorted_dirs(&device.join("block"))
        .into_iter()
        .chain(sorted_dirs(&device))
        .find(|dir| dir.join("size").is_file())?;
    // `size` counts 512-byte sectors whatever block size the drive itself
    // reports, which is the one part of this that never varies.
    let sectors: u64 = fs::read_to_string(namespace.join("size"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (sectors > 0).then(|| sectors.saturating_mul(512))
}

/// A drive's size the way it was sold, in as few characters as a ring caption
/// can hold. Decimal units on purpose: the 1024GB NVMe on the desk this was
/// written for holds 1.02e12 bytes, which is "1TB" to its owner and a
/// meaningless "954G" in the binary units memory is measured in.
fn format_drive_capacity(bytes: u64) -> String {
    let terabytes = bytes as f64 / 1e12;
    if terabytes >= 1.0 {
        let rounded = (terabytes * 10.0).round() / 10.0;
        if rounded.fract() == 0.0 {
            format!("{rounded:.0}TB")
        } else {
            format!("{rounded:.1}TB")
        }
    } else {
        format!("{:.0}G", bytes as f64 / 1e9)
    }
}

/// Every directory inside `parent`, ordered by the number their name ends in
/// so `hwmon2` comes before `hwmon10`. The kernel hands them over in whatever
/// order the drivers loaded, which would otherwise let the two NVMe drives of
/// a laptop swap captions between one sample and the next.
fn sorted_dirs(parent: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    dirs.sort_by_key(|dir| {
        let name = dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned();
        let number = name
            .trim_start_matches(|character: char| !character.is_ascii_digit())
            .parse::<u64>()
            .unwrap_or(u64::MAX);
        (number, name)
    });
    dirs
}

fn hwmon_name(chip: &Path) -> Option<String> {
    Some(
        fs::read_to_string(chip.join("name"))
            .ok()?
            .trim()
            .to_owned(),
    )
}

/// The `tempN_input` this chip labels as one of `labels`, or its first sensor
/// when it labels nothing the caller asked for. `k10temp` files the package
/// reading under "Tctl" and an NVMe drive calls the one that matters
/// "Composite"; both also happen to be `temp1`, but only by convention.
fn hwmon_temperature_path(chip: &Path, labels: &[&str]) -> Option<PathBuf> {
    for index in 1..=MAX_HWMON_SENSORS {
        let Ok(label) = fs::read_to_string(chip.join(format!("temp{index}_label"))) else {
            continue;
        };
        if labels
            .iter()
            .any(|wanted| wanted.eq_ignore_ascii_case(label.trim()))
        {
            let input = chip.join(format!("temp{index}_input"));
            if input.exists() {
                return Some(input);
            }
        }
    }
    let first = chip.join("temp1_input");
    first.exists().then_some(first)
}

/// How many sensors one chip is searched for. Well past what any consumer chip
/// exposes, and it costs a failed `open` per miss rather than a directory scan.
const MAX_HWMON_SENSORS: u32 = 16;

fn thermal_zone_path(types: &[&str]) -> Option<PathBuf> {
    let zones = sorted_dirs(Path::new("/sys/class/thermal"));
    for wanted in types {
        for zone in &zones {
            let matches = fs::read_to_string(zone.join("type"))
                .is_ok_and(|found| found.trim().eq_ignore_ascii_case(wanted));
            if matches {
                return Some(zone.join("temp"));
            }
        }
    }
    None
}

fn read_millidegrees(path: &Path) -> Option<f64> {
    let raw = fs::read_to_string(path).ok()?;
    let millidegrees = raw.trim().parse::<f64>().ok()?;
    // Sensors report thousandths of a degree. A zero is a driver that has not
    // taken a reading yet rather than a component at freezing point.
    (millidegrees > 0.0).then_some(millidegrees / 1000.0)
}

fn read_network_counters() -> Option<(u64, u64)> {
    let raw = fs::read_to_string("/proc/net/dev").ok()?;
    Some(parse_network_counters(&raw, |name| {
        // Only a real device has one. Loopback, bridges, and the interfaces
        // Docker and a VPN put up all carry bytes that a physical interface is
        // already counting.
        Path::new("/sys/class/net")
            .join(name)
            .join("device")
            .exists()
    }))
}

fn parse_network_counters(raw: &str, is_physical: impl Fn(&str) -> bool) -> (u64, u64) {
    let mut received = 0u64;
    let mut transmitted = 0u64;
    for line in raw.lines() {
        let Some((name, counters)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || !is_physical(name) {
            continue;
        }
        let fields: Vec<u64> = counters
            .split_whitespace()
            .filter_map(|value| value.parse().ok())
            .collect();
        // Received bytes is the first column of the row, transmitted the ninth.
        let (Some(rx), Some(tx)) = (fields.first(), fields.get(8)) else {
            continue;
        };
        received = received.saturating_add(*rx);
        transmitted = transmitted.saturating_add(*tx);
    }
    (received, transmitted)
}

fn network_rates(previous: (u64, u64), current: (u64, u64), elapsed_seconds: f64) -> NetworkRates {
    // An interface that went away takes its share of the total with it, so a
    // counter can fall. That is a gap in the measurement, not negative
    // traffic.
    NetworkRates {
        down_bytes_per_sec: current.0.saturating_sub(previous.0) as f64 / elapsed_seconds,
        up_bytes_per_sec: current.1.saturating_sub(previous.1) as f64 / elapsed_seconds,
    }
}

fn read_process_stat(pid: u32) -> Option<(String, u64)> {
    let raw = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let open = raw.find('(')?;
    let close = raw.rfind(')')?;
    let name = raw.get(open + 1..close)?.to_owned();
    let fields: Vec<&str> = raw.get(close + 1..)?.split_whitespace().collect();
    let user_ticks = fields.get(11)?.parse::<u64>().ok()?;
    let system_ticks = fields.get(12)?.parse::<u64>().ok()?;
    Some((name, user_ticks.saturating_add(system_ticks)))
}

/// `/proc/<pid>/stat` truncates the command to 15 characters, which is how a
/// Firefox content process ends up listed as "Isolated Web Co" and a Chromium
/// helper as "Chrome_ChildIO". argv[0]'s file name is both complete and the
/// name the user knows the program by, so it wins wherever a process has one.
fn read_process_name(pid: u32, comm: String) -> String {
    fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .as_deref()
        .and_then(process_name_from_cmdline)
        .unwrap_or(comm)
}

fn process_name_from_cmdline(raw: &[u8]) -> Option<String> {
    // Arguments are meant to be NUL-separated, but Chromium and every Electron
    // app built on it rewrite their argv into one contiguous blob with spaces
    // between the arguments instead. Splitting on the NUL alone hands back that
    // whole command line, and taking the part after its last '/' then lands
    // somewhere inside --user-data-dir or a crash-reporter GUID. Treat
    // whitespace as a separator too and the first token is the executable
    // either way.
    let text = String::from_utf8_lossy(raw);
    let argv0 = text
        .split(|character: char| character == '\0' || character.is_whitespace())
        .find(|token| !token.is_empty())?;
    // Servers such as postgres rewrite argv[0] into a status line — "postgres:
    // checkpointer" — with no path in it. There is nothing to strip there, and
    // the bare program name is still the useful half.
    let name = argv0.rsplit('/').next()?.trim_end_matches(':');
    (!name.is_empty()).then(|| name.to_owned())
}

fn read_process_memory(pid: u32) -> Option<u64> {
    // Match GNOME System Monitor's process-memory column: libgtop exposes
    // `resident - shared`, both from /proc/<pid>/statm. Raw VmRSS charges all
    // shared Chromium/Electron pages to every child process and looks far too
    // high in a per-process list.
    let raw = fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    parse_process_memory_statm(&raw, page_size_kib())
}

fn page_size_kib() -> u64 {
    let bytes = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(bytes).unwrap_or(4096).max(1) / 1024
}

fn parse_process_memory_statm(raw: &str, page_kib: u64) -> Option<u64> {
    let mut fields = raw.split_whitespace();
    let _virtual_pages = fields.next()?;
    let resident_pages = fields.next()?.parse::<u64>().ok()?;
    let shared_pages = fields.next()?.parse::<u64>().ok()?;
    Some(
        resident_pages
            .saturating_sub(shared_pages)
            .saturating_mul(page_kib.max(1)),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        drive_vendor, format_drive_capacity, home_disk_row, network_rates, number_repeated_labels,
        parse_memory, parse_network_counters, parse_nvidia_gpus, parse_process_memory_statm,
        process_name_from_cmdline, sort_processes, GpuSnapshot, ProcessSnapshot, Usage,
    };

    #[test]
    fn process_memory_matches_system_monitor_resident_minus_shared() {
        assert_eq!(
            parse_process_memory_statm("1000 200 50 0 0 0 0", 4),
            Some(600)
        );
    }

    #[test]
    fn process_memory_never_underflows_when_kernel_reports_more_shared_pages() {
        assert_eq!(parse_process_memory_statm("1000 20 50", 4), Some(0));
    }

    #[test]
    fn top_processes_are_ranked_by_current_cpu_then_memory() {
        let mut processes = vec![
            ProcessSnapshot {
                name: "large-idle".into(),
                cpu_percent: 0.0,
                memory_kib: 8_000,
                ..ProcessSnapshot::default()
            },
            ProcessSnapshot {
                name: "busy".into(),
                cpu_percent: 12.0,
                memory_kib: 200,
                ..ProcessSnapshot::default()
            },
            ProcessSnapshot {
                name: "busier".into(),
                cpu_percent: 30.0,
                memory_kib: 100,
                ..ProcessSnapshot::default()
            },
        ];
        sort_processes(&mut processes);
        assert_eq!(processes[0].name, "busier");
        assert_eq!(processes[1].name, "busy");
        assert_eq!(processes[2].name, "large-idle");
    }

    #[test]
    fn a_process_is_named_after_argv0_rather_than_the_truncated_comm() {
        // What the kernel would have called "Isolated Web Co".
        assert_eq!(
            process_name_from_cmdline(b"/usr/lib/firefox/firefox\0-contentproc\0").as_deref(),
            Some("firefox")
        );
        assert_eq!(
            process_name_from_cmdline(b"/usr/bin/python3\0script.py\0").as_deref(),
            Some("python3")
        );
        // A rewritten argv[0] has no path to strip; the program name is the
        // half worth keeping.
        assert_eq!(
            process_name_from_cmdline(b"postgres: checkpointer\0").as_deref(),
            Some("postgres")
        );
    }

    #[test]
    fn a_chromium_style_cmdline_is_not_mistaken_for_one_long_argument() {
        // Both observed verbatim. Chromium and Electron write every argument
        // into a single NUL-terminated blob, so splitting on the NUL alone used
        // to take the last '/' out of --user-data-dir or a crash-reporter GUID
        // and call the process "VQ6BtEUZqoCU04zoRU=--c" or "Codex --owl-...".
        assert_eq!(
            process_name_from_cmdline(
                b"/opt/brave.com/brave/brave --type=renderer \
--enable-crash-reporter=254aa1ef-75fb-4888-b266-41763658ad75,VQ6BtEUZqoCU04zoRU=\0"
            )
            .as_deref(),
            Some("brave")
        );
        assert_eq!(
            process_name_from_cmdline(
                b"/usr/lib/chatgpt/ChatGPT --type=renderer \
--user-data-dir=/home/someone/.config/Codex --owl-electron-scheme\0"
            )
            .as_deref(),
            Some("ChatGPT")
        );
    }

    #[test]
    fn a_process_with_no_cmdline_keeps_the_name_the_kernel_gave_it() {
        // Kernel threads, and anything whose argv[0] is unusable, fall back to
        // `comm` rather than showing an empty row.
        assert_eq!(process_name_from_cmdline(b""), None);
        assert_eq!(process_name_from_cmdline(b"\0\0"), None);
        assert_eq!(process_name_from_cmdline(b"   \0"), None);
        assert_eq!(process_name_from_cmdline(b"/usr/bin/\0"), None);
    }

    #[test]
    fn nvidia_csv_keeps_every_gpu_and_clamps_what_the_driver_reports() {
        let values = parse_nvidia_gpus(
            "0, NVIDIA GeForce RTX 4060 Laptop GPU, 57, 43\n1, NVIDIA GTX 1080, 101, 62\n",
        );
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].percent, Some(57.0));
        assert_eq!(values[0].temperature, Some(43.0));
        assert_eq!(values[1].percent, Some(100.0));
        assert_eq!(values[1].temperature, Some(62.0));
        // A driver too old to answer for the temperature still reports a load.
        let older = parse_nvidia_gpus("0, NVIDIA GTX 1080, 12\n");
        assert_eq!(older[0].percent, Some(12.0));
        assert_eq!(older[0].temperature, None);
        // "[N/A]" in one column does not take the other one down with it: a
        // card that knows its temperature and not its load keeps the reading
        // it has, and vice versa.
        let hot = parse_nvidia_gpus("0, NVIDIA RTX A2000, [N/A], 51\n");
        assert_eq!(hot[0].percent, None);
        assert_eq!(hot[0].temperature, Some(51.0));
        let busy = parse_nvidia_gpus("0, NVIDIA RTX A2000, 34, [N/A]\n");
        assert_eq!(busy[0].percent, Some(34.0));
        assert_eq!(busy[0].temperature, None);
        // A line with nothing usable in either column is skipped rather than
        // shown as zero.
        assert!(parse_nvidia_gpus("0, NVIDIA RTX A2000, [N/A], [N/A]\n").is_empty());
        assert!(parse_nvidia_gpus("nvidia-smi: command failed\n").is_empty());
    }

    #[test]
    fn home_earns_a_row_only_when_it_is_not_the_same_pool_of_space_as_root() {
        let root = Usage {
            used_kib: 40_000_000,
            total_kib: 192_000_000,
        };
        // The stock Fedora layout: two btrfs subvolumes, two device numbers,
        // one pool. It reports itself twice and must be shown once.
        assert_eq!(home_disk_row(Some(root), root), None);
        let home = Usage {
            used_kib: 97_000_000,
            total_kib: 915_000_000,
        };
        assert_eq!(home_disk_row(Some(root), home), Some(home));
        // Nothing to compare against is no reason to drop the row.
        assert_eq!(home_disk_row(None, home), Some(home));
    }

    #[test]
    fn meminfo_gives_both_halves_of_memory_and_swap() {
        let raw = "MemTotal:       16777216 kB\n\
                   MemFree:         1000000 kB\n\
                   MemAvailable:   10777216 kB\n\
                   SwapTotal:      16777212 kB\n\
                   SwapFree:       11349852 kB\n";
        let info = parse_memory(raw).expect("meminfo should parse");
        assert_eq!(
            info.memory,
            Usage {
                used_kib: 6_000_000,
                total_kib: 16_777_216
            }
        );
        assert_eq!(
            info.swap,
            Some(Usage {
                used_kib: 5_427_360,
                total_kib: 16_777_212
            })
        );
    }

    #[test]
    fn a_machine_with_swap_turned_off_reports_no_swap_at_all() {
        // Zero of zero would draw a full-looking meter out of nothing.
        let raw = "MemTotal:       16777216 kB\n\
                   MemAvailable:   10777216 kB\n\
                   SwapTotal:             0 kB\n\
                   SwapFree:              0 kB\n";
        assert_eq!(parse_memory(raw).expect("meminfo should parse").swap, None);
        // An old kernel that lists no swap lines at all is the same answer.
        let raw = "MemTotal: 16777216 kB\nMemAvailable: 10777216 kB\n";
        assert_eq!(parse_memory(raw).expect("meminfo should parse").swap, None);
    }

    #[test]
    fn usage_percent_never_exceeds_a_full_meter() {
        assert_eq!(
            Usage {
                used_kib: 50,
                total_kib: 200
            }
            .percent(),
            25.0
        );
        // A filesystem whose reserved blocks are in use reports more used than
        // there is room for; the ring still stops at full.
        assert_eq!(
            Usage {
                used_kib: 300,
                total_kib: 200
            }
            .percent(),
            100.0
        );
        assert_eq!(Usage::default().percent(), 0.0);
    }

    #[test]
    fn network_counters_come_from_the_physical_interfaces_only() {
        // Verbatim shape of /proc/net/dev: the two header lines, then one row
        // per interface with sixteen counters.
        let raw = "Inter-|   Receive                    |  Transmit\n\
                    face |bytes packets errs drop fifo frame compressed multicast|bytes packets errs drop fifo colls carrier compressed\n\
                        lo: 4605869 17104 0 0 0 0 0 0 4605869 17104 0 0 0 0 0 0\n\
                    wlp3s0: 1000 10 0 0 0 0 0 0 2000 20 0 0 0 0 0 0\n\
                   docker0: 9999 99 0 0 0 0 0 0 9999 99 0 0 0 0 0 0\n\
                    enp2s0: 300 3 0 0 0 0 0 0 400 4 0 0 0 0 0 0\n";
        // Loopback and the Docker bridge carry bytes a real interface already
        // counted, so counting them would report a download as twice its size.
        let physical = |name: &str| name == "wlp3s0" || name == "enp2s0";
        assert_eq!(parse_network_counters(raw, physical), (1300, 2400));
    }

    #[test]
    fn a_throughput_rate_is_the_change_over_the_time_between_samples() {
        let rates = network_rates((1_000, 2_000), (3_048, 2_512), 2.0);
        assert_eq!(rates.down_bytes_per_sec, 1024.0);
        assert_eq!(rates.up_bytes_per_sec, 256.0);
        // An interface that was unplugged between samples takes its bytes out
        // of the total. That is a gap in the measurement, not negative traffic.
        let rates = network_rates((9_000, 9_000), (1_000, 1_000), 2.0);
        assert_eq!(rates.down_bytes_per_sec, 0.0);
        assert_eq!(rates.up_bytes_per_sec, 0.0);
    }

    #[test]
    fn two_cards_from_the_same_vendor_are_numbered_and_a_mixed_pair_is_not() {
        let gpu = |label: &str| GpuSnapshot {
            label: label.into(),
            percent: Some(0.0),
            temperature: None,
        };
        // The hybrid laptop this was written for: one of each, no numbering.
        let mut mixed = vec![gpu("NVIDIA"), gpu("AMD")];
        number_repeated_labels(&mut mixed, |gpu| &mut gpu.label);
        assert_eq!(mixed[0].label, "NVIDIA");
        assert_eq!(mixed[1].label, "AMD");

        let mut alike = vec![gpu("NVIDIA"), gpu("NVIDIA"), gpu("AMD")];
        number_repeated_labels(&mut alike, |gpu| &mut gpu.label);
        assert_eq!(alike[0].label, "NVIDIA 1");
        assert_eq!(alike[1].label, "NVIDIA 2");
        assert_eq!(alike[2].label, "AMD");

        // Two drives from the same maker are told apart the same way, and a
        // mixed pair needs no numbering at all.
        let mut alike = vec![("SAMSUNG".to_owned(), 45.0), ("SAMSUNG".to_owned(), 39.0)];
        number_repeated_labels(&mut alike, |drive| &mut drive.0);
        assert_eq!(alike[0].0, "SAMSUNG 1");
        assert_eq!(alike[1].0, "SAMSUNG 2");

        let mut different = vec![("SAMSUNG".to_owned(), 45.0), ("UMIS".to_owned(), 39.0)];
        number_repeated_labels(&mut different, |drive| &mut drive.0);
        assert_eq!(different[0].0, "SAMSUNG");
        assert_eq!(different[1].0, "UMIS");
    }

    #[test]
    fn a_drive_is_captioned_with_the_maker_named_anywhere_in_its_model() {
        // Both drives on the desk this was written for, verbatim including the
        // padding the firmware reports.
        assert_eq!(
            drive_vendor("PM981a NVMe Samsung 1024GB              ").as_deref(),
            Some("SAMSUNG")
        );
        assert_eq!(
            drive_vendor("UMIS RPJYJ512MKN1QWY                    ").as_deref(),
            Some("UMIS")
        );
        // A name too long for the caption is the one place a shorter form is
        // worth keeping.
        assert_eq!(
            drive_vendor("WDC WDS500G2B0A-00SM50").as_deref(),
            Some("WD")
        );
        // Nobody recognisable: the product line is still better than "SSD".
        assert_eq!(drive_vendor("T-FORCE Z440 1TB").as_deref(), Some("T-FORCE"));
        // The interface is not a name, so it is skipped in favour of what
        // follows it.
        assert_eq!(drive_vendor("NVMe BC711 512GB").as_deref(), Some("BC711"));
        // Nothing to go on falls through to the size instead.
        assert_eq!(drive_vendor("   "), None);
        assert_eq!(drive_vendor("SSD 256"), None);
    }

    #[test]
    fn a_drive_is_captioned_with_the_size_it_was_sold_as() {
        // Both drives on the desk this was written for, in the sectors their
        // `size` files report.
        assert_eq!(format_drive_capacity(2_000_409_264 * 512), "1TB");
        assert_eq!(format_drive_capacity(1_000_215_216 * 512), "512G");
        // A drive whose size lands between the round numbers keeps one decimal
        // rather than rounding away half a terabyte.
        assert_eq!(format_drive_capacity(1_500_000_000_000), "1.5TB");
        assert_eq!(format_drive_capacity(2_048_408_248_320), "2TB");
        assert_eq!(format_drive_capacity(250_059_350_016), "250G");
    }
}
