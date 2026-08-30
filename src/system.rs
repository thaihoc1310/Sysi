use std::{
    collections::HashMap,
    fs, io,
    path::Path,
    process::{Command, Stdio},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SystemReadOptions {
    pub processes: bool,
    pub cores: bool,
    pub gpus: bool,
    pub root_disk: bool,
    pub home_disk: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GpuSnapshot {
    pub label: String,
    pub percent: f64,
}

#[derive(Clone, Debug, Default)]
pub struct SystemSnapshot {
    pub cpu_percent: f64,
    pub memory_percent: f64,
    pub gpus: Vec<GpuSnapshot>,
    pub root_disk_percent: Option<f64>,
    pub home_disk_percent: Option<f64>,
    pub cores: Vec<f64>,
    pub processes: Vec<ProcessSnapshot>,
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

        let (total_kib, available_kib) = read_memory().unwrap_or((0, 0));
        let used_kib = total_kib.saturating_sub(available_kib);
        let memory_percent = if total_kib == 0 {
            0.0
        } else {
            used_kib as f64 * 100.0 / total_kib as f64
        };
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

        SystemSnapshot {
            cpu_percent,
            memory_percent,
            gpus: if options.gpus {
                self.read_gpus()
            } else {
                Vec::new()
            },
            root_disk_percent: options.root_disk.then(|| read_disk_percent("/")).flatten(),
            home_disk_percent: options
                .home_disk
                .then(|| read_disk_percent("/home"))
                .flatten(),
            cores,
            processes,
        }
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
        number_repeated_labels(&mut gpus);
        gpus
    }

    fn read_nvidia_gpus(&mut self) -> Vec<GpuSnapshot> {
        if self.nvidia_missing {
            return Vec::new();
        }
        let output = Command::new("nvidia-smi")
            .args([
                "--query-gpu=index,name,utilization.gpu",
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
            let mut fields = line.split(',').map(str::trim);
            let _index = fields.next()?.parse::<usize>().ok()?;
            let percent = fields.next_back()?.parse::<f64>().ok()?;
            Some(GpuSnapshot {
                label: "NVIDIA".into(),
                percent: percent.clamp(0.0, 100.0),
            })
        })
        .collect()
}

/// A caption has to fit in the gap at the bottom of its ring, so a spelt-out
/// "NVIDIA GeForce RTX 4060 Laptop GPU" is no use. The vendor is what tells the
/// two cards of a hybrid laptop apart; only a machine with two from the same
/// vendor needs them numbered.
fn number_repeated_labels(gpus: &mut [GpuSnapshot]) {
    let mut totals: HashMap<&str, usize> = HashMap::new();
    for gpu in gpus.iter() {
        *totals.entry(gpu.label.as_str()).or_default() += 1;
    }
    let repeated: Vec<String> = totals
        .into_iter()
        .filter(|(_, total)| *total > 1)
        .map(|(label, _)| label.to_owned())
        .collect();
    for label in repeated {
        let mut nth = 0;
        for gpu in gpus.iter_mut().filter(|gpu| gpu.label == label) {
            nth += 1;
            gpu.label = format!("{label} {nth}");
        }
    }
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
        let Some(percent) = fs::read_to_string(device.join("gpu_busy_percent"))
            .ok()
            .and_then(|raw| raw.trim().parse::<f64>().ok())
        else {
            continue;
        };
        values.push(GpuSnapshot {
            label: "AMD".into(),
            percent: percent.clamp(0.0, 100.0),
        });
    }
    values
}

fn read_disk_percent(path: impl AsRef<Path>) -> Option<f64> {
    let path = path.as_ref();
    let total = fs2::total_space(path).ok()?;
    let available = fs2::available_space(path).ok()?;
    if total == 0 {
        return None;
    }
    Some((total.saturating_sub(available)) as f64 * 100.0 / total as f64)
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

fn read_memory() -> Option<(u64, u64)> {
    let raw = fs::read_to_string("/proc/meminfo").ok()?;
    let mut total = None;
    let mut available = None;
    for line in raw.lines() {
        let mut bits = line.split_whitespace();
        match bits.next()? {
            "MemTotal:" => total = bits.next().and_then(|v| v.parse().ok()),
            "MemAvailable:" => available = bits.next().and_then(|v| v.parse().ok()),
            _ => {}
        }
    }
    Some((total?, available?))
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
    // Kernel threads publish an empty cmdline; their `comm` is all there is.
    let argv0 = String::from_utf8_lossy(raw.split(|byte| *byte == 0).next()?);
    let argv0 = argv0.trim();
    // Servers such as postgres rewrite argv[0] into a status line with no path
    // in it. Splitting on '/' leaves that sentence intact, which is still the
    // most informative thing available.
    let name = argv0.rsplit('/').next()?.trim();
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
        number_repeated_labels, parse_nvidia_gpus, parse_process_memory_statm,
        process_name_from_cmdline, sort_processes, GpuSnapshot, ProcessSnapshot,
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
        // A rewritten argv[0] has no path to strip and stays as it is.
        assert_eq!(
            process_name_from_cmdline(b"postgres: checkpointer\0").as_deref(),
            Some("postgres: checkpointer")
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
            "0, NVIDIA GeForce RTX 4060 Laptop GPU, 57\n1, NVIDIA GTX 1080, 101\n",
        );
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].percent, 57.0);
        assert_eq!(values[1].percent, 100.0);
        // A line the driver mangled is skipped rather than shown as zero.
        assert!(parse_nvidia_gpus("nvidia-smi: command failed\n").is_empty());
    }

    #[test]
    fn two_cards_from_the_same_vendor_are_numbered_and_a_mixed_pair_is_not() {
        let gpu = |label: &str| GpuSnapshot {
            label: label.into(),
            percent: 0.0,
        };
        // The hybrid laptop this was written for: one of each, no numbering.
        let mut mixed = vec![gpu("NVIDIA"), gpu("AMD")];
        number_repeated_labels(&mut mixed);
        assert_eq!(mixed[0].label, "NVIDIA");
        assert_eq!(mixed[1].label, "AMD");

        let mut alike = vec![gpu("NVIDIA"), gpu("NVIDIA"), gpu("AMD")];
        number_repeated_labels(&mut alike);
        assert_eq!(alike[0].label, "NVIDIA 1");
        assert_eq!(alike[1].label, "NVIDIA 2");
        assert_eq!(alike[2].label, "AMD");
    }
}
