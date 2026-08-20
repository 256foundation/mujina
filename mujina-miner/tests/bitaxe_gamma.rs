//! Bitaxe Gamma hardware tests.
//!
//! Every test runs the real daemon against an attached, powered
//! Bitaxe Gamma. With `MUJINA_POOL_URL` and `MUJINA_POOL_USER` set,
//! the daemon mines to that pool and the tests judge shares by the
//! pool's verdicts. With `MUJINA_POOL_URL` unset, the daemon mines
//! to its built-in dummy job source and a share counts as soon as
//! it reaches the source, so the tests run without a network. Each
//! test's name starts with its tier, which the just recipe uses to
//! select tests. All tests are ignored so `just checks` and CI skip
//! them by default.
//!
//! The tests scan the daemon's log output because the REST API does
//! not yet report the pool's accept and reject verdicts. When the
//! API reports them, replace the log scan with an API probe, and
//! drive shutdown through the API instead of a signal.

use std::io::{BufRead, BufReader, Read, Write};
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs, thread};

use mujina_miner::api_client::Client;
use mujina_miner::types::HashRate;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::runtime::Builder;

/// Time allowed for the Bitaxe Gamma to appear on USB.
const BOARD_DEADLINE: Duration = Duration::from_secs(15);

/// Time allowed for an accepted share, from daemon start.
const SHARE_DEADLINE: Duration = Duration::from_secs(90);

/// Time allowed for graceful shutdown after SIGINT.
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(5);

/// Rejected shares tolerated before a test fails. A share in
/// flight when a new block arrives is rejected as stale, so a
/// single reject does not prove the pipeline is broken.
const REJECT_LIMIT: u32 = 3;

/// Hashrate measurement window after the first accepted share. The
/// daemon estimates hashrate from shares in a sliding window, fed
/// at about one share per second by the scheduler's measurement
/// difficulty, so the estimate needs tens of seconds of samples to
/// settle.
const MEASURE_TIME: Duration = Duration::from_secs(60);

/// How often to poll the REST API during measurement.
const API_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Band the measured hashrate must fall in, around the 1 TH/s the
/// thread declares for the chip. Wide enough that estimator noise
/// cannot fail it; tight enough to catch a frequency ramp that
/// stopped halfway or a mostly-dead chip. If this band ever flakes,
/// widen it before tightening anything else.
const HASHRATE_BAND: RangeInclusive<HashRate> =
    HashRate::from_terahashes(0.6)..=HashRate::from_terahashes(1.6);

/// Passes as soon as a share succeeds, proving the whole pipeline
/// mines. Run with `just test-bitaxe-gamma`.
#[test]
#[ignore = "needs a powered Bitaxe Gamma; run with just test-bitaxe-gamma"]
fn smoke_mines_a_share() {
    let mut run = Run::start();
    let verdict = run.watch_for_share().map(|()| match run.mode {
        Mode::Pool => "share accepted by the pool".to_string(),
        Mode::Dummy => "share received by the dummy source".to_string(),
    });
    run.finish(verdict);
}

/// Keeps mining for a measurement window after the first accepted
/// share and checks the hashrate the REST API measures against a
/// band around the chip's declared rate. Run with
/// `just test-bitaxe-gamma baseline`.
#[test]
#[ignore = "needs a powered Bitaxe Gamma; run with just test-bitaxe-gamma"]
fn baseline_mines_at_declared_hashrate() {
    let mut run = Run::start();
    let verdict = run.watch_for_share().and_then(|()| {
        let rate = run.measure_hashrate()?;
        if !HASHRATE_BAND.contains(&rate) {
            return Err(format!(
                "measured hashrate {rate} outside the {} to {} band",
                HASHRATE_BAND.start(),
                HASHRATE_BAND.end()
            ));
        }
        Ok(format!("hashrate {rate} within band"))
    });
    run.finish(verdict);
}

/// Where the daemon sends shares, and so what counts as a share's
/// success. Follows the daemon's own convention: a set
/// `MUJINA_POOL_URL` selects the pool, an unset one the built-in
/// dummy job source.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Pool,
    Dummy,
}

/// One daemon run: the spawned process, its captured output, and
/// the log file the output is teed to.
struct Run {
    daemon: Daemon,
    lines: mpsc::Receiver<String>,
    log: fs::File,
    log_path: PathBuf,
    rejects: u32,
    mode: Mode,
}

impl Run {
    /// Check the environment and start the daemon.
    fn start() -> Self {
        let mode = if env::var_os("MUJINA_POOL_URL").is_some() {
            require_env("MUJINA_POOL_USER");
            Mode::Pool
        } else {
            Mode::Dummy
        };
        match mode {
            Mode::Pool => println!("mining to the pool named in MUJINA_POOL_URL"),
            Mode::Dummy => println!("no MUJINA_POOL_URL; mining to the built-in dummy source"),
        }

        let log_path = log_path();
        let log = fs::File::create(&log_path).expect("create daemon log file");
        println!("daemon log: {}", log_path.display());

        // A leftover MUJINA_CPUMINER_THREADS would let a CPU thread
        // mine the accepted share, and MUJINA_LOG overrides RUST_LOG
        // and could hide the verdict lines.
        let mut daemon = Daemon {
            child: Command::new(env!("CARGO_BIN_EXE_mujina-minerd"))
                .env("RUST_LOG", "mujina_miner=trace")
                .env_remove("MUJINA_CPUMINER_THREADS")
                .env_remove("MUJINA_LOG")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn mujina-minerd"),
        };

        let (line_tx, line_rx) = mpsc::channel();
        forward_lines(daemon.child.stdout.take().unwrap(), line_tx.clone());
        forward_lines(daemon.child.stderr.take().unwrap(), line_tx);

        Self {
            daemon,
            lines: line_rx,
            log,
            log_path,
            rejects: 0,
            mode,
        }
    }

    /// Scan daemon output until the pool accepts a share.
    ///
    /// The accepted-share log lines do not name a board, so the
    /// check is in two parts: a Bitaxe Gamma must connect, and then
    /// any accepted share passes. Only the Bitaxe hashes today;
    /// revisit when a second board model mines.
    fn watch_for_share(&mut self) -> Result<(), String> {
        let start = Instant::now();
        let mut board_seen = false;

        loop {
            match self.lines.recv_timeout(Duration::from_millis(500)) {
                Ok(line) => {
                    if line.to_lowercase().contains("bitaxe gamma") {
                        board_seen = true;
                    }
                    if self.note_line(line)? {
                        return Ok(());
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("the daemon exited before a share succeeded".into());
                }
            }

            if !board_seen && start.elapsed() > BOARD_DEADLINE {
                return Err(format!(
                    "no Bitaxe Gamma connected within {} s; check that the board is plugged in and powered",
                    BOARD_DEADLINE.as_secs()
                ));
            }
            if start.elapsed() > SHARE_DEADLINE {
                return Err(format!(
                    "no share succeeded within {} s",
                    SHARE_DEADLINE.as_secs()
                ));
            }
        }
    }

    /// Keep the daemon mining through the measurement window, then
    /// report the hashrate the REST API measured. Daemon output
    /// keeps flowing to the log file, and rejected shares keep
    /// counting against the limit.
    fn measure_hashrate(&mut self) -> Result<HashRate, String> {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime");
        let client = Client::new();
        let end = Instant::now() + MEASURE_TIME;
        let mut last_poll: Option<Instant> = None;
        let mut hashrate = None;

        while Instant::now() < end {
            match self.lines.recv_timeout(Duration::from_millis(500)) {
                Ok(line) => {
                    self.note_line(line)?;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("the daemon exited during hashrate measurement".into());
                }
            }

            if last_poll.is_none_or(|at| at.elapsed() >= API_POLL_INTERVAL) {
                if let Ok(miner) = runtime.block_on(client.get_miner()) {
                    hashrate = Some(HashRate::from(miner.hashrate));
                }
                last_poll = Some(Instant::now());
            }
        }

        hashrate.ok_or_else(|| "the REST API never answered during measurement".into())
    }

    /// Shut the daemon down, save the rest of its output, and turn
    /// the verdict into the test result.
    fn finish(mut self, verdict: Result<String, String>) {
        self.daemon.stop();
        while let Ok(line) = self.lines.recv_timeout(Duration::from_millis(500)) {
            writeln!(self.log, "{line}").expect("write daemon log file");
        }
        match verdict {
            Ok(what) => println!("{what}; daemon log: {}", self.log_path.display()),
            Err(why) => panic!("{why}; daemon log: {}", self.log_path.display()),
        }
    }

    /// Record a line in the log file and count pool verdicts against
    /// the reject limit. Returns true when the line reports a share's
    /// success: accepted by the pool, or received by the dummy
    /// source.
    fn note_line(&mut self, line: String) -> Result<bool, String> {
        writeln!(self.log, "{line}").expect("write daemon log file");
        let lower = line.to_lowercase();
        if lower.contains("share rejected") {
            self.rejects += 1;
            if self.rejects >= REJECT_LIMIT {
                return Err(format!("the pool rejected {} shares", self.rejects));
            }
        }
        Ok(match self.mode {
            Mode::Pool => lower.contains("share accepted"),
            Mode::Dummy => lower.contains("share received"),
        })
    }
}

/// The daemon process, killed on drop so a panic anywhere in a test
/// does not leave it holding the board and the serial port.
struct Daemon {
    child: Child,
}

impl Daemon {
    /// Ask the daemon to shut down cleanly, so the board is left
    /// with the chip in reset and the core rail off; kill it if it
    /// lingers.
    fn stop(&mut self) {
        let _ = kill(Pid::from_raw(self.child.id() as i32), Signal::SIGINT);
        let deadline = Instant::now() + SHUTDOWN_DEADLINE;
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Forward a child stream to the line channel from its own thread.
fn forward_lines<R: Read + Send + 'static>(stream: R, tx: mpsc::Sender<String>) {
    thread::spawn(move || {
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
}

fn require_env(name: &str) {
    if env::var_os(name).is_none() {
        panic!(
            "{name} is not set; set it alongside MUJINA_POOL_URL, or unset both to mine to the dummy source"
        );
    }
}

fn log_path() -> PathBuf {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs();
    env::temp_dir().join(format!("bitaxe-gamma-{secs}.log"))
}
