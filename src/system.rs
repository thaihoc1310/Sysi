use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Instant,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SystemReadOptions {
    pub gpus: bool,
    pub cpu_temp: bool,
    pub gpu_temp: bool,
    pub ssd_temp: bool,
    /// How full each drive is, over the filesystems mounted from it.
    pub ssd_usage: bool,
    pub network: bool,
    /// What the machine draws: the battery's discharge, or on mains the
    /// processor package and the GPUs.
    pub power: bool,
    /// How long the battery lasts, or takes to fill.
    pub battery: bool,
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
    /// Its own video memory in use. For a GPU built into the processor this
    /// is only the slice of RAM set aside for it, which it overflows into
    /// shared memory, so it often reads close to full.
    pub memory: Option<Usage>,
    /// Watts drawn. For a GPU built into the processor this is the whole
    /// package, CPU cores included: the driver reports it for the chip.
    pub power: Option<f64>,
}

/// One physical drive: what to call it, how full the filesystems mounted from
/// it are between them, and how hot it runs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DriveSnapshot {
    /// Its maker ("SAMSUNG", "UMIS"), or its size, numbered when repeated.
    pub label: String,
    /// Space used over the filesystems mounted from it. `None` when nothing
    /// on it is mounted, or it was not asked.
    pub usage: Option<Usage>,
    pub temperature: Option<f64>,
}

#[derive(Clone, Debug, Default)]
pub struct SystemSnapshot {
    pub cpu_percent: f64,
    pub memory: Usage,
    /// `None` on a machine with no swap configured at all.
    pub swap: Option<Usage>,
    pub gpus: Vec<GpuSnapshot>,
    pub cpu_temperature: Option<f64>,
    /// Every internal drive that was asked about, in a stable order.
    pub drives: Vec<DriveSnapshot>,
    /// `None` until a second sample exists, since a rate needs two counters.
    pub network: Option<NetworkRates>,
    pub power: Option<PowerDraw>,
    /// `None` on a machine with no battery that says how much it holds, or
    /// when it was not asked.
    pub battery: Option<Battery>,
}

/// Which way the battery is going, and how long until it gets there.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Battery {
    pub state: BatteryState,
    /// Hours until empty, or until full while charging, at the rate of the
    /// last couple of minutes. `None` while it is neither, or at no rate.
    pub hours: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatteryState {
    Discharging,
    Charging,
    /// Full, or plugged in and held where it is (a charge limit).
    Idle,
}

/// What the batteries do between them, read once: the way they are going,
/// at what rate, and the watt-hours until they get there.
#[derive(Clone, Copy, Debug, PartialEq)]
struct BatteryReading {
    state: BatteryState,
    watts: f64,
    watt_hours: f64,
}

/// What the machine is drawing, and from where.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PowerDraw {
    pub watts: f64,
    /// Running on its battery: `watts` is then the whole machine's draw. On
    /// mains, no laptop reports that, so it is the processor package and the
    /// GPUs, the parts that draw the most and vary the most.
    pub on_battery: bool,
}

/// How far back the battery's time looks at its rate. The rate of the moment
/// swings from 15 W to 40 W and back as a page loads or a build runs; a time
/// worked out from it would jump by an hour every two seconds.
const BATTERY_AVERAGE: f64 = 120.0;

#[derive(Default)]
pub struct SystemReader {
    previous_total: u64,
    previous_idle: u64,
    /// Set once `nvidia-smi` turns out not to be installed. Without it a
    /// machine with no NVIDIA driver pays for spawning a missing process every
    /// two seconds for as long as the GPU meters are on.
    nvidia_missing: bool,
    /// The NVIDIA cards' PCI devices, found on first use, whose power state
    /// says whether asking `nvidia-smi` would wake one.
    nvidia_devices: Option<Vec<PathBuf>>,
    /// The last interface counters and when they were read, which is what a
    /// throughput rate is measured against.
    previous_network: Option<(Instant, u64, u64)>,
    /// Where the CPU package temperature is read from, resolved on first use.
    /// The outer `None` means the search has not run yet; the inner one means
    /// it ran and this machine exposes no such sensor.
    cpu_temp_path: Option<Option<PathBuf>>,
    /// The drives and the caption each one earned, found on first use. Kept
    /// for the same reason as `cpu_temp_path`, and for one more: a caption
    /// worked out afresh every two seconds would renumber a pair of
    /// same-vendor drives the moment one of them missed a read.
    drives: Option<Vec<Drive>>,
    /// Which way the battery was going, its rate averaged so far, when it was
    /// last read, and since when it has been going that way (see `settle`).
    battery_rate: Option<(BatteryState, f64, Instant, Instant)>,
}

/// One drive as found: its caption, its block device, and its temperature
/// sensor if it has one.
#[derive(Clone, Debug)]
struct Drive {
    label: String,
    /// The whole-disk name under /sys/block: `nvme0n1`, `sda`.
    block: String,
    temperature: Option<PathBuf>,
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
        // The temperature of a card is read from the same place its load is,
        // so the GPU readers run for either meter.
        let gpus = if options.gpus || options.gpu_temp || options.power {
            self.read_gpus()
        } else {
            Vec::new()
        };
        let cpu_temperature = if options.cpu_temp {
            self.read_cpu_temperature()
        } else {
            None
        };
        let drives = if options.ssd_temp || options.ssd_usage {
            self.read_drives(options.ssd_usage, options.ssd_temp)
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

        let power = options.power.then(|| read_power(&gpus)).flatten();
        let battery = options
            .battery
            .then(|| read_batteries(Path::new("/sys/class/power_supply")))
            .flatten();
        let battery = self.time_battery(battery);
        SystemSnapshot {
            cpu_percent,
            memory: memory_info.memory,
            swap: memory_info.swap,
            gpus,
            cpu_temperature,
            power,
            battery,
            drives,
            network,
        }
    }

    /// How long the battery takes to get where it is going, at its rate
    /// averaged since it started that way.
    fn time_battery(&mut self, reading: Option<BatteryReading>) -> Option<Battery> {
        let reading = reading?;
        let state = reading.state;
        let now = Instant::now();
        let (average, since) = match self.battery_rate {
            _ if state == BatteryState::Idle => {
                self.battery_rate = None;
                return Some(Battery { state, hours: None });
            }
            Some((was, average, then, since)) if was == state => (
                settle(
                    average,
                    reading.watts,
                    now.duration_since(then).as_secs_f64(),
                    now.duration_since(since).as_secs_f64(),
                ),
                since,
            ),
            // Plugged in or out: the rate before says nothing about this one.
            _ => (reading.watts, now),
        };
        self.battery_rate = Some((state, average, now, since));
        Some(Battery {
            state,
            hours: (average >= 0.1).then(|| reading.watt_hours / average),
        })
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

    fn read_drives(&mut self, usage: bool, temperature: bool) -> Vec<DriveSnapshot> {
        let drives = self.drives.get_or_insert_with(find_drives);
        // One pass over the mount table for all the drives: which disk each
        // mounted filesystem lives on, found once rather than once per drive.
        let mounted = if usage {
            fs::read_to_string("/proc/mounts")
                .map(|raw| mounted_disks(&parse_mounts(&raw)))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        drives
            .iter()
            .map(|drive| DriveSnapshot {
                label: drive.label.clone(),
                usage: usage.then(|| drive_usage(&drive.block, &mounted)).flatten(),
                temperature: temperature
                    .then(|| drive.temperature.as_deref().and_then(read_millidegrees))
                    .flatten(),
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
        // A laptop's discrete card sleeps while nothing uses it, and
        // nvidia-smi wakes it to answer: polled every two seconds, it would
        // never sleep again, and cost the battery watts for a reading of 0%.
        // A sleeping card reads as idle, and is left asleep.
        // The first reading is the one the bar is laid out from, so it asks
        // even a sleeping card what it can report.
        let first = self.nvidia_devices.is_none();
        let devices = self.nvidia_devices.get_or_insert_with(find_nvidia_devices);
        if !first
            && !devices.is_empty()
            && devices.iter().all(|device| {
                fs::read_to_string(device.join("power/runtime_status"))
                    .is_ok_and(|status| status.trim() == "suspended")
            })
        {
            return devices
                .iter()
                .map(|_| GpuSnapshot {
                    label: "NVIDIA".into(),
                    percent: Some(0.0),
                    temperature: None,
                    memory: None,
                    // Asleep, it draws next to nothing.
                    power: Some(0.0),
                })
                .collect();
        }
        let output = Command::new("nvidia-smi")
            .args([
                "--query-gpu=index,name,utilization.gpu,temperature.gpu,memory.used,memory.total,power.draw",
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
            // index, name, load, temperature, memory used, memory total (MiB),
            // power (W). Counted from the right, because a card whose name has
            // a comma in it would otherwise shift every column along.
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            let _index = fields.first()?.parse::<usize>().ok()?;
            let full = fields.len();
            if full < 7 {
                return None;
            }
            let power = fields[full - 1].parse::<f64>().ok();
            let length = full - 1;
            // A driver that answers "[N/A]" for one column still means what it
            // says in the others, so no reading is allowed to take the card's
            // whole row down with it.
            let number = |field: &str| field.parse::<f64>().ok();
            let percent = number(fields[length - 4]).map(clamp_percent);
            let temperature = number(fields[length - 3]);
            let memory = match (number(fields[length - 2]), number(fields[length - 1])) {
                (Some(used), Some(total)) if total > 0.0 => Some(Usage {
                    used_kib: (used * 1024.0) as u64,
                    total_kib: (total * 1024.0) as u64,
                }),
                _ => None,
            };
            (percent.is_some() || temperature.is_some() || memory.is_some()).then(|| GpuSnapshot {
                label: "NVIDIA".into(),
                percent,
                temperature,
                memory,
                power,
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

/// Every NVIDIA display controller on the PCI bus.
fn find_nvidia_devices() -> Vec<PathBuf> {
    sorted_dirs(Path::new("/sys/bus/pci/devices"))
        .into_iter()
        .filter(|device| {
            let read = |name: &str| fs::read_to_string(device.join(name)).unwrap_or_default();
            read("vendor").trim() == "0x10de" && read("class").trim().starts_with("0x03")
        })
        .collect()
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
        let bytes = |name: &str| {
            fs::read_to_string(device.join(name))
                .ok()
                .and_then(|raw| raw.trim().parse::<u64>().ok())
        };
        let memory = match (bytes("mem_info_vram_used"), bytes("mem_info_vram_total")) {
            (Some(used), Some(total)) if total > 0 => Some(Usage {
                used_kib: used / 1024,
                total_kib: total / 1024,
            }),
            _ => None,
        };
        let power = amd_gpu_power(&device);
        if percent.is_none() && temperature.is_none() && memory.is_none() {
            continue;
        }
        values.push(GpuSnapshot {
            label: "AMD".into(),
            percent,
            temperature,
            memory,
            power,
        });
    }
    values
}

/// An AMD GPU's draw in watts, averaged by the driver: power1_average, or on
/// chips without it the instant reading. On a processor with the GPU built in
/// it is the whole package's (PPT), CPU cores included.
fn amd_gpu_power(device: &Path) -> Option<f64> {
    sorted_dirs(&device.join("hwmon")).iter().find_map(|dir| {
        ["power1_average", "power1_input"].iter().find_map(|name| {
            fs::read_to_string(dir.join(name))
                .ok()
                .and_then(|raw| raw.trim().parse::<f64>().ok())
                .map(|microwatts| microwatts / 1e6)
        })
    })
}

/// A battery's discharge in watts, while it is discharging: from power_now,
/// or current_now × voltage_now where the firmware reports those instead.
fn battery_discharge(supply: &Path) -> Option<f64> {
    if fs::read_to_string(supply.join("type")).ok()?.trim() != "Battery"
        || fs::read_to_string(supply.join("status")).ok()?.trim() != "Discharging"
    {
        return None;
    }
    supply_watts(supply)
}

/// A number a power supply reports, as it reports it (µW, µWh, µV, µA...).
fn supply_number(supply: &Path, name: &str) -> Option<f64> {
    fs::read_to_string(supply.join(name))
        .ok()
        .and_then(|raw| raw.trim().parse::<f64>().ok())
}

/// The rate a battery is emptying or filling at, in watts: power_now, or
/// current_now × voltage_now where the firmware reports those instead. Some
/// firmware signs it by direction; the direction is in the status.
fn supply_watts(supply: &Path) -> Option<f64> {
    let watts = match supply_number(supply, "power_now") {
        Some(microwatts) => microwatts / 1e6,
        None => {
            supply_number(supply, "current_now")? * supply_number(supply, "voltage_now")? / 1e12
        }
    };
    Some(watts.abs())
}

/// A battery's energy in watt-hours: `energy_*`, or where the firmware
/// counts charge instead, `charge_*` at the voltage the battery is rated for
/// (at the one it is at, failing that).
fn supply_watt_hours(supply: &Path, which: &str) -> Option<f64> {
    if let Some(microwatt_hours) = supply_number(supply, &format!("energy_{which}")) {
        return Some(microwatt_hours / 1e6);
    }
    // A design voltage of 0 is firmware that does not know it.
    let volts = supply_number(supply, "voltage_min_design")
        .filter(|volts| *volts > 0.0)
        .or_else(|| supply_number(supply, "voltage_now").filter(|volts| *volts > 0.0))?;
    Some(supply_number(supply, &format!("charge_{which}"))? * volts / 1e12)
}

/// What the batteries under `supplies` do between them. Discharging, they
/// last for all they hold, an idle second battery included, since it is
/// drawn on next. Charging, they fill to where charging stops: full, or a
/// charge limit (charge_control_end_threshold) set below it. `None` if no
/// battery says how much it holds.
fn read_batteries(supplies: &Path) -> Option<BatteryReading> {
    let (mut found, mut held, mut missing) = (false, 0.0, 0.0);
    let (mut watts_out, mut watts_in) = (None::<f64>, None::<f64>);
    for supply in sorted_dirs(supplies) {
        if fs::read_to_string(supply.join("type"))
            .ok()
            .as_deref()
            .map(str::trim)
            != Some("Battery")
        {
            continue;
        }
        let Some(now) = supply_watt_hours(&supply, "now") else {
            continue;
        };
        found = true;
        held += now;
        let limit = supply_number(&supply, "charge_control_end_threshold")
            .filter(|percent| (1.0..100.0).contains(percent))
            .map_or(1.0, |percent| percent / 100.0);
        if let Some(full) = supply_watt_hours(&supply, "full") {
            missing += (full * limit - now).max(0.0);
        }
        let status = fs::read_to_string(supply.join("status")).unwrap_or_default();
        let rate = match status.trim() {
            "Discharging" => &mut watts_out,
            "Charging" => &mut watts_in,
            _ => continue,
        };
        *rate = Some(rate.unwrap_or(0.0) + supply_watts(&supply).unwrap_or(0.0));
    }
    if !found {
        return None;
    }
    Some(match (watts_out, watts_in) {
        (Some(watts), _) => BatteryReading {
            state: BatteryState::Discharging,
            watts,
            watt_hours: held,
        },
        // Some firmware says Charging at full, or at its charge limit, with a
        // trickle going in: nothing is left to fill, so nothing to time.
        (None, Some(watts)) if missing >= 0.05 => BatteryReading {
            state: BatteryState::Charging,
            watts,
            watt_hours: missing,
        },
        _ => BatteryReading {
            state: BatteryState::Idle,
            watts: 0.0,
            watt_hours: 0.0,
        },
    })
}

/// A rate averaged over the last `BATTERY_AVERAGE` seconds, taking in one
/// more reading `seconds` after the last, `going` seconds after the
/// battery started the way it is going. Until that long has gone by, the
/// readings since the first count the same (the first only starts the
/// clock: off the mains it is often a spike), so none hangs on for minutes;
/// after it, older readings fade. (A suspend is no gap: Instant
/// stands still through it, and the rate before it is as good a guess as any
/// for after.)
fn settle(average: f64, watts: f64, seconds: f64, going: f64) -> f64 {
    let fading = 1.0 - (-seconds / BATTERY_AVERAGE).exp();
    let even = if going > 0.0 { seconds / going } else { 1.0 };
    average + (watts - average) * fading.max(even).min(1.0)
}

/// What the machine draws: every discharging battery's output, or on mains
/// the processor package and the GPUs, `None` when neither can be read.
fn read_power(gpus: &[GpuSnapshot]) -> Option<PowerDraw> {
    let batteries: Vec<f64> = sorted_dirs(Path::new("/sys/class/power_supply"))
        .iter()
        .filter_map(|supply| battery_discharge(supply))
        .collect();
    if !batteries.is_empty() {
        return Some(PowerDraw {
            watts: batteries.iter().sum(),
            on_battery: true,
        });
    }
    let parts: Vec<f64> = gpus.iter().filter_map(|gpu| gpu.power).collect();
    (!parts.is_empty()).then(|| PowerDraw {
        watts: parts.iter().sum(),
        on_battery: false,
    })
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

/// How full a filesystem is, the way `df` has it: the blocks in use over
/// those in use plus those an ordinary user may still fill. The ones ext4
/// keeps back for root (five percent by default) are counted as neither;
/// counting them as used read an ext4 root 30% full as 33%.
fn read_disk_usage(path: impl AsRef<Path>) -> Option<Usage> {
    let path = std::ffi::CString::new(path.as_ref().as_os_str().as_encoded_bytes()).ok()?;
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated and `stats` is a valid out pointer.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stats) } != 0 {
        return None;
    }
    let block = stats.f_frsize as u64;
    let used = (stats.f_blocks as u64).saturating_sub(stats.f_bfree as u64) * block / 1024;
    let available = stats.f_bavail as u64 * block / 1024;
    (used + available > 0).then_some(Usage {
        used_kib: used,
        total_kib: used + available,
    })
}

/// Every mounted block device and where it is mounted, from /proc/mounts,
/// with the octal escapes the kernel writes for spaces undone.
fn parse_mounts(raw: &str) -> Vec<(String, String)> {
    let unescape = |field: &str| {
        let mut out = String::new();
        let mut chars = field.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                let code: String = chars.by_ref().take(3).collect();
                if let Ok(byte) = u8::from_str_radix(&code, 8) {
                    out.push(char::from(byte));
                    continue;
                }
                out.push(c);
                out.push_str(&code);
            } else {
                out.push(c);
            }
        }
        out
    };
    raw.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let source = fields.next()?;
            let target = fields.next()?;
            source
                .starts_with("/dev/")
                .then(|| (unescape(source), unescape(target)))
        })
        .collect()
}

/// The whole disk a block device lives on: a partition's parent, or, for a
/// device-mapper or RAID volume (LUKS, LVM), the disk under it.
fn disk_of(name: &str, depth: usize) -> Option<String> {
    let class = Path::new("/sys/class/block").join(name);
    if class.join("partition").is_file() {
        let parent = fs::canonicalize(&class).ok()?;
        return parent.parent()?.file_name()?.to_str().map(str::to_owned);
    }
    // Device-mapper and md volumes sit on "slaves"; follow the first.
    let slave = sorted_dirs(&class.join("slaves")).into_iter().next();
    match slave {
        Some(slave) if depth < 8 => disk_of(slave.file_name()?.to_str()?, depth + 1),
        _ => Some(name.to_owned()),
    }
}

/// Each filesystem mounted from a block device, once however many places it
/// is mounted (a bind mount, a subvolume), with the disk it lives on and one
/// place to ask it how full it is. Snap packages mount dozens of loop devices,
/// which live on no disk and are dropped before anything is looked up.
fn mounted_disks(mounts: &[(String, String)]) -> Vec<(String, String)> {
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for (source, target) in mounts {
        if source.starts_with("/dev/loop") {
            continue;
        }
        let Ok(device) = fs::canonicalize(source) else {
            continue;
        };
        let Some(name) = device.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if seen.iter().any(|known| known == name) {
            continue;
        }
        seen.push(name.to_owned());
        if let Some(disk) = disk_of(name, 0) {
            out.push((disk, target.clone()));
        }
    }
    out
}

/// How full a drive is, over every filesystem mounted from it.
fn drive_usage(block: &str, mounted: &[(String, String)]) -> Option<Usage> {
    let mut total = Usage::default();
    for (_, target) in mounted.iter().filter(|(disk, _)| disk == block) {
        if let Some(usage) = read_disk_usage(target) {
            total.used_kib += usage.used_kib;
            total.total_kib += usage.total_kib;
        }
    }
    (total.total_kib > 0).then_some(total)
}

fn read_cpu_lines() -> Option<Vec<(u64, u64)>> {
    parse_cpu_lines(&fs::read_to_string("/proc/stat").ok()?)
}

/// Each CPU line of /proc/stat as its busy-or-idle total and its idle part.
fn parse_cpu_lines(raw: &str) -> Option<Vec<(u64, u64)>> {
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
        // user nice system idle iowait irq softirq steal. The two after
        // them, guest and guest_nice, are already counted in user and nice:
        // summing them too counted a virtual machine's time twice and read
        // the CPU as idler than it was, the way top and htop do not.
        result.push((
            nums.iter().take(8).sum(),
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

/// The machine's own drives, from /sys/block: no loop, RAM, optical, device
/// mapper or removable devices. Each is captioned by its maker, and finds its
/// temperature sensor by the device both hang off.
fn find_drives() -> Vec<Drive> {
    let sensors: Vec<(PathBuf, PathBuf)> = sorted_dirs(Path::new("/sys/class/hwmon"))
        .into_iter()
        // `nvme` is the drive's own sensor; `drivetemp` is SATA SMART.
        .filter(|chip| matches!(hwmon_name(chip).as_deref(), Some("nvme" | "drivetemp")))
        .filter_map(|chip| {
            let device = fs::canonicalize(chip.join("device")).ok()?;
            Some((device, hwmon_temperature_path(&chip, &["Composite"])?))
        })
        .collect();
    let mut drives = Vec::new();
    for disk in sorted_dirs(Path::new("/sys/block")) {
        let Some(block) = disk
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
        else {
            continue;
        };
        if ["loop", "ram", "zram", "dm-", "md", "sr", "fd", "nbd"]
            .iter()
            .any(|prefix| block.starts_with(prefix))
            || fs::read_to_string(disk.join("removable")).is_ok_and(|value| value.trim() == "1")
        {
            continue;
        }
        let sectors: u64 = fs::read_to_string(disk.join("size"))
            .ok()
            .and_then(|raw| raw.trim().parse().ok())
            .unwrap_or(0);
        if sectors == 0 {
            continue;
        }
        let device = fs::canonicalize(disk.join("device")).ok();
        let temperature = device.as_ref().and_then(|device| {
            sensors
                .iter()
                .find(|(owner, _)| owner == device)
                .map(|(_, path)| path.clone())
        });
        // "SSD 1" and "SSD 2" say nothing about which drive is which. The
        // vendor is what the owner of the machine knows them by; a drive
        // whose maker cannot be named still gets its size.
        let model = fs::read_to_string(disk.join("device/model")).unwrap_or_default();
        let label = drive_vendor(&model)
            .unwrap_or_else(|| format_drive_capacity(sectors.saturating_mul(512)));
        drives.push(Drive {
            label,
            block,
            temperature,
        });
    }
    number_repeated_labels(&mut drives, |drive| &mut drive.label);
    drives
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

#[cfg(test)]
mod tests {
    use super::{
        drive_vendor, format_drive_capacity, network_rates, number_repeated_labels,
        parse_cpu_lines, parse_memory, parse_mounts, parse_network_counters, parse_nvidia_gpus,
        read_batteries, settle, BatteryReading, BatteryState, GpuSnapshot, Usage,
    };

    /// A power_supply directory with these supplies, each a list of files.
    fn supplies(name: &str, list: &[(&str, &[(&str, &str)])]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sysi-supplies-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (supply, files) in list {
            std::fs::create_dir_all(dir.join(supply)).unwrap();
            for (file, contents) in *files {
                std::fs::write(dir.join(supply).join(file), format!("{contents}\n")).unwrap();
            }
        }
        dir
    }

    #[test]
    fn the_batteries_say_which_way_they_go_and_how_far() {
        let mains = ("AC", &[("type", "Mains"), ("online", "0")][..]);
        let discharging = supplies(
            "out",
            &[
                mains,
                (
                    "BAT0",
                    &[
                        ("type", "Battery"),
                        ("status", "Discharging"),
                        ("energy_now", "30000000"),
                        ("energy_full", "50000000"),
                        ("power_now", "-12000000"),
                    ],
                ),
                // A second battery waiting its turn still counts to the time.
                (
                    "BAT1",
                    &[
                        ("type", "Battery"),
                        ("status", "Unknown"),
                        ("energy_now", "20000000"),
                        ("energy_full", "20000000"),
                    ],
                ),
            ],
        );
        assert_eq!(
            read_batteries(&discharging),
            Some(BatteryReading {
                state: BatteryState::Discharging,
                watts: 12.0,
                watt_hours: 50.0
            })
        );
        // Counted in charge, filling to a limit of 80%: 4 Ah at 15 V rated is
        // 60 Wh full, 48 Wh at the limit, and 30 Wh are in.
        let charging = supplies(
            "in",
            &[(
                "BAT0",
                &[
                    ("type", "Battery"),
                    ("status", "Charging"),
                    ("charge_now", "2000000"),
                    ("charge_full", "4000000"),
                    ("voltage_min_design", "15000000"),
                    ("voltage_now", "16000000"),
                    ("current_now", "1500000"),
                    ("charge_control_end_threshold", "80"),
                ],
            )],
        );
        assert_eq!(
            read_batteries(&charging),
            Some(BatteryReading {
                state: BatteryState::Charging,
                watts: 24.0,
                watt_hours: 18.0
            })
        );
        let full = supplies(
            "full",
            &[(
                "BAT0",
                &[
                    ("type", "Battery"),
                    ("status", "Full"),
                    ("energy_now", "50000000"),
                    ("power_now", "0"),
                ],
            )],
        );
        assert_eq!(
            read_batteries(&full).map(|reading| reading.state),
            Some(BatteryState::Idle)
        );
        // Charging with nothing left to fill (a trickle at full, or at a
        // charge limit) is idle, not `0m` to go.
        let trickle = supplies(
            "trickle",
            &[(
                "BAT0",
                &[
                    ("type", "Battery"),
                    ("status", "Charging"),
                    ("energy_now", "50000000"),
                    ("energy_full", "50000000"),
                    ("power_now", "900000"),
                ],
            )],
        );
        assert_eq!(
            read_batteries(&trickle).map(|reading| reading.state),
            Some(BatteryState::Idle)
        );
        // A design voltage of 0 is not believed: the voltage now stands in.
        let unrated = supplies(
            "unrated",
            &[(
                "BAT0",
                &[
                    ("type", "Battery"),
                    ("status", "Discharging"),
                    ("charge_now", "2000000"),
                    ("voltage_min_design", "0"),
                    ("voltage_now", "15000000"),
                    ("current_now", "1000000"),
                ],
            )],
        );
        assert_eq!(
            read_batteries(&unrated).map(|reading| reading.watt_hours),
            Some(30.0)
        );
        for dir in [&trickle, &unrated] {
            let _ = std::fs::remove_dir_all(dir);
        }
        // A battery that does not say what it holds is no battery to time.
        let mute = supplies(
            "mute",
            &[
                mains,
                ("BAT0", &[("type", "Battery"), ("status", "Discharging")]),
            ],
        );
        assert_eq!(read_batteries(&mute), None);
        for dir in [discharging, charging, full, mute] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn the_battery_draw_is_averaged_so_the_time_left_holds_still() {
        // The first minutes off the mains: every reading counts the same.
        let mut average = 22.0;
        for (step, watts) in [40.0, 22.0, 22.0].into_iter().enumerate() {
            average = settle(average, watts, 2.0, 2.0 * (step + 1) as f64);
        }
        assert!((average - 28.0).abs() < 1e-9, "{average}");
        // Settled at 22 W, one spike to 60 W moves it by under 2 W...
        let spiked = settle(22.0, 60.0, 2.0, 600.0);
        assert!(spiked > 22.0 && spiked < 23.0, "{spiked}");
        // ...while a draw that stays up is taken in within a few minutes.
        let mut average = 22.0;
        for _ in 0..180 {
            average = settle(average, 40.0, 2.0, 600.0);
        }
        assert!(average > 39.0, "{average}");
    }

    #[test]
    fn nvidia_csv_keeps_every_gpu_and_clamps_what_the_driver_reports() {
        let values = parse_nvidia_gpus(
            "0, NVIDIA GeForce RTX 4060 Laptop GPU, 57, 43, 20, 8188, 1.66\n\
             1, NVIDIA GTX 1080, 101, 62, 4096, 8192, [N/A]\n",
        );
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].percent, Some(57.0));
        assert_eq!(values[0].temperature, Some(43.0));
        assert_eq!(
            values[0].memory,
            Some(Usage {
                used_kib: 20 * 1024,
                total_kib: 8188 * 1024
            })
        );
        assert_eq!(values[0].power, Some(1.66));
        assert_eq!(values[1].percent, Some(100.0));
        assert_eq!(values[1].temperature, Some(62.0));
        assert_eq!(values[1].power, None);
        // A name with a comma in it does not shift the columns along.
        let comma = parse_nvidia_gpus("0, NVIDIA RTX, Ada, 12, 40, 100, 4096, 5\n");
        assert_eq!(comma[0].percent, Some(12.0));
        assert_eq!(comma[0].temperature, Some(40.0));
        // "[N/A]" in one column does not take the others down with it.
        let hot = parse_nvidia_gpus("0, NVIDIA RTX A2000, [N/A], 51, [N/A], [N/A], [N/A]\n");
        assert_eq!(hot[0].percent, None);
        assert_eq!(hot[0].temperature, Some(51.0));
        assert_eq!(hot[0].memory, None);
        let busy = parse_nvidia_gpus("0, NVIDIA RTX A2000, 34, [N/A], 512, 6144, 20\n");
        assert_eq!(busy[0].percent, Some(34.0));
        assert_eq!(busy[0].temperature, None);
        assert!(busy[0].memory.is_some());
        // A line with nothing usable in any column is skipped rather than
        // shown as zero.
        assert!(
            parse_nvidia_gpus("0, NVIDIA RTX A2000, [N/A], [N/A], [N/A], [N/A], [N/A]\n")
                .is_empty()
        );
        assert!(parse_nvidia_gpus("nvidia-smi: command failed\n").is_empty());
    }

    #[test]
    fn a_virtual_machines_time_is_counted_once() {
        // guest (400) and guest_nice (0) are already inside user (1000).
        let raw = "cpu  1000 0 500 8000 100 0 0 0 400 0\nintr 1\n";
        assert_eq!(parse_cpu_lines(raw), Some(vec![(9600, 8100)]));
    }

    #[test]
    fn the_mount_table_gives_devices_and_where_they_are_mounted() {
        let raw = "/dev/nvme0n1p5 / ext4 rw,relatime 0 0\n\
                   proc /proc proc rw 0 0\n\
                   /dev/nvme1n1p1 /home ext4 rw 0 0\n\
                   /dev/sda1 /media/me/My\\040Disk vfat rw 0 0\n\
                   tmpfs /run tmpfs rw 0 0\n";
        assert_eq!(
            parse_mounts(raw),
            vec![
                ("/dev/nvme0n1p5".to_owned(), "/".to_owned()),
                ("/dev/nvme1n1p1".to_owned(), "/home".to_owned()),
                ("/dev/sda1".to_owned(), "/media/me/My Disk".to_owned()),
            ]
        );
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
            memory: None,
            power: None,
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
