//! Power-rail bring-up, reset sequencing, and voltage/frequency application for the BZM2 board.

use std::collections::BTreeMap;
use std::env;
use std::time::Duration;

use crate::api_client::types::{PowerMeasurement, TemperatureSensor};
use crate::asic::bzm2::{Bzm2ClockController, Bzm2Pll};
use crate::board::power::{
    FileGpioPin, FilePowerRail, GpioResetLine, PowerRail, VoltageStackBringupPlan, VoltageStackStep,
};
use crate::tracing::prelude::*;
use crate::types::Temperature;

use super::calibration::Bzm2BusLayout;
use super::config::{
    DEFAULT_BOARD_TEMP_SCALE, DEFAULT_BRINGUP_POST_POWER_MS, DEFAULT_BRINGUP_PRE_POWER_MS,
    DEFAULT_BRINGUP_RELEASE_RESET_MS, DEFAULT_CURRENT_SCALE, DEFAULT_POWER_SCALE,
    DEFAULT_VOLTAGE_SCALE, env_csv_strings_any, env_flag_any, env_flag_default_any, env_var_any,
    parse_csv_numbers, parse_csv_numbers_any,
};
use super::telemetry::{Bzm2TelemetrySnapshot, SensorSpec, sensor_specs_from_env};
use super::{BoardError, Bzm2Board};

#[derive(Debug, Clone)]
pub struct Bzm2BringupConfig {
    pub enabled: bool,
    pub rail_set_paths: Vec<String>,
    pub rail_write_scales: Vec<f32>,
    pub domain_rail_indices: Vec<usize>,
    pub rail_enable_paths: Vec<String>,
    pub rail_enable_values: Vec<String>,
    pub rail_vin: Vec<SensorSpec>,
    pub rail_vout: Vec<SensorSpec>,
    pub rail_current: Vec<SensorSpec>,
    pub rail_power: Vec<SensorSpec>,
    pub rail_temperature: Vec<SensorSpec>,
    pub reset_path: Option<String>,
    pub reset_active_low: bool,
    pub plan: VoltageStackBringupPlan,
}

impl Default for Bzm2BringupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            rail_set_paths: Vec::new(),
            rail_write_scales: Vec::new(),
            domain_rail_indices: Vec::new(),
            rail_enable_paths: Vec::new(),
            rail_enable_values: Vec::new(),
            rail_vin: Vec::new(),
            rail_vout: Vec::new(),
            rail_current: Vec::new(),
            rail_power: Vec::new(),
            rail_temperature: Vec::new(),
            reset_path: None,
            reset_active_low: true,
            plan: VoltageStackBringupPlan {
                pre_power_delay: Duration::from_millis(DEFAULT_BRINGUP_PRE_POWER_MS),
                post_power_delay: Duration::from_millis(DEFAULT_BRINGUP_POST_POWER_MS),
                release_reset_delay: Duration::from_millis(DEFAULT_BRINGUP_RELEASE_RESET_MS),
                ..Default::default()
            },
        }
    }
}

impl Bzm2BringupConfig {
    pub(super) fn from_env() -> Self {
        let rail_set_paths = env_csv_strings_any(&[
            "MUJINA_BZM2_RAIL_SET_PATHS",
            "MUJINA_BZM2_BRINGUP_RAIL_SET_PATHS",
        ]);
        let rail_target_volts = parse_csv_numbers::<f32>("MUJINA_BZM2_RAIL_TARGET_VOLTS")
            .or_else(|| parse_csv_numbers::<f32>("MUJINA_BZM2_BRINGUP_RAIL_TARGET_VOLTS"))
            .unwrap_or_default();
        let rail_write_scales = parse_csv_numbers::<f32>("MUJINA_BZM2_RAIL_WRITE_SCALES")
            .or_else(|| parse_csv_numbers::<f32>("MUJINA_BZM2_BRINGUP_RAIL_WRITE_SCALES"))
            .unwrap_or_default();
        let domain_rail_indices =
            parse_csv_numbers_any::<usize>(&["MUJINA_BZM2_DOMAIN_RAIL_INDICES"])
                .unwrap_or_default();
        let rail_enable_paths = env_csv_strings_any(&[
            "MUJINA_BZM2_RAIL_ENABLE_PATHS",
            "MUJINA_BZM2_BRINGUP_RAIL_ENABLE_PATHS",
        ]);
        let rail_enable_values = env_csv_strings_any(&[
            "MUJINA_BZM2_RAIL_ENABLE_VALUES",
            "MUJINA_BZM2_BRINGUP_RAIL_ENABLE_VALUES",
        ]);
        let rail_vin = sensor_specs_from_env(
            &["MUJINA_BZM2_RAIL_VIN_PATHS"],
            &["MUJINA_BZM2_RAIL_VIN_SCALES"],
            DEFAULT_VOLTAGE_SCALE,
        );
        let rail_vout = sensor_specs_from_env(
            &["MUJINA_BZM2_RAIL_VOUT_PATHS"],
            &["MUJINA_BZM2_RAIL_VOUT_SCALES"],
            DEFAULT_VOLTAGE_SCALE,
        );
        let rail_current = sensor_specs_from_env(
            &["MUJINA_BZM2_RAIL_CURRENT_PATHS"],
            &["MUJINA_BZM2_RAIL_CURRENT_SCALES"],
            DEFAULT_CURRENT_SCALE,
        );
        let rail_power = sensor_specs_from_env(
            &["MUJINA_BZM2_RAIL_POWER_PATHS"],
            &["MUJINA_BZM2_RAIL_POWER_SCALES"],
            DEFAULT_POWER_SCALE,
        );
        let rail_temperature = sensor_specs_from_env(
            &["MUJINA_BZM2_RAIL_TEMP_PATHS"],
            &["MUJINA_BZM2_RAIL_TEMP_SCALES"],
            DEFAULT_BOARD_TEMP_SCALE,
        );
        let reset_path = env_var_any(&["MUJINA_BZM2_RESET_PATH", "MUJINA_BZM2_BRINGUP_RESET_PATH"]);
        let enabled = env_flag_any(&["MUJINA_BZM2_ENABLE_BRINGUP", "MUJINA_BZM2_BRINGUP_ENABLE"])
            || !rail_set_paths.is_empty()
            || reset_path.is_some();

        let mut plan = VoltageStackBringupPlan {
            assert_reset_before_power: env_flag_default_any(
                &["MUJINA_BZM2_ASSERT_RESET_BEFORE_POWER"],
                true,
            ),
            pre_power_delay: Duration::from_millis(
                env::var("MUJINA_BZM2_BRINGUP_PRE_POWER_MS")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(DEFAULT_BRINGUP_PRE_POWER_MS),
            ),
            post_power_delay: Duration::from_millis(
                env::var("MUJINA_BZM2_BRINGUP_POST_POWER_MS")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(DEFAULT_BRINGUP_POST_POWER_MS),
            ),
            release_reset_delay: Duration::from_millis(
                env::var("MUJINA_BZM2_BRINGUP_RELEASE_RESET_MS")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(DEFAULT_BRINGUP_RELEASE_RESET_MS),
            ),
            ..Default::default()
        };
        plan.steps = rail_set_paths
            .iter()
            .enumerate()
            .filter_map(|(index, _)| {
                rail_target_volts
                    .get(index)
                    .or_else(|| rail_target_volts.last())
                    .copied()
                    .map(|voltage| VoltageStackStep {
                        rail_index: index,
                        voltage,
                        settle_for: Duration::ZERO,
                    })
            })
            .collect();

        Self {
            enabled,
            rail_set_paths,
            rail_write_scales,
            domain_rail_indices,
            rail_enable_paths,
            rail_enable_values,
            rail_vin,
            rail_vout,
            rail_current,
            rail_power,
            rail_temperature,
            reset_path,
            reset_active_low: env_flag_default_any(&["MUJINA_BZM2_RESET_ACTIVE_LOW"], true),
            plan,
        }
    }

    pub(super) fn rail_index_for_domain(&self, domain_id: u16) -> Option<usize> {
        self.domain_rail_indices
            .get(domain_id as usize)
            .copied()
            .or_else(|| {
                let fallback = domain_id as usize;
                (fallback < self.rail_set_paths.len()).then_some(fallback)
            })
    }

    pub(super) fn has_telemetry(&self) -> bool {
        !self.rail_vin.is_empty()
            || !self.rail_vout.is_empty()
            || !self.rail_current.is_empty()
            || !self.rail_power.is_empty()
            || !self.rail_temperature.is_empty()
    }

    pub(super) fn snapshot_telemetry(&self) -> Bzm2TelemetrySnapshot {
        let rail_count = [
            self.rail_set_paths.len(),
            self.rail_vin.len(),
            self.rail_vout.len(),
            self.rail_current.len(),
            self.rail_power.len(),
            self.rail_temperature.len(),
        ]
        .into_iter()
        .max()
        .unwrap_or(0);

        let mut temperatures = Vec::new();
        let mut powers = Vec::new();
        for index in 0..rail_count {
            let vin = self.rail_vin.get(index).and_then(SensorSpec::read);
            let vout = self.rail_vout.get(index).and_then(SensorSpec::read);
            let current = self.rail_current.get(index).and_then(SensorSpec::read);
            let power = self
                .rail_power
                .get(index)
                .and_then(SensorSpec::read)
                .or_else(|| vout.zip(current).map(|(v, c)| v * c));
            let temperature_c = self.rail_temperature.get(index).and_then(SensorSpec::read);

            if let Some(temperature_c) = temperature_c {
                temperatures.push(TemperatureSensor {
                    name: format!("rail{}-regulator", index),
                    temperature: Some(Temperature::from_celsius(temperature_c)),
                    observed_at: Some(std::time::Instant::now()),
                });
            }
            if vin.is_some() {
                powers.push(PowerMeasurement {
                    name: format!("rail{}-input", index),
                    voltage_v: vin,
                    current_a: None,
                    power_w: None,
                });
            }
            if vout.is_some() || current.is_some() || power.is_some() {
                powers.push(PowerMeasurement {
                    name: format!("rail{}-output", index),
                    voltage_v: vout,
                    current_a: current,
                    power_w: power,
                });
            }
        }

        Bzm2TelemetrySnapshot {
            fans: Vec::new(),
            temperatures,
            powers,
            trip_reason: None,
            // Rail telemetry carries no configured limit, so it is never blind:
            // there is nothing here for a missing reading to disarm.
            blind: Vec::new(),
        }
    }
    fn build_rails(&self) -> Vec<FilePowerRail> {
        self.rail_set_paths
            .iter()
            .enumerate()
            .map(|(index, path)| {
                let write_scale = *self
                    .rail_write_scales
                    .get(index)
                    .or_else(|| self.rail_write_scales.last())
                    .unwrap_or(&1.0);
                let mut rail = FilePowerRail::new(path.clone(), write_scale);
                if let Some(enable_path) = self
                    .rail_enable_paths
                    .get(index)
                    .or_else(|| self.rail_enable_paths.last())
                {
                    let enable_value = self
                        .rail_enable_values
                        .get(index)
                        .or_else(|| self.rail_enable_values.last())
                        .cloned()
                        .unwrap_or_else(|| "1".into());
                    rail = rail.with_enable(enable_path.clone(), enable_value);
                }
                rail
            })
            .collect()
    }

    fn build_reset_line(&self) -> Option<GpioResetLine<FileGpioPin>> {
        self.reset_path.as_ref().map(|path| {
            GpioResetLine::new(
                FileGpioPin::new(path.clone(), "1", "0"),
                self.reset_active_low,
            )
        })
    }
}

impl Bzm2Board {
    pub(super) async fn apply_bringup_sequence(&mut self) -> Result<(), BoardError> {
        if self.bringup_applied || !self.config.bringup.enabled {
            return Ok(());
        }

        let mut rails = self.config.bringup.build_rails();
        let mut reset_line = self.config.bringup.build_reset_line();
        self.config
            .bringup
            .plan
            .apply(&mut rails, reset_line.as_mut())
            .await
            .map_err(|err| {
                BoardError::InitializationFailed(format!("BZM2 bring-up sequence failed: {err}"))
            })?;
        self.bringup_applied = true;
        Ok(())
    }

    pub(super) async fn apply_shutdown_sequence(&mut self) -> Result<(), BoardError> {
        // `bringup_applied` IS NOT A PRECONDITION FOR SHUTTING DOWN.
        //
        // This returned Ok having touched nothing whenever Mujina had not
        // itself performed the bring-up -- which is precisely the case where
        // shutting down matters most: a board somebody else energised, a
        // restart, a crash recovery, a handover. The caller was told the
        // shutdown succeeded. It is also the flag most likely to be wrong,
        // because it records what THIS process did, not what state the hardware
        // is in.
        //
        // Configuration is still a precondition: with bring-up disabled there
        // are no rails or reset line described to this driver, so there is
        // nothing it could act on and saying so is honest. Not knowing how to
        // reach the hardware and believing it is already off are different
        // answers, and only the first one is true here.
        if !self.config.bringup.enabled {
            return Ok(());
        }
        if !self.bringup_applied {
            tracing::info!(
                "BZM2 shutdown on a board this process did not bring up: \
                 de-energising anyway, because the flag records what we did and \
                 not what the hardware is doing"
            );
        }

        let mut rails = self.config.bringup.build_rails();
        let mut reset_line = self.config.bringup.build_reset_line();
        self.config
            .bringup
            .plan
            .shutdown(&mut rails, reset_line.as_mut())
            .await
            .map_err(|err| {
                BoardError::HardwareControl(format!("BZM2 shutdown sequence failed: {err}"))
            })?;
        self.bringup_applied = false;
        // The setpoints are written. Whether the rails fell is a different
        // question, and it is the one that matters.
        self.confirm_rails_dark().await;
        Ok(())
    }

    pub(super) async fn apply_domain_voltage_map(
        &self,
        per_domain_voltage_mv: &BTreeMap<u16, u32>,
    ) -> Result<(), BoardError> {
        if per_domain_voltage_mv.is_empty() {
            return Ok(());
        }
        if self.config.bringup.rail_set_paths.is_empty() {
            warn!(
                board = %self.config.device_id(),
                ?per_domain_voltage_mv,
                "planner produced per-domain voltages, but no BZM2 rail control path is configured"
            );
            return Ok(());
        }

        let mut rail_targets_mv = BTreeMap::<usize, u32>::new();
        for (&domain_id, &voltage_mv) in per_domain_voltage_mv {
            let rail_index = self
                .config
                .bringup
                .rail_index_for_domain(domain_id)
                .ok_or_else(|| {
                    BoardError::HardwareControl(format!(
                        "BZM2 domain {domain_id} has no mapped rail index"
                    ))
                })?;
            if rail_index >= self.config.bringup.rail_set_paths.len() {
                return Err(BoardError::HardwareControl(format!(
                    "BZM2 domain {domain_id} mapped to rail {rail_index}, but only {} rail set paths are configured",
                    self.config.bringup.rail_set_paths.len()
                )));
            }
            match rail_targets_mv.entry(rail_index) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(voltage_mv);
                }
                std::collections::btree_map::Entry::Occupied(entry)
                    if *entry.get() != voltage_mv =>
                {
                    return Err(BoardError::HardwareControl(format!(
                        "BZM2 rail {rail_index} received conflicting domain voltages: {}mV vs {}mV",
                        entry.get(),
                        voltage_mv
                    )));
                }
                std::collections::btree_map::Entry::Occupied(_) => {}
            }
        }

        let mut rails = self.config.bringup.build_rails();
        for (&rail_index, &voltage_mv) in &rail_targets_mv {
            let rail = rails.get_mut(rail_index).ok_or_else(|| {
                BoardError::HardwareControl(format!(
                    "BZM2 rail {rail_index} is missing from configured rail controls"
                ))
            })?;
            rail.set_voltage(voltage_mv as f32 / 1000.0)
                .await
                .map_err(|err| {
                    BoardError::HardwareControl(format!(
                        "Failed to apply BZM2 domain voltage {voltage_mv}mV on rail {rail_index}: {err}"
                    ))
                })?;
        }

        // CONFIRM IT THROUGH AN INSTRUMENT THAT CAN ACTUALLY SEE THE RAIL.
        //
        // Every write above went to a sysfs setpoint that is WRITE-ONLY:
        // FilePowerRail::telemetry() refuses by design rather than returning
        // zeros, because zeros are a reading that says the rail is dark. So
        // the commanded voltage had no confirmation of any kind -- a regulator
        // that ignored the write, a path that did not exist, and a rail that
        // came up exactly as asked were indistinguishable.
        //
        // The board MCU reads the rail with its own ADC, over I2C. That shares
        // nothing with a sysfs write to a regulator driver, which is what makes
        // it a witness rather than an echo.
        let commanded_mv = rail_targets_mv.values().copied().max().unwrap_or(0);
        if commanded_mv > 0 {
            self.confirm_rail_reached(commanded_mv).await;
        }
        Ok(())
    }

    /// Arm and confirm the on-die sensors on every chain, before the stack.
    ///
    /// Reports one of three outcomes per chain, and the distinctions matter:
    /// armed and confirmed; the chain could not be reached at all, which on
    /// this carrier most likely means the parts are not talking with the stack
    /// down; or reached but the part did not report its sensors in force.
    /// Only the first is protection.
    /// WHY THIS IS NOT CALLED "BEFORE THE RAMP", THOUGH THAT WAS CONSIDERED.
    ///
    /// It cannot be. `VoltageStackBringupPlan::apply` ASSERTS reset first
    /// (`board/power.rs`: `reset_line.disable()`, which is `drive(true)`),
    /// then initialises and steps every rail, and releases reset only at the
    /// end. Any sensor configuration written before that call is cleared by
    /// the reset that follows it, so the position the original name describes
    /// is one in which this function provably achieves nothing.
    ///
    /// The window that exists is AFTER reset release and BEFORE the
    /// calibration sweep -- which is where the exposure actually is. The ramp
    /// itself is short; calibration is about ten minutes of raised voltage and
    /// frequency across every part on every chain, and that is the interval
    /// that must not be run with the silicon's last-resort protection off.
    pub(super) async fn arm_on_die_protection_before_calibration(&self) {
        use crate::asic::bzm2::uart::{
            Bzm2DtsVsConfig, configure_dts_vs_stream, confirm_on_die_protection,
        };

        // Short. If the parts are not talking with the stack down, this is
        // paid on every bring-up and must not become the reason one is slow.
        const REACH_TIMEOUT: Duration = Duration::from_millis(1500);

        for serial_path in &self.config.serial_paths {
            let stream = match crate::transport::serial::open_with_platform_cflag(
                serial_path,
                self.config.baud_rate,
            ) {
                Ok(stream) => stream,
                Err(err) => {
                    warn!(
                        serial_path,
                        %err,
                        "Cannot open this chain to arm its sensors; it will be CALIBRATED with \
                         the on-die protection UNARMED, until its thread attaches"
                    );
                    continue;
                }
            };
            // THROUGH THE DRY-RUN VETO, like every other chain port. A raw
            // `split()` here sent seven broadcast WRITEREGs to an energised
            // chain in a run declared write-free, with nothing refused and
            // nothing logged -- so a dry-run proof passed with a hole in it.
            let (mut reader, mut writer, _control) = super::config::split_chain_port(stream);

            match tokio::time::timeout(
                REACH_TIMEOUT,
                configure_dts_vs_stream(&mut writer, &mut reader, &Bzm2DtsVsConfig::from_env()),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    warn!(serial_path, %err,
                        "Could not configure the sensor block before the ramp");
                    continue;
                }
                Err(_) => {
                    info!(
                        serial_path,
                        "Chain did not answer when arming its sensors; the thread will arm them \
                         when it attaches, as it does today."
                    );
                    continue;
                }
            }

            match confirm_on_die_protection(
                &mut reader,
                self.config.dts_vs_generation,
                crate::asic::bzm2::protocol::BROADCAST_ASIC,
                REACH_TIMEOUT,
            )
            .await
            {
                Ok(who) => info!(
                    serial_path,
                    asic = who,
                    "On-die protection armed and confirmed BEFORE the stack ramp"
                ),
                Err(err) => warn!(
                    serial_path, %err,
                    "Sensors were configured before the ramp and no device reported them in                      force, so the stack is about to come up UNVERIFIED"
                ),
            }
        }
    }

    /// Read one board's rail from its MCU, or `None` if it cannot be read.
    ///
    /// The one home for reaching the witness. Both the bring-up confirmation
    /// and the shutdown confirmation need it, and a second copy of how to find
    /// a board's MCU is a second thing to get wrong when platform two arrives.
    async fn read_board_rail_mv(&self, board_index: usize) -> Option<u16> {
        use crate::hw_trait::i2c::I2c;
        let platform = super::platform::DEFAULT;
        let bus_path = platform.i2c_bus_path(board_index)?;
        let mut bus = crate::hw_trait::i2c::linux::LinuxI2c::open(&bus_path).ok()?;
        let selector = [super::board_mcu::OP_GET_VDD_TOTAL, 0];
        let mut reply = [0u8; 2];
        bus.write_read(super::board_mcu::MCU_I2C_ADDRESS, &selector, &mut reply)
            .await
            .ok()
            .map(|()| u16::from_le_bytes(reply))
    }

    /// Board indices whose chains this driver is actually driving.
    pub(super) fn driven_board_indices(&self) -> Vec<usize> {
        let platform = super::platform::DEFAULT;
        self.config
            .serial_paths
            .iter()
            .filter_map(|p| platform.board_index_for_chain(std::path::Path::new(p)))
            .collect()
    }

    /// Confirm the rails actually fell after a shutdown.
    ///
    /// THE MOST IMPORTANT CONFIRMATION IN THIS FILE. `plan.shutdown` zeroes
    /// every setpoint and de-asserts every enable through write-only sysfs
    /// nodes and returns Ok -- so a regulator that ignored the write, a path
    /// that did not exist, and a stack that genuinely went dark were
    /// indistinguishable, and the caller was told the machine was safe.
    ///
    /// Reports rather than fails, because there is nothing further this layer
    /// can do: the setpoints are already written. What it must never do is stay
    /// silent, which leaves a process exiting in the belief it de-energised a
    /// board it did not.
    async fn confirm_rails_dark(&self) {
        // Rails fall fast, but not instantly, and a shutdown that is merely
        // slow must not read as one that failed.
        const SETTLE_ATTEMPTS: usize = 10;
        const SETTLE_GAP: Duration = Duration::from_millis(300);

        for board_index in self.driven_board_indices() {
            let mut last = None;
            for _ in 0..SETTLE_ATTEMPTS {
                match self.read_board_rail_mv(board_index).await {
                    Some(mv) => {
                        last = Some(mv);
                        if mv < super::board_mcu::RAIL_DARK_MV {
                            break;
                        }
                    }
                    None => last = None,
                }
                tokio::time::sleep(SETTLE_GAP).await;
            }
            match last {
                Some(mv) if mv < super::board_mcu::RAIL_DARK_MV => {
                    info!(
                        board_index,
                        rail_mv = mv,
                        "Rail confirmed dark after shutdown"
                    )
                }
                Some(mv) => error!(
                    board_index,
                    rail_mv = mv,
                    "RAIL IS STILL UP AFTER SHUTDOWN. Every setpoint was written and the stack \
                     did not de-energise. Do not treat this board as safe."
                ),
                None => warn!(
                    board_index,
                    "Could not read this board's rail after shutdown, so whether it de-energised \
                     is UNMEASURED -- not confirmed, and not known to have failed"
                ),
            }
        }
    }

    /// Read each driven board's rail from its MCU and say whether it arrived.
    ///
    /// Reports rather than fails. A board that did not reach its commanded
    /// voltage is a serious finding, but bring-up is already past the point
    /// where refusing helps -- the rails are up or they are not -- and the
    /// caller that can act on it is the monitor, which reads the same MCU.
    /// What this must not do is stay silent, which is what it did before.
    async fn confirm_rail_reached(&self, commanded_mv: u32) {
        // Rails do not step. Poll rather than sleeping once and judging.
        const SETTLE_ATTEMPTS: usize = 10;
        const SETTLE_GAP: Duration = Duration::from_millis(300);
        // Generous: a loaded rail sags, and the failure worth catching is a
        // rail at zero or at half, not one a few percent low.
        const TOLERANCE: f32 = 0.15;

        for board_index in self.driven_board_indices() {
            let mut measured = None;
            for _ in 0..SETTLE_ATTEMPTS {
                if let Some(mv) = self.read_board_rail_mv(board_index).await {
                    measured = Some(mv);
                    let want = commanded_mv as f32;
                    if (mv as f32 - want).abs() / want <= TOLERANCE {
                        break;
                    }
                }
                tokio::time::sleep(SETTLE_GAP).await;
            }

            match measured {
                Some(mv) if mv < super::board_mcu::RAIL_DARK_MV => error!(
                    board_index,
                    commanded_mv,
                    measured_mv = mv,
                    "RAIL IS DARK after being commanded to a live voltage. The setpoint write                      succeeded and the rail did not come up."
                ),
                Some(mv) => {
                    let off = (mv as f32 - commanded_mv as f32).abs() / commanded_mv as f32;
                    if off <= TOLERANCE {
                        info!(
                            board_index,
                            commanded_mv,
                            measured_mv = mv,
                            "Rail confirmed at the commanded voltage by the board MCU"
                        );
                    } else {
                        warn!(
                            board_index,
                            commanded_mv,
                            measured_mv = mv,
                            "Rail is up but not at the commanded voltage"
                        );
                    }
                }
                None => warn!(
                    board_index,
                    commanded_mv,
                    "Could not read this board's rail, so the commanded voltage is UNMEASURED                      -- not confirmed, and not known to have failed"
                ),
            }
        }
    }

    pub(super) async fn apply_frequency_map(
        &self,
        bus_layouts: &[Bzm2BusLayout],
        initial_frequencies_mhz: [f32; 2],
        per_asic_pll_mhz: &BTreeMap<u16, [f32; 2]>,
    ) -> Result<(), BoardError> {
        for bus in bus_layouts {
            self.apply_bus_frequency_map(bus, initial_frequencies_mhz, per_asic_pll_mhz)
                .await?;
        }
        Ok(())
    }

    pub(super) async fn apply_bus_frequency_map(
        &self,
        bus: &Bzm2BusLayout,
        initial_frequencies_mhz: [f32; 2],
        per_asic_pll_mhz: &BTreeMap<u16, [f32; 2]>,
    ) -> Result<(), BoardError> {
        if bus.asic_count == 0 {
            return Ok(());
        }
        let stream = crate::transport::serial::open_with_platform_cflag(
            &bus.serial_path,
            self.config.baud_rate,
        )
        .map_err(|err| {
            BoardError::InitializationFailed(format!(
                "Failed to open BZM2 calibration transport {}: {}",
                bus.serial_path, err
            ))
        })?;
        let (reader, writer, _control) = super::config::split_chain_port(stream);
        let mut clock = Bzm2ClockController::new(reader, writer);

        for (pll, frequency_mhz) in [Bzm2Pll::Pll0, Bzm2Pll::Pll1]
            .into_iter()
            .zip(initial_frequencies_mhz)
        {
            clock
                .broadcast_pll_frequency(
                    pll,
                    frequency_mhz,
                    self.config.calibration.pll_post1_divider,
                )
                .await
                .map_err(|err| calibration_error(&bus.serial_path, err))?;
            clock
                .broadcast_enable_pll(pll)
                .await
                .map_err(|err| calibration_error(&bus.serial_path, err))?;
        }

        if !self.config.calibration.skip_lock_check {
            for local_asic in 0..bus.asic_count {
                for pll in [Bzm2Pll::Pll0, Bzm2Pll::Pll1] {
                    clock
                        .wait_for_pll_lock(
                            local_asic as u8,
                            pll,
                            self.config.calibration.lock_timeout,
                            self.config.calibration.lock_poll_interval,
                        )
                        .await
                        .map_err(|err| calibration_error(&bus.serial_path, err))?;
                }
            }
        }

        for asic_id in bus.asic_start..bus.asic_start + bus.asic_count {
            let Some(frequencies_mhz) = per_asic_pll_mhz.get(&asic_id) else {
                continue;
            };
            let local_asic = bus
                .local_asic_id(asic_id)
                .expect("bus layout must contain loop asic id");
            for (index, frequency_mhz) in frequencies_mhz.iter().enumerate() {
                let pll = if index == 0 {
                    Bzm2Pll::Pll0
                } else {
                    Bzm2Pll::Pll1
                };
                clock
                    .set_pll_frequency(
                        local_asic,
                        pll,
                        *frequency_mhz,
                        self.config.calibration.pll_post1_divider,
                    )
                    .await
                    .map_err(|err| calibration_error(&bus.serial_path, err))?;
                clock
                    .enable_pll(local_asic, pll)
                    .await
                    .map_err(|err| calibration_error(&bus.serial_path, err))?;
                if !self.config.calibration.skip_lock_check {
                    clock
                        .wait_for_pll_lock(
                            local_asic,
                            pll,
                            self.config.calibration.lock_timeout,
                            self.config.calibration.lock_poll_interval,
                        )
                        .await
                        .map_err(|err| calibration_error(&bus.serial_path, err))?;
                }
            }
        }

        Ok(())
    }
}

fn calibration_error(serial_path: &str, err: impl std::fmt::Display) -> BoardError {
    BoardError::InitializationFailed(format!(
        "BZM2 calibration failed on {}: {}",
        serial_path, err
    ))
}
