//! Chain enumeration, calibration planner I/O, and operating-point persistence for the BZM2 board.

use crate::asic::bzm2::Bzm2UartController;
use crate::tracing::prelude::*;

use super::config::DEFAULT_ENUMERATION_MAX_ASICS_PER_BUS;
use super::{BoardError, Bzm2Board};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Bzm2BusLayout {
    pub(super) serial_path: String,
    pub(super) asic_start: u16,
    pub(super) asic_count: u16,
}

impl Bzm2BusLayout {
    /// The ASIC ids that appear **on this bus's wire**.
    ///
    /// `asic_start` is a global index accumulated across buses, and exists so
    /// board-level bookkeeping can name every device on the machine uniquely.
    /// It is not what the chain says. Each bus is an independent chain
    /// addressed from `start_id`, so bus 1 reports 0..n whatever its global
    /// offset is.
    ///
    /// Handing the global range to a thread that compares it against wire ids
    /// disabled over-temperature shutdown on every bus after the first, and
    /// discarded their results, while a single-bus machine worked perfectly.
    pub(super) fn wire_asic_ids(&self, start_id: u8) -> Vec<u8> {
        (0..self.asic_count)
            .filter_map(|offset| u8::try_from(u16::from(start_id) + offset).ok())
            .collect()
    }
}

impl Bzm2Board {
    pub(super) async fn resolve_bus_layouts(&self) -> Result<Vec<Bzm2BusLayout>, BoardError> {
        let configured = build_bus_layouts(
            &self.config.serial_paths,
            &self.config.calibration.asics_per_bus,
        );
        if !self.config.enumeration.enabled {
            return Ok(configured);
        }

        let discovered = self.enumerate_bus_layouts().await?;
        if should_fallback_to_configured_bus_layouts(&discovered, &configured) {
            warn!(
                board = %self.config.device_id(),
                "BZM2 startup enumeration found no ASICs on the default id; falling back to configured bus topology"
            );
            return Ok(configured);
        }

        Ok(discovered)
    }

    async fn enumerate_bus_layouts(&self) -> Result<Vec<Bzm2BusLayout>, BoardError> {
        let mut counts = Vec::with_capacity(self.config.serial_paths.len());

        for (index, serial_path) in self.config.serial_paths.iter().enumerate() {
            let max_asics = *self
                .config
                .enumeration
                .max_asics_per_bus
                .get(index)
                .or_else(|| self.config.enumeration.max_asics_per_bus.last())
                .unwrap_or(&DEFAULT_ENUMERATION_MAX_ASICS_PER_BUS);
            let max_asics = max_asics.min(u8::MAX as u16) as u8;

            let stream = crate::transport::serial::open_with_platform_cflag(
                serial_path,
                self.config.baud_rate,
            )
            .map_err(|err| {
                BoardError::InitializationFailed(format!(
                    "Failed to open BZM2 enumeration transport {}: {}",
                    serial_path, err
                ))
            })?;
            let (reader, writer, _control) = super::config::split_chain_port(stream);
            let mut uart = Bzm2UartController::new(reader, writer);
            let assigned = uart
                .enumerate_chain(max_asics, self.config.enumeration.start_id)
                .await
                .map_err(|err| {
                    BoardError::InitializationFailed(format!(
                        "BZM2 startup enumeration failed on {}: {}",
                        serial_path, err
                    ))
                })?;
            counts.push(assigned.len() as u16);
            info!(
                board = %self.config.device_id(),
                serial_path,
                asic_count = assigned.len(),
                "BZM2 startup enumeration completed"
            );
        }

        Ok(build_discovered_bus_layouts(
            &self.config.serial_paths,
            &counts,
        ))
    }
}

fn build_bus_layouts(serial_paths: &[String], asics_per_bus: &[u16]) -> Vec<Bzm2BusLayout> {
    build_bus_layouts_with_minimum(serial_paths, asics_per_bus, 1)
}

fn build_discovered_bus_layouts(
    serial_paths: &[String],
    asics_per_bus: &[u16],
) -> Vec<Bzm2BusLayout> {
    build_bus_layouts_with_minimum(serial_paths, asics_per_bus, 0)
}

fn build_bus_layouts_with_minimum(
    serial_paths: &[String],
    asics_per_bus: &[u16],
    minimum_asic_count: u16,
) -> Vec<Bzm2BusLayout> {
    let mut next_asic = 0u16;
    serial_paths
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let asic_count = *asics_per_bus
                .get(index)
                .or_else(|| asics_per_bus.last())
                .unwrap_or(&1)
                .max(&minimum_asic_count);
            let layout = Bzm2BusLayout {
                serial_path: path.clone(),
                asic_start: next_asic,
                asic_count,
            };
            next_asic = next_asic.saturating_add(asic_count);
            layout
        })
        .collect()
}

fn should_fallback_to_configured_bus_layouts(
    discovered: &[Bzm2BusLayout],
    configured: &[Bzm2BusLayout],
) -> bool {
    let discovered_total = discovered
        .iter()
        .map(|layout| layout.asic_count as usize)
        .sum::<usize>();
    let configured_total = configured
        .iter()
        .map(|layout| layout.asic_count as usize)
        .sum::<usize>();
    discovered_total == 0 && configured_total > 0
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A non-zero enumeration start shifts the wire ids and nothing else.
    #[test]
    fn wire_ids_follow_the_enumeration_start_id() {
        let paths = vec!["/dev/ttyA".to_string(), "/dev/ttyB".to_string()];
        let layouts = build_bus_layouts(&paths, &[4, 4]);
        assert_eq!(layouts[1].wire_asic_ids(0), vec![0, 1, 2, 3]);
        assert_eq!(layouts[1].wire_asic_ids(16), vec![16, 17, 18, 19]);
        // Still nothing to do with where the bus sits globally.
        assert_eq!(layouts[1].asic_start, 4);
    }

    use super::super::config::{
        Bzm2CalibrationConfig, Bzm2EnumerationConfig, Bzm2RuntimeConfig, DEFAULT_BAUD_RATE,
        DEFAULT_NOMINAL_HASHRATE_THS,
    };
    use super::super::test_support::spawn_chain_emulator;

    use crate::api_client::types::BoardTelemetry;
    use crate::asic::bzm2::protocol::{OPCODE_UART_NOOP, encode_noop};

    use nix::pty::openpty;
    use std::fs;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::time::Duration;
    use tokio::sync::watch;

    #[test]
    fn build_bus_layouts_assigns_global_ranges() {
        let layouts = build_bus_layouts(&["/dev/ttyUSB0".into(), "/dev/ttyUSB1".into()], &[4, 6]);
        assert_eq!(layouts[0].asic_start, 0);
        assert_eq!(layouts[0].asic_count, 4);
        assert_eq!(layouts[1].asic_start, 4);
        assert_eq!(layouts[1].asic_count, 6);
    }

    #[tokio::test]
    async fn resolve_bus_layouts_uses_startup_enumeration_counts() {
        let pty = openpty(None, None).unwrap();
        let master = pty.master;
        let slave = pty.slave;
        let serial_path = fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd()))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let emulator = spawn_chain_emulator(master, 2, 0);

        let config = Bzm2RuntimeConfig {
            serial_paths: vec![serial_path],
            baud_rate: DEFAULT_BAUD_RATE,
            timestamp_count: crate::asic::bzm2::protocol::DEFAULT_TIMESTAMP_COUNT,
            nonce_gap: crate::asic::bzm2::protocol::DEFAULT_NONCE_GAP,
            result_min_difficulty: None,
            dispatch_interval: Duration::from_millis(50),
            nominal_hashrate_ths: DEFAULT_NOMINAL_HASHRATE_THS,
            dts_vs_generation: crate::asic::bzm2::protocol::DtsVsGeneration::Gen2,
            enumeration: Bzm2EnumerationConfig {
                enabled: true,
                start_id: 0,
                max_asics_per_bus: vec![4],
            },
            calibration: Bzm2CalibrationConfig::default(),
        };
        let (telemetry_tx, _telemetry_rx) = watch::channel(BoardTelemetry {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            ..Default::default()
        });
        let board = Bzm2Board::new(config, telemetry_tx);

        let layouts = board.resolve_bus_layouts().await.unwrap();
        assert_eq!(layouts.len(), 1);
        assert_eq!(layouts[0].asic_count, 2);

        emulator.join().unwrap();
    }

    #[tokio::test]
    async fn resolve_bus_layouts_falls_back_to_configured_counts_when_default_id_is_silent() {
        let pty = openpty(None, None).unwrap();
        let master = pty.master;
        let slave = pty.slave;
        let serial_path = fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd()))
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let emulator = std::thread::spawn(move || {
            let mut file = fs::File::from(master);
            let mut probe = vec![0u8; encode_noop(crate::asic::bzm2::DEFAULT_ASIC_ID).len()];
            file.read_exact(&mut probe).unwrap();
            assert_eq!(probe, encode_noop(crate::asic::bzm2::DEFAULT_ASIC_ID));
            file.write_all(&[
                crate::asic::bzm2::DEFAULT_ASIC_ID,
                OPCODE_UART_NOOP,
                b'N',
                b'O',
                b'P',
            ])
            .unwrap();
        });

        let config = Bzm2RuntimeConfig {
            serial_paths: vec![serial_path],
            baud_rate: DEFAULT_BAUD_RATE,
            timestamp_count: crate::asic::bzm2::protocol::DEFAULT_TIMESTAMP_COUNT,
            nonce_gap: crate::asic::bzm2::protocol::DEFAULT_NONCE_GAP,
            result_min_difficulty: None,
            dispatch_interval: Duration::from_millis(50),
            nominal_hashrate_ths: DEFAULT_NOMINAL_HASHRATE_THS,
            dts_vs_generation: crate::asic::bzm2::protocol::DtsVsGeneration::Gen2,
            enumeration: Bzm2EnumerationConfig {
                enabled: true,
                start_id: 0,
                max_asics_per_bus: vec![4],
            },
            calibration: Bzm2CalibrationConfig {
                asics_per_bus: vec![3],
                ..Default::default()
            },
        };
        let (telemetry_tx, _telemetry_rx) = watch::channel(BoardTelemetry {
            name: "bzm2-test".into(),
            model: "BZM2".into(),
            serial: Some("bzm2-test".into()),
            ..Default::default()
        });
        let board = Bzm2Board::new(config, telemetry_tx);

        let layouts = board.resolve_bus_layouts().await.unwrap();
        assert_eq!(layouts.len(), 1);
        assert_eq!(layouts[0].asic_count, 3);

        emulator.join().unwrap();
    }
}
