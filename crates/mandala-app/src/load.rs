//! System CPU sampling and background admission policy.

use std::time::{Duration, Instant};

pub const LOAD_INTERVAL: Duration = Duration::from_millis(500);
// Initial policy thresholds, not hardware-specific performance claims.
const CPU_GROW_BELOW: f32 = 0.60;
const CPU_SHRINK_ABOVE: f32 = 0.85;

pub struct BackgroundBudget {
    maximum: usize,
    limit: usize,
}

impl Default for BackgroundBudget {
    fn default() -> Self {
        Self::for_workers(2)
    }
}

impl BackgroundBudget {
    pub fn for_workers(workers: usize) -> Self {
        Self { maximum: workers.saturating_sub(1).max(1), limit: 1 }
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn pause(&mut self) {
        self.limit = 0;
    }

    pub fn observe(&mut self, cpu: Option<f32>, foreground: bool) {
        if foreground {
            self.pause();
            return;
        }
        match cpu.filter(|load| load.is_finite() && (0.0..=1.0).contains(load)) {
            Some(load) if load >= CPU_SHRINK_ABOVE => self.limit /= 2,
            Some(load) if load < CPU_GROW_BELOW => self.limit = (self.limit + 1).min(self.maximum),
            Some(_) => self.limit = self.limit.max(1),
            None => self.limit = 1,
        }
    }
}

#[derive(Clone, Copy)]
struct CpuTimes {
    idle: u64,
    total: u64,
}

fn busy_fraction(previous: CpuTimes, current: CpuTimes) -> Option<f32> {
    let total = current.total.checked_sub(previous.total)?;
    let idle = current.idle.checked_sub(previous.idle)?;
    if total == 0 || idle > total {
        return None;
    }
    Some((total - idle) as f32 / total as f32)
}

#[derive(Default)]
pub struct CpuMonitor {
    previous: Option<CpuTimes>,
    last_sample: Option<Instant>,
}

impl CpuMonitor {
    /// Outer `None` means not due; inner `None` means no usable CPU reading.
    pub fn poll(&mut self, now: Instant) -> Option<Option<f32>> {
        if self.last_sample.is_some_and(|last| now.saturating_duration_since(last) < LOAD_INTERVAL)
        {
            return None;
        }
        self.last_sample = Some(now);
        let current = system_times();
        let load = self.previous.zip(current).and_then(|(old, new)| busy_fraction(old, new));
        self.previous = current;
        Some(load)
    }
}

#[cfg(windows)]
fn system_times() -> Option<CpuTimes> {
    use windows::Win32::{Foundation::FILETIME, System::Threading::GetSystemTimes};
    let mut idle = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }.ok()?;
    let ticks =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    // Kernel time already includes idle time. Above 64 logical CPUs Windows
    // reports the caller's primary processor group rather than all groups.
    Some(CpuTimes { idle: ticks(idle), total: ticks(kernel).checked_add(ticks(user))? })
}

#[cfg(not(windows))]
fn system_times() -> Option<CpuTimes> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spare_cpu_gradually_increases_parallelism_and_reserves_a_worker() {
        let mut budget = BackgroundBudget::for_workers(8);
        assert_eq!(budget.limit(), 1);
        for expected in 2..=7 {
            budget.observe(Some(0.2), false);
            assert_eq!(budget.limit(), expected);
        }
        budget.observe(Some(0.2), false);
        assert_eq!(budget.limit(), 7);
    }

    #[test]
    fn cpu_pressure_reduces_parallelism_and_can_pause_background_work() {
        let mut budget = BackgroundBudget::for_workers(8);
        for _ in 0..6 {
            budget.observe(Some(0.2), false);
        }
        for expected in [3, 1, 0] {
            budget.observe(Some(0.95), false);
            assert_eq!(budget.limit(), expected);
        }
        budget.observe(Some(0.2), false);
        assert_eq!(budget.limit(), 1);
    }

    #[test]
    fn foreground_pressure_pauses_admission_and_unknown_cpu_uses_one_worker() {
        let mut budget = BackgroundBudget::for_workers(8);
        budget.observe(Some(0.1), true);
        assert_eq!(budget.limit(), 0);
        budget.observe(None, false);
        assert_eq!(budget.limit(), 1);
        budget.observe(Some(0.75), false);
        assert_eq!(budget.limit(), 1);
        budget.pause();
        budget.observe(Some(0.75), false);
        assert_eq!(budget.limit(), 1);
    }

    #[test]
    fn cpu_sampling_is_throttled_even_when_no_reading_is_available() {
        let mut monitor = CpuMonitor::default();
        let now = Instant::now();
        assert!(monitor.poll(now).is_some());
        assert!(monitor.poll(now).is_none());
        assert!(monitor.poll(now + LOAD_INTERVAL).is_some());
    }

    #[test]
    fn cpu_busy_fraction_subtracts_idle_time_included_in_kernel_time() {
        assert_eq!(
            busy_fraction(CpuTimes { idle: 100, total: 500 }, CpuTimes { idle: 140, total: 600 }),
            Some(0.6)
        );
        assert!(
            busy_fraction(CpuTimes { idle: 100, total: 500 }, CpuTimes { idle: 100, total: 500 })
                .is_none()
        );
    }
}
