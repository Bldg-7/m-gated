use m_gated::{
    config::{self, ConfigInputs},
    runtime::{RouteClass, Runtime},
    telemetry::{self, Collector, Metric},
};
use std::{
    io::{Read, Write},
    os::{fd::OwnedFd, unix::net::UnixStream},
};
mod support;
static ADMISSION_METRICS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn runtime_failure_rejection_and_collector_have_fixed_numeric_cardinality() {
    let _serial = ADMISSION_METRICS.lock().await;
    let cfg = config::load(ConfigInputs {
        yaml: support::YAML,
        file_environment: None,
        environment: &support::environment(),
    })
    .unwrap();
    let runtime = Runtime::new(cfg.limits().clone()).unwrap();
    let alarm = telemetry::get(Metric::AlarmLatch);
    let rejected = telemetry::get(Metric::AdmissionRejected);
    runtime.record_finalization_failure();
    assert!(runtime.try_admit(RouteClass::Inference).is_err());
    let (writer, mut reader) = UnixStream::pair().unwrap();
    let collector = Collector::from_fd(OwnedFd::from(writer)).unwrap();
    collector.publish();
    let mut line = String::new();
    let mut byte = [0];
    while reader.read_exact(&mut byte).is_ok() {
        line.push(byte[0] as char);
        if byte[0] == b'\n' {
            break;
        }
    }
    let snapshot: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert!(
        snapshot["telemetry"]
            .as_object()
            .unwrap()
            .values()
            .all(serde_json::Value::is_u64)
    );
    assert!(snapshot["telemetry"].as_object().unwrap().len() < 100);
    assert_eq!(
        telemetry::get(Metric::AlarmLatch),
        alarm + 1,
        "실제 마감 latch 경보 누락"
    );
    assert_eq!(
        telemetry::get(Metric::AdmissionRejected),
        rejected + 1,
        "실제 입장 거부 계수 누락"
    );
}
#[tokio::test]
async fn saturated_collector_never_waits_or_records_dynamic_secret_content() {
    let (mut writer, _reader) = UnixStream::pair().unwrap();
    writer.set_nonblocking(true).unwrap();
    let collector = Collector::from_fd(OwnedFd::from(writer.try_clone().unwrap())).unwrap();
    let sentinel = b"synthetic-JWT-cookie-CSRF-company-key-conversation-tool-sentinel";
    let mut filled = false;
    for _ in 0..100_000 {
        match writer.write(sentinel) {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                filled = true;
                break;
            }
            Ok(_) => {}
            Err(error) => panic!("{error}"),
        }
    }
    assert!(filled);
    let before = telemetry::get(Metric::SinkDropped);
    collector.publish();
    assert!(telemetry::get(Metric::SinkDropped) > before);
    let line = telemetry::snapshot().to_string();
    assert!(!line.contains(std::str::from_utf8(sentinel).unwrap()));
    assert!(line.len() < 8192);
}

#[tokio::test]
async fn ready_degradation_alarms_once_per_outage_without_dynamic_labels() {
    use m_gated::runtime::Dependency;
    let cfg = config::load(ConfigInputs {
        yaml: support::YAML,
        file_environment: None,
        environment: &support::environment(),
    })
    .unwrap();
    let runtime = Runtime::new(cfg.limits().clone()).unwrap();
    runtime.start_http().unwrap();
    let before = telemetry::get(Metric::AlarmReady);
    let now = tokio::time::Instant::now();
    runtime.observe_telemetry(now);
    runtime.observe_telemetry(now + std::time::Duration::from_secs(29));
    assert_eq!(telemetry::get(Metric::AlarmReady), before);
    runtime.observe_telemetry(now + std::time::Duration::from_secs(30));
    runtime.observe_telemetry(now + std::time::Duration::from_secs(60));
    assert_eq!(telemetry::get(Metric::AlarmReady), before + 1);
    for dependency in [
        Dependency::Database,
        Dependency::Jwks,
        Dependency::Schema,
        Dependency::Ui,
    ] {
        runtime.record_health(dependency, true, tokio::time::Instant::now());
    }
    runtime.observe_telemetry(tokio::time::Instant::now());
    assert_eq!(telemetry::get(Metric::Ready), 1);
}

#[test]
fn actual_main_collector_declares_every_fixed_metric_without_dropping_later_boundaries() {
    let collector = include_str!("process/main.mjs");
    for name in [
        "cgroup_cpu_usage_microseconds",
        "cgroup_cpu_user_microseconds",
        "cgroup_cpu_system_microseconds",
    ] {
        assert!(
            telemetry::snapshot()["telemetry"].get(name).is_some(),
            "CPU 사용량 고정 계측 누락: {name}"
        );
    }
    for name in telemetry::snapshot()["telemetry"]
        .as_object()
        .unwrap()
        .keys()
    {
        assert!(
            collector.contains(&format!("\"{name}\"")),
            "실제 main collector의 고정 metric 누락: {name}"
        );
    }
}

#[test]
fn database_wide_migration_test_is_excluded_from_parallel_pg_and_runs_exact_last() {
    let script = include_str!("../scripts/test-s2.sh");
    let name = "s10::two_migrators_serialize_and_actual_advisory30s_total120s_fail_closed";
    assert!(script.contains(&format!("migration_test={name}")));
    let parallel = script.lines().position(|line| {
        line.contains("--ignored --skip \"$clock_test\" --skip \"$migration_test\"")
    });
    let isolated = script.lines().position(|line| {
        line.contains("\"$migration_test\" -- --ignored --exact --test-threads=1")
    });
    assert!(
        parallel.zip(isolated).is_some_and(|(a, b)| a < b),
        "database-wide 시험은 병렬 PG 이후 exact로 실행해야 한다"
    );
}

#[tokio::test]
async fn later_admission_boundaries_count_one_rejection_and_no_success_samples() {
    let _serial = ADMISSION_METRICS.lock().await;
    use m_gated::{
        config::Limits,
        runtime::{AdmissionError, Dependency},
    };
    for boundary in ["authentication", "principal", "nonstream"] {
        let runtime = Runtime::new(Limits {
            authentication: 1,
            ..Default::default()
        })
        .unwrap();
        runtime.start_http().unwrap();
        for d in [
            Dependency::Database,
            Dependency::Jwks,
            Dependency::Schema,
            Dependency::Ui,
        ] {
            runtime.record_health(d, true, tokio::time::Instant::now());
        }
        let mut held = Vec::new();
        let principal = uuid::Uuid::new_v4();
        for _ in 0..if boundary == "authentication" { 1 } else { 4 } {
            let mut lease = runtime.try_admit(RouteClass::Inference).unwrap();
            lease.begin_authentication().unwrap();
            if boundary != "authentication" {
                lease
                    .identify(if boundary == "principal" {
                        principal
                    } else {
                        uuid::Uuid::new_v4()
                    })
                    .unwrap();
                if boundary == "nonstream" {
                    lease.acquire_nonstream().unwrap();
                }
            }
            held.push(lease);
        }
        let mut candidate = runtime.try_admit(RouteClass::Inference).unwrap();
        if boundary != "authentication" {
            candidate.begin_authentication().unwrap();
        }
        if boundary == "nonstream" {
            candidate.identify(uuid::Uuid::new_v4()).unwrap();
        }
        let before = telemetry::get(Metric::AdmissionRejected);
        let reason = match boundary {
            "authentication" => Metric::AdmissionAuthentication,
            "principal" => Metric::AdmissionPrincipal,
            _ => Metric::AdmissionNonstream,
        };
        let before_reason = telemetry::get(reason);
        let rejected = match boundary {
            "authentication" => candidate.begin_authentication(),
            "principal" => candidate.identify(principal),
            _ => candidate.acquire_nonstream(),
        };
        assert_eq!(
            rejected,
            Err(if boundary == "authentication" {
                AdmissionError::AuthenticationCapacity
            } else {
                AdmissionError::Concurrency
            })
        );
        assert_eq!(
            telemetry::get(Metric::AdmissionRejected),
            before + 1,
            "후속 {boundary} 거부 누락"
        );
        assert_eq!(telemetry::get(reason), before_reason + 1);
        drop(candidate);
        drop(held);
        assert_eq!(runtime.available_inference(), 16);
        assert_eq!(runtime.active_principals(), 0);
        let mut successful = runtime.try_admit(RouteClass::Inference).unwrap();
        successful.begin_authentication().unwrap();
        successful.identify(principal).unwrap();
        successful.acquire_nonstream().unwrap();
        assert_eq!(telemetry::get(Metric::AdmissionRejected), before + 1);
        assert_eq!(telemetry::get(reason), before_reason + 1);
    }
}
