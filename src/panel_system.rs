//! SYSTEM in the GNOME top bar. Sysi samples the machine (see `system.rs`)
//! and writes what the bar shows to a small file the shell extension watches;
//! the extension lays it out beside its gear, and its SYSTEM menu turns each
//! reading on and off by sending `system-metric:<key>:on|off` back.
//!
//! The bar reads in groups, one per kind of thing, and a caption per device:
//! `CPU 13% 56°C | RAM 48%  SWAP 1% | NVI 12% 45°C  AMD 1% 38°C | SAM 13% 42°C
//! UMI 45% 38°C | NET ↓9K ↑28K`.
//!
//! The file lives in the runtime directory, which is memory: it is rewritten
//! every couple of seconds and has no business on a disk.

use crate::state::{AppState, SystemDetails};
use crate::system::{BatteryState, SystemReadOptions, SystemReader, SystemSnapshot, Usage};
use gtk::glib;
use serde::Serialize;
use std::{cell::RefCell, fs, io, path::PathBuf, rc::Rc, time::Duration};

/// How often the bar is brought up to date.
const SAMPLE_EVERY: Duration = Duration::from_secs(2);

/// SYSTEM's side of the bar: samples while it is on, and answers the menu.
pub struct PanelSystem {
    state: Rc<RefCell<AppState>>,
    last: RefCell<Option<SystemSnapshot>>,
    /// What the file holds, so a sample that changed nothing is not written
    /// and wakes nobody in the shell.
    written: RefCell<String>,
    /// The first sample, taken with every reader on: which devices this
    /// machine has and what each can report. The bar's groups are laid out
    /// from it, and the readings it cannot give are not offered.
    machine: RefCell<Option<SystemSnapshot>>,
    request: async_channel::Sender<SystemReadOptions>,
}

impl PanelSystem {
    /// Start sampling. The readers run on a thread of their own: NVIDIA's are
    /// a helper process, and a slow driver must not stall the overlay.
    pub fn start(state: Rc<RefCell<AppState>>) -> Rc<Self> {
        let (request, requests) = async_channel::bounded::<SystemReadOptions>(1);
        let (snapshots_tx, snapshots) = async_channel::bounded::<SystemSnapshot>(1);
        let _ = std::thread::Builder::new()
            .name("sysi-system-sampler".into())
            .spawn(move || {
                let mut reader = SystemReader::default();
                while let Ok(options) = requests.recv_blocking() {
                    if snapshots_tx.send_blocking(reader.read(options)).is_err() {
                        break;
                    }
                }
            });
        let this = Rc::new(Self {
            state,
            last: RefCell::new(None),
            written: RefCell::new(String::new()),
            machine: RefCell::new(None),
            request,
        });
        glib::MainContext::default().spawn_local({
            let this = Rc::downgrade(&this);
            async move {
                while let Ok(snapshot) = snapshots.recv().await {
                    let Some(this) = this.upgrade() else {
                        break;
                    };
                    this.machine
                        .borrow_mut()
                        .get_or_insert_with(|| snapshot.clone());
                    *this.last.borrow_mut() = Some(snapshot);
                    this.publish();
                }
            }
        });
        // One sample with every reader on finds out what this machine has, so
        // the menu can leave out what it cannot show.
        let _ = this.request.try_send(read_everything());
        this.publish();
        glib::timeout_add_local(SAMPLE_EVERY, {
            let this = Rc::downgrade(&this);
            move || {
                let Some(this) = this.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                if this.state.borrow().settings.system {
                    this.sample();
                }
                glib::ControlFlow::Continue
            }
        });
        this
    }

    fn sample(&self) {
        let details = self.state.borrow().settings.system_details;
        let _ = self.request.try_send(read_options(&details));
    }

    fn publish(&self) {
        let data = self.state.borrow();
        let contents = render(
            data.settings.system,
            &data.settings.system_details,
            self.machine.borrow().as_ref(),
            self.last.borrow().as_ref(),
        );
        if *self.written.borrow() == contents {
            return;
        }
        if write(&contents).is_ok() {
            *self.written.borrow_mut() = contents;
        }
    }

    /// SYSTEM in the bar on or off; `None` flips it. The menu says which it
    /// wants rather than asking for a flip, so a click that arrives twice, or
    /// after another, cannot leave it the wrong way round.
    pub fn set_on(&self, on: Option<bool>) {
        let on = {
            let mut data = self.state.borrow_mut();
            data.settings.system = on.unwrap_or(!data.settings.system);
            let _ = data.save();
            data.settings.system
        };
        self.publish();
        if on {
            self.sample();
        }
    }

    /// One reading on or off by its key; `None` flips it.
    pub fn set_metric(&self, key: &str, on: Option<bool>) {
        {
            let mut data = self.state.borrow_mut();
            if !set(&mut data.settings.system_details, key, on) {
                return;
            }
            let _ = data.save();
        }
        // Shown at once, with its value from the next sample.
        self.publish();
        self.sample();
    }
}

impl PanelSystem {
    /// RAM, swap and drives as used/total, or as a percentage; `None` flips.
    pub fn set_amounts(&self, on: Option<bool>) {
        {
            let mut data = self.state.borrow_mut();
            let details = &mut data.settings.system_details;
            details.amounts = on.unwrap_or(!details.amounts);
            let _ = data.save();
        }
        self.publish();
    }
}

/// What a `system…` panel action asks for: `system:on`, `system-metric:ram:off`,
/// and the flips older extensions send (`toggle-system`, `system-metric:ram`).
pub fn apply_action(system: &PanelSystem, action: &str) -> bool {
    let wanted = |rest: &str| match rest {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    };
    if action == "toggle-system" {
        system.set_on(None);
    } else if let Some(rest) = action.strip_prefix("system:") {
        system.set_on(wanted(rest));
    } else if let Some(rest) = action.strip_prefix("system-amounts:") {
        system.set_amounts(wanted(rest));
    } else if let Some(rest) = action.strip_prefix("system-metric:") {
        match rest.rsplit_once(':') {
            Some((key, state)) if wanted(state).is_some() => system.set_metric(key, wanted(state)),
            _ => system.set_metric(rest, None),
        }
    } else {
        return false;
    }
    true
}

/// One reading the SYSTEM menu turns on and off, in the order the bar
/// shows them.
struct Metric {
    key: &'static str,
    /// What the menu calls it.
    name: &'static str,
    enabled: fn(&SystemDetails) -> bool,
    set: fn(&mut SystemDetails, bool),
}

const METRICS: [Metric; 12] = [
    Metric {
        key: "cpu",
        name: "cpu",
        enabled: |d| d.cpu,
        set: |d, on| d.cpu = on,
    },
    Metric {
        key: "cpu_temp",
        name: "cpu temp",
        enabled: |d| d.cpu_temp,
        set: |d, on| d.cpu_temp = on,
    },
    Metric {
        key: "ram",
        name: "ram",
        enabled: |d| d.ram,
        set: |d, on| d.ram = on,
    },
    Metric {
        key: "swap",
        name: "swap",
        enabled: |d| d.swap,
        set: |d, on| d.swap = on,
    },
    Metric {
        key: "gpus",
        name: "gpu",
        enabled: |d| d.gpus,
        set: |d, on| d.gpus = on,
    },
    Metric {
        key: "gpu_temp",
        name: "gpu temp",
        enabled: |d| d.gpu_temp,
        set: |d, on| d.gpu_temp = on,
    },
    Metric {
        key: "gpu_memory",
        name: "gpu memory",
        enabled: |d| d.gpu_memory,
        set: |d, on| d.gpu_memory = on,
    },
    Metric {
        key: "ssd_usage",
        name: "ssd",
        enabled: |d| d.ssd_usage,
        set: |d, on| d.ssd_usage = on,
    },
    Metric {
        key: "ssd_temp",
        name: "ssd temp",
        enabled: |d| d.ssd_temp,
        set: |d, on| d.ssd_temp = on,
    },
    Metric {
        key: "power",
        name: "power",
        enabled: |d| d.power,
        set: |d, on| d.power = on,
    },
    Metric {
        key: "battery_time",
        name: "battery time",
        enabled: |d| d.battery_time,
        set: |d, on| d.battery_time = on,
    },
    Metric {
        key: "network",
        name: "network",
        enabled: |d| d.network,
        set: |d, on| d.network = on,
    },
];

const PERCENT_WIDEST: &str = "99%";
const CELSIUS_WIDEST: &str = "99°C";
const NETWORK_WIDEST: &str = "↓888M ↑888M";
const WATTS_WIDEST: &str = "888W";
const TIME_LEFT_WIDEST: &str = "8h88";

/// A percentage, held at 99%: a full 100 says nothing 99 does not, and a
/// third digit is room the bar would keep free all day for it.
fn percent(value: f64) -> String {
    format!("{:.0}%", value.clamp(0.0, 99.0))
}

/// A temperature, held at 99°C: a part that hot is throttling or failing,
/// and a third digit is room the bar would keep free all day for nothing.
fn celsius(value: f64) -> String {
    format!("{:.0}°C", value.clamp(0.0, 99.0))
}

/// A throughput in three digits at most, the way the bar has room for:
/// `0K`, `340K`, `1.2M`, `88M`, `120M`. A tenth of a megabyte is only worth
/// its character below ten.
fn rate(bytes_per_sec: f64) -> String {
    let kilobytes = bytes_per_sec.max(0.0) / 1024.0;
    let megabytes = kilobytes / 1024.0;
    if kilobytes < 999.5 {
        format!("{kilobytes:.0}K")
    } else if megabytes < 9.95 {
        format!("{megabytes:.1}M")
    } else if megabytes < 999.5 {
        format!("{megabytes:.0}M")
    } else {
        format!("{:.1}G", megabytes / 1024.0)
    }
}

/// Watts in as few characters as the bar can spare: `6.2W`, `25W`, `150W`.
fn watts(value: f64) -> String {
    let value = value.clamp(0.0, 999.0);
    if value < 9.95 {
        format!("{value:.1}W")
    } else {
        format!("{value:.0}W")
    }
}

/// How long the battery lasts, no finer than the guess it is: `45m`, then
/// to five minutes (`2h30`), then to the hour (`12h`), held at 99h.
fn time_left(hours: f64) -> String {
    let minutes = (hours.clamp(0.0, 99.0) * 60.0).round() as u32;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let minutes = (minutes + 2) / 5 * 5;
    if minutes < 600 {
        format!("{}h{:02}", minutes / 60, minutes % 60)
    } else {
        format!("{}h", (minutes + 30) / 60)
    }
}

/// Memory is counted in powers of two (16 GiB of RAM is "16G"), drives in the
/// powers of ten they are sold in (a 1024 GB drive is "1T", not "954G").
#[derive(Clone, Copy)]
enum Units {
    Binary,
    Decimal,
}

/// A size in as few characters as the bar can spare: `9.5G`, `16G`, `512G`,
/// `1T`, `1.5T`.
fn size(kib: u64, units: Units) -> String {
    let bytes = kib as f64 * 1024.0;
    let (giga, step) = match units {
        Units::Binary => (1_073_741_824.0, 1024.0),
        Units::Decimal => (1e9, 1000.0),
    };
    let gigabytes = bytes / giga;
    if gigabytes < 0.995 {
        // Video memory is often under a gigabyte: 20M, 481M.
        return format!("{:.0}M", gigabytes * step);
    }
    if gigabytes >= 999.5 {
        let terabytes = gigabytes / step;
        let text = format!("{terabytes:.1}");
        format!("{}T", text.trim_end_matches(".0"))
    } else if gigabytes < 9.95 {
        let text = format!("{gigabytes:.1}");
        format!("{}G", text.trim_end_matches(".0"))
    } else {
        format!("{gigabytes:.0}G")
    }
}

/// How full something is, the way the menu asked: `48%` or `12G/16G`.
fn fullness(usage: Usage, units: Units, amounts: bool) -> String {
    if amounts {
        format!(
            "{}/{}",
            size(usage.used_kib, units),
            size(usage.total_kib, units)
        )
    } else {
        percent(usage.percent())
    }
}

/// The widest `fullness` can be for something of this size. Used can take
/// four characters whatever the total (`999M`, `9.5G`, `123G`, `1.5T`), and
/// `888M` is the widest of them, M being the widest unit. The total never
/// changes.
fn fullness_widest(total_kib: u64, units: Units, amounts: bool) -> String {
    if amounts {
        format!(
            "888M/{}",
            size(total_kib, units).replace(|c: char| c.is_ascii_digit(), "8")
        )
    } else {
        PERCENT_WIDEST.to_owned()
    }
}

/// A widest value cut to two digits wherever it has three or more: `999%` to
/// `88%`, `↓888M` to `↓88M`. Used/total is left whole.
fn usual(widest: &str) -> String {
    // Used over total: the total never changes, and the used part runs to
    // four characters (7.3G, 123G) as a matter of course.
    if widest.contains('/') {
        return widest.to_owned();
    }
    let mut out = String::new();
    let mut run = 0;
    for c in widest.chars() {
        if c.is_ascii_digit() {
            run += 1;
            if run > 2 {
                continue;
            }
            out.push('8');
        } else {
            run = 0;
            out.push(c);
        }
    }
    out
}

/// A device's caption: the first three letters of its maker (`NVI`, `AMD`,
/// `SAM`), with the number a repeated one was given; a drive known only by
/// its size keeps it.
fn caption(label: &str) -> String {
    let (name, number) = label
        .rsplit_once(' ')
        .filter(|(_, number)| number.bytes().all(|byte| byte.is_ascii_digit()))
        .unwrap_or((label, ""));
    if !name.starts_with(|c: char| c.is_alphabetic()) {
        return label.replace(' ', "");
    }
    let short: String = name
        .chars()
        .filter(|c| c.is_alphanumeric())
        .take(3)
        .collect();
    format!("{}{number}", short.to_uppercase())
}

#[derive(Serialize, Debug, PartialEq)]
struct Group {
    key: &'static str,
    devices: Vec<Device>,
}

#[derive(Serialize, Debug, PartialEq)]
struct Device {
    label: String,
    cells: Vec<Cell>,
}

/// One value a device can show, and the reading that turns it on.
#[derive(Serialize, Debug, PartialEq)]
struct Cell {
    metric: &'static str,
    /// The widest this value can be: the extension counts on it when it
    /// decides whether the next reading fits before the clock.
    widest: String,
    /// What it usually is at its widest, two digits: the width it is given at
    /// least, so the row only moves when a value grows a third digit.
    usual: String,
    /// `None` while its reading is off, or before a sample has it.
    value: Option<String>,
}

/// The bar's groups, laid out from what the machine has, with a value for
/// every cell whose reading is on and has been sampled.
fn groups(
    details: &SystemDetails,
    machine: &SystemSnapshot,
    last: Option<&SystemSnapshot>,
) -> Vec<Group> {
    let on = |key: &str| {
        METRICS
            .iter()
            .find(|metric| metric.key == key)
            .is_some_and(|metric| (metric.enabled)(details))
    };
    let cell = |metric: &'static str, widest: String, value: Option<String>| Cell {
        metric,
        usual: usual(&widest),
        widest,
        value: value.filter(|_| on(metric)),
    };
    let amounts = details.amounts;
    let mut cpu = vec![cell(
        "cpu",
        PERCENT_WIDEST.into(),
        last.map(|s| percent(s.cpu_percent)),
    )];
    if machine.cpu_temperature.is_some() {
        cpu.push(cell(
            "cpu_temp",
            CELSIUS_WIDEST.into(),
            last.and_then(|s| s.cpu_temperature).map(celsius),
        ));
    }
    let mut memory = vec![Device {
        label: "RAM".into(),
        cells: vec![cell(
            "ram",
            fullness_widest(machine.memory.total_kib, Units::Binary, amounts),
            last.map(|s| fullness(s.memory, Units::Binary, amounts)),
        )],
    }];
    if let Some(swap) = machine.swap {
        memory.push(Device {
            label: "SWAP".into(),
            cells: vec![cell(
                "swap",
                fullness_widest(swap.total_kib, Units::Binary, amounts),
                last.and_then(|s| s.swap)
                    .map(|swap| fullness(swap, Units::Binary, amounts)),
            )],
        });
    }
    let gpus = machine
        .gpus
        .iter()
        .map(|gpu| {
            let now = last.and_then(|s| s.gpus.iter().find(|now| now.label == gpu.label));
            let mut cells = Vec::new();
            if gpu.percent.is_some() {
                cells.push(cell(
                    "gpus",
                    PERCENT_WIDEST.into(),
                    now.and_then(|g| g.percent).map(percent),
                ));
            }
            if gpu.temperature.is_some() {
                cells.push(cell(
                    "gpu_temp",
                    CELSIUS_WIDEST.into(),
                    now.and_then(|g| g.temperature).map(celsius),
                ));
            }
            // Always used/total: as a percentage it would read like the load
            // beside it.
            if let Some(memory) = gpu.memory {
                cells.push(cell(
                    "gpu_memory",
                    fullness_widest(memory.total_kib, Units::Binary, true),
                    now.and_then(|g| g.memory)
                        .map(|memory| fullness(memory, Units::Binary, true)),
                ));
            }
            Device {
                label: caption(&gpu.label),
                cells,
            }
        })
        .collect();
    let drives = machine
        .drives
        .iter()
        .map(|drive| {
            let now = last.and_then(|s| s.drives.iter().find(|now| now.label == drive.label));
            let mut cells = Vec::new();
            if let Some(usage) = drive.usage {
                cells.push(cell(
                    "ssd_usage",
                    fullness_widest(usage.total_kib, Units::Decimal, amounts),
                    now.and_then(|d| d.usage)
                        .map(|usage| fullness(usage, Units::Decimal, amounts)),
                ));
            }
            if drive.temperature.is_some() {
                cells.push(cell(
                    "ssd_temp",
                    CELSIUS_WIDEST.into(),
                    now.and_then(|d| d.temperature).map(celsius),
                ));
            }
            Device {
                label: caption(&drive.label),
                cells,
            }
        })
        .collect();
    let network = Cell {
        // Kilobytes most of the time. Being last, the rate moves nothing when
        // it grows past that.
        usual: "↓88K ↑88K".into(),
        ..cell(
            "network",
            NETWORK_WIDEST.into(),
            last.and_then(|s| s.network).map(|rates| {
                format!(
                    "↓{} ↑{}",
                    rate(rates.down_bytes_per_sec),
                    rate(rates.up_bytes_per_sec)
                )
            }),
        )
    };
    vec![
        Group {
            key: "cpu",
            devices: vec![Device {
                label: "CPU".into(),
                cells: cpu,
            }],
        },
        Group {
            key: "memory",
            devices: memory,
        },
        Group {
            key: "gpu",
            devices: gpus,
        },
        Group {
            key: "ssd",
            devices: drives,
        },
        Group {
            key: "power",
            // On the battery the reading is the whole machine's; on mains it
            // is the processor package and the GPUs. The caption says which.
            // After it, how long the battery lasts (LEFT) or takes to fill
            // (FULL), while it is doing either.
            devices: machine
                .power
                .map(|_| {
                    let power = last.and_then(|s| s.power);
                    Device {
                        label: if power.is_some_and(|p| p.on_battery) {
                            "BAT".into()
                        } else {
                            "PWR".into()
                        },
                        cells: vec![cell(
                            "power",
                            WATTS_WIDEST.into(),
                            power.map(|p| watts(p.watts)),
                        )],
                    }
                })
                .into_iter()
                .chain(
                    last.and_then(|s| s.battery)
                        .filter(|_| machine.battery.is_some())
                        .and_then(|battery| {
                            let label = match battery.state {
                                BatteryState::Discharging => "LEFT",
                                BatteryState::Charging => "FULL",
                                BatteryState::Idle => return None,
                            };
                            Some(Device {
                                label: label.into(),
                                cells: vec![cell(
                                    "battery_time",
                                    TIME_LEFT_WIDEST.into(),
                                    battery.hours.map(time_left),
                                )],
                            })
                        }),
                )
                .collect(),
        },
        Group {
            key: "network",
            devices: vec![Device {
                label: "NET".into(),
                cells: vec![network],
            }],
        },
    ]
}

/// What the readers need to run for the readings that are on.
pub fn read_options(details: &SystemDetails) -> SystemReadOptions {
    SystemReadOptions {
        gpus: details.gpus,
        cpu_temp: details.cpu_temp,
        // One read of the GPUs answers for load, temperature and memory.
        gpu_temp: details.gpu_temp || details.gpu_memory,
        ssd_temp: details.ssd_temp,
        ssd_usage: details.ssd_usage,
        network: details.network,
        power: details.power,
        battery: details.battery_time,
    }
}

/// Every reader, for the one sample that finds out what this machine has.
pub fn read_everything() -> SystemReadOptions {
    SystemReadOptions {
        gpus: true,
        cpu_temp: true,
        gpu_temp: true,
        ssd_temp: true,
        ssd_usage: true,
        network: true,
        power: true,
        battery: true,
    }
}

/// Turn one reading on or off by its key; `None` flips it. False for a key
/// that is not one.
pub fn set(details: &mut SystemDetails, key: &str, on: Option<bool>) -> bool {
    let Some(metric) = METRICS.iter().find(|metric| metric.key == key) else {
        return false;
    };
    let on = on.unwrap_or(!(metric.enabled)(details));
    (metric.set)(details, on);
    true
}

#[derive(Serialize)]
struct Published<'a> {
    /// Whether SYSTEM is in the bar at all.
    on: bool,
    /// Used/total rather than percentages.
    amounts: bool,
    metrics: Vec<PublishedMetric<'a>>,
    groups: Vec<Group>,
}

#[derive(Serialize)]
struct PublishedMetric<'a> {
    key: &'a str,
    name: &'a str,
    on: bool,
    /// Whether any device can show it.
    available: bool,
}

fn render(
    on: bool,
    details: &SystemDetails,
    machine: Option<&SystemSnapshot>,
    last: Option<&SystemSnapshot>,
) -> String {
    // Until the first sample says what the machine has, there is nothing to
    // lay out, and every reading is offered.
    let groups = machine
        .map(|machine| groups(details, machine, last))
        .unwrap_or_default();
    let metrics = METRICS
        .iter()
        .map(|metric| PublishedMetric {
            key: metric.key,
            name: metric.name,
            on: (metric.enabled)(details),
            available: machine.is_none()
                // Plugged in and full, the battery has no time to show, but
                // the reading is still there to turn on and off.
                || (metric.key == "battery_time" && machine.is_some_and(|m| m.battery.is_some()))
                || groups.iter().any(|group| {
                    group
                        .devices
                        .iter()
                        .any(|device| device.cells.iter().any(|cell| cell.metric == metric.key))
                }),
        })
        .collect();
    serde_json::to_string(&Published {
        on,
        amounts: details.amounts,
        metrics,
        groups,
    })
    .unwrap_or_default()
}

fn path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(|dir| PathBuf::from(dir).join("sysi"))
        .unwrap_or_else(crate::state::cache_dir)
        .join("system.json")
}

/// Write what the bar should show. The extension reads the file each time a
/// write to it is done; it is rewritten whole.
fn write(contents: &str) -> io::Result<()> {
    let path = path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, contents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::{Battery, DriveSnapshot, GpuSnapshot, NetworkRates, PowerDraw};

    const GIB: u64 = 1024 * 1024;

    fn machine() -> SystemSnapshot {
        SystemSnapshot {
            cpu_percent: 12.6,
            memory: Usage {
                used_kib: 12 * GIB,
                total_kib: 16 * GIB,
            },
            swap: Some(Usage {
                used_kib: GIB / 10,
                total_kib: 2 * GIB,
            }),
            gpus: vec![
                GpuSnapshot {
                    label: "NVIDIA".into(),
                    percent: Some(12.0),
                    temperature: Some(45.0),
                    memory: Some(Usage {
                        used_kib: 20 * 1024,
                        total_kib: 8 * GIB,
                    }),
                    power: Some(1.66),
                },
                GpuSnapshot {
                    label: "AMD".into(),
                    percent: None,
                    temperature: Some(38.0),
                    memory: Some(Usage {
                        used_kib: 481 * 1024,
                        total_kib: 512 * 1024,
                    }),
                    power: Some(6.2),
                },
            ],
            cpu_temperature: Some(56.0),
            drives: vec![
                DriveSnapshot {
                    label: "SAMSUNG".into(),
                    // 90 GB used of a 1024 GB drive, in KiB.
                    usage: Some(Usage {
                        used_kib: 87_890_625,
                        total_kib: 1_000_000_000,
                    }),
                    temperature: Some(42.0),
                },
                DriveSnapshot {
                    label: "UMIS".into(),
                    usage: None,
                    temperature: Some(38.0),
                },
            ],
            network: Some(NetworkRates {
                down_bytes_per_sec: 9.0 * 1024.0,
                up_bytes_per_sec: 28.0 * 1024.0,
            }),
            power: Some(PowerDraw {
                watts: 7.86,
                on_battery: false,
            }),
            battery: Some(Battery {
                state: BatteryState::Charging,
                hours: Some(1.34),
            }),
        }
    }

    fn everything() -> SystemDetails {
        SystemDetails {
            cpu: true,
            cpu_temp: true,
            ram: true,
            swap: true,
            gpus: true,
            gpu_temp: true,
            gpu_memory: true,
            power: true,
            battery_time: true,
            ssd_usage: true,
            ssd_temp: true,
            network: true,
            amounts: false,
        }
    }

    fn nothing() -> SystemDetails {
        SystemDetails {
            cpu: false,
            ram: false,
            ..SystemDetails::default()
        }
    }

    /// The bar as text, the way the extension lays it out.
    fn bar(groups: &[Group]) -> String {
        groups
            .iter()
            .map(|group| {
                group
                    .devices
                    .iter()
                    .filter(|device| device.cells.iter().any(|cell| cell.value.is_some()))
                    .map(|device| {
                        let values: Vec<&str> = device
                            .cells
                            .iter()
                            .filter_map(|cell| cell.value.as_deref())
                            .collect();
                        format!("{} {}", device.label, values.join(" "))
                    })
                    .collect::<Vec<_>>()
                    .join("  ")
            })
            .filter(|group| !group.is_empty())
            .collect::<Vec<_>>()
            .join(" | ")
    }

    #[test]
    fn the_bar_reads_in_groups_with_a_caption_per_device() {
        let machine = machine();
        assert_eq!(
            bar(&groups(&everything(), &machine, Some(&machine))),
            "CPU 13% 56°C | RAM 75%  SWAP 5% | NVI 12% 45°C 20M/8G  AMD 38°C 481M/512M | SAM 9% 42°C  UMI 38°C | PWR 7.9W  FULL 1h20 | NET ↓9K ↑28K"
        );
    }

    #[test]
    fn a_reading_that_is_off_leaves_its_device_or_its_whole_group() {
        let machine = machine();
        let details = SystemDetails {
            cpu: true,
            gpu_temp: true,
            ..nothing()
        };
        assert_eq!(
            bar(&groups(&details, &machine, Some(&machine))),
            "CPU 13% | NVI 45°C  AMD 38°C"
        );
    }

    #[test]
    fn amounts_read_as_used_over_total_in_the_units_each_is_sold_in() {
        let machine = machine();
        let details = SystemDetails {
            ram: true,
            swap: true,
            ssd_usage: true,
            amounts: true,
            ..nothing()
        };
        assert_eq!(
            bar(&groups(&details, &machine, Some(&machine))),
            "RAM 12G/16G  SWAP 102M/2G | SAM 90G/1T"
        );
    }

    #[test]
    fn the_draw_says_whether_it_is_the_battery_or_the_chips() {
        let details = SystemDetails {
            power: true,
            ..nothing()
        };
        let mut machine = machine();
        assert_eq!(bar(&groups(&details, &machine, Some(&machine))), "PWR 7.9W");
        let unplugged = SystemSnapshot {
            power: Some(PowerDraw {
                watts: 16.4,
                on_battery: true,
            }),
            ..machine.clone()
        };
        assert_eq!(
            bar(&groups(&details, &machine, Some(&unplugged))),
            "BAT 16W"
        );
        // A machine that can report neither is not offered the reading.
        machine.power = None;
        assert!(groups(&details, &machine, None)
            .iter()
            .find(|group| group.key == "power")
            .is_some_and(|group| group.devices.is_empty()));
    }

    #[test]
    fn the_battery_time_says_which_way_the_battery_goes() {
        let details = SystemDetails {
            power: true,
            battery_time: true,
            ..nothing()
        };
        let machine = machine();
        let at = |state, hours| SystemSnapshot {
            power: Some(PowerDraw {
                watts: 16.4,
                on_battery: state == BatteryState::Discharging,
            }),
            battery: Some(Battery { state, hours }),
            ..machine.clone()
        };
        let read = |last: &SystemSnapshot| bar(&groups(&details, &machine, Some(last)));
        assert_eq!(
            read(&at(BatteryState::Discharging, Some(2.49))),
            "BAT 16W  LEFT 2h30"
        );
        assert_eq!(
            read(&at(BatteryState::Charging, Some(0.75))),
            "PWR 16W  FULL 45m"
        );
        // Plugged in and full, there is no time to tell...
        let full = at(BatteryState::Idle, None);
        assert_eq!(read(&full), "PWR 16W");
        // ...but the reading is still offered, to be on when it unplugs.
        let offered = |machine: &SystemSnapshot| {
            let json: serde_json::Value =
                serde_json::from_str(&render(true, &details, Some(machine), Some(&full))).unwrap();
            json["metrics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|metric| metric["key"] == "battery_time" && metric["available"] == true)
        };
        assert!(offered(&machine));
        // A machine with no battery is not.
        assert!(!offered(&SystemSnapshot {
            battery: None,
            ..machine.clone()
        }));
        // Off, it is not on the bar, whichever way the battery goes.
        let off = SystemDetails {
            battery_time: false,
            ..details
        };
        assert_eq!(
            bar(&groups(
                &off,
                &machine,
                Some(&at(BatteryState::Discharging, Some(2.49)))
            )),
            "BAT 16W"
        );
    }

    #[test]
    fn the_time_left_is_no_finer_than_the_guess() {
        assert_eq!(time_left(0.0), "0m");
        assert_eq!(time_left(44.6 / 60.0), "45m");
        assert_eq!(time_left(59.4 / 60.0), "59m");
        assert_eq!(time_left(59.6 / 60.0), "1h00");
        assert_eq!(time_left(2.0 + 32.0 / 60.0), "2h30");
        assert_eq!(time_left(2.0 + 33.0 / 60.0), "2h35");
        assert_eq!(time_left(9.0 + 58.0 / 60.0), "10h");
        assert_eq!(time_left(12.4), "12h");
        assert_eq!(time_left(500.0), "99h");
        assert_eq!(time_left(f64::NAN), "0m");
    }

    #[test]
    fn watts_take_four_characters_at_most() {
        assert_eq!(watts(0.0), "0.0W");
        assert_eq!(watts(6.21), "6.2W");
        assert_eq!(watts(9.96), "10W");
        assert_eq!(watts(150.4), "150W");
        assert_eq!(watts(5000.0), "999W");
    }

    #[test]
    fn a_rate_never_takes_more_than_three_digits() {
        let kib = 1024.0;
        let mib = 1024.0 * kib;
        assert_eq!(rate(500.0), "0K");
        assert_eq!(rate(340.0 * kib), "340K");
        assert_eq!(rate(999.4 * kib), "999K");
        assert_eq!(rate(1.25 * mib), "1.2M");
        assert_eq!(rate(9.94 * mib), "9.9M");
        assert_eq!(rate(88.0 * mib), "88M");
        assert_eq!(rate(120.0 * mib), "120M");
        for bytes in [0.0, 999.6 * kib, 9.96 * mib, 999.4 * mib] {
            assert!(
                rate(bytes).chars().count() <= "888M".chars().count(),
                "{}",
                rate(bytes)
            );
        }
    }

    #[test]
    fn a_value_is_usually_given_room_for_two_digits() {
        assert_eq!(usual("99%"), "88%");
        assert_eq!(usual("99°C"), "88°C");
        assert_eq!(usual("888M/88.8T"), "888M/88.8T");
        assert_eq!(usual("↓888M ↑888M"), "↓88M ↑88M");
    }

    #[test]
    fn makers_are_cut_to_three_letters_and_repeats_keep_their_number() {
        assert_eq!(caption("NVIDIA"), "NVI");
        assert_eq!(caption("SAMSUNG"), "SAM");
        assert_eq!(caption("AMD 2"), "AMD2");
        assert_eq!(caption("512G"), "512G");
        assert_eq!(caption("1TB 1"), "1TB1");
    }

    #[test]
    fn what_the_machine_lacks_is_not_offered() {
        let mut machine = machine();
        machine.swap = None;
        machine.drives.clear();
        let json: serde_json::Value =
            serde_json::from_str(&render(true, &everything(), Some(&machine), None)).unwrap();
        let offered = |key: &str| {
            json["metrics"]
                .as_array()
                .unwrap()
                .iter()
                .find(|metric| metric["key"] == key)
                .unwrap()["available"]
                == true
        };
        assert!(offered("cpu") && offered("gpus") && offered("network"));
        assert!(!offered("swap") && !offered("ssd_usage") && !offered("ssd_temp"));
    }

    #[test]
    fn asking_for_on_twice_leaves_it_on() {
        // The menu says which way it wants a reading, so a click delivered
        // twice cannot flip it back off the way a toggle did.
        let mut details = SystemDetails::default();
        assert!(set(&mut details, "gpus", Some(true)));
        assert!(set(&mut details, "gpus", Some(true)));
        assert!(details.gpus);
        assert!(set(&mut details, "gpus", Some(false)));
        assert!(!details.gpus);
        // An older extension still flips.
        assert!(set(&mut details, "gpus", None));
        assert!(details.gpus);
        assert!(!set(&mut details, "processes", Some(true)));
    }

    #[test]
    fn every_value_fits_the_width_its_cell_is_given() {
        // Wider than the template, the bar would jump as the digits changed.
        let mut worst = machine();
        worst.cpu_percent = 100.0;
        worst.cpu_temperature = Some(100.0);
        worst.memory = Usage {
            used_kib: 15 * GIB + GIB / 2,
            total_kib: 16 * GIB,
        };
        worst.swap = Some(Usage {
            used_kib: 9 * GIB / 10,
            total_kib: 2 * GIB,
        });
        worst.drives[0].usage = Some(Usage {
            used_kib: 999_000_000,
            total_kib: 1_000_000_000,
        });
        worst.network = Some(NetworkRates {
            down_bytes_per_sec: 999.4 * 1_048_576.0,
            up_bytes_per_sec: 999.4 * 1024.0,
        });
        for amounts in [false, true] {
            let details = SystemDetails {
                amounts,
                ..everything()
            };
            for group in groups(&details, &machine(), Some(&worst)) {
                for device in group.devices {
                    for cell in device.cells {
                        let value = cell.value.unwrap_or_default();
                        assert!(
                            value.chars().count() <= cell.widest.chars().count(),
                            "{}: {value} is wider than {}",
                            cell.metric,
                            cell.widest
                        );
                    }
                }
            }
        }
    }
}
