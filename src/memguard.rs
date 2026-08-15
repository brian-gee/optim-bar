//! Memory-pressure watchdog.
//!
//! Commit charge, not physical RAM, is what actually falls over. On 2026-08-14
//! a leaking service walked commit to 55.3 of 56.6 GB and the box was unusable
//! — even `Get-CimInstance` timed out — before anything said a word. This
//! samples quietly and toasts once while there's still room to act.
//!
//! Attribution goes through `NtQuerySystemInformation`, not
//! `OpenProcess` + `GetProcessMemoryInfo`: the leaker was a LocalService
//! svchost, which a non-elevated process cannot open at all (verified: access
//! denied even for QUERY_LIMITED_INFORMATION), so the PSAPI route would have
//! skipped it and confidently named the largest *user* process instead.

use std::time::Duration;

use windows::Wdk::System::SystemInformation::{NtQuerySystemInformation, SYSTEM_INFORMATION_CLASS};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::WindowsProgramming::SYSTEM_PROCESS_INFORMATION;

use crate::config::BarConfig;
use crate::toast;

/// `SystemProcessInformation`.
const PROCESS_INFO_CLASS: SYSTEM_INFORMATION_CLASS = SYSTEM_INFORMATION_CLASS(5);
const CHECK_EVERY: Duration = Duration::from_secs(30);
/// Commit must climb this much further before we speak up again.
const ESCALATION_PCT: f32 = 5.0;
/// And must fall this far below the threshold before we re-arm.
const RECOVERY_PCT: f32 = 5.0;

/// Commit charge as (used, limit) in bytes.
fn commit_charge() -> Option<(u64, u64)> {
    unsafe {
        let mut ms = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        GlobalMemoryStatusEx(&mut ms).ok()?;
        Some((
            ms.ullTotalPageFile.saturating_sub(ms.ullAvailPageFile),
            ms.ullTotalPageFile,
        ))
    }
}

/// Every process's (name, commit bytes, private page bytes).
fn process_list() -> Option<Vec<(String, u64, u64)>> {
    unsafe {
        let mut size: u32 = 1 << 20;
        let mut buf: Vec<u8> = Vec::new();
        let mut ok = false;
        // Processes come and go between the sizing call and the real one, so
        // ask for headroom and retry rather than trusting the first answer.
        for _ in 0..5 {
            buf.clear();
            buf.resize(size as usize, 0);
            let mut needed: u32 = 0;
            let status = NtQuerySystemInformation(
                PROCESS_INFO_CLASS,
                buf.as_mut_ptr() as *mut _,
                size,
                &mut needed,
            );
            if status.is_ok() {
                ok = true;
                break;
            }
            if needed <= size {
                return None; // failed for a reason more buffer won't fix
            }
            size = needed.saturating_add(needed / 4);
        }
        if !ok {
            return None;
        }

        let mut offset = 0usize;
        let mut out = Vec::new();
        loop {
            if offset + std::mem::size_of::<SYSTEM_PROCESS_INFORMATION>() > buf.len() {
                break;
            }
            let p = &*(buf.as_ptr().add(offset) as *const SYSTEM_PROCESS_INFORMATION);
            let name = if p.ImageName.Buffer.is_null() || p.ImageName.Length == 0 {
                // The kernel's own entry carries no image name.
                "System".to_string()
            } else {
                String::from_utf16_lossy(std::slice::from_raw_parts(
                    p.ImageName.Buffer.0,
                    (p.ImageName.Length / 2) as usize,
                ))
            };
            out.push((name, p.PagefileUsage as u64, p.PrivatePageCount as u64));
            if p.NextEntryOffset == 0 {
                break;
            }
            offset += p.NextEntryOffset as usize;
        }
        Some(out)
    }
}

/// Largest consumer of commit, as (name, bytes). Commit is the metric that
/// matters here: the leaking svchost showed 7.7 GB of working set in Task
/// Manager while holding 19.2 GB of commit.
fn top_process() -> Option<(String, u64)> {
    process_list()?
        .into_iter()
        .max_by_key(|(_, commit, _)| *commit)
        .map(|(name, commit, _)| (name, commit))
}

/// Diagnostic behind `--mem-top`: the ranking the watchdog is working from.
pub fn dump_top(n: usize) {
    let Some(mut list) = process_list() else {
        println!("process list unavailable");
        return;
    };
    list.sort_by_key(|(_, commit, _)| std::cmp::Reverse(*commit));
    if let Some((used, limit)) = commit_charge() {
        println!(
            "commit {:.1} of {:.1} GB ({:.0}%)",
            gb(used),
            gb(limit),
            used as f32 / limit.max(1) as f32 * 100.0
        );
    }
    println!("{:>10}  {:>10}  {}", "commit", "private", "process");
    for (name, commit, private) in list.into_iter().take(n) {
        println!(
            "{:>9.1}G  {:>9.1}G  {}",
            gb(commit),
            gb(private),
            name
        );
    }
}

/// Should this sample toast, and what alert level do we carry forward?
///
/// `last` is the commit percentage we last warned at, or None when disarmed.
/// Pure so the hysteresis is testable without waiting on a real leak.
fn evaluate(pct: f32, threshold: f32, last: Option<f32>) -> (bool, Option<f32>) {
    if pct < threshold - RECOVERY_PCT {
        return (false, None); // recovered: arm again
    }
    if pct < threshold {
        return (false, last); // dead band: no news either way
    }
    match last {
        None => (true, Some(pct)),
        Some(prev) if pct >= prev + ESCALATION_PCT => (true, Some(pct)),
        Some(prev) => (false, Some(prev)),
    }
}

fn gb(bytes: u64) -> f32 {
    bytes as f32 / (1024.0 * 1024.0 * 1024.0)
}

pub fn install(cfg: &BarConfig) {
    let values = &cfg.values;
    if values.get_or("memguard", "enabled", "true") == "false" {
        return;
    }
    // 90 rather than 85: this machine idles near 79% with a game up, and a
    // watchdog that cries during normal load is one you learn to ignore.
    let threshold = values
        .get_f32("memguard", "threshold_pct", 90.0)
        .clamp(50.0, 99.0);
    std::thread::spawn(move || run(threshold));
}

fn run(threshold: f32) {
    let mut last: Option<f32> = None;
    loop {
        std::thread::sleep(CHECK_EVERY);
        let Some((used, limit)) = commit_charge() else {
            continue;
        };
        if limit == 0 {
            continue;
        }
        let pct = used as f32 / limit as f32 * 100.0;
        let (alert, next) = evaluate(pct, threshold, last);
        last = next;
        if !alert {
            continue;
        }
        let body = match top_process() {
            Some((name, bytes)) => format!(
                "{:.0}% committed ({:.1} of {:.1} GB). Biggest: {} at {:.1} GB.",
                pct,
                gb(used),
                gb(limit),
                name,
                gb(bytes)
            ),
            None => format!(
                "{:.0}% committed ({:.1} of {:.1} GB).",
                pct,
                gb(used),
                gb(limit)
            ),
        };
        toast::show("Memory pressure", &body);
    }
}

#[cfg(test)]
mod tests {
    use super::{evaluate, top_process};

    #[test]
    fn quiet_until_the_threshold() {
        assert_eq!(evaluate(70.0, 85.0, None), (false, None));
        assert_eq!(evaluate(84.9, 85.0, None), (false, None));
    }

    #[test]
    fn warns_once_then_only_on_escalation() {
        let (alert, state) = evaluate(86.0, 85.0, None);
        assert!(alert);
        // Sitting at the same level must not toast every 30 seconds.
        let (alert, state) = evaluate(87.0, 85.0, state);
        assert!(!alert);
        let (alert, state) = evaluate(89.9, 85.0, state);
        assert!(!alert);
        // Materially worse is worth interrupting for again.
        let (alert, state) = evaluate(91.0, 85.0, state);
        assert!(alert);
        assert_eq!(state, Some(91.0));
    }

    #[test]
    fn re_arms_only_after_real_recovery() {
        let (_, state) = evaluate(90.0, 85.0, None);
        // Still hovering under the line: stay armed, don't re-warn on a blip.
        let (alert, state) = evaluate(83.0, 85.0, state);
        assert!(!alert);
        assert_eq!(state, Some(90.0));
        let (alert, state) = evaluate(86.0, 85.0, state);
        assert!(!alert, "86% is not worse than the 90% we already warned at");
        // Properly recovered, so the next climb speaks up again.
        let (_, state) = evaluate(60.0, 85.0, state);
        assert_eq!(state, None);
        assert!(evaluate(86.0, 85.0, state).0);
    }

    /// The whole point of using NtQuerySystemInformation: it must see
    /// processes this one cannot open, including protected service hosts.
    #[test]
    fn finds_the_biggest_process_on_this_machine() {
        let (name, bytes) = top_process().expect("process list should be readable");
        assert!(!name.is_empty());
        assert!(bytes > 8 * 1024 * 1024, "{name} reported only {bytes} bytes");
    }
}
