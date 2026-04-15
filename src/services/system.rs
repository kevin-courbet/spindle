use std::path::Path;

use crate::protocol::{self, SystemStatsResult};
use sysinfo::System;
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
    let mut sys = System::new();
    sys.refresh_memory();
    (bytes_to_mb(sys.total_memory()), bytes_to_mb(sys.used_memory()))
}

fn get_load_avg() -> f64 {
    System::load_average().one
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

fn bytes_to_mb(value_bytes: u64) -> u32 {
    let mb = value_bytes / (1024 * 1024);
    u32::try_from(mb).unwrap_or(u32::MAX)
}
