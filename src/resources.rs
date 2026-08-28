//! Per-session resource monitoring (CPU, memory) and GPU usage, for ALL tmux
//! sessions (not just showrunner-managed ones).
//!
//! CPU/mem are computed by walking each session's pane process trees via
//! `/proc`. GPU usage comes from `nvidia-smi`.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// CPU + memory usage attributed to a tmux session.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SessionResources {
    /// Total CPU across the session's process tree, as a percentage of a
    /// single core (can exceed 100 on multi-core work).
    pub cpu_percent: f32,
    /// Resident set size summed over the session's process tree, in KiB.
    pub mem_kb: u64,
}

/// Per-session CPU/mem for every tmux session on the server.
pub fn all_sessions_resources() -> Vec<(String, SessionResources)> {
    sample_sessions(&crate::tmux::list_all_tmux_sessions()).into_iter().collect()
}

/// CPU + memory for a single tmux session: sum over all descendants of the
/// session's pane processes.
pub fn session_resources(session_name: &str) -> SessionResources {
    sample_sessions(&[session_name.to_string()])
        .get(session_name)
        .copied()
        .unwrap_or_default()
}

/// Batched sampler: CPU + memory for a set of tmux sessions in ONE pass.
///
/// Builds the ppid map once, collects each session's descendant pids, then
/// takes a single ~200ms CPU sample across every pid together and one RSS
/// pass — so N sessions cost about one sample, not N. Sessions with no live
/// pids are simply absent from the result.
pub fn sample_sessions(names: &[String]) -> HashMap<String, SessionResources> {
    let mut out = HashMap::new();
    if names.is_empty() {
        return out;
    }

    let map = build_ppid_map();

    // pid -> session name, so each sampled pid can be attributed back.
    let mut owners: HashMap<u32, &str> = HashMap::new();
    let mut all_pids: Vec<u32> = Vec::new();
    for name in names {
        let pane_pids = crate::tmux::list_pane_pids(name);
        if pane_pids.is_empty() {
            continue;
        }
        for pid in descendants(&pane_pids, &map) {
            owners.insert(pid, name);
            all_pids.push(pid);
        }
    }
    let active: HashSet<&str> = owners.values().copied().collect();
    if all_pids.is_empty() {
        return out;
    }

    // CPU: sample utime+stime per pid, ~200ms apart, in a single pass over all
    // sessions' pids. utime/stime are in USER_HZ ticks (typically 100/s), so
    // ticks/sec == percentage of one core.
    let ticks1: HashMap<u32, u64> = all_pids
        .iter()
        .map(|&pid| (pid, read_cpu_ticks(pid).unwrap_or(0)))
        .collect();
    let start = Instant::now();
    std::thread::sleep(Duration::from_millis(200));
    let mut cpu: HashMap<&str, u64> = HashMap::new();
    let mut mem: HashMap<&str, u64> = HashMap::new();
    for &pid in &all_pids {
        let name = owners[&pid];
        // If the pid vanished between passes, read_cpu_ticks is None: contribute 0.
        let Some(ticks2) = read_cpu_ticks(pid) else {
            continue;
        };
        // If the first read missed this pid, treat its baseline as the second
        // read so its full-lifetime CPU isn't counted inside the ~200ms window
        // (otherwise the first sample spikes).
        let base = ticks1.get(&pid).copied().unwrap_or(ticks2);
        *cpu.entry(name).or_insert(0) += ticks2.saturating_sub(base);
        *mem.entry(name).or_insert(0) += read_rss_kb(pid);
    }
    let elapsed = start.elapsed().as_secs_f32();

    for name in names.iter().filter(|n| active.contains(n.as_str())) {
        let ticks = cpu.get(name.as_str()).copied().unwrap_or(0);
        out.insert(
            name.clone(),
            SessionResources {
                cpu_percent: if elapsed > 0.0 {
                    ticks as f32 / elapsed
                } else {
                    0.0
                },
                mem_kb: mem.get(name.as_str()).copied().unwrap_or(0),
            },
        );
    }
    out
}

/// Build a ppid -> children map by scanning /proc/*/stat.
fn build_ppid_map() -> HashMap<u32, Vec<u32>> {
    let mut map: HashMap<u32, Vec<u32>> = HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return map;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let pid: u32 = match name.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            if let Some(close) = stat.rfind(')') {
                let rest = &stat[close + 1..];
                if let Some(ppid) = rest
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.parse::<u32>().ok())
                {
                    map.entry(ppid).or_default().push(pid);
                }
            }
        }
    }
    map
}

/// All pids in the tree rooted at `roots`, inclusive (DFS over children).
fn descendants(roots: &[u32], map: &HashMap<u32, Vec<u32>>) -> Vec<u32> {
    let mut out = Vec::new();
    let mut stack = roots.to_vec();
    while let Some(pid) = stack.pop() {
        out.push(pid);
        if let Some(children) = map.get(&pid) {
            stack.extend(children.iter().copied());
        }
    }
    out
}

/// (utime, stime) from /proc/<pid>/stat, or None if the process vanished.
fn read_cpu_ticks(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm can contain spaces/parens; fields resume after the last ')'.
    let close = stat.rfind(')')?;
    let rest = &stat[close + 1..];
    let mut it = rest.split_whitespace();
    // After comm: field 3 (state) is token 0 ... field 14 (utime) is token 11,
    // field 15 (stime) is token 12.
    let utime = it.nth(11)?.parse::<u64>().ok()?;
    let stime = it.next()?.parse::<u64>().ok()?;
    Some(utime + stime)
}

/// Resident set size in KiB from /proc/<pid>/status. `VmRSS:` is already in
/// KiB, so no page-size assumption — correct on 4K/16K/64K-page kernels.
fn read_rss_kb(pid: u32) -> u64 {
    let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return 0;
    };
    status
        .lines()
        .find_map(|line| {
            let mut it = line.split_whitespace();
            (it.next() == Some("VmRSS:")).then(|| {
                it.next().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0)
            })
        })
        .unwrap_or(0)
}

/// Whether nvidia-smi is available.
pub fn gpu_available() -> bool {
    std::process::Command::new("nvidia-smi")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// (pid, used GPU memory MiB) for every process currently using a GPU, from
/// `nvidia-smi`. Empty if nvidia-smi is missing or errors.
pub fn gpu_processes() -> Vec<(u32, u64)> {
    let Ok(out) = std::process::Command::new("nvidia-smi")
        .args([
            "--query-compute-apps=pid,used_gpu_memory",
            "--format=csv,noheader,nounits",
        ])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    parse_gpu_processes(&String::from_utf8_lossy(&out.stdout))
}

/// Parse `nvidia-smi --query-compute-apps=pid,used_gpu_memory` CSV output.
fn parse_gpu_processes(csv: &str) -> Vec<(u32, u64)> {
    csv.lines()
        .filter_map(|line| {
            let mut parts = line.split(',');
            let pid = parts.next()?.trim().parse::<u32>().ok()?;
            let mem = parts.next()?.trim().parse::<u64>().ok()?;
            Some((pid, mem))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nvidia_smi_compute_apps() {
        let csv = "1234, 512 MiB\n5678, 1024 MiB\n";
        // Our parser expects plain integers (nounits); feed the nounits form.
        let csv2 = "1234, 512\n5678, 1024\n";
        assert_eq!(parse_gpu_processes(csv2), vec![(1234, 512), (5678, 1024)]);
        // A header line is skipped.
        let with_header = "pid, used_gpu_memory\n9, 64\n";
        assert_eq!(parse_gpu_processes(with_header), vec![(9, 64)]);
        // Non-numeric / malformed lines are dropped.
        assert_eq!(parse_gpu_processes("abc, xyz\n12, bad\n"), Vec::<(u32, u64)>::new());
        // Lines with a unit suffix are dropped too (we query with `nounits`,
        // so a suffix means unexpected output).
        assert_eq!(parse_gpu_processes("1234, 512 MiB\n"), Vec::<(u32, u64)>::new());
        let _ = csv; // silence unused for the illustrative sample
    }

    #[test]
    fn empty_input_gives_no_processes() {
        assert_eq!(parse_gpu_processes(""), Vec::<(u32, u64)>::new());
    }

    #[test]
    fn sample_sessions_empty_or_unknown_is_empty() {
        // No names at all.
        assert!(sample_sessions(&[]).is_empty());
        // A name with no tmux session yields no entry (and no 200ms sample).
        assert!(sample_sessions(&[String::from("no-such-session-xyz")]).is_empty());
    }

    // Runtime check against a real tmux session (requires tmux; skipped by
    // default, run with: cargo test -- --ignored resources::runtime).
    #[test]
    #[ignore]
    fn runtime_session_resources() {
        use std::process::Command;

        let _ = Command::new("tmux")
            .args(["new-session", "-d", "-s", "_sr_runtime"])
            .output();
        let _ = Command::new("tmux")
            .args([
                "send-keys",
                "-t",
                "_sr_runtime",
                "python3 -c 'import time; [time.sleep(0.001) for _ in range(200000)]'",
                "Enter",
            ])
            .output();
        std::thread::sleep(std::time::Duration::from_millis(800));

        let all = all_sessions_resources();
        let found = all.iter().any(|(n, _)| n == "_sr_runtime");
        assert!(found, "session _sr_runtime should appear; got: {all:?}");

        let r = session_resources("_sr_runtime");
        assert!(r.mem_kb > 0, "expected some memory, got {}", r.mem_kb);

        let _ = Command::new("tmux")
            .args(["kill-session", "-t", "_sr_runtime"])
            .output();
    }
}
