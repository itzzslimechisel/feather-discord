//! Logging with secret redaction, and startup timing marks for the benchmark harness.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static START: OnceLock<(Instant, f64)> = OnceLock::new();
static BENCH: AtomicBool = AtomicBool::new(false);

/// Records the reference point for marks. `process_age_ms` is how long the process had
/// already been running when `main` started (from the OS where available).
pub fn init_clock() {
    START.get_or_init(|| (Instant::now(), process_age_ms().unwrap_or(0.0)));
    BENCH.store(std::env::var_os("FEATHER_BENCH").is_some(), Ordering::Relaxed);
}

/// Milliseconds since the process was created.
pub fn since_launch_ms() -> f64 {
    let (t0, age) = START.get_or_init(|| (Instant::now(), 0.0));
    age + t0.elapsed().as_secs_f64() * 1000.0
}

pub fn bench_enabled() -> bool {
    BENCH.load(Ordering::Relaxed)
}

/// Emits `FEATHER_MARK <name> <ms since process creation>` once per name when benchmarking.
pub fn mark(name: &str) {
    static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
    if !bench_enabled() {
        return;
    }
    let mut seen = SEEN.lock().unwrap();
    if seen.iter().any(|s| s == name) {
        return;
    }
    seen.push(name.to_string());
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "FEATHER_MARK {name} {:.1}", since_launch_ms());
    let _ = out.flush();
}

/// Free-form benchmark output line (`FEATHER_STAT key value`).
pub fn stat(key: &str, value: impl std::fmt::Display) {
    if bench_enabled() {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "FEATHER_STAT {key} {value}");
        let _ = out.flush();
    }
}

#[cfg(target_os = "linux")]
fn process_age_ms() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let start_ticks: f64 = stat[stat.rfind(')')? + 2..].split_whitespace().nth(19)?.parse().ok()?;
    let uptime: f64 = std::fs::read_to_string("/proc/uptime").ok()?.split_whitespace().next()?.parse().ok()?;
    // SAFETY: sysconf is always safe to call.
    let hz = unsafe { libc_sysconf_clk_tck() } as f64;
    Some(((uptime - start_ticks / hz) * 1000.0).max(0.0))
}

#[cfg(target_os = "linux")]
unsafe fn libc_sysconf_clk_tck() -> i64 {
    extern "C" {
        fn sysconf(name: i32) -> i64;
    }
    const SC_CLK_TCK: i32 = 2;
    sysconf(SC_CLK_TCK)
}

#[cfg(not(target_os = "linux"))]
fn process_age_ms() -> Option<f64> {
    None
}

/// Replaces anything shaped like a Discord token (three dot-separated base64 segments,
/// the first decoding to a user id) or an `Authorization` header value with `[redacted]`.
pub fn redact(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, word) in s.split(' ').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let core = word.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-'));
        if looks_like_token(core) {
            out.push_str(&word.replace(core, "[redacted]"));
        } else {
            out.push_str(word);
        }
    }
    out
}

fn looks_like_token(w: &str) -> bool {
    let parts: Vec<&str> = w.split('.').collect();
    parts.len() == 3
        && parts[0].len() >= 16
        && parts[2].len() >= 20
        && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
}

struct Logger {
    file: Option<Mutex<std::fs::File>>,
    level: log::LevelFilter,
}

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= self.level && (m.target().starts_with("feather") || m.level() <= log::Level::Warn)
    }
    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let line = redact(&format!("{:>5} {}: {}\n", r.level(), r.target(), r.args()));
        let _ = std::io::stderr().write_all(line.as_bytes());
        if let Some(f) = &self.file {
            let _ = f.lock().unwrap().write_all(line.as_bytes());
        }
    }
    fn flush(&self) {}
}

/// Installs the logger. Level comes from `FEATHER_LOG` (error|warn|info|debug), default warn.
/// The log file is truncated when it exceeds 1 MiB so it can never grow without bound.
pub fn init_logging(log_path: Option<std::path::PathBuf>) {
    let level = match std::env::var("FEATHER_LOG").as_deref() {
        Ok("debug") => log::LevelFilter::Debug,
        Ok("info") => log::LevelFilter::Info,
        Ok("error") => log::LevelFilter::Error,
        Ok("trace") => log::LevelFilter::Trace,
        _ => log::LevelFilter::Warn,
    };
    let file = log_path.and_then(|p| {
        if std::fs::metadata(&p).map(|m| m.len() > 1 << 20).unwrap_or(false) {
            let _ = std::fs::remove_file(&p);
        }
        std::fs::OpenOptions::new().create(true).append(true).open(p).ok().map(Mutex::new)
    });
    let _ = log::set_boxed_logger(Box::new(Logger { file, level })).map(|_| log::set_max_level(level));
}

/// Exponential backoff with full jitter: base * 2^attempt, capped, randomised in [cap/2, cap].
pub fn backoff(attempt: u32, base: Duration, max: Duration) -> Duration {
    let exp = base.saturating_mul(1u32.checked_shl(attempt.min(16)).unwrap_or(u32::MAX));
    let cap = exp.min(max);
    cap / 2 + cap.mul_f64(fastrand::f64() * 0.5)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn redacts_tokens() {
        let fake = format!("{}.{}.{}", "MTIzNDU2Nzg5MDEyMzQ1Njc4", "GabcDE", "abcdefghijklmnopqrstuvwxyz0123");
        let s = format!("connecting with Bot {fake}, done");
        let r = redact(&s);
        assert!(!r.contains(&fake), "{r}");
        assert!(r.contains("[redacted]"));
        assert_eq!(redact("version 1.2.3 is fine"), "version 1.2.3 is fine");
    }
    #[test]
    fn backoff_bounded() {
        for a in 0..40 {
            let d = backoff(a, Duration::from_secs(1), Duration::from_secs(60));
            assert!(d <= Duration::from_secs(60));
            assert!(d >= Duration::from_millis(500));
        }
    }
}
