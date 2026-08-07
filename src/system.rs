use std::fs;

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemSnapshot {
    pub cpu_percent: f64,
    pub memory_percent: f64,
    pub memory_used_gib: f64,
    pub memory_total_gib: f64,
    pub load_one: f64,
}

#[derive(Default)]
pub struct SystemReader {
    previous_total: u64,
    previous_idle: u64,
}

impl SystemReader {
    pub fn read(&mut self) -> SystemSnapshot {
        let (total, idle) = read_cpu().unwrap_or((0, 0));
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
        let load_one = fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|s| s.split_whitespace().next()?.parse().ok())
            .unwrap_or(0.0);

        SystemSnapshot {
            cpu_percent,
            memory_percent,
            memory_used_gib: used_kib as f64 / 1_048_576.0,
            memory_total_gib: total_kib as f64 / 1_048_576.0,
            load_one,
        }
    }
}

fn read_cpu() -> Option<(u64, u64)> {
    let raw = fs::read_to_string("/proc/stat").ok()?;
    let mut values = raw.lines().next()?.split_whitespace();
    if values.next()? != "cpu" {
        return None;
    }
    let nums: Vec<u64> = values.filter_map(|v| v.parse().ok()).collect();
    if nums.len() < 5 {
        return None;
    }
    let idle = nums[3] + nums.get(4).copied().unwrap_or(0);
    Some((nums.iter().sum(), idle))
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
