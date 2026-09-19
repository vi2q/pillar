//! Native host adapter gates (design §9, §10): the broker runs an authorized
//! command, bounds and pages its output, cancels it, refuses a foreign owner,
//! and the metadata host refreshes only through the effect gate.
//!
//! These use `sh` as the scripted producer so the test is offline and does not
//! invoke a real Cargo build.

#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use pillar_agent::rust_tools::host::{
    CargoJobBroker, ExitStatus, OutputStream, OwnerId, RequestId, RunId, RunState,
    SourceSnapshotPort, StartRequest, WorkspaceCatalogPort,
};
use pillar_agent::rust_tools::{RustToolErrorCode, command_digest};
use pillar_coding_agent::core::effects::{EffectAuthorizer, EffectDecision, EffectIntent};
use pillar_coding_agent::core::rust_host::{NativeCargoBroker, NativeMetadataHost, NativeSources};

fn allow_all() -> EffectAuthorizer {
    Arc::new(|_: &EffectIntent| EffectDecision::Allow)
}

fn deny_all() -> EffectAuthorizer {
    Arc::new(|_: &EffectIntent| EffectDecision::Deny {
        reason: "denied by policy".to_string(),
    })
}

fn scripted_broker(authorizer: EffectAuthorizer) -> NativeCargoBroker {
    NativeCargoBroker::new("/tmp", Some(authorizer))
        .with_program("sh", vec!["-c".to_string()])
        .with_timeout_ms(Some(30_000))
}

fn start_request(owner: &OwnerId, request_id: &str, script: &str) -> StartRequest {
    let argv = vec!["sh".to_string(), "-c".to_string(), script.to_string()];
    StartRequest {
        owner: owner.clone(),
        plan_id: "vp1".to_string(),
        step_id: "s1".to_string(),
        request_id: RequestId::new(request_id),
        configuration_id: "native-default".to_string(),
        metadata_digest: "meta".to_string(),
        configuration_fingerprint: "fp".to_string(),
        command_digest: command_digest(&argv),
        argv,
    }
}

async fn wait_for(broker: &NativeCargoBroker, owner: &OwnerId, run_id: &RunId) -> RunState {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let record = broker.status(owner, run_id).await.expect("status");
        if !matches!(record.state, RunState::Running | RunState::Cancelling) {
            return record.state;
        }
        assert!(Instant::now() < deadline, "the run did not finish in time");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn the_broker_runs_and_captures_both_streams() {
    let broker = scripted_broker(allow_all());
    let owner = OwnerId::new("session-a");
    let record = broker
        .start(start_request(
            &owner,
            "rq1",
            "printf out; printf err 1>&2; exit 3",
        ))
        .await
        .expect("start");
    assert_eq!(record.run_id.as_str(), "run1");

    assert_eq!(
        wait_for(&broker, &owner, &record.run_id).await,
        RunState::Exited
    );
    let status = broker.status(&owner, &record.run_id).await.expect("status");
    assert_eq!(
        status.exit_status,
        Some(ExitStatus::Exited { code: 3 }),
        "the real exit code reaches the record"
    );

    let out = broker
        .output(&owner, &record.run_id, OutputStream::Stdout, 0, 1024)
        .await
        .expect("stdout");
    assert_eq!(String::from_utf8_lossy(&out.bytes), "out");
    let err = broker
        .output(&owner, &record.run_id, OutputStream::Stderr, 0, 1024)
        .await
        .expect("stderr");
    assert_eq!(String::from_utf8_lossy(&err.bytes), "err");
}

#[tokio::test]
async fn the_broker_streams_output_while_the_process_runs() {
    let broker = scripted_broker(allow_all());
    let owner = OwnerId::new("session-a");
    let record = broker
        .start(start_request(
            &owner,
            "rq1",
            "printf first; sleep 1; printf second",
        ))
        .await
        .expect("start");

    // `first` must be readable while the process is still sleeping, and
    // `second` must not be there yet.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let page = broker
            .output(&owner, &record.run_id, OutputStream::Stdout, 0, 1024)
            .await
            .expect("output");
        let text = String::from_utf8_lossy(&page.bytes);
        if text.contains("first") {
            assert!(
                !text.contains("second"),
                "the second write must not be visible yet: {text}"
            );
            break;
        }
        assert!(Instant::now() < deadline, "`first` never appeared");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(
        wait_for(&broker, &owner, &record.run_id).await,
        RunState::Exited
    );
    let page = broker
        .output(&owner, &record.run_id, OutputStream::Stdout, 0, 1024)
        .await
        .expect("output");
    assert_eq!(String::from_utf8_lossy(&page.bytes), "firstsecond");
}

#[tokio::test]
async fn the_broker_deduplicates_by_request_id_and_command() {
    let broker = scripted_broker(allow_all());
    let owner = OwnerId::new("session-a");
    let first = broker
        .start(start_request(&owner, "rq1", "printf once"))
        .await
        .expect("first");
    let again = broker
        .start(start_request(&owner, "rq1", "printf once"))
        .await
        .expect("duplicate");
    assert_eq!(first.run_id, again.run_id);

    let error = broker
        .start(start_request(&owner, "rq1", "printf different"))
        .await
        .expect_err("id mismatch");
    assert_eq!(error.code, RustToolErrorCode::OperationIdMismatch);
}

#[tokio::test]
async fn the_broker_cancels_a_running_command() {
    let broker = scripted_broker(allow_all());
    let owner = OwnerId::new("session-a");
    let record = broker
        .start(start_request(&owner, "rq1", "sleep 30"))
        .await
        .expect("start");

    let started = Instant::now();
    let accepted = broker.cancel(&owner, &record.run_id).await.expect("cancel");
    assert_eq!(
        accepted.state,
        RunState::Cancelling,
        "acceptance is not proof"
    );
    assert_eq!(
        wait_for(&broker, &owner, &record.run_id).await,
        RunState::Cancelled
    );
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "cancellation returns promptly: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn the_broker_refuses_an_unauthorized_start() {
    let broker = scripted_broker(deny_all());
    let owner = OwnerId::new("session-a");
    let error = broker
        .start(start_request(&owner, "rq1", "printf hi"))
        .await
        .expect_err("denied");
    assert_eq!(error.code, RustToolErrorCode::PermissionDenied);
}

#[tokio::test]
async fn the_broker_refuses_an_unapproved_program_or_subcommand() {
    let broker = scripted_broker(allow_all());
    let owner = OwnerId::new("session-a");
    let mut request = start_request(&owner, "rq1", "printf hi");
    request.argv[0] = "rm".to_string();
    request.command_digest = command_digest(&request.argv);
    let error = broker.start(request).await.expect_err("program");
    assert_eq!(error.code, RustToolErrorCode::PermissionDenied);

    let mut request = start_request(&owner, "rq2", "printf hi");
    request.argv[1] = "install".to_string();
    request.command_digest = command_digest(&request.argv);
    let error = broker.start(request).await.expect_err("subcommand");
    assert_eq!(error.code, RustToolErrorCode::PermissionDenied);
}

#[tokio::test]
async fn a_foreign_owner_cannot_read_or_cancel_a_run() {
    let broker = scripted_broker(allow_all());
    let owner = OwnerId::new("session-a");
    let record = broker
        .start(start_request(&owner, "rq1", "printf hi"))
        .await
        .expect("start");
    let other = OwnerId::new("session-b");

    assert_eq!(
        broker
            .status(&other, &record.run_id)
            .await
            .expect_err("foreign")
            .code,
        RustToolErrorCode::PermissionDenied
    );
    assert_eq!(
        broker
            .cancel(&other, &record.run_id)
            .await
            .expect_err("foreign")
            .code,
        RustToolErrorCode::PermissionDenied
    );
}

#[tokio::test]
async fn the_metadata_host_refreshes_through_the_effect_gate() {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pillar-rust-host-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let manifest = dir.join("metadata.json");
    std::fs::write(
        &manifest,
        r#"{"packages":[],"workspace_members":[],"workspace_root":"/ws"}"#,
    )
    .expect("fixture");

    let host =
        NativeMetadataHost::new(dir.to_string_lossy(), Some(allow_all())).with_command(vec![
            "cat".to_string(),
            manifest.to_string_lossy().to_string(),
        ]);
    assert_eq!(
        host.catalog().expect_err("no saved metadata").code,
        RustToolErrorCode::MetadataUnavailable
    );
    host.refresh().expect("refresh");
    let catalog = host.catalog().expect("catalog");
    assert_eq!(catalog.workspace_root(), Some("/ws"));

    let denied =
        NativeMetadataHost::new(dir.to_string_lossy(), Some(deny_all())).with_command(vec![
            "cat".to_string(),
            manifest.to_string_lossy().to_string(),
        ]);
    assert_eq!(
        denied.refresh().expect_err("denied").code,
        RustToolErrorCode::PermissionDenied
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_source_host_refuses_outside_the_workspace_and_reads_inside_it() {
    let dir = std::env::temp_dir().join(format!("pillar-rust-src-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let file = dir.join("lib.rs");
    std::fs::write(&file, "fn a() {}\nfn b() {}\nfn c() {}\n").expect("fixture");

    let sources = NativeSources::new(dir.to_string_lossy(), Some(allow_all()));
    let slice = sources
        .read_range(&file.to_string_lossy(), 2, 1)
        .expect("inside");
    assert_eq!(slice.text, "fn b() {}\n");
    assert_eq!(slice.total_lines, 3);

    assert_eq!(
        sources
            .read_range("/etc/hostname", 1, 1)
            .expect_err("outside")
            .code,
        RustToolErrorCode::PermissionDenied
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_source_host_is_deterministic_about_an_unknown_file() {
    let sources = NativeSources::new("/tmp", Some(allow_all()));
    assert_eq!(
        sources
            .read_range("/tmp/definitely-not-here-xyz", 1, 1)
            .expect_err("missing")
            .code,
        RustToolErrorCode::SourceUnbound
    );
}
