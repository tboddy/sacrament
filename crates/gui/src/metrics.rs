//! Throughput and memory instrumentation.
//!
//! **Opt-in.** Set `SACRAMENT_METRICS=1` to mirror these to stderr; otherwise
//! nothing is collected or printed and the app has no visible diagnostics
//! surface. They earned their keep proving the grid was viable, and they'll
//! earn it again when the editor surface lands, but they are not UI.
//!
//! Three things matter:
//!
//! - **feed time** — how long `alacritty_terminal` takes to parse PTY bytes.
//!   This is the cost that scales with output volume.
//! - **bytes/sec** — whether we keep up with a flood at all.
//! - **footprint** — the memory question. Physical footprint, not RSS; see
//!   `resident_bytes` for why that distinction cost us a wrong conclusion once.

use std::time::{Duration, Instant};

pub struct Metrics {
    enabled: bool,
    started: Instant,
    feeds: u64,
    bytes: u64,
    feed_time: Duration,
    worst_feed: Duration,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            enabled: std::env::var_os("SACRAMENT_METRICS").is_some(),
            started: Instant::now(),
            feeds: 0,
            bytes: 0,
            feed_time: Duration::ZERO,
            worst_feed: Duration::ZERO,
        }
    }

    pub fn reset(&mut self) {
        let enabled = self.enabled;
        *self = Self::new();
        self.enabled = enabled;
    }

    pub fn record_feed(&mut self, bytes: usize, elapsed: Duration) {
        if !self.enabled {
            return;
        }
        self.feeds += 1;
        self.bytes += bytes as u64;
        self.feed_time += elapsed;
        if elapsed > self.worst_feed {
            self.worst_feed = elapsed;
        }
    }

    pub fn render(&self) -> String {
        let secs = self.started.elapsed().as_secs_f64().max(0.001);
        let mib = self.bytes as f64 / (1024.0 * 1024.0);
        let avg_us = if self.feeds > 0 {
            self.feed_time.as_secs_f64() * 1e6 / self.feeds as f64
        } else {
            0.0
        };
        format!(
            "{mib:.1} MiB in {} chunks ({:.1} MiB/s)  │  \
             feed avg {avg_us:.0}µs worst {:.1}ms  │  mem {}",
            self.feeds,
            mib / secs,
            self.worst_feed.as_secs_f64() * 1000.0,
            rss_human(),
        )
    }
}

fn rss_human() -> String {
    match resident_bytes() {
        Some(b) => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
        None => "n/a".to_string(),
    }
}

/// Physical footprint — the number Activity Monitor's "Memory" column shows.
///
/// **Not** RSS. RSS counts every resident page including shared system
/// frameworks and file-backed mappings (notably the memory-mapped font files
/// cosmic-text touches), which for a GUI app massively overstates what the
/// process actually costs: this spike measured ~117 MB RSS against ~30 MB
/// footprint. Footprint is dirty, process-owned memory, so it's the honest
/// number and the one to compare against v1.
#[cfg(target_os = "macos")]
fn resident_bytes() -> Option<u64> {
    use std::os::raw::{c_int, c_void};

    // proc_pid_rusage(RUSAGE_INFO_V2) -> struct rusage_info_v2. Layout starts
    // with ri_uuid[16], then u64 fields; ri_phys_footprint is the 8th u64.
    unsafe extern "C" {
        fn proc_pid_rusage(pid: c_int, flavor: c_int, buffer: *mut c_void) -> c_int;
    }
    const RUSAGE_INFO_V2: c_int = 2;
    const BUF_SIZE: usize = 512;
    const PHYS_FOOTPRINT_OFFSET: usize = 72;

    let mut buf = vec![0u8; BUF_SIZE];
    let pid = std::process::id() as c_int;
    let ret =
        unsafe { proc_pid_rusage(pid, RUSAGE_INFO_V2, buf.as_mut_ptr() as *mut c_void) };
    if ret != 0 {
        return None;
    }
    let bytes: [u8; 8] = buf[PHYS_FOOTPRINT_OFFSET..PHYS_FOOTPRINT_OFFSET + 8]
        .try_into()
        .ok()?;
    Some(u64::from_ne_bytes(bytes))
}

#[cfg(target_os = "linux")]
fn resident_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn resident_bytes() -> Option<u64> {
    None
}

impl Metrics {
    /// Throttled stderr logging: every 200th chunk, and only when enabled.
    /// Frequent enough to see a trend during a flood, rare enough that the
    /// logging isn't itself the bottleneck.
    pub fn should_log(&self) -> bool {
        self.enabled && self.feeds > 0 && self.feeds.is_multiple_of(200)
    }
}
