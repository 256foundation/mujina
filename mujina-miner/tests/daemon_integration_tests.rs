//! Daemon integration tests.
//!
//! Each test starts a real `Daemon` instance and verifies runtime behaviour
//! end-to-end: config priority, board lifecycle, API responses.
//!
//! Tests mutate process-wide environment variables and are serialized with
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

/// Verify that the daemon reads the default config file and binds the API
/// server to the port declared there.
///
/// Uses port 17785 (≠ default 7785) so a passing test proves the file was
/// actually read, not that the default happened to match.
///
/// A temporary directory is used instead of /etc/mujina so the test runs
/// without elevated permissions. `MUJINA_DEFAULT_CONFIG_PATH` redirects
/// `Config::load()` to that temp file.
#[tokio::test]
#[serial]
async fn test_default_config_file() {
    const TEST_PORT: u16 = 17785;
    let listen_addr = format!("127.0.0.1:{TEST_PORT}");

    let tmp_dir = tempfile::tempdir().expect("failed to create tempdir");
    let config_path = tmp_dir.path().join("mujina.yaml");
    std::fs::write(
        &config_path,
        format!("api:\n  listen: \"{listen_addr}\"\nbackplane:\n  usb_enabled: false\n"),
    )
    .expect("failed to write temp config");

    // Redirect the default config path to our temp file. Also clear
    // MUJINA_CONFIG_FILE_PATH so a value set in the shell cannot override
    // the default config file we are trying to test.
    // SAFETY: test is marked #[serial] so no other threads touch the environment.
    unsafe {
        std::env::set_var("MUJINA_DEFAULT_CONFIG_PATH", &config_path);
        std::env::remove_var("MUJINA_CONFIG_FILE_PATH");
    }

    let config = Config::load().expect("Config::load() failed");

    // SAFETY: same serial-test guarantee as above.
    unsafe { std::env::remove_var("MUJINA_DEFAULT_CONFIG_PATH") };

    assert_eq!(
        config.api.listen, listen_addr,
        "config.api.listen should reflect the value from the default config file"
    );

    let daemon = Daemon::new(config);
    let shutdown = daemon.shutdown_token();
    let daemon_handle = tokio::spawn(async move { daemon.run().await });

    let listening = wait_for_port(&listen_addr, Duration::from_secs(5)).await;

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), daemon_handle).await;

    assert!(
        listening,
        "mujina-minerd should be listening on {listen_addr} (port from default config file)"
    );
}

/// Verify that a user-specified config file (via `MUJINA_CONFIG_FILE_PATH`)
/// overrides the value set in the default config file, which itself overrides
/// the hard-coded default listen address.
///
/// Priority chain exercised:
///   user config file (17786)  >  default config file (17785)  >  built-in default (7785)
#[tokio::test]
#[serial]
async fn test_user_config_override() {
    const TEST_PORT_DEFAULT_CONFIG: u16 = 17785;
    const TEST_PORT_USER_CONFIG: u16 = 17786;

    let default_listen = format!("127.0.0.1:{TEST_PORT_DEFAULT_CONFIG}");
    let user_listen = format!("127.0.0.1:{TEST_PORT_USER_CONFIG}");

    let tmp_default = tempfile::tempdir().expect("failed to create tempdir for default config");
    let default_config_path = tmp_default.path().join("mujina.yaml");
    std::fs::write(
        &default_config_path,
        format!("api:\n  listen: \"{default_listen}\"\nbackplane:\n  usb_enabled: false\n"),
    )
    .expect("failed to write default temp config");

    let tmp_user = tempfile::tempdir().expect("failed to create tempdir for user config");
    let user_config_path = tmp_user.path().join("mujina.yaml");
    std::fs::write(
        &user_config_path,
        format!("api:\n  listen: \"{user_listen}\"\n"),
    )
    .expect("failed to write user temp config");

    // SAFETY: test is marked #[serial] so no other threads touch the environment.
    unsafe {
        std::env::set_var("MUJINA_DEFAULT_CONFIG_PATH", &default_config_path);
        std::env::set_var("MUJINA_CONFIG_FILE_PATH", &user_config_path);
    }

    let config = Config::load().expect("Config::load() failed");

    // SAFETY: same serial-test guarantee as above.
    unsafe {
        std::env::remove_var("MUJINA_DEFAULT_CONFIG_PATH");
        std::env::remove_var("MUJINA_CONFIG_FILE_PATH");
    }

    assert_eq!(
        config.api.listen, user_listen,
        "user config file (port {TEST_PORT_USER_CONFIG}) should override default config file (port {TEST_PORT_DEFAULT_CONFIG})"
    );

    let daemon = Daemon::new(config);
    let shutdown = daemon.shutdown_token();
    let daemon_handle = tokio::spawn(async move { daemon.run().await });

    let listening = wait_for_port(&user_listen, Duration::from_secs(5)).await;

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), daemon_handle).await;

    assert!(
        listening,
        "mujina-minerd should be listening on {user_listen} (port from user config file)"
    );
}

/// Verify that a `MUJINA__*` environment variable overrides both config files
/// and the hard-coded default.
///
/// Priority chain exercised:
///   env var (17787)  >  user config file (17786)  >  default config file (17785)  >  built-in default (7785)
#[tokio::test]
#[serial]
async fn test_env_var_override() {
    const TEST_PORT_DEFAULT_CONFIG: u16 = 17785;
    const TEST_PORT_USER_CONFIG: u16 = 17786;
    const TEST_PORT_ENV_VAR: u16 = 17787;

    let default_listen = format!("127.0.0.1:{TEST_PORT_DEFAULT_CONFIG}");
    let user_listen = format!("127.0.0.1:{TEST_PORT_USER_CONFIG}");
    let env_listen = format!("127.0.0.1:{TEST_PORT_ENV_VAR}");

    let tmp_default = tempfile::tempdir().expect("failed to create tempdir for default config");
    let default_config_path = tmp_default.path().join("mujina.yaml");
    std::fs::write(
        &default_config_path,
        format!("api:\n  listen: \"{default_listen}\"\nbackplane:\n  usb_enabled: false\n"),
    )
    .expect("failed to write default temp config");

    let tmp_user = tempfile::tempdir().expect("failed to create tempdir for user config");
    let user_config_path = tmp_user.path().join("mujina.yaml");
    std::fs::write(
        &user_config_path,
        format!("api:\n  listen: \"{user_listen}\"\n"),
    )
    .expect("failed to write user temp config");

    // SAFETY: test is marked #[serial] so no other threads touch the environment.
    unsafe {
        std::env::set_var("MUJINA_DEFAULT_CONFIG_PATH", &default_config_path);
        std::env::set_var("MUJINA_CONFIG_FILE_PATH", &user_config_path);
        // Layer 2: env var override — highest priority short of CLI flags.
        std::env::set_var("MUJINA__API__LISTEN", &env_listen);
    }

    let config = Config::load().expect("Config::load() failed");

    // SAFETY: same serial-test guarantee as above.
    unsafe {
        std::env::remove_var("MUJINA_DEFAULT_CONFIG_PATH");
        std::env::remove_var("MUJINA_CONFIG_FILE_PATH");
        std::env::remove_var("MUJINA__API__LISTEN");
    }

    assert_eq!(
        config.api.listen, env_listen,
        "MUJINA__API__LISTEN (port {TEST_PORT_ENV_VAR}) should override user config (port {TEST_PORT_USER_CONFIG}) and default config (port {TEST_PORT_DEFAULT_CONFIG})"
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

/// Verify that a CLI flag override wins over env vars, both config files, and
/// the hard-coded default.
///
/// CLI flags are not handled inside `Config::load_with`; they are applied by
/// the caller (see `minerd.rs`) as a direct field assignment after loading.
/// The test mirrors that pattern exactly.
///
/// Priority chain exercised:
///   CLI flag (17788)  >  env var (17787)  >  user config (17786)  >  default config (17785)  >  built-in default (7785)
#[tokio::test]
#[serial]
async fn test_command_line_arg_override() {
    const TEST_PORT_DEFAULT_CONFIG: u16 = 17785;
    const TEST_PORT_USER_CONFIG: u16 = 17786;
    const TEST_PORT_ENV_VAR: u16 = 17787;
    const TEST_PORT_COMMAND_LINE_ARG: u16 = 17788;

    let default_listen = format!("127.0.0.1:{TEST_PORT_DEFAULT_CONFIG}");
    let user_listen = format!("127.0.0.1:{TEST_PORT_USER_CONFIG}");
    let env_listen = format!("127.0.0.1:{TEST_PORT_ENV_VAR}");
    let cli_listen = format!("127.0.0.1:{TEST_PORT_COMMAND_LINE_ARG}");

    let tmp_default = tempfile::tempdir().expect("failed to create tempdir for default config");
    let default_config_path = tmp_default.path().join("mujina.yaml");
    std::fs::write(
        &default_config_path,
        format!("api:\n  listen: \"{default_listen}\"\nbackplane:\n  usb_enabled: false\n"),
    )
    .expect("failed to write default temp config");

    let tmp_user = tempfile::tempdir().expect("failed to create tempdir for user config");
    let user_config_path = tmp_user.path().join("mujina.yaml");
    std::fs::write(
        &user_config_path,
        format!("api:\n  listen: \"{user_listen}\"\n"),
    )
    .expect("failed to write user temp config");

    // SAFETY: test is marked #[serial] so no other threads touch the environment.
    unsafe {
        std::env::set_var("MUJINA_DEFAULT_CONFIG_PATH", &default_config_path);
        std::env::set_var("MUJINA_CONFIG_FILE_PATH", &user_config_path);
        std::env::set_var("MUJINA__API__LISTEN", &env_listen);
    }

    let mut config = Config::load().expect("Config::load() failed");

    // SAFETY: same serial-test guarantee as above.
    unsafe {
        std::env::remove_var("MUJINA_DEFAULT_CONFIG_PATH");
        std::env::remove_var("MUJINA_CONFIG_FILE_PATH");
        std::env::remove_var("MUJINA__API__LISTEN");
    }

    // Layer 1 (highest): CLI flag applied as a direct field assignment,
    // exactly as minerd.rs does after calling Config::load_with().
    config.api.listen = cli_listen.clone();

    assert_eq!(
        config.api.listen, cli_listen,
        "CLI flag (port {TEST_PORT_COMMAND_LINE_ARG}) should override env var (port {TEST_PORT_ENV_VAR}), user config (port {TEST_PORT_USER_CONFIG}), and default config (port {TEST_PORT_DEFAULT_CONFIG})"
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
        std::env::set_var("MUJINA_DEFAULT_CONFIG_PATH", &config_path);
        // Clear the user config file path so a shell value cannot override
        // the test config.
        std::env::remove_var("MUJINA_CONFIG_FILE_PATH");
        // Ensure legacy env vars are absent — they must not be required.
        std::env::remove_var("MUJINA_CPUMINER_THREADS");
        std::env::remove_var("MUJINA_CPUMINER_DUTY");
    }

    let config = Config::load().expect("Config::load() failed");

    // SAFETY: same serial-test guarantee as above.
    unsafe { std::env::remove_var("MUJINA_DEFAULT_CONFIG_PATH") };

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
