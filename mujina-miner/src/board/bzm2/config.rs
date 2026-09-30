//! Environment-driven configuration for the BZM2 board driver.

use std::env;
use std::path::Path;
use std::time::Duration;

use crate::types::Difficulty;

pub(super) const DEFAULT_BAUD_RATE: u32 = 5_000_000;
const DEFAULT_DISPATCH_INTERVAL_MS: u64 = 500;
pub(super) const DEFAULT_NOMINAL_HASHRATE_THS: f64 = 40.0;

pub(super) const DEFAULT_ENUMERATION_MAX_ASICS_PER_BUS: u16 = 100;

#[derive(Debug, Clone)]
pub struct Bzm2RuntimeConfig {
    pub serial_paths: Vec<String>,
    pub baud_rate: u32,
    pub timestamp_count: u8,
    pub nonce_gap: u32,
    /// Difficulty floor for forwarding reconstructed results; see
    /// `Bzm2ThreadConfig::result_min_difficulty`.
    pub result_min_difficulty: Option<Difficulty>,
    pub dispatch_interval: Duration,
    pub nominal_hashrate_ths: f64,
    pub dts_vs_generation: crate::asic::bzm2::protocol::DtsVsGeneration,
    pub calibration: Bzm2CalibrationConfig,
    pub enumeration: Bzm2EnumerationConfig,
}

impl Bzm2RuntimeConfig {
    pub fn from_env() -> Option<Self> {
        let raw_paths = env::var("MUJINA_BZM2_SERIAL")
            .ok()
            .or_else(|| env::var("MUJINA_BZM2_SERIAL_PATHS").ok())?;

        let serial_paths: Vec<String> = raw_paths
            .split(',')
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        if serial_paths.is_empty() {
            return None;
        }

        let baud_rate = env::var("MUJINA_BZM2_BAUD")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_BAUD_RATE);
        let timestamp_count = env::var("MUJINA_BZM2_TIMESTAMP_COUNT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(crate::asic::bzm2::protocol::DEFAULT_TIMESTAMP_COUNT);
        let nonce_gap = env::var("MUJINA_BZM2_NONCE_GAP")
            .ok()
            .and_then(|value| parse_u32(&value))
            .unwrap_or(crate::asic::bzm2::protocol::DEFAULT_NONCE_GAP);
        let result_min_difficulty = env::var("MUJINA_BZM2_RESULT_MIN_DIFF")
            .ok()
            .and_then(|value| value.trim().parse::<f64>().ok())
            .filter(|value| *value > 0.0 && value.is_finite())
            .map(Difficulty::from_f64);
        let dispatch_interval = Duration::from_millis(
            env::var("MUJINA_BZM2_DISPATCH_MS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(DEFAULT_DISPATCH_INTERVAL_MS),
        );
        let nominal_hashrate_ths = env::var("MUJINA_BZM2_HASHRATE_THS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_NOMINAL_HASHRATE_THS);
        let dts_vs_generation = env::var("MUJINA_BZM2_DTS_VS_GEN")
            .ok()
            .as_deref()
            .and_then(crate::asic::bzm2::protocol::DtsVsGeneration::from_env_value)
            .unwrap_or(crate::asic::bzm2::protocol::DtsVsGeneration::Gen2);
        let calibration = Bzm2CalibrationConfig::from_env(serial_paths.len());

        Some(Self {
            serial_paths: serial_paths.clone(),
            baud_rate,
            timestamp_count,
            nonce_gap,
            result_min_difficulty,
            dispatch_interval,
            nominal_hashrate_ths,
            dts_vs_generation,
            enumeration: Bzm2EnumerationConfig::from_env(serial_paths.len(), &calibration),
            calibration,
        })
    }

    pub fn device_id(&self) -> String {
        let suffix = self
            .serial_paths
            .iter()
            .map(|path| {
                Path::new(path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(path)
                    .chars()
                    .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("-");
        format!("bzm2-{}", suffix)
    }
}

#[derive(Debug, Clone)]
pub struct Bzm2EnumerationConfig {
    pub enabled: bool,
    pub start_id: u8,
    pub max_asics_per_bus: Vec<u16>,
}

impl Default for Bzm2EnumerationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            start_id: 0,
            max_asics_per_bus: vec![DEFAULT_ENUMERATION_MAX_ASICS_PER_BUS],
        }
    }
}

impl Bzm2EnumerationConfig {
    fn from_env(serial_count: usize, calibration: &Bzm2CalibrationConfig) -> Self {
        let mut max_asics_per_bus = parse_csv_numbers::<u16>("MUJINA_BZM2_ENUM_MAX_ASICS_PER_BUS")
            .unwrap_or_else(|| {
                if calibration.asics_per_bus.iter().any(|count| *count > 1) {
                    calibration.asics_per_bus.clone()
                } else if serial_count == 0 {
                    Vec::new()
                } else {
                    vec![DEFAULT_ENUMERATION_MAX_ASICS_PER_BUS; serial_count]
                }
            });
        if max_asics_per_bus.is_empty() && serial_count > 0 {
            max_asics_per_bus = vec![DEFAULT_ENUMERATION_MAX_ASICS_PER_BUS; serial_count];
        }

        Self {
            enabled: env_flag_any(&["MUJINA_BZM2_ENUMERATE_CHAIN", "MUJINA_BZM2_AUTO_ENUMERATE"]),
            start_id: env::var("MUJINA_BZM2_ENUM_START_ID")
                .ok()
                .and_then(|value| value.parse::<u8>().ok())
                .unwrap_or(0),
            max_asics_per_bus,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Bzm2CalibrationConfig {
    pub asics_per_bus: Vec<u16>,
}

impl Default for Bzm2CalibrationConfig {
    fn default() -> Self {
        Self {
            asics_per_bus: vec![1],
        }
    }
}

impl Bzm2CalibrationConfig {
    fn from_env(serial_count: usize) -> Self {
        Self {
            asics_per_bus: resolve_asics_per_bus(
                parse_csv_numbers::<u16>("MUJINA_BZM2_ASICS_PER_BUS"),
                serial_count,
            ),
        }
    }
}

/// Resolve the per-bus ASIC count from an already-parsed
/// `MUJINA_BZM2_ASICS_PER_BUS` value: `None` if the variable is unset,
/// `Some(vec![])` if it was set to an empty or blank CSV.
///
/// There is no safe default here. A silently-defaulted `vec![1; serial_count]`
/// is indistinguishable, downstream, from a completely dark chain: three
/// configured serial paths with no explicit count used to read back as "3
/// ASICs total" instead of the ~300 a real chain holds, which looks exactly
/// like catastrophic enumeration failure rather than the config mistake it
/// is. Once a serial path is configured, this is exactly the class of
/// cheap-if-caught-now, expensive-if-caught-later failure imperative 1 (fail
/// fast, fail cheap) exists for, so it panics at config load, naming the
/// variable, instead of letting the board mine at 1% capacity for hours.
fn resolve_asics_per_bus(explicit: Option<Vec<u16>>, serial_count: usize) -> Vec<u16> {
    let counts = explicit.unwrap_or_default();
    if !counts.is_empty() {
        return counts;
    }
    if serial_count == 0 {
        return Vec::new();
    }
    panic!(
        "MUJINA_BZM2_ASICS_PER_BUS is required once BZM2 serial paths are configured \
         ({serial_count} path(s) here) -- set a CSV with one ASIC count per bus \
         (e.g. \"100,100\"). There is no safe default: silently defaulting to 1 \
         ASIC per bus has previously been mistaken for a fully dark chain."
    );
}

pub(super) fn env_var_any(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| env::var(key).ok())
}

pub(super) fn env_flag_any(keys: &[&str]) -> bool {
    env_var_any(keys).as_deref().is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

pub(super) fn parse_u32(value: &str) -> Option<u32> {
    let trimmed = value.trim();
    if let Some(hex) = trimmed.strip_prefix("0x") {
        u32::from_str_radix(hex, 16).ok()
    } else {
        trimmed.parse().ok()
    }
}

pub(super) fn parse_csv_numbers<T>(key: &str) -> Option<Vec<T>>
where
    T: std::str::FromStr,
{
    let value = env::var(key).ok()?;
    let parsed = value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.parse().ok())
        .collect::<Option<Vec<_>>>()?;
    Some(parsed)
}

/// Split a chain port, installing the read-only veto when dry run is asked for.
///
/// Every production path that opens a chain port goes through here, so the
/// guarantee is "this build cannot write to a chain in dry run", not "the
/// places we remembered do not write".
pub(super) fn split_chain_port(
    stream: crate::transport::serial::SerialStream,
) -> (
    crate::transport::serial::SerialReader,
    crate::transport::serial::SerialWriter,
    crate::transport::serial::SerialControl,
) {
    if dry_run_enabled() {
        tracing::warn!(
            "BZM2 DRY RUN: this port will refuse every state-changing frame. \
             Reads, telemetry and diagnostics work; writes, job dispatch and \
             the id assignment enumeration performs are refused and logged."
        );
        return stream.split_guarded(std::sync::Arc::new(
            crate::asic::bzm2::protocol::ReadOnlyPolicy,
        ));
    }
    stream.split()
}

/// Dry run is opt-in and read once, so a run cannot change mode underneath
/// itself.
pub fn dry_run_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| env_flag_any(&["MUJINA_BZM2_DRY_RUN", "MUJINA_BZM2_READ_ONLY"]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// EVERY CHAIN PORT GOES THROUGH THE DRY-RUN VETO. Ratchet, not a review.
    ///
    /// The pre-calibration sensor arm opened the chain with a raw
    /// `SerialStream::split()`, so a run declared write-free sent seven
    /// broadcast register writes to 100 energised ASICs with nothing refused
    /// and nothing logged. The veto lives in `split_chain_port` and is only as
    /// good as every caller remembering to use it. This makes forgetting a
    /// build failure: no file under board/bzm2 except this one may call a bare
    /// `.split()` on a stream.
    #[test]
    fn every_chain_port_goes_through_the_dry_run_veto() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/board/bzm2");
        let mut offenders = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs")
                || path.file_name().and_then(|n| n.to_str()) == Some("config.rs")
            {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            for (n, line) in text.lines().enumerate() {
                let code = line.split("//").next().unwrap_or("");
                if code.contains("stream.split()") {
                    offenders.push(format!("{}:{}", path.display(), n + 1));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "chain port opened without the dry-run veto -- use split_chain_port: {offenders:?}"
        );
    }

    // resolve_asics_per_bus is exercised directly on its already-parsed
    // input rather than through real environment variables: this process
    // runs its tests in parallel threads, and mutating MUJINA_BZM2_ASICS_PER_BUS
    // from a test would race every other test that reads BZM2 env config.

    #[test]
    fn resolve_asics_per_bus_passes_through_explicit_counts() {
        // Not a round trip through the same default this function would
        // otherwise produce: the explicit counts here (100, 100) are
        // deliberately chosen values the function must return verbatim, not
        // a value it invented.
        assert_eq!(
            resolve_asics_per_bus(Some(vec![100, 100]), 2),
            vec![100, 100]
        );
    }

    #[test]
    fn resolve_asics_per_bus_allows_zero_serial_paths_without_config() {
        // No board configured at all is not a misconfiguration; nothing to
        // complain about.
        assert_eq!(resolve_asics_per_bus(None, 0), Vec::<u16>::new());
        assert_eq!(
            resolve_asics_per_bus(Some(Vec::new()), 0),
            Vec::<u16>::new()
        );
    }

    #[test]
    #[should_panic(expected = "MUJINA_BZM2_ASICS_PER_BUS")]
    fn resolve_asics_per_bus_panics_when_unset_with_configured_serial_paths() {
        resolve_asics_per_bus(None, 3);
    }

    #[test]
    #[should_panic(expected = "MUJINA_BZM2_ASICS_PER_BUS")]
    fn resolve_asics_per_bus_panics_when_set_but_empty() {
        // MUJINA_BZM2_ASICS_PER_BUS="" (present but blank) parses to
        // Some(vec![]), which must fail exactly like an unset variable --
        // not fall back to a silent 1, which was the second of the two
        // silent-default sites this closes.
        resolve_asics_per_bus(Some(Vec::new()), 3);
    }
}
