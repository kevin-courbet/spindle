use crate::protocol::{self, SystemStatsResult};
use tokio::process::Command;

pub struct SystemService;

impl SystemService {
    pub async fn stats(_params: protocol::SystemStatsParams) -> Result<SystemStatsResult, String> {
        let (total_mb, used_mb) = get_memory_stats();
        let load_avg_1m = get_load_avg();
        let opencode_instances = get_opencode_instances().await;

        Ok(SystemStatsResult {
            load_avg_1m,
            memory_total_mb: total_mb,
            memory_used_mb: used_mb,
            opencode_instances,
        })
    }
}

fn get_memory_stats() -> (u32, u32) {
    if let Ok(content) = std::fs::read_to_string("/proc/meminfo") {
        let mut total = 0;
        let mut free = 0;
        let mut buffers = 0;
        let mut cached = 0;

        for line in content.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 2 {
                continue;
            }
            if let Ok(val) = parts[1].parse::<u32>() {
                match parts[0] {
                    "MemTotal:" => total = val,
                    "MemFree:" => free = val,
                    "Buffers:" => buffers = val,
                    "Cached:" => cached = val,
                    _ => {}
                }
            }
        }

        let used = total
            .saturating_sub(free)
            .saturating_sub(buffers)
            .saturating_sub(cached);
        return (total / 1024, used / 1024);
    }
    (0, 0)
}

fn get_load_avg() -> f64 {
    if let Ok(content) = std::fs::read_to_string("/proc/loadavg") {
        if let Some(first) = content.split_whitespace().next() {
            return first.parse::<f64>().unwrap_or(0.0);
        }
    }
    0.0
}

async fn get_opencode_instances() -> u32 {
    if let Ok(output) = Command::new("pgrep")
        .arg("-f")
        .arg("opencode")
        .output()
        .await
    {
        if output.status.success() {
            let s = String::from_utf8_lossy(&output.stdout);
            return s.lines().count() as u32;
        }
    }
    0
}
