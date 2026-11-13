//! 계측 입력은 고정 enum과 정수뿐이다. 사용자 문자열/ID/본문을 label이나 sink에 전달할 수 없다.
use std::{
    io::{self, Write},
    os::fd::{AsFd, OwnedFd},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

macro_rules! metrics {
    ($($name:ident => $wire:literal),+ $(,)?) => {
        #[derive(Clone, Copy)]
        #[repr(usize)]
        pub enum Metric { $($name),+ }
        const NAMES: &[&str] = &[$($wire),+];
    }
}
metrics! {
    AdmissionRejected => "admission_rejected_total",
    AdmissionAuthentication => "admission_authentication_total", AdmissionPrincipal => "admission_principal_total", AdmissionNonstream => "admission_nonstream_total",
    AdmissionNotReady => "admission_not_ready_total", AdmissionDraining => "admission_draining_total", AdmissionCapacity => "admission_capacity_total", AdmissionInvalid => "admission_invalid_total",
    Http429 => "http_429_total", Http413 => "http_413_total", Http503 => "http_503_total",
    Authentication => "authentication_total", AuthenticationFailed => "authentication_failed_total", AuthenticationMicros => "authentication_microseconds_total",
    JwksFetch => "jwks_fetch_total", JwksFailed => "jwks_failed_total", JwksMicros => "jwks_fetch_microseconds_total", JwksAge => "jwks_cache_age_seconds",
    DbAcquire => "db_acquire_total", DbAcquireFailed => "db_acquire_failed_total", DbAcquireMicros => "db_acquire_microseconds_total",
    DbLock => "db_lock_total", DbLockFailed => "db_lock_failed_total", DbLockMicros => "db_lock_microseconds_total",
    Finalization => "finalization_total", FinalizationFailed => "finalization_failed_total", FinalizationMicros => "finalization_microseconds_total",
    ActiveTasks => "active_tasks", ActivePermits => "active_permits", BodyBytes => "body_bytes", QueueBytes => "queue_bytes", QueueWaiters => "queue_waiters", QueuePeak => "queue_peak_bytes", ParserPeak => "parser_peak_bytes", ParserBytes => "parser_bytes", InputBytes => "input_bytes",
    Ttft => "ttft_count", TtftMicros => "ttft_microseconds_total", Streams => "stream_count", StreamMicros => "stream_microseconds_total", Cancelled => "stream_cancelled_total",
    Upstream2xx => "upstream_2xx_total", Upstream4xx => "upstream_4xx_total", Upstream5xx => "upstream_5xx_total", UpstreamOther => "upstream_other_total",
    Unknown => "unknown_requests", BlockedPrincipals => "unknown_blocked_principals", OldestUnknown => "oldest_unknown_seconds",
    PriceUnresolved => "price_unresolved_total", BoundViolation => "pricing_bound_violation_total",
    Recovered => "reaper_transitions_total", Deleted => "deleted_requests_total", ReaperFailed => "reaper_failed_total",
    Panics => "panic_total", ShutdownRemaining => "shutdown_remaining_milliseconds", Ready => "ready",
    Rss => "process_rss_bytes", Anon => "process_anon_bytes", CgroupAnon => "cgroup_anon_bytes", MemoryCurrent => "cgroup_memory_current_bytes", MemoryPeak => "cgroup_memory_peak_bytes",
    CpuUsage => "cgroup_cpu_usage_microseconds", CpuUser => "cgroup_cpu_user_microseconds", CpuSystem => "cgroup_cpu_system_microseconds",
    SinkDropped => "telemetry_dropped_total",
    AlarmUnknown => "alarm_new_unknown_total", AlarmLatch => "alarm_finalization_latch_total", AlarmCompanyKey => "alarm_company_key_error_total", AlarmReady => "alarm_ready_degraded_total", AlarmBound => "alarm_pricing_bound_total",
    ProviderCredentialsUnavailable => "provider_credentials_unavailable"
}
static VALUES: [AtomicU64; NAMES.len()] = [const { AtomicU64::new(0) }; NAMES.len()];
pub fn add(metric: Metric, value: u64) {
    let _ = VALUES[metric as usize].fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
        Some(old.saturating_add(value))
    });
}
pub fn set(metric: Metric, value: u64) {
    VALUES[metric as usize].store(value, Ordering::Relaxed);
}
pub fn max(metric: Metric, value: u64) {
    VALUES[metric as usize].fetch_max(value, Ordering::Relaxed);
}
pub fn get(metric: Metric) -> u64 {
    VALUES[metric as usize].load(Ordering::Relaxed)
}
pub fn snapshot() -> serde_json::Value {
    let values: serde_json::Map<_, _> = NAMES
        .iter()
        .enumerate()
        .map(|(i, name)| ((*name).to_owned(), VALUES[i].load(Ordering::Relaxed).into()))
        .collect();
    serde_json::json!({"telemetry":values})
}

pub struct Timer {
    started: Instant,
    elapsed: Metric,
    failed: Metric,
    success: bool,
}
impl Timer {
    pub fn start(count: Metric, elapsed: Metric, failed: Metric) -> Self {
        add(count, 1);
        Self {
            started: Instant::now(),
            elapsed,
            failed,
            success: false,
        }
    }
    pub fn success(&mut self) {
        self.success = true;
    }
}
impl Drop for Timer {
    fn drop(&mut self) {
        add(
            self.elapsed,
            self.started.elapsed().as_micros().min(u64::MAX as u128) as u64,
        );
        if !self.success {
            add(self.failed, 1);
        }
    }
}

enum Sink {
    Pipe(std::fs::File),
    Socket(std::os::unix::net::UnixStream),
}
/// 한 번의 비대기 write만 수행한다. 포화/부분 쓰기는 버리고 counter만 남긴다.
pub struct Collector {
    sink: Mutex<Sink>,
}
impl Collector {
    pub fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        use std::os::unix::fs::FileTypeExt;
        let kind = std::fs::File::from(fd.try_clone()?).metadata()?.file_type();
        let sink = if kind.is_socket() {
            let socket = std::os::unix::net::UnixStream::from(fd);
            socket.set_nonblocking(true)?;
            Sink::Socket(socket)
        } else {
            let pipe = tokio::net::unix::pipe::Sender::from_owned_fd(fd)?;
            Sink::Pipe(std::fs::File::from(pipe.into_nonblocking_fd()?))
        };
        Ok(Self {
            sink: Mutex::new(sink),
        })
    }
    pub fn publish(&self) {
        let mut line = serde_json::to_vec(&snapshot()).expect("고정 계측 정수 직렬화");
        line.push(b'\n');
        let sent = self
            .sink
            .try_lock()
            .ok()
            .and_then(|mut sink| match &mut *sink {
                Sink::Pipe(p) => p.write(&line).ok(),
                Sink::Socket(s) => s.write(&line).ok(),
            });
        if sent != Some(line.len()) {
            add(Metric::SinkDropped, 1);
        }
    }
}
static COLLECTOR: OnceLock<Collector> = OnceLock::new();
pub fn install_stderr() {
    if let Ok(fd) = std::io::stderr().as_fd().try_clone_to_owned()
        && let Ok(collector) = Collector::from_fd(fd)
    {
        let _ = COLLECTOR.set(collector);
    }
}
pub fn publish() {
    if let Some(collector) = COLLECTOR.get() {
        collector.publish();
    }
}

pub fn admission(error: crate::runtime::AdmissionError) {
    use crate::runtime::AdmissionError;
    add(Metric::AdmissionRejected, 1);
    add(
        match error {
            AdmissionError::NotReady => Metric::AdmissionNotReady,
            AdmissionError::Draining => Metric::AdmissionDraining,
            AdmissionError::InvalidTransition => Metric::AdmissionInvalid,
            _ => Metric::AdmissionCapacity,
        },
        1,
    );
}
pub fn http_status(status: u16) {
    match status {
        429 => add(Metric::Http429, 1),
        413 => add(Metric::Http413, 1),
        503 => add(Metric::Http503, 1),
        _ => {}
    }
}
/// Linux 제품 프로세스/cgroup 수치는 서로 합산하지 않는다. 다른 OS에서는 수집 불가다.
pub fn sample_memory() {
    fn read(path: &str) -> Option<String> {
        use std::io::Read;
        let mut value = String::new();
        std::fs::File::open(path)
            .ok()?
            .take(16 * 1024)
            .read_to_string(&mut value)
            .ok()?;
        Some(value)
    }
    if let Some(status) = read("/proc/self/status") {
        for (key, metric) in [("VmRSS:", Metric::Rss), ("RssAnon:", Metric::Anon)] {
            if let Some(value) = status
                .lines()
                .find_map(|line| line.strip_prefix(key))
                .and_then(|v| v.split_whitespace().next())
                .and_then(|v| v.parse::<u64>().ok())
            {
                set(metric, value.saturating_mul(1024));
            }
        }
    }
    if let Some(stat) = read("/sys/fs/cgroup/memory.stat")
        && let Some(value) = stat
            .lines()
            .find_map(|line| line.strip_prefix("anon "))
            .and_then(|v| v.parse().ok())
    {
        set(Metric::CgroupAnon, value);
    }
    if let Some(stat) = read("/sys/fs/cgroup/cpu.stat") {
        for (key, metric) in [
            ("usage_usec ", Metric::CpuUsage),
            ("user_usec ", Metric::CpuUser),
            ("system_usec ", Metric::CpuSystem),
        ] {
            if let Some(value) = stat
                .lines()
                .find_map(|line| line.strip_prefix(key))
                .and_then(|v| v.parse().ok())
            {
                set(metric, value);
            }
        }
    }
    for (path, metric) in [
        ("/sys/fs/cgroup/memory.current", Metric::MemoryCurrent),
        ("/sys/fs/cgroup/memory.peak", Metric::MemoryPeak),
    ] {
        if let Some(value) = read(path).and_then(|v| v.trim().parse().ok()) {
            set(metric, value);
        }
    }
}

pub fn subtract(metric: Metric, value: u64) {
    let _ = VALUES[metric as usize].fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
        Some(old.saturating_sub(value))
    });
}
