use std::{collections::HashMap, fs};

#[derive(Clone, Debug, Default)]
pub struct SystemSnapshot {
    pub cpu_percent: f64,
    pub memory_percent: f64,
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
}

impl SystemReader {
    pub fn read(&mut self, include_processes: bool, include_cores: bool) -> SystemSnapshot {
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
        let cores = if include_cores {
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
        let processes = if include_processes {
            self.read_processes(delta_total)
        } else {
            self.previous_process_ticks.clear();
            Vec::new()
        };

        SystemSnapshot {
            cpu_percent,
            memory_percent,
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
            let Some((name, ticks)) = read_process_stat(pid) else {
                continue;
            };
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
        processes.sort_by(|left, right| {
            right
                .memory_kib
                .cmp(&left.memory_kib)
                .then_with(|| right.cpu_percent.total_cmp(&left.cpu_percent))
        });
        processes
    }
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
    use super::parse_process_memory_statm;

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
}
