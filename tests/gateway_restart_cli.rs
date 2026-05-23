use std::time::Duration;

use rsclaw::cmd::gateway::{
    HealthWaitOutcome, RestartFallbackDecision, RestartStrategy, StopWaitOutcome,
    health_wait_result, restart_fallback_after_stop_result, should_remove_pid_after_stop,
};

#[test]
fn stop_wait_outcome_reports_stopped_when_process_disappears() {
    let outcome = StopWaitOutcome::from_alive_samples(1234, [true, true, false]);

    assert!(outcome.is_stopped());
    assert_eq!(outcome.error_message(), None);
}

#[test]
fn stop_wait_outcome_reports_timeout_when_process_stays_alive() {
    let outcome = StopWaitOutcome::from_alive_samples(1234, [true, true, true]);

    assert!(!outcome.is_stopped());
    assert_eq!(
        outcome.error_message(),
        Some("gateway process 1234 did not stop before timeout".to_owned())
    );
}

#[test]
fn stop_wait_timeout_duration_covers_observed_graceful_shutdowns() {
    assert!(rsclaw::cmd::gateway::STOP_TIMEOUT >= Duration::from_secs(45));
}

#[test]
fn pid_file_is_not_removed_when_stop_times_out() {
    let outcome = StopWaitOutcome::from_alive_samples(1234, [true, true, true]);

    assert!(!should_remove_pid_after_stop(&outcome));
}

#[test]
fn pid_file_is_removed_when_process_has_stopped() {
    let outcome = StopWaitOutcome::from_alive_samples(1234, [true, false]);

    assert!(should_remove_pid_after_stop(&outcome));
}

#[test]
fn health_wait_succeeds_when_any_probe_is_healthy() {
    let outcome = health_wait_result([false, false, true], "http://127.0.0.1:19042/api/v1/health");

    assert_eq!(outcome, HealthWaitOutcome::Healthy);
}

#[test]
fn health_wait_times_out_when_all_probes_fail() {
    let outcome = health_wait_result(
        [false, false, false],
        "http://127.0.0.1:19042/api/v1/health",
    );

    assert_eq!(
        outcome,
        HealthWaitOutcome::Timeout {
            url: "http://127.0.0.1:19042/api/v1/health".to_owned()
        }
    );
}

#[test]
fn restart_prefers_graceful_http_when_gateway_is_reachable() {
    assert_eq!(RestartStrategy::choose(true), RestartStrategy::HttpGraceful);
}

#[test]
fn restart_falls_back_to_direct_when_gateway_is_not_reachable() {
    assert_eq!(
        RestartStrategy::choose(false),
        RestartStrategy::DirectStopStart
    );
}

#[test]
fn direct_restart_continues_when_stop_succeeded() {
    assert_eq!(
        restart_fallback_after_stop_result(Ok(())),
        RestartFallbackDecision::StartFresh
    );
}

#[test]
fn direct_restart_continues_when_gateway_was_not_running() {
    assert_eq!(
        restart_fallback_after_stop_result(Err("gateway is not running")),
        RestartFallbackDecision::StartFresh
    );
}

#[test]
fn direct_restart_stops_when_shutdown_times_out() {
    assert_eq!(
        restart_fallback_after_stop_result(Err("gateway process 1234 did not stop before timeout")),
        RestartFallbackDecision::Abort {
            reason: "gateway process 1234 did not stop before timeout".to_owned()
        }
    );
}
