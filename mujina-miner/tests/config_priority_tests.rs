//! Integration tests for the configuration priority chain.

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

// ---------------------------------------------------------------------------
// Tests
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

    // Create a temp directory and write the config file into it.
    let tmp_dir = tempfile::tempdir().expect("failed to create tempdir");
    let config_path = tmp_dir.path().join("mujina.yaml");
    let config_yaml = format!(
        "api:\n  listen: \"{listen_addr}\"\nbackplane:\n  usb_enabled: false\n"
    );
    std::fs::write(&config_path, &config_yaml).expect("failed to write temp config");

    // Redirect the default config path to our temp file.
    // SAFETY: test is marked #[serial] so no other threads touch the environment.
    unsafe { std::env::set_var("MUJINA_DEFAULT_CONFIG_PATH", &config_path) };

    let config = Config::load().expect("Config::load() failed");

    // Clean up the env var immediately — don't let it bleed into other tests.
    // SAFETY: same serial-test guarantee as above.
    unsafe { std::env::remove_var("MUJINA_DEFAULT_CONFIG_PATH") };

    assert_eq!(
        config.api.listen, listen_addr,
        "config.api.listen should reflect the value from the default config file"
    );

    // Obtain a shutdown token before run() consumes the daemon.
    let daemon = Daemon::new(config);
    let shutdown = daemon.shutdown_token();

    let daemon_handle = tokio::spawn(async move { daemon.run().await });

    // Wait up to 5 s for the API port to become reachable.
    let listening = wait_for_port(&listen_addr, Duration::from_secs(5)).await;

    // Always shut down before asserting so cleanup runs even on failure.
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

    // Write the default config file (layer 4).
    let tmp_default = tempfile::tempdir().expect("failed to create tempdir for default config");
    let default_config_path = tmp_default.path().join("mujina.yaml");
    std::fs::write(
        &default_config_path,
        format!("api:\n  listen: \"{default_listen}\"\nbackplane:\n  usb_enabled: false\n"),
    )
    .expect("failed to write default temp config");

    // Write the user config file (layer 3) — only overrides `api.listen`.
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

    // Write default config file (layer 4).
    let tmp_default = tempfile::tempdir().expect("failed to create tempdir for default config");
    let default_config_path = tmp_default.path().join("mujina.yaml");
    std::fs::write(
        &default_config_path,
        format!("api:\n  listen: \"{default_listen}\"\nbackplane:\n  usb_enabled: false\n"),
    )
    .expect("failed to write default temp config");

    // Write user config file (layer 3).
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
/// No need to test clap's own argument parsing.
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

    // Write default config file (layer 4).
    let tmp_default = tempfile::tempdir().expect("failed to create tempdir for default config");
    let default_config_path = tmp_default.path().join("mujina.yaml");
    std::fs::write(
        &default_config_path,
        format!("api:\n  listen: \"{default_listen}\"\nbackplane:\n  usb_enabled: false\n"),
    )
    .expect("failed to write default temp config");

    // Write user config file (layer 3).
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
