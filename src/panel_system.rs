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
    available: RefCell<Option<Vec<&'static str>>>,
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
        let _ = publish(
            data.settings.system,
            &data.settings.system_details,
            self.last.borrow().as_ref(),
            available.as_deref(),
        );
    }

    /// SYSTEM in the bar, on or off.
    pub fn toggle_on(&self) {
        let on = {
            let mut data = self.state.borrow_mut();
            data.settings.system ^= true;
            let _ = data.save();
            data.settings.system
        };
        self.publish();
        if on {
            self.sample();
        }
    }

    /// One reading on or off, by its key.
    pub fn toggle_metric(&self, key: &str) {
        {
            let mut data = self.state.borrow_mut();
            if !toggle(&mut data.settings.system_details, key) {
                return;
            }
            let _ = data.save();
        }
        // Shown at once, with its value from the next sample.
        self.publish();
        self.sample();
    }
}

/// One reading the bar can show, in the order it shows them.
struct Metric {
    key: &'static str,
    /// What the SYSTEM menu calls it.
    name: &'static str,
    /// The caption in front of the value in the bar.
    label: &'static str,
    /// The widest value it can take, so the extension can give it a fixed
    /// width and the bar does not shuffle every time a digit changes.
    widest: &'static str,
    enabled: fn(&SystemDetails) -> bool,
    toggle: fn(&mut SystemDetails),
    value: fn(&SystemSnapshot) -> Option<String>,
}

const METRICS: [Metric; 10] = [
    Metric {
        key: "cpu",
        name: "CPU",
        label: "CPU",
        widest: "100%",
        enabled: |d| d.cpu,
        toggle: |d| d.cpu ^= true,
        value: |s| Some(percent(s.cpu_percent)),
    },
    Metric {
        key: "cpu_temp",
        name: "CPU temp",
        label: "CPU",
        widest: "100°C",
        enabled: |d| d.cpu_temp,
        toggle: |d| d.cpu_temp ^= true,
        value: |s| s.cpu_temperature.map(celsius),
    },
    Metric {
        key: "ram",
        name: "RAM",
        label: "RAM",
        widest: "100%",
        enabled: |d| d.ram,
        toggle: |d| d.ram ^= true,
        value: |s| Some(percent(s.memory_percent)),
    },
    Metric {
        key: "swap",
        name: "Swap",
        label: "SWAP",
        widest: "100%",
        enabled: |d| d.swap,
        toggle: |d| d.swap ^= true,
        value: |s| s.swap.map(|swap| percent(swap.percent())),
    },
    Metric {
        key: "gpus",
        name: "GPU",
        label: "GPU",
        widest: "100%",
        enabled: |d| d.gpus,
        toggle: |d| d.gpus ^= true,
        value: |s| s.gpus.iter().find_map(|gpu| gpu.percent).map(percent),
    },
    Metric {
        key: "gpu_temp",
        name: "GPU temp",
        label: "GPU",
        widest: "100°C",
        enabled: |d| d.gpu_temp,
        toggle: |d| d.gpu_temp ^= true,
        value: |s| s.gpus.iter().find_map(|gpu| gpu.temperature).map(celsius),
    },
    Metric {
        key: "ssd_temp",
        name: "SSD temp",
        label: "SSD",
        widest: "100°C",
        enabled: |d| d.ssd_temp,
        toggle: |d| d.ssd_temp ^= true,
        // The hottest drive is the one worth a glance.
        value: |s| {
            s.storage_temperatures
                .iter()
                .map(|(_, celsius)| *celsius)
                .reduce(f64::max)
                .map(celsius)
        },
    },
    Metric {
        key: "root_disk",
        name: "Disk /",
        label: "DISK",
        widest: "100%",
        enabled: |d| d.root_disk,
        toggle: |d| d.root_disk ^= true,
        value: |s| s.root_disk.map(|disk| percent(disk.percent())),
    },
    Metric {
        key: "home_disk",
        name: "Disk /home",
        label: "HOME",
        widest: "100%",
        enabled: |d| d.home_disk,
        toggle: |d| d.home_disk ^= true,
        value: |s| s.home_disk.map(|disk| percent(disk.percent())),
    },
    Metric {
        key: "network",
        name: "Network",
        label: "NET",
        widest: "↓888.8M ↑888.8M",
        enabled: |d| d.network,
        toggle: |d| d.network ^= true,
        value: |s| {
            s.network.map(|rates| {
                format!(
                    "↓{} ↑{}",
                    rate(rates.down_bytes_per_sec),
                    rate(rates.up_bytes_per_sec)
                )
            })
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

/// Which readings this machine can give at all, from a sample taken with
/// every reader on. A rate needs two samples, so the network always can.
pub fn available(snapshot: &SystemSnapshot) -> Vec<&'static str> {
    METRICS
        .iter()
        .filter(|metric| metric.key == "network" || (metric.value)(snapshot).is_some())
        .map(|metric| metric.key)
        .collect()
}

/// Flip one reading by its key. False for a key that is not one.
pub fn toggle(details: &mut SystemDetails, key: &str) -> bool {
    let Some(metric) = METRICS.iter().find(|metric| metric.key == key) else {
        return false;
    };
    (metric.toggle)(details);
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
    widest: &'a str,
    on: bool,
    available: bool,
    /// `None` until a sample has it, or while it is off.
    value: Option<String>,
}

fn render(
    on: bool,
    details: &SystemDetails,
    snapshot: Option<&SystemSnapshot>,
    available: Option<&[&str]>,
) -> String {
    let metrics = METRICS
        .iter()
        .map(|metric| {
            let enabled = (metric.enabled)(details);
            PublishedMetric {
                key: metric.key,
                name: metric.name,
                label: metric.label,
                widest: metric.widest,
                on: enabled,
                // Unknown until the first sample: offer it rather than hide it.
                available: available.is_none_or(|keys| keys.contains(&metric.key)),
                value: snapshot
                    .filter(|_| enabled)
                    .and_then(|snapshot| (metric.value)(snapshot)),
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

/// Write what the bar should show. The extension reads the file whenever it
/// changes; it is rewritten whole each time, and a torn read just waits for
/// the next one.
pub fn publish(
    on: bool,
    details: &SystemDetails,
    snapshot: Option<&SystemSnapshot>,
    available: Option<&[&str]>,
) -> io::Result<()> {
    let path = path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, render(on, details, snapshot, available))
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
            gpus: vec![GpuSnapshot {
                label: "GPU".into(),
                percent: Some(30.0),
                temperature: Some(54.4),
            }],
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

    #[test]
    fn the_bar_gets_short_values_for_the_readings_that_are_on() {
        let details = SystemDetails {
            cpu: true,
            ram: true,
            ssd_temp: true,
            network: true,
            ..SystemDetails::default()
        };
        let json: serde_json::Value =
            serde_json::from_str(&render(true, &details, Some(&sample()), None)).unwrap();
        let value = |key: &str| {
            json["metrics"]
                .as_array()
                .unwrap()
                .iter()
                .find(|metric| metric["key"] == key)
                .unwrap()["value"]
                .clone()
        };
        assert_eq!(json["on"], true);
        assert_eq!(value("cpu"), "12%");
        assert_eq!(value("ram"), "49%");
        assert_eq!(value("ssd_temp"), "44°C");
        assert_eq!(value("network"), "↓1.2M ↑40K");
        // Off, so nothing is sampled or shown for it.
        assert_eq!(value("gpus"), serde_json::Value::Null);
    }

    #[test]
    fn a_machine_without_swap_or_home_does_not_offer_them() {
        let keys = available(&sample());
        assert!(keys.contains(&"cpu") && keys.contains(&"gpus") && keys.contains(&"network"));
        assert!(!keys.contains(&"swap"));
        assert!(!keys.contains(&"home_disk"));
    }

    #[test]
    fn a_metric_is_flipped_by_its_key_and_nothing_else() {
        let mut details = SystemDetails::default();
        let before = details.gpus;
        assert!(toggle(&mut details, "gpus"));
        assert_ne!(details.gpus, before);
        assert!(!toggle(&mut details, "processes"));
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
            if let Some(value) = (metric.value)(&widest) {
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
