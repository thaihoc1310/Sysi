//! SYSTEM in the GNOME top bar. Sysi samples the machine (see `system.rs`)
//! and writes what the bar shows to a small file the shell extension watches;
//! the extension lays it out beside its gear, and its SYSTEM menu turns each
//! reading on and off by sending `system-metric:<key>` back.
//!
//! The file lives in the runtime directory, which is memory: it is rewritten
//! every couple of seconds and has no business on a disk.

use crate::state::{AppState, SystemDetails};
use crate::system::{SystemReadOptions, SystemReader, SystemSnapshot};
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
    available: RefCell<Option<Vec<(&'static str, usize)>>>,
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
            available: RefCell::new(None),
            request,
        });
        glib::MainContext::default().spawn_local({
            let this = Rc::downgrade(&this);
            async move {
                while let Ok(snapshot) = snapshots.recv().await {
                    let Some(this) = this.upgrade() else {
                        break;
                    };
                    this.available
                        .borrow_mut()
                        .get_or_insert_with(|| available(&snapshot));
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
        let available = self.available.borrow();
        let contents = render(
            data.settings.system,
            &data.settings.system_details,
            self.last.borrow().as_ref(),
            available.as_deref(),
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

/// One reading the bar can show, in the order it shows them.
struct Metric {
    key: &'static str,
    /// What the SYSTEM menu calls it.
    name: &'static str,
    /// The caption in front of the value in the bar.
    label: &'static str,
    /// The widest one value can be, so the extension can give the reading a
    /// fixed width and the bar does not shuffle every time a digit changes.
    widest: &'static str,
    enabled: fn(&SystemDetails) -> bool,
    set: fn(&mut SystemDetails, bool),
    /// One value per device: two GPUs read `30% 12%`. Empty when this
    /// machine has none to read.
    values: fn(&SystemSnapshot) -> Vec<String>,
}

const METRICS: [Metric; 10] = [
    Metric {
        key: "cpu",
        name: "cpu",
        label: "CPU",
        widest: "100%",
        enabled: |d| d.cpu,
        set: |d, on| d.cpu = on,
        values: |s| vec![percent(s.cpu_percent)],
    },
    Metric {
        key: "cpu_temp",
        name: "cpu temp",
        label: "CPU",
        widest: "100°C",
        enabled: |d| d.cpu_temp,
        set: |d, on| d.cpu_temp = on,
        values: |s| s.cpu_temperature.map(celsius).into_iter().collect(),
    },
    Metric {
        key: "ram",
        name: "ram",
        label: "RAM",
        widest: "100%",
        enabled: |d| d.ram,
        set: |d, on| d.ram = on,
        values: |s| vec![percent(s.memory_percent)],
    },
    Metric {
        key: "swap",
        name: "swap",
        label: "SWAP",
        widest: "100%",
        enabled: |d| d.swap,
        set: |d, on| d.swap = on,
        values: |s| s.swap.map(|swap| percent(swap.percent())).into_iter().collect(),
    },
    Metric {
        key: "gpus",
        name: "gpu",
        label: "GPU",
        widest: "100%",
        enabled: |d| d.gpus,
        set: |d, on| d.gpus = on,
        values: |s| s.gpus.iter().filter_map(|gpu| gpu.percent).map(percent).collect(),
    },
    Metric {
        key: "gpu_temp",
        name: "gpu temp",
        label: "GPU",
        widest: "100°C",
        enabled: |d| d.gpu_temp,
        set: |d, on| d.gpu_temp = on,
        values: |s| s.gpus.iter().filter_map(|gpu| gpu.temperature).map(celsius).collect(),
    },
    Metric {
        key: "ssd_temp",
        name: "ssd temp",
        label: "SSD",
        widest: "100°C",
        enabled: |d| d.ssd_temp,
        set: |d, on| d.ssd_temp = on,
        values: |s| {
            s.storage_temperatures
                .iter()
                .map(|(_, value)| celsius(*value))
                .collect()
        },
    },
    Metric {
        key: "root_disk",
        name: "disk /",
        label: "DISK",
        widest: "100%",
        enabled: |d| d.root_disk,
        set: |d, on| d.root_disk = on,
        values: |s| s.root_disk.map(|disk| percent(disk.percent())).into_iter().collect(),
    },
    Metric {
        key: "home_disk",
        name: "disk /home",
        label: "HOME",
        widest: "100%",
        enabled: |d| d.home_disk,
        set: |d, on| d.home_disk = on,
        values: |s| s.home_disk.map(|disk| percent(disk.percent())).into_iter().collect(),
    },
    Metric {
        key: "network",
        name: "network",
        label: "NET",
        widest: "↓888.8M ↑888.8M",
        enabled: |d| d.network,
        set: |d, on| d.network = on,
        values: |s| {
            s.network
                .map(|rates| {
                    format!(
                        "↓{} ↑{}",
                        rate(rates.down_bytes_per_sec),
                        rate(rates.up_bytes_per_sec)
                    )
                })
                .into_iter()
                .collect()
        },
    },
];

fn percent(value: f64) -> String {
    format!("{:.0}%", value.clamp(0.0, 100.0))
}

fn celsius(value: f64) -> String {
    format!("{value:.0}°C")
}

/// A throughput short enough for the bar: `1.2M`, `340K`, `0K`.
fn rate(bytes_per_sec: f64) -> String {
    let rate = bytes_per_sec.max(0.0);
    if rate >= 1_048_576.0 {
        format!("{:.1}M", rate / 1_048_576.0)
    } else {
        format!("{:.0}K", rate / 1024.0)
    }
}

/// What the readers need to run for the readings that are on.
pub fn read_options(details: &SystemDetails) -> SystemReadOptions {
    SystemReadOptions {
        gpus: details.gpus,
        cpu_temp: details.cpu_temp,
        gpu_temp: details.gpu_temp,
        ssd_temp: details.ssd_temp,
        root_disk: details.root_disk,
        home_disk: details.home_disk,
        network: details.network,
    }
}

/// Every reader, for the one sample that finds out what this machine has.
pub fn read_everything() -> SystemReadOptions {
    SystemReadOptions {
        gpus: true,
        cpu_temp: true,
        gpu_temp: true,
        ssd_temp: true,
        root_disk: true,
        home_disk: true,
        network: true,
    }
}

/// Which readings this machine can give at all, and how many devices each
/// has, from a sample taken with every reader on. A rate needs two samples,
/// so the network always can.
pub fn available(snapshot: &SystemSnapshot) -> Vec<(&'static str, usize)> {
    METRICS
        .iter()
        .filter_map(|metric| {
            let count = (metric.values)(snapshot).len();
            let count = if metric.key == "network" { 1 } else { count };
            (count > 0).then_some((metric.key, count))
        })
        .collect()
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
    metrics: Vec<PublishedMetric<'a>>,
}

#[derive(Serialize)]
struct PublishedMetric<'a> {
    key: &'a str,
    name: &'a str,
    label: &'a str,
    /// The widest the whole reading can be: one `widest` per device.
    widest: String,
    on: bool,
    available: bool,
    /// `None` until a sample has it, or while it is off.
    value: Option<String>,
}

fn render(
    on: bool,
    details: &SystemDetails,
    snapshot: Option<&SystemSnapshot>,
    available: Option<&[(&str, usize)]>,
) -> String {
    let metrics = METRICS
        .iter()
        .map(|metric| {
            let enabled = (metric.enabled)(details);
            let devices = available
                .and_then(|keys| keys.iter().find(|(key, _)| *key == metric.key))
                .map(|(_, count)| *count);
            PublishedMetric {
                key: metric.key,
                name: metric.name,
                label: metric.label,
                widest: vec![metric.widest; devices.unwrap_or(1).max(1)].join(" "),
                on: enabled,
                // Unknown until the first sample: offer it rather than hide it.
                available: available.is_none() || devices.is_some(),
                value: snapshot
                    .filter(|_| enabled)
                    .map(|snapshot| (metric.values)(snapshot).join(" "))
                    .filter(|value| !value.is_empty()),
            }
        })
        .collect();
    serde_json::to_string(&Published { on, metrics }).unwrap_or_default()
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
    use crate::system::{GpuSnapshot, NetworkRates, Usage};

    fn sample() -> SystemSnapshot {
        SystemSnapshot {
            cpu_percent: 12.4,
            memory_percent: 48.6,
            swap: None,
            gpus: vec![
                GpuSnapshot {
                    label: "NVIDIA".into(),
                    percent: Some(30.0),
                    temperature: Some(54.4),
                },
                GpuSnapshot {
                    label: "AMD".into(),
                    percent: Some(12.0),
                    temperature: Some(61.0),
                },
            ],
            cpu_temperature: Some(61.0),
            storage_temperatures: vec![("SSD 1".into(), 38.0), ("SSD 2".into(), 44.0)],
            root_disk: Some(Usage {
                used_kib: 62,
                total_kib: 100,
            }),
            network: Some(NetworkRates {
                down_bytes_per_sec: 1.25 * 1_048_576.0,
                up_bytes_per_sec: 40.0 * 1024.0,
            }),
            ..SystemSnapshot::default()
        }
    }

    fn published(json: &serde_json::Value, key: &str, field: &str) -> serde_json::Value {
        json["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|metric| metric["key"] == key)
            .unwrap()[field]
            .clone()
    }

    #[test]
    fn the_bar_gets_short_values_for_the_readings_that_are_on() {
        let details = SystemDetails {
            cpu: true,
            ram: true,
            network: true,
            ..SystemDetails::default()
        };
        let json: serde_json::Value =
            serde_json::from_str(&render(true, &details, Some(&sample()), None)).unwrap();
        assert_eq!(json["on"], true);
        assert_eq!(published(&json, "cpu", "value"), "12%");
        assert_eq!(published(&json, "ram", "value"), "49%");
        assert_eq!(published(&json, "network", "value"), "↓1.2M ↑40K");
        // Off, so nothing is sampled or shown for it.
        assert_eq!(published(&json, "gpus", "value"), serde_json::Value::Null);
    }

    #[test]
    fn every_gpu_and_every_drive_is_shown_and_given_room() {
        let details = SystemDetails {
            gpus: true,
            gpu_temp: true,
            ssd_temp: true,
            ..SystemDetails::default()
        };
        let snapshot = sample();
        let keys = available(&snapshot);
        let json: serde_json::Value = serde_json::from_str(&render(
            true,
            &details,
            Some(&snapshot),
            Some(&keys),
        ))
        .unwrap();
        assert_eq!(published(&json, "gpus", "value"), "30% 12%");
        assert_eq!(published(&json, "gpu_temp", "value"), "54°C 61°C");
        assert_eq!(published(&json, "ssd_temp", "value"), "38°C 44°C");
        assert_eq!(published(&json, "gpu_temp", "widest"), "100°C 100°C");
        assert_eq!(published(&json, "cpu", "widest"), "100%");
    }

    #[test]
    fn a_machine_without_swap_or_home_does_not_offer_them() {
        let keys: Vec<&str> = available(&sample()).into_iter().map(|(key, _)| key).collect();
        assert!(keys.contains(&"cpu") && keys.contains(&"gpus") && keys.contains(&"network"));
        assert!(!keys.contains(&"swap"));
        assert!(!keys.contains(&"home_disk"));
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
    fn every_value_fits_the_width_its_metric_is_given() {
        // Wider than the template, the bar would jump as the digits changed.
        let widest = SystemSnapshot {
            cpu_percent: 100.0,
            memory_percent: 100.0,
            cpu_temperature: Some(100.0),
            network: Some(NetworkRates {
                down_bytes_per_sec: 888.8 * 1_048_576.0,
                up_bytes_per_sec: 888.8 * 1_048_576.0,
            }),
            ..sample()
        };
        for metric in &METRICS {
            for value in (metric.values)(&widest) {
                assert!(
                    value.chars().count() <= metric.widest.chars().count(),
                    "{}: {value} is wider than {}",
                    metric.key,
                    metric.widest
                );
            }
        }
    }
}
