//! Integration tests for [`StratumV2Source`].
//!
//! Each `#[ignore]` test spins up a real SV2 pool behind a [`Sniffer`] proxy
//! and asserts on the message exchange at the sniffer boundary.
//! `integration_tests_sv2` downloads Bitcoin Core and the SV2 template
//! provider binary on first run (~200 MB, cached in `~/.cargo/`).
//!
//! Use `--test-threads=4` to avoid port-binding races from concurrent `bitcoind`
//! instances when running the full suite:
//!
//! ```text
//! cargo test -p mujina-miner -- stratum_v2::integration_tests --ignored --nocapture --test-threads=4
//! ```

use integration_tests_sv2::{
    interceptor::{IgnoreMessage, MessageDirection, ReplaceMessage},
    template_provider::DifficultyLevel,
    *,
};
use stratum_apps::{
    key_utils::Secp256k1PublicKey,
    stratum_core::{
        common_messages_sv2::*,
        mining_sv2::*,
        parsers_sv2::{AnyMessage, CommonMessages, Mining},
    },
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    job_source::{
        Extranonce2, MerkleRootKind, Share, SourceCommand, SourceEvent, stratum_v2::StratumV2Source,
    },
    stratum_v2::PoolConfig,
    types::HashRate,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Well-known authority public key used by the Sniffer's Noise responder.
///
/// Our client must be configured with this key so the Noise handshake
/// succeeds when connecting to the sniffer address instead of the real pool.
const SNIFFER_AUTHORITY_PUBKEY: &str = "9auqWEzQDVyd2oe1JVGFLMLHZtCo2FFqZwtKA5gd9xbuEu7PH72";

/// Build a [`PoolConfig`] pointing at `addr` with the Sniffer authority key.
fn pool_config(addr: std::net::SocketAddr) -> PoolConfig {
    let authority_pubkey = SNIFFER_AUTHORITY_PUBKEY
        .parse::<Secp256k1PublicKey>()
        .expect("test authority pubkey must parse");
    PoolConfig::new(
        addr.ip().to_string(),
        addr.port(),
        authority_pubkey,
        "test-worker".to_string(),
        "test-vendor".to_string(),
        "1.0".to_string(),
        "1.0".to_string(),
        String::new(),
        HashRate::from_megahashes(1.0),
    )
    .expect("PoolConfig fields are within Str0255 limits")
}

/// Spawn a [`StratumV2Source`] and pre-seed it with a non-zero hash-rate so
/// it proceeds to connect without waiting for an `UpdateHashRate` command.
///
/// Returns `(cmd_tx, event_rx, join_handle)`.
fn spawn_source(
    addr: std::net::SocketAddr,
    shutdown: CancellationToken,
) -> (
    mpsc::Sender<SourceCommand>,
    mpsc::Receiver<SourceEvent>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let (cmd_tx, cmd_rx) = mpsc::channel(10);
    let (event_tx, event_rx) = mpsc::channel(100);

    // Pre-buffer a non-zero hashrate so the source's startup loop breaks
    // immediately when it first polls the command channel.
    cmd_tx
        .try_send(SourceCommand::UpdateHashRate(HashRate::from_megahashes(
            1.0,
        )))
        .expect("channel must have capacity for one message");

    let source = StratumV2Source::new(pool_config(addr), cmd_rx, event_tx, shutdown);
    let handle = tokio::spawn(async move { source.run().await });
    (cmd_tx, event_rx, handle)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A cancelled shutdown token causes [`StratumV2Source::run`] to return
/// `Ok(())` cleanly before the source ever connects to a pool.
///
/// This test requires no external binaries.
#[tokio::test]
async fn clean_shutdown_before_connect() {
    let shutdown = CancellationToken::new();
    let (_cmd_tx, cmd_rx) = mpsc::channel(10);
    let (event_tx, _event_rx) = mpsc::channel(100);

    let authority_pubkey = SNIFFER_AUTHORITY_PUBKEY
        .parse::<Secp256k1PublicKey>()
        .expect("test key must parse");
    let config = PoolConfig::new(
        "127.0.0.1".to_string(),
        1, // never connected to
        authority_pubkey,
        "w".to_string(),
        "v".to_string(),
        "h".to_string(),
        "f".to_string(),
        String::new(),
        HashRate::default(), // zero → source stays in startup loop
    )
    .unwrap();

    let source = StratumV2Source::new(config, cmd_rx, event_tx, shutdown.clone());
    let handle = tokio::spawn(async move { source.run().await });

    // Cancel while the source is still waiting for a non-zero hashrate.
    shutdown.cancel();

    let result = handle.await.expect("task must not panic");
    assert!(result.is_ok(), "expected Ok(()), got {result:?}");
}

/// [`StratumV2Source`] sends `SetupConnection` with `protocol =
/// MiningProtocol`, `min_version = 2`, `max_version = 2`, and `flags = 0`
/// (no `requires_standard_job` flag) as its first message.
#[tokio::test]
#[ignore = "downloads Bitcoin Core and sv2-tp binaries on first run"]
async fn setup_connection_uses_mining_protocol() {
    start_tracing();

    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (_pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let (sniffer, sniffer_addr) =
        start_sniffer("setup-connection", pool_addr, false, vec![], Some(30));

    let shutdown = CancellationToken::new();
    let (_cmd_tx, _event_rx, _handle) = spawn_source(sniffer_addr, shutdown.clone());

    sniffer
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;

    assert_common_message!(
        &sniffer.next_message_from_downstream(),
        SetupConnection,
        protocol,
        Protocol::MiningProtocol,
        min_version,
        2u16,
        max_version,
        2u16,
        flags,
        0u32
    );

    shutdown.cancel();
}

/// After `SetupConnectionSuccess`, the source opens an Extended Mining Channel
/// and the pool replies with `OpenExtendedMiningChannelSuccess`.
#[tokio::test]
#[ignore = "downloads Bitcoin Core and sv2-tp binaries on first run"]
async fn extended_channel_opened_after_setup() {
    start_tracing();

    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (_pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let (sniffer, sniffer_addr) = start_sniffer("channel-open", pool_addr, false, vec![], Some(30));

    let shutdown = CancellationToken::new();
    let (_cmd_tx, _event_rx, _handle) = spawn_source(sniffer_addr, shutdown.clone());

    // Drain SetupConnection so the next pop returns OpenExtendedMiningChannel.
    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SETUP_CONNECTION,
        )
        .await;

    sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
        )
        .await;
    assert_mining_message!(
        &sniffer.next_message_from_downstream(),
        OpenExtendedMiningChannel
    );

    // Drain SetupConnectionSuccess so the next pop returns OpenExtendedMiningChannelSuccess.
    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
        )
        .await;
    assert_mining_message!(
        &sniffer.next_message_from_upstream(),
        OpenExtendedMiningChannelSuccess
    );

    shutdown.cancel();
}

/// [`StratumV2Source`] emits [`SourceEvent::ReplaceJob`] after the pool delivers
/// a `NewExtendedMiningJob` / `SetNewPrevHash` pair.
#[tokio::test]
#[ignore = "downloads Bitcoin Core and sv2-tp binaries on first run"]
async fn replace_job_emitted_on_job_and_prevhash() {
    start_tracing();

    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (_pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let (sniffer, sniffer_addr) = start_sniffer("job-flow", pool_addr, false, vec![], Some(60));

    let shutdown = CancellationToken::new();
    let (_cmd_tx, mut event_rx, _handle) = spawn_source(sniffer_addr, shutdown.clone());

    // Wait until the pool has delivered both halves of the job pair.
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
        )
        .await;
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH,
        )
        .await;

    let got_replace_job = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match event_rx.recv().await {
                Some(SourceEvent::ReplaceJob(_)) => return true,
                Some(_) => {}         // skip ClearJobs, SharesAccepted, …
                None => return false, // sender dropped
            }
        }
    })
    .await
    .unwrap_or(false);

    assert!(
        got_replace_job,
        "expected SourceEvent::ReplaceJob within 10 s"
    );

    shutdown.cancel();
}

/// `OpenExtendedMiningChannel` carries the worker name from [`PoolConfig`] as
/// `user_identity`, a positive nominal hash rate, and a non-zero
/// `min_extranonce_size`.
#[tokio::test]
#[ignore = "downloads Bitcoin Core and sv2-tp binaries on first run"]
async fn open_extended_mining_channel_fields() {
    start_tracing();

    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (_pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let (sniffer, sniffer_addr) =
        start_sniffer("open-channel-fields", pool_addr, false, vec![], Some(30));

    let shutdown = CancellationToken::new();
    let (_cmd_tx, _event_rx, _handle) = spawn_source(sniffer_addr, shutdown.clone());

    // Drain SetupConnection first so the next pop is OpenExtendedMiningChannel.
    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SETUP_CONNECTION,
        )
        .await;

    sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
        )
        .await;

    let msg = sniffer.next_message_from_downstream();
    match msg {
        Some((_, AnyMessage::Mining(Mining::OpenExtendedMiningChannel(m)))) => {
            assert_eq!(
                m.user_identity.as_utf8_or_hex(),
                "test-worker",
                "user_identity must match pool config worker name"
            );
            assert!(
                m.nominal_hash_rate > 0.0,
                "nominal_hash_rate must be positive"
            );
            assert!(
                m.min_extranonce_size >= 4,
                "min_extranonce_size must be at least 4 bytes"
            );
        }
        other => panic!("expected OpenExtendedMiningChannel, got {:?}", other),
    }

    shutdown.cancel();
}

/// A share for a job ID that was never assigned by the pool is silently dropped
/// by [`StratumV2Source`] — no `SubmitSharesExtended` must reach the pool.
#[tokio::test]
#[ignore = "downloads Bitcoin Core and sv2-tp binaries on first run"]
async fn stale_share_not_forwarded() {
    start_tracing();

    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (_pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let (sniffer, sniffer_addr) = start_sniffer("stale-share", pool_addr, false, vec![], Some(30));

    let shutdown = CancellationToken::new();
    let (cmd_tx, _event_rx, _handle) = spawn_source(sniffer_addr, shutdown.clone());

    // Wait for the channel to be fully open before sending any share.
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
        )
        .await;

    // Send a share with a job ID unknown to the source; it will be dropped.
    cmd_tx
        .send(SourceCommand::SubmitShare(Share {
            job_id: "99999".to_string(),
            nonce: 0,
            time: 0,
            version: bitcoin::block::Version::from_consensus(0x2000_0000),
            extranonce2: Some(Extranonce2::new(0, 4).expect("valid extranonce2")),
        }))
        .await
        .expect("command channel must be open");

    let not_forwarded = sniffer
        .assert_message_not_present(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED,
            std::time::Duration::from_secs(2),
        )
        .await;
    assert!(
        not_forwarded,
        "stale share must not be forwarded to the pool"
    );

    shutdown.cancel();
}

/// Cancelling the shutdown token while an Extended Mining Channel is active
/// causes [`StratumV2Source::run`] to return `Ok(())` cleanly.
#[tokio::test]
#[ignore = "downloads Bitcoin Core and sv2-tp binaries on first run"]
async fn shutdown_during_active_session() {
    start_tracing();

    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (_pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let (sniffer, sniffer_addr) =
        start_sniffer("shutdown-active", pool_addr, false, vec![], Some(30));

    let shutdown = CancellationToken::new();
    let (_cmd_tx, _event_rx, handle) = spawn_source(sniffer_addr, shutdown.clone());

    // Wait until the channel is fully open before triggering shutdown.
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
        )
        .await;

    shutdown.cancel();

    let result = handle.await.expect("task must not panic");
    assert!(result.is_ok(), "expected Ok(()), got {result:?}");
}

/// Source receives a job, submits shares until the pool accepts one, and emits
/// [`SourceEvent::SharesAccepted`]. With [`DifficultyLevel::Low`] acceptance
/// almost always happens on the first nonce.
#[tokio::test]
#[ignore = "downloads Bitcoin Core and sv2-tp binaries on first run"]
async fn share_submission_accepted() {
    start_tracing();

    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (_pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;
    let (sniffer, sniffer_addr) =
        start_sniffer("share-accepted", pool_addr, false, vec![], Some(30));

    let shutdown = CancellationToken::new();
    let (cmd_tx, mut event_rx, _handle) = spawn_source(sniffer_addr, shutdown.clone());

    // Wait for ReplaceJob — proof the channel is open and a valid job is active.
    let template = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            match event_rx.recv().await {
                Some(SourceEvent::ReplaceJob(t)) => return t,
                Some(_) => {}
                None => panic!("event channel closed before ReplaceJob arrived"),
            }
        }
    })
    .await
    .expect("ReplaceJob must arrive within 30 s");

    let en2_size = match &template.merkle_root {
        MerkleRootKind::Computed(mrt) => mrt.extranonce2_range.size,
        MerkleRootKind::Fixed(_) => panic!("SV2 source must produce a Computed merkle root"),
    };

    // Retry up to 32 nonces; DifficultyLevel::Low means ~255/256 succeed immediately.
    let mut accepted = false;
    for nonce in 0_u32..32 {
        cmd_tx
            .send(SourceCommand::SubmitShare(Share {
                job_id: template.id.clone(),
                nonce,
                time: template.time,
                version: template.version.base(),
                extranonce2: Some(Extranonce2::new(0, en2_size).expect("valid extranonce2")),
            }))
            .await
            .expect("command channel must be open");

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match event_rx.recv().await {
                    Some(SourceEvent::SharesAccepted(_)) => return Some(true),
                    Some(SourceEvent::SharesRejected) => return Some(false),
                    Some(_) => {}
                    None => return None,
                }
            }
        })
        .await;

        match outcome {
            Ok(Some(true)) => {
                accepted = true;
                break;
            }
            Ok(Some(false)) => {} // pool sent difficulty-too-low; try next nonce
            Ok(None) => panic!("event channel closed before share response"),
            Err(_) => {} // no response within 5 s; try next nonce
        }
    }

    assert!(
        accepted,
        "no share accepted within 32 nonces with DifficultyLevel::Low"
    );

    sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED,
        )
        .await;

    shutdown.cancel();
}

/// When the sniffer drops all `NewExtendedMiningJob` messages before they reach
/// the source, `SetNewPrevHash` arrives with no matching future job buffered.
/// The source must emit `SourceEvent::ClearJobs` rather than crashing or
/// stalling.
#[tokio::test]
#[ignore = "downloads Bitcoin Core and sv2-tp binaries on first run"]
async fn set_new_prev_hash_with_no_future_job_emits_clear_jobs() {
    start_tracing();

    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (_pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;

    // Drop all NewExtendedMiningJob messages; SetNewPrevHash still flows through.
    let drop_jobs = IgnoreMessage::new(
        MessageDirection::ToDownstream,
        MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
    );
    let (sniffer, sniffer_addr) = start_sniffer(
        "no-future-job",
        pool_addr,
        false,
        vec![drop_jobs.into()],
        Some(60),
    );

    let shutdown = CancellationToken::new();
    let (_cmd_tx, mut event_rx, _handle) = spawn_source(sniffer_addr, shutdown.clone());

    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_MINING_SET_NEW_PREV_HASH,
        )
        .await;

    let got_clear = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match event_rx.recv().await {
                Some(SourceEvent::ClearJobs) => return true,
                Some(_) => {}
                None => return false,
            }
        }
    })
    .await
    .unwrap_or(false);

    assert!(
        got_clear,
        "expected SourceEvent::ClearJobs when SetNewPrevHash arrives with no buffered job"
    );

    shutdown.cancel();
}

/// The sniffer replaces `SubmitSharesSuccess` with a crafted `SubmitSharesError`;
/// the source must emit [`SourceEvent::SharesRejected`].
#[tokio::test]
#[ignore = "downloads Bitcoin Core and sv2-tp binaries on first run"]
async fn share_submission_rejected() {
    start_tracing();

    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (_pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;

    // channel_id = 2: sv2-apps assigns group_id = 1, first individual channel_id = 2.
    let rejection = SubmitSharesError {
        channel_id: 2,
        sequence_number: 0,
        error_code: "bad-share".to_string().try_into().unwrap(),
    };
    let replace_success = ReplaceMessage::new(
        MessageDirection::ToDownstream,
        MESSAGE_TYPE_SUBMIT_SHARES_SUCCESS,
        AnyMessage::Mining(Mining::SubmitSharesError(rejection)),
    );
    let (sniffer, sniffer_addr) = start_sniffer(
        "share-rejected",
        pool_addr,
        false,
        vec![replace_success.into()],
        Some(30),
    );

    let shutdown = CancellationToken::new();
    let (cmd_tx, mut event_rx, _handle) = spawn_source(sniffer_addr, shutdown.clone());

    // Wait for a valid job before submitting a share.
    let template = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            match event_rx.recv().await {
                Some(SourceEvent::ReplaceJob(t)) => return t,
                Some(_) => {}
                None => panic!("event channel closed before ReplaceJob arrived"),
            }
        }
    })
    .await
    .expect("ReplaceJob must arrive within 60 s");

    let en2_size = match &template.merkle_root {
        MerkleRootKind::Computed(mrt) => mrt.extranonce2_range.size,
        MerkleRootKind::Fixed(_) => panic!("SV2 source must produce a Computed merkle root"),
    };

    cmd_tx
        .send(SourceCommand::SubmitShare(Share {
            job_id: template.id.clone(),
            nonce: 0,
            time: template.time,
            version: template.version.base(),
            extranonce2: Some(Extranonce2::new(0, en2_size).expect("valid extranonce2")),
        }))
        .await
        .expect("command channel must be open");

    sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED,
        )
        .await;

    let rejected = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            match event_rx.recv().await {
                Some(SourceEvent::SharesRejected) => return true,
                Some(SourceEvent::SharesAccepted(_)) => return false, // wrong path
                Some(_) => {}
                None => return false,
            }
        }
    })
    .await
    .unwrap_or(false);

    assert!(rejected, "expected SourceEvent::SharesRejected");

    shutdown.cancel();
}

/// The sniffer advertises `extranonce_size = 16`; the source caps its internal
/// counter at 8 bytes and zero-pads to fill the allocation on the wire.
#[tokio::test]
#[ignore = "downloads Bitcoin Core and sv2-tp binaries on first run"]
async fn extranonce2_wire_length_matches_alloc() {
    start_tracing();

    let (_tp, tp_addr) = start_template_provider(None, DifficultyLevel::Low);
    let (_pool, pool_addr, _) = start_pool(sv2_tp_config(tp_addr), vec![], vec![], false).await;

    // Replace OpenExtendedMiningChannelSuccess with one advertising extranonce_size=16
    // (miner's portion only; 4-byte prefix is separate).
    let fake_success = OpenExtendedMiningChannelSuccess {
        request_id: 1, // first channel open
        channel_id: 2, // sv2-apps: group_id=1, first individual channel_id=2
        target: [0xff_u8; 32].to_vec().try_into().unwrap(),
        extranonce_size: 16u16,
        extranonce_prefix: vec![0u8; 4].try_into().unwrap(),
        group_channel_id: 1,
    };
    let replace_open = ReplaceMessage::new(
        MessageDirection::ToDownstream,
        MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS,
        AnyMessage::Mining(Mining::OpenExtendedMiningChannelSuccess(fake_success)),
    );
    let (sniffer, sniffer_addr) = start_sniffer(
        "extranonce-padding",
        pool_addr,
        false,
        vec![replace_open.into()],
        Some(30),
    );

    let shutdown = CancellationToken::new();
    let (cmd_tx, mut event_rx, _handle) = spawn_source(sniffer_addr, shutdown.clone());

    let template = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            match event_rx.recv().await {
                Some(SourceEvent::ReplaceJob(t)) => return t,
                Some(_) => {}
                None => panic!("event channel closed before ReplaceJob arrived"),
            }
        }
    })
    .await
    .expect("ReplaceJob must arrive within 60 s");

    let en2_size = match &template.merkle_root {
        MerkleRootKind::Computed(mrt) => mrt.extranonce2_range.size,
        MerkleRootKind::Fixed(_) => panic!("SV2 source must produce a Computed merkle root"),
    };

    cmd_tx
        .send(SourceCommand::SubmitShare(Share {
            job_id: template.id.clone(),
            nonce: 0,
            time: template.time,
            version: template.version.base(),
            extranonce2: Some(Extranonce2::new(0, en2_size).expect("valid extranonce2")),
        }))
        .await
        .expect("command channel must be open");

    // Drain handshake messages so the queue front is SubmitSharesExtended.
    sniffer
        .wait_for_message_type_and_clean_queue(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
        )
        .await;

    sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_SUBMIT_SHARES_EXTENDED,
        )
        .await;

    let msg = sniffer.next_message_from_downstream();
    match msg {
        Some((_, AnyMessage::Mining(Mining::SubmitSharesExtended(m)))) => {
            assert_eq!(
                m.extranonce.inner_as_ref().len(),
                16,
                "extranonce wire length must match miner alloc"
            );
        }
        other => panic!("expected SubmitSharesExtended, got {:?}", other),
    }

    shutdown.cancel();
}
