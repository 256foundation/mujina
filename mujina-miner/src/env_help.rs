//! Registry of environment variables that control the daemon, plus the
//! help text shown by `mujina-minerd --help`.
//!
//! Each variable is read in its own module; this registry is the single
//! place that documents them for users. Keep it in sync when adding or
//! changing a control variable.

use std::fmt::Write;

/// Render the grouped environment-variable reference for `--help`.
pub fn help_text() -> String {
    let mut out = String::from("Configuration is read from environment variables:\n");

    for group in GROUPS {
        write!(out, "\n{}:\n", group.title).unwrap();
        for var in group.vars {
            writeln!(out, "  {}", var.name).unwrap();
            out.push_str(&wrap(var.summary, INDENT, WIDTH));
            if let Some(default) = var.default {
                writeln!(out, "{INDENT}default: {default}").unwrap();
            }
            if let Some(example) = var.example {
                writeln!(out, "{INDENT}example: {example}").unwrap();
            }
        }
    }

    out
}

/// Hanging indent applied to every line under a variable name.
const INDENT: &str = "      ";

/// Maximum rendered line width before wrapping.
const WIDTH: usize = 79;

/// Wrap `text` into `indent`-prefixed lines no wider than `width`,
/// breaking on spaces. Words longer than the available width are kept
/// whole rather than split.
fn wrap(text: &str, indent: &str, width: usize) -> String {
    let avail = width.saturating_sub(indent.len());
    let mut out = String::new();
    let mut line = String::new();

    for word in text.split_whitespace() {
        if !line.is_empty() && line.len() + 1 + word.len() > avail {
            writeln!(out, "{indent}{line}").unwrap();
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        writeln!(out, "{indent}{line}").unwrap();
    }

    out
}

/// A titled group of related variables.
struct EnvGroup {
    title: &'static str,
    vars: &'static [EnvVar],
}

/// A single environment variable users can set to control the daemon.
struct EnvVar {
    /// Variable name, e.g. `MUJINA_POOL_URL`.
    name: &'static str,

    /// One-line explanation of what the variable does.
    summary: &'static str,

    /// Behavior when the variable is unset, when there is a meaningful
    /// fallback worth naming.
    default: Option<&'static str>,

    /// Example value, when one clarifies the expected format.
    example: Option<&'static str>,
}

const GROUPS: &[EnvGroup] = &[
    EnvGroup {
        title: "Pool (job source)",
        vars: &[
            EnvVar {
                name: "MUJINA_POOL_URL",
                summary: "Stratum v1 pool URL. When unset, the daemon runs a \
                          built-in dummy job source instead.",
                default: None,
                example: Some("stratum+tcp://pool.example.com:3333"),
            },
            EnvVar {
                name: "MUJINA_POOL_USER",
                summary: "Worker username sent to the pool.",
                default: Some("mujina-testing"),
                example: Some("myworker.1"),
            },
            EnvVar {
                name: "MUJINA_POOL_PASS",
                summary: "Worker password sent to the pool.",
                default: Some("x"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_POOL_FORCED_RATE",
                summary: "Override the share target so the source receives \
                          roughly this many shares per minute regardless of \
                          pool difficulty, for testing share submission at low \
                          hashrate. Requires MUJINA_POOL_URL.",
                default: Some("18 when set to an invalid value"),
                example: None,
            },
        ],
    },
    EnvGroup {
        title: "CPU miner",
        vars: &[
            EnvVar {
                name: "MUJINA_CPUMINER_THREADS",
                summary: "Number of CPU mining threads. Setting this enables the \
                          CPU mining backend, which needs no ASIC hardware.",
                default: Some("unset disables CPU mining"),
                example: Some("4"),
            },
            EnvVar {
                name: "MUJINA_CPUMINER_DUTY",
                summary: "CPU duty cycle percent (1-100). Each thread hashes this \
                          fraction of every second and sleeps the rest, capping \
                          sustained CPU load.",
                default: Some("50"),
                example: None,
            },
        ],
    },
    EnvGroup {
        title: "API server",
        vars: &[
            EnvVar {
                name: "MUJINA_API_LISTEN",
                summary: "Address the REST API listens on. A bare host or IP \
                          gets the default port :7785 appended.",
                default: Some("127.0.0.1:7785"),
                example: Some("0.0.0.0:7785"),
            },
            EnvVar {
                name: "MUJINA_API_RAW_REGISTERS",
                summary: "Serve the raw BZM2 register read/write endpoints, \
                          which drive silicon directly with no higher-level \
                          guard (fan law, calibration, thermal interlock) in \
                          front of them.",
                default: Some("off"),
                example: Some("MUJINA_API_RAW_REGISTERS=1"),
            },
            EnvVar {
                name: "MUJINA_API_RAW_REGISTERS_ALLOW_REMOTE",
                summary: "Also serve the raw register endpoints when the API \
                          is bound to a non-loopback address. Meaningless \
                          unless MUJINA_API_RAW_REGISTERS is also set.",
                default: Some("off"),
                example: None,
            },
        ],
    },
    EnvGroup {
        title: "Hardware",
        vars: &[
            EnvVar {
                name: "MUJINA_USB_DISABLE",
                summary: "Set to any value to skip USB board discovery, useful for \
                          CPU-only runs.",
                default: Some("unset enables USB discovery"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_SERIAL_CFLAG",
                summary: "Platform control-flag override for a serial port, as \
                          `mask,value` applied to `c_cflag` last at open and after \
                          every baud-rate change. For drivers that take their line \
                          rate from private control-flag bits rather than \
                          c_ispeed/c_ospeed.",
                default: Some("unset applies no override"),
                example: Some("MUJINA_SERIAL_CFLAG=0x300F,0x2001"),
            },
            EnvVar {
                name: "MUJINA_BZM2_DRY_RUN",
                summary: "Set to 1 for dry run: every state-changing frame to a \
                          BZM2 chain is REFUSED at the transport and logged. Reads, \
                          telemetry and diagnostics work. Stronger than observer \
                          mode, which withholds work but still enumerates -- and \
                          enumeration assigns ASIC ids, which is a write. Use this \
                          for first contact with a powered part.",
                default: Some("unset permits writes"),
                example: Some("MUJINA_BZM2_DRY_RUN=1"),
            },
            EnvVar {
                name: "MUJINA_OBSERVE",
                summary: "0/false/no/off/empty or unset mines normally; 1/true/yes/on, or \
                          any other value, is observer mode: boards are attached, \
                          enumerated and read, telemetry streams and read-only \
                          diagnostics work, but no job source is created and mining \
                          starts paused, so the hardware is never given work. Resume \
                          through the API to start mining.",
                default: Some("unset mines normally"),
                example: None,
            },
        ],
    },
    EnvGroup {
        title: "Scheduler",
        vars: &[
            EnvVar {
                name: "MUJINA_STAGGER_START_S",
                summary: "Spacing in whole seconds between releasing chains that \
                          share one supply at start. The first assignment gives \
                          work to one thread; each next thread is released once \
                          the previous one has returned a share and the spacing \
                          has passed, so the supply sees one chain's load step at \
                          a time rather than all of them at once.",
                default: Some("unset starts every chain together"),
                example: Some("MUJINA_STAGGER_START_S=5"),
            },
            EnvVar {
                name: "MUJINA_STAGGER_STOP_S",
                summary: "Spacing in whole seconds between idling chains on a \
                          pause. The first thread idles at once and each next one \
                          idles one spacing later, so a shared supply sees each \
                          chain's load come off as its own step. A second pause \
                          or a resume mid-stop finishes the stop at once.",
                default: Some("unset idles every chain together"),
                example: Some("MUJINA_STAGGER_STOP_S=5"),
            },
        ],
    },
    EnvGroup {
        title: "BZM2 board: chain",
        vars: &[
            EnvVar {
                name: "MUJINA_BZM2_SERIAL",
                summary: "Comma-separated serial device paths, one per UART bus. \
                          Also MUJINA_BZM2_SERIAL_PATHS. Setting this is what \
                          enables a BZM2 board at all.",
                default: Some("unset disables the BZM2 board"),
                example: Some("MUJINA_BZM2_SERIAL=/dev/ttyUSB0,/dev/ttyUSB1"),
            },
            EnvVar {
                name: "MUJINA_BZM2_BAUD",
                summary: "UART baud rate for every configured bus.",
                default: Some("5000000"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_HASHRATE_THS",
                summary: "Per-ASIC nameplate hash rate in TH/s, used to report \
                          hash rate before the first measured share. A property \
                          of the silicon, not something with a safe default.",
                default: Some("required once a chain is configured; startup error if missing"),
                example: Some("MUJINA_BZM2_HASHRATE_THS=63.0"),
            },
            EnvVar {
                name: "MUJINA_BZM2_TIMESTAMP_COUNT",
                summary: "Protocol timestamp field count used when framing UART \
                          instructions.",
                default: Some("60"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_NONCE_GAP",
                summary: "Protocol nonce search gap between ASICs on a chain.",
                default: Some("0x4c (76)"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_RESULT_MIN_DIFF",
                summary: "Minimum difficulty a result must clear to be accepted; \
                          a positive finite value only.",
                default: Some("unset accepts every result"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_DISPATCH_MS",
                summary: "Interval in milliseconds between work dispatches to a \
                          chain.",
                default: Some("500"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_DTS_VS_GEN",
                summary: "DTS/VS telemetry generation a chain speaks: gen1 \
                          publishes one voltage channel and no die temperature, \
                          gen2 publishes three channels and a temperature.",
                default: Some("gen2"),
                example: Some("MUJINA_BZM2_DTS_VS_GEN=gen1"),
            },
            EnvVar {
                name: "MUJINA_BZM2_DTS_VS_GAP",
                summary: "TDM interval between sensor-stream frames. A property \
                          of the controller's CPU budget, not the silicon: the \
                          fastest setting saturates a two-core control board.",
                default: Some("100"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_STORED_CALIBRATION",
                summary: "Path to a saved calibration profile to replay at \
                          startup instead of recalibrating.",
                default: Some("unset replays nothing"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_HEARTBEAT",
                summary: "Enable a periodic liveness ping to each configured \
                          bus.",
                default: Some("off"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_HEARTBEAT_MS",
                summary: "Interval between heartbeat pings, when enabled.",
                default: Some("3000"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_VARIANT",
                summary: "Override the detected platform variant, for example \
                          to declare a test host that is not the real chassis.",
                default: Some("read from the platform's own definition"),
                example: None,
            },
        ],
    },
    EnvGroup {
        title: "BZM2 board: enumeration and calibration",
        vars: &[
            EnvVar {
                name: "MUJINA_BZM2_ENUMERATE_CHAIN",
                summary: "Run chain-ID enumeration at startup. Also \
                          MUJINA_BZM2_AUTO_ENUMERATE.",
                default: Some("off"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_ENUM_START_ID",
                summary: "First ASIC id that enumeration assigns.",
                default: Some("0"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_ENUM_MAX_ASICS_PER_BUS",
                summary: "Comma-separated cap on ASICs enumerated per bus.",
                default: Some("MUJINA_BZM2_ASICS_PER_BUS's own counts, or 100 per bus"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_CALIBRATE",
                summary: "Run calibration at startup. Also \
                          MUJINA_BZM2_ENABLE_PNP.",
                default: Some("off"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_ASICS_PER_BUS",
                summary: "Comma-separated ASIC count per bus, one entry per \
                          configured serial path. No safe default: a silent \
                          guess here is indistinguishable, downstream, from a \
                          dark chain.",
                default: Some("required once a chain is configured; startup panic if missing"),
                example: Some("MUJINA_BZM2_ASICS_PER_BUS=100,100"),
            },
            EnvVar {
                name: "MUJINA_BZM2_ASICS_PER_DOMAIN",
                summary: "Comma-separated ASIC count per voltage domain.",
                default: Some("1"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_DOMAIN_VOLTAGE_OFFSETS_MV",
                summary: "Comma-separated per-domain voltage trim, in mV.",
                default: Some("0 for every domain"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_APPLY_SAVED_OPERATING_POINT",
                summary: "Replay a saved operating point at startup instead of \
                          recalibrating. Also \
                          MUJINA_BZM2_REPLAY_STORED_CALIBRATION.",
                default: Some("on"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_CALIBRATION_DISCOVER_ENGINES",
                summary: "Run engine-map discovery during calibration. Also \
                          MUJINA_BZM2_DISCOVER_ENGINES_FOR_CALIBRATION.",
                default: Some("on"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_OPERATING_CLASS",
                summary: "Select the board's operating class (silicon bin). \
                          Also MUJINA_BZM2_BOARD_BIN.",
                default: Some("generic"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_PERFORMANCE_MODE",
                summary: "Select the calibration performance mode. Also \
                          MUJINA_BZM2_MINING_STRATEGY.",
                default: Some("standard"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_SWEEP_MODE",
                summary: "Calibration sweep strategy toggle. Also \
                          MUJINA_BZM2_SWEEP_STRATEGY, and the narrower \
                          MUJINA_BZM2_SWEEP_VOLTAGE, MUJINA_BZM2_SWEEP_FREQUENCY, \
                          MUJINA_BZM2_SWEEP_PASS_RATE.",
                default: Some("off"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_PER_STACK_CLOCKING",
                summary: "Clock each PLL stack independently rather than \
                          together. Also MUJINA_BZM2_SPLIT_STACK_FREQUENCY.",
                default: Some("off"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_FORCE_RETUNE",
                summary: "Force a retune even when a valid saved operating \
                          point exists. Also MUJINA_BZM2_FORCE_RECALIBRATION.",
                default: Some("off"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_SAVED_OPERATING_POINT_PATH",
                summary: "Path to write and read the saved operating-point \
                          profile. Also MUJINA_BZM2_CALIBRATION_PROFILE.",
                default: Some("unset"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_SITE_TEMP_C",
                summary: "Ambient temperature calibration assumes, in Celsius. \
                          Also MUJINA_BZM2_AMBIENT_TEMP_C.",
                default: Some("20.0"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_CALIBRATION_POST1_DIVIDER",
                summary: "PLL POST1 divider used while calibrating.",
                default: Some("0"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_CALIBRATION_SKIP_LOCK_CHECK",
                summary: "Skip waiting for PLL lock during calibration.",
                default: Some("off"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_CALIBRATION_LOCK_TIMEOUT_MS",
                summary: "How long to wait for PLL lock during calibration.",
                default: Some("1000"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_CALIBRATION_LOCK_POLL_MS",
                summary: "Poll interval while waiting for PLL lock.",
                default: Some("100"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_ENGINE_DISCOVERY_TDM_PREDIV_RAW",
                summary: "Raw TDM pre-divider used for engine-map discovery. \
                          Also MUJINA_BZM2_CALIBRATION_ENGINE_DISCOVERY_TDM_PREDIV_RAW.",
                default: Some("0x0f"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_ENGINE_DISCOVERY_TDM_COUNTER",
                summary: "TDM counter used for engine-map discovery. Also \
                          MUJINA_BZM2_CALIBRATION_ENGINE_DISCOVERY_TDM_COUNTER.",
                default: Some("16"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_ENGINE_DISCOVERY_TIMEOUT_MS",
                summary: "Per-engine probe timeout during engine-map discovery. \
                          Also MUJINA_BZM2_CALIBRATION_ENGINE_DISCOVERY_TIMEOUT_MS.",
                default: Some("100"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_RUNTIME_RETUNE",
                summary: "Re-evaluate tuning while mining, not only at \
                          startup. Also MUJINA_BZM2_ENABLE_RUNTIME_RETUNE.",
                default: Some("on"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_RUNTIME_RETUNE_PERSISTENCE_POLLS",
                summary: "Consecutive polls a condition must persist before a \
                          runtime retune fires. Also \
                          MUJINA_BZM2_RETUNE_PERSISTENCE_POLLS.",
                default: Some("3"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_RUNTIME_RETUNE_THERMAL_C",
                summary: "Die temperature, in Celsius, that triggers a runtime \
                          retune. Also MUJINA_BZM2_RETUNE_THERMAL_C.",
                default: Some("85.0"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_RUNTIME_RETUNE_VOLTAGE_IMBALANCE_MV",
                summary: "Domain voltage imbalance, in mV, that triggers a \
                          runtime retune. Also \
                          MUJINA_BZM2_RETUNE_VOLTAGE_IMBALANCE_MV.",
                default: Some("150"),
                example: None,
            },
        ],
    },
    EnvGroup {
        title: "BZM2 board: telemetry and limits",
        vars: &[
            EnvVar {
                name: "MUJINA_BZM2_TELEMETRY_INTERVAL_SECS",
                summary: "Board telemetry poll interval.",
                default: Some("5"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_ASIC_TEMP_PATH",
                summary: "Die-temperature sysfs sensor path, scaled by \
                          MUJINA_BZM2_ASIC_TEMP_SCALE.",
                default: Some("unset disables the reading; scale 0.001"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_BOARD_TEMP_PATH",
                summary: "Board-temperature sysfs sensor path, scaled by \
                          MUJINA_BZM2_BOARD_TEMP_SCALE.",
                default: Some("unset disables the reading; scale 0.001"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_FAN_RPM_PATHS",
                summary: "Comma-separated tachometer sysfs paths, one per fan \
                          (singular MUJINA_BZM2_FAN_RPM_PATH also works), \
                          scaled by MUJINA_BZM2_FAN_RPM_SCALES.",
                default: Some("the platform's four hwmon speed paths; scale 30.0"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_FAN_PERCENT_PATHS",
                summary: "Comma-separated PWM duty-cycle sysfs paths, one per \
                          fan (singular MUJINA_BZM2_FAN_PERCENT_PATH also \
                          works), scaled by MUJINA_BZM2_FAN_PERCENT_SCALES.",
                default: Some("the platform's four pwm paths; scale 0.0025"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_INPUT_VOLTAGE_PATH",
                summary: "Chassis input-voltage sysfs sensor, scaled by \
                          MUJINA_BZM2_INPUT_VOLTAGE_SCALE.",
                default: Some("unset disables the reading; scale 0.001"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_INPUT_CURRENT_PATH",
                summary: "Chassis input-current sysfs sensor, scaled by \
                          MUJINA_BZM2_INPUT_CURRENT_SCALE.",
                default: Some("unset disables the reading; scale 0.001"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_INPUT_POWER_PATH",
                summary: "Chassis input-power sysfs sensor, scaled by \
                          MUJINA_BZM2_INPUT_POWER_SCALE.",
                default: Some("unset disables the reading; scale 0.000001"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_MAX_ASIC_TEMP_C",
                summary: "Die-temperature thermal trip, in Celsius.",
                default: Some("100.0, the platform's own enforced limit"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_MAX_BOARD_TEMP_C",
                summary: "Board-temperature thermal trip, in Celsius. Not \
                          defaulted: a chassis-wide figure would have the \
                          wrong denominator applied to one hashboard.",
                default: Some("unset, unarmed"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_MAX_INPUT_POWER_W",
                summary: "Input-power trip, in watts. Not defaulted for the \
                          same reason as MUJINA_BZM2_MAX_BOARD_TEMP_C.",
                default: Some("unset, unarmed"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_MIN_FAN_RPM",
                summary: "Tachometer floor below which a nonzero reading is \
                          judged a stalling fan rather than a stopped one.",
                default: Some("300"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_MAX_RAIL_V",
                summary: "Regulator output ceiling, in volts. Not defaulted: \
                          no measured ceiling exists for this figure.",
                default: Some("unset, unarmed"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_STALL_BUDGET_S",
                summary: "How long a chain may return no result before the \
                          monitor judges it stalled. A placeholder pending a \
                          measured dispatch-to-result interval, not a derived \
                          limit.",
                default: Some("120"),
                example: None,
            },
        ],
    },
    EnvGroup {
        title: "BZM2 board: power-rail bring-up",
        vars: &[
            EnvVar {
                name: "MUJINA_BZM2_RAIL_SET_PATHS",
                summary: "Comma-separated sysfs paths written to set each \
                          rail's voltage at bring-up. Also \
                          MUJINA_BZM2_BRINGUP_RAIL_SET_PATHS. Setting this (or \
                          a reset path) is what enables bring-up.",
                default: Some("unset disables bring-up unless a reset path is set"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_RAIL_TARGET_VOLTS",
                summary: "Comma-separated target volts, index-paired with \
                          MUJINA_BZM2_RAIL_SET_PATHS. Also \
                          MUJINA_BZM2_BRINGUP_RAIL_TARGET_VOLTS.",
                default: Some("empty"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_RAIL_WRITE_SCALES",
                summary: "Comma-separated scale applied before writing each \
                          rail's set path. Also \
                          MUJINA_BZM2_BRINGUP_RAIL_WRITE_SCALES.",
                default: Some("empty (1.0, unscaled)"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_DOMAIN_RAIL_INDICES",
                summary: "Comma-separated mapping of each voltage domain to \
                          its rail index.",
                default: Some("empty"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_RAIL_ENABLE_PATHS",
                summary: "Comma-separated sysfs paths that enable each rail, \
                          paired with MUJINA_BZM2_RAIL_ENABLE_VALUES. Also \
                          MUJINA_BZM2_BRINGUP_RAIL_ENABLE_PATHS and \
                          MUJINA_BZM2_BRINGUP_RAIL_ENABLE_VALUES.",
                default: Some("empty"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_RAIL_VIN_PATHS",
                summary: "Comma-separated per-rail input-voltage sysfs \
                          sensors, scaled by MUJINA_BZM2_RAIL_VIN_SCALES.",
                default: Some("empty; scale 0.001"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_RAIL_VOUT_PATHS",
                summary: "Comma-separated per-rail output-voltage sysfs \
                          sensors, scaled by MUJINA_BZM2_RAIL_VOUT_SCALES.",
                default: Some("empty; scale 0.001"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_RAIL_CURRENT_PATHS",
                summary: "Comma-separated per-rail current sysfs sensors, \
                          scaled by MUJINA_BZM2_RAIL_CURRENT_SCALES.",
                default: Some("empty; scale 0.001"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_RAIL_POWER_PATHS",
                summary: "Comma-separated per-rail power sysfs sensors, \
                          scaled by MUJINA_BZM2_RAIL_POWER_SCALES.",
                default: Some("empty; scale 0.000001"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_RAIL_TEMP_PATHS",
                summary: "Comma-separated per-rail temperature sysfs sensors, \
                          scaled by MUJINA_BZM2_RAIL_TEMP_SCALES.",
                default: Some("empty; scale 0.001"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_RESET_PATH",
                summary: "GPIO sysfs path asserting the board reset line. \
                          Also MUJINA_BZM2_BRINGUP_RESET_PATH.",
                default: Some("unset"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_RESET_ACTIVE_LOW",
                summary: "Whether the reset line is active-low.",
                default: Some("on (active-low)"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_ENABLE_BRINGUP",
                summary: "Force bring-up on even with no rail-set or reset \
                          path configured. Also MUJINA_BZM2_BRINGUP_ENABLE.",
                default: Some("on automatically once a rail-set or reset path is set"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_ASSERT_RESET_BEFORE_POWER",
                summary: "Hold reset asserted before powering the rails.",
                default: Some("on"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_BRINGUP_PRE_POWER_MS",
                summary: "Delay after asserting reset, before powering rails.",
                default: Some("10"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_BRINGUP_POST_POWER_MS",
                summary: "Delay after powering rails, before releasing \
                          reset.",
                default: Some("25"),
                example: None,
            },
            EnvVar {
                name: "MUJINA_BZM2_BRINGUP_RELEASE_RESET_MS",
                summary: "Delay after releasing reset, before the chain is \
                          treated as up.",
                default: Some("25"),
                example: None,
            },
        ],
    },
    EnvGroup {
        title: "Logging",
        vars: &[
            EnvVar {
                name: "MUJINA_LOG",
                summary: "Log filter for Mujina's own modules, overriding \
                          RUST_LOG. Module names are written as the log output \
                          shows them, without a crate prefix. A bare level like \
                          'debug' applies to all of Mujina.",
                default: None,
                example: Some("asic::bm13xx=trace"),
            },
            EnvVar {
                name: "RUST_LOG",
                summary: "Log filter in tracing-subscriber EnvFilter syntax. \
                          A directive that names a crate adds to the built-in \
                          defaults; a bare level like 'debug' replaces them, \
                          as in any Rust program.",
                default: Some("warn,mujina_miner=info"),
                example: Some("nusb=debug"),
            },
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_text_wraps_within_width() {
        for line in help_text().lines() {
            assert!(
                line.len() <= WIDTH,
                "line exceeds {WIDTH} columns ({}): {line:?}",
                line.len()
            );
        }
    }

    #[test]
    fn help_text_lists_every_registered_variable() {
        let text = help_text();
        for group in GROUPS {
            assert!(
                text.contains(group.title),
                "missing group title: {}",
                group.title
            );
            for var in group.vars {
                assert!(text.contains(var.name), "missing variable: {}", var.name);
                if let Some(example) = var.example {
                    assert!(text.contains(example), "missing example for: {}", var.name);
                }
            }
        }
    }
}
