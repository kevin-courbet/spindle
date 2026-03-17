use std::path::Path;

use crate::protocol::{self, SystemStatsResult};
use tokio::process::Command;

pub struct SystemService;

impl SystemService {
    pub async fn stats(_params: protocol::SystemStatsParams) -> Result<SystemStatsResult, String> {
        let ((total_mb, used_mb), load_avg_1m, opencode_instances) =
            tokio::join!(get_memory_stats(), get_load_avg(), get_opencode_instances());

        Ok(SystemStatsResult {
            load_avg_1m,
            memory_total_mb: total_mb,
            memory_used_mb: used_mb,
            opencode_instances,
        })
    }
}

async fn get_memory_stats() -> (u32, u32) {
    match tokio::fs::read_to_string("/proc/meminfo").await {
        Ok(content) => parse_memory_stats(&content),
        Err(_) => (0, 0),
    }
}

fn parse_memory_stats(content: &str) -> (u32, u32) {
    let mut total_kb = 0_u64;
    let mut free_kb = 0_u64;
    let mut buffers_kb = 0_u64;
    let mut cached_kb = 0_u64;

    for line in content.lines() {
        let mut parts = line.split_whitespace();
        let Some(name) = parts.next() else {
            continue;
        };
        let Some(value_raw) = parts.next() else {
            continue;
        };
        let Ok(value_kb) = value_raw.parse::<u64>() else {
            continue;
        };

        match name {
            "MemTotal:" => total_kb = value_kb,
            "MemFree:" => free_kb = value_kb,
            "Buffers:" => buffers_kb = value_kb,
            "Cached:" => cached_kb = value_kb,
            _ => {}
        }
    }

    let used_kb = total_kb
        .saturating_sub(free_kb)
        .saturating_sub(buffers_kb)
        .saturating_sub(cached_kb);

    (to_mb(total_kb), to_mb(used_kb))
}

async fn get_load_avg() -> f64 {
    match tokio::fs::read_to_string("/proc/loadavg").await {
        Ok(content) => parse_load_avg(&content),
        Err(_) => 0.0,
    }
}

fn parse_load_avg(content: &str) -> f64 {
    content
        .split_whitespace()
        .next()
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(0.0)
}

async fn get_opencode_instances() -> u32 {
    match Command::new("ps").args(["-eo", "args="]).output().await {
        Ok(output) if output.status.success() => {
            let table = String::from_utf8_lossy(&output.stdout);
            count_opencode_instances(&table)
        }
        _ => 0,
    }
}

fn count_opencode_instances(process_table: &str) -> u32 {
    process_table
        .lines()
        .filter(|line| is_opencode_command(line))
        .count() as u32
}

fn is_opencode_command(command_line: &str) -> bool {
    let mut args = command_line.split_whitespace();
    let Some(first) = args.next() else {
        return false;
    };

    if is_opencode_executable(first) {
        return true;
    }

    if !is_bun_executable(first) {
        return false;
    }

    args.any(is_opencode_executable)
}

fn is_opencode_executable(token: &str) -> bool {
    executable_name(token) == "opencode"
}

fn is_bun_executable(token: &str) -> bool {
    matches!(executable_name(token), "bun" | "bunx")
}

fn executable_name(token: &str) -> &str {
    Path::new(token)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(token)
}

fn to_mb(value_kb: u64) -> u32 {
    let mb = value_kb / 1024;
    u32::try_from(mb).unwrap_or(u32::MAX)
}
