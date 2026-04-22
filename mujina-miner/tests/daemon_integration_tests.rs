//! Daemon integration tests.
//!
//! Each test starts a real `Daemon` instance and verifies runtime behaviour
//! end-to-end: config priority, board lifecycle, API responses.
//!
//! Tests that set process-wide environment variables are serialized with
//! `#[serial]`. Do not run with `--test-threads > 1`.

use std::time::Duration;

use mujina_miner::{config::Config, daemon::Daemon};
use serial_test::serial;
use tokio::net::TcpStream;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Poll a TCP address every 50 ms until it accepts a connection or `timeout`
/// elapses. Returns true if the port became reachable within the deadline.
async fn wait_for_port(addr: &str, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if TcpStream::connect(addr).await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Poll `GET /api/v0/miner` until the boards list is non-empty or `timeout`
/// elapses. Returns the number of boards registered.
async fn wait_for_boards(base_url: &str, timeout: Duration) -> usize {
    let deadline = tokio::time::Instant::now() + timeout;
    let client = reqwest::Client::new();
    while tokio::time::Instant::now() < deadline {
        if let Ok(resp) = client.get(format!("{base_url}/api/v0/miner")).send().await {
            if let Ok(state) = resp.json::<serde_json::Value>().await {
                if let Some(boards) = state.get("boards").and_then(|b| b.as_array()) {
                    if !boards.is_empty() {
                        return boards.len();
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    0
}

// ---------------------------------------------------------------------------
// Config priority tests
// ---------------------------------------------------------------------------

/// Verify that `--config` is read and its values take effect.
///
/// Uses port 17785 (≠ default 7785) so a passing test proves the file was
/// actually read rather than the hard-coded default matching by coincidence.
#[tokio::test]
#[serial]
async fn test_cli_config_file_is_read() {
    const TEST_PORT: u16 = 17785;
    let listen_addr = format!("127.0.0.1:{TEST_PORT}");

    let tmp = tempfile::tempdir().expect("failed to create tempdir");
    let config_path = tmp.path().join("mujina.yaml");
    std::fs::write(
        &config_path,
        format!("api:\n  listen: \"{listen_addr}\"\nbackplane:\n  usb_enabled: false\n"),
    )
    .expect("failed to write temp config");

    let config = Config::load_with(Some(config_path), &[]).expect("Config::load_with() failed");

    assert_eq!(
        config.api.listen, listen_addr,
        "config.api.listen should reflect the value from the --config file"
    );

    let daemon = Daemon::new(config);
    let shutdown = daemon.shutdown_token();
    let daemon_handle = tokio::spawn(async move { daemon.run().await });

    let listening = wait_for_port(&listen_addr, Duration::from_secs(5)).await;

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), daemon_handle).await;

    assert!(
        listening,
        "mujina-minerd should be listening on {listen_addr} (port from --config file)"
    );
}

/// Verify that a `MUJINA__*` environment variable overrides both the config
/// file and the hard-coded default.
///
/// Priority chain exercised:
///   env var (17787)  >  --config file (17785)  >  built-in default (7785)
#[tokio::test]
#[serial]
async fn test_env_var_overrides_config_file() {
    const TEST_PORT_CONFIG: u16 = 17785;
    const TEST_PORT_ENV: u16 = 17787;

    let config_listen = format!("127.0.0.1:{TEST_PORT_CONFIG}");
    let env_listen = format!("127.0.0.1:{TEST_PORT_ENV}");

    let tmp = tempfile::tempdir().expect("failed to create tempdir");
    let config_path = tmp.path().join("mujina.yaml");
    std::fs::write(
        &config_path,
        format!("api:\n  listen: \"{config_listen}\"\nbackplane:\n  usb_enabled: false\n"),
    )
    .expect("failed to write temp config");

    // SAFETY: test is marked #[serial] so no other threads touch the environment.
    unsafe {
        std::env::set_var("MUJINA__API__LISTEN", &env_listen);
    }

    let config = Config::load_with(Some(config_path), &[]).expect("Config::load_with() failed");

    // SAFETY: same serial-test guarantee as above.
    unsafe {
        std::env::remove_var("MUJINA__API__LISTEN");
    }

    assert_eq!(
        config.api.listen, env_listen,
        "MUJINA__API__LISTEN (port {TEST_PORT_ENV}) should override --config file (port {TEST_PORT_CONFIG})"
    );

    let daemon = Daemon::new(config);
    let shutdown = daemon.shutdown_token();
    let daemon_handle = tokio::spawn(async move { daemon.run().await });

    let listening = wait_for_port(&env_listen, Duration::from_secs(5)).await;

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), daemon_handle).await;

    assert!(
        listening,
        "mujina-minerd should be listening on {env_listen} (port from MUJINA__API__LISTEN env var)"
    );
}

/// Verify that a CLI flag override wins over env vars, the config file, and
/// the hard-coded default.
///
/// CLI flags are not handled inside `Config::load_with`; they are applied by
/// the caller (see `minerd.rs`) as a direct field assignment after loading.
/// The test mirrors that pattern exactly.
///
/// Priority chain exercised:
///   CLI flag (17788)  >  env var (17787)  >  --config file (17785)  >  built-in default (7785)
#[tokio::test]
#[serial]
async fn test_command_line_arg_override() {
    const TEST_PORT_CONFIG: u16 = 17785;
    const TEST_PORT_ENV: u16 = 17787;
    const TEST_PORT_CLI: u16 = 17788;

    let config_listen = format!("127.0.0.1:{TEST_PORT_CONFIG}");
    let env_listen = format!("127.0.0.1:{TEST_PORT_ENV}");
    let cli_listen = format!("127.0.0.1:{TEST_PORT_CLI}");

    let tmp = tempfile::tempdir().expect("failed to create tempdir");
    let config_path = tmp.path().join("mujina.yaml");
    std::fs::write(
        &config_path,
        format!("api:\n  listen: \"{config_listen}\"\nbackplane:\n  usb_enabled: false\n"),
    )
    .expect("failed to write temp config");

    // SAFETY: test is marked #[serial] so no other threads touch the environment.
    unsafe {
        std::env::set_var("MUJINA__API__LISTEN", &env_listen);
    }

    let set_overrides = vec![("api.listen".to_string(), cli_listen.clone())];
    let config =
        Config::load_with(Some(config_path), &set_overrides).expect("Config::load_with() failed");

    // SAFETY: same serial-test guarantee as above.
    unsafe {
        std::env::remove_var("MUJINA__API__LISTEN");
    }

    assert_eq!(
        config.api.listen, cli_listen,
        "CLI flag (port {TEST_PORT_CLI}) should override env var (port {TEST_PORT_ENV}) and --config file (port {TEST_PORT_CONFIG})"
    );

    let daemon = Daemon::new(config);
    let shutdown = daemon.shutdown_token();
    let daemon_handle = tokio::spawn(async move { daemon.run().await });

    let listening = wait_for_port(&cli_listen, Duration::from_secs(5)).await;

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), daemon_handle).await;

    assert!(
        listening,
        "mujina-minerd should be listening on {cli_listen} (port from CLI --api-listen flag)"
    );
}

// ---------------------------------------------------------------------------
// Board lifecycle tests
// ---------------------------------------------------------------------------

/// Verify that the CPU miner board starts when configured via the unified
/// config system (`boards.cpu_miner.enabled = true`), without the legacy
/// `MUJINA_CPUMINER_THREADS` environment variable being set.
#[tokio::test]
#[serial]
async fn test_cpu_miner_starts_from_config() {
    const TEST_PORT: u16 = 17790;
    let listen_addr = format!("127.0.0.1:{TEST_PORT}");
    let base_url = format!("http://{listen_addr}");

    let tmp = tempfile::tempdir().expect("failed to create tempdir");
    let config_path = tmp.path().join("mujina.yaml");
    std::fs::write(
        &config_path,
        format!(
            "api:\n  listen: \"{listen_addr}\"\n\
             backplane:\n  usb_enabled: false\n\
             boards:\n  cpu_miner:\n    enabled: true\n    threads: 1\n"
        ),
    )
    .expect("failed to write temp config");

    // SAFETY: test is marked #[serial] so no other threads touch the environment.
    unsafe {
        // Ensure legacy env vars are absent — they must not be required.
        std::env::remove_var("MUJINA_CPUMINER_THREADS");
        std::env::remove_var("MUJINA_CPUMINER_DUTY");
    }

    let config = Config::load_with(Some(config_path), &[]).expect("Config::load_with() failed");

    assert!(
        config.boards.cpu_miner.enabled,
        "config should have cpu_miner.enabled = true"
    );
    assert_eq!(
        config.boards.cpu_miner.threads, 1,
        "config should have cpu_miner.threads = 1"
    );

    let daemon = Daemon::new(config);
    let shutdown = daemon.shutdown_token();
    let daemon_handle = tokio::spawn(async move { daemon.run().await });

    let listening = wait_for_port(&listen_addr, Duration::from_secs(5)).await;
    assert!(listening, "API server did not start on {listen_addr}");

    let board_count = wait_for_boards(&base_url, Duration::from_secs(5)).await;

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), daemon_handle).await;

    assert!(
        board_count > 0,
        "CPU miner board should be registered when enabled via config"
    );
}
