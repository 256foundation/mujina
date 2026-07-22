//! Power-rail bring-up, reset sequencing, and voltage/frequency application for the BZM2 board.

use std::collections::BTreeMap;
use std::env;
use std::time::Duration;

use crate::api_client::types::{Bzm2StartupPath, PowerMeasurement, TemperatureSensor};
use crate::asic::bzm2::{Bzm2ClockController, Bzm2Pll};
use crate::board::power::{
    FileGpioPin, FilePowerRail, GpioResetLine, PowerRail, VoltageStackBringupPlan, VoltageStackStep,
};
use crate::tracing::prelude::*;
use crate::transport::SerialStream;
use crate::types::Temperature;

use super::calibration::{
    Bzm2BusLayout, Bzm2PersistedCalibrationProfile, store_applied_operating_state,
};
use super::config::{
    DEFAULT_BOARD_TEMP_SCALE, DEFAULT_BRINGUP_POST_POWER_MS, DEFAULT_BRINGUP_PRE_POWER_MS,
    DEFAULT_BRINGUP_RELEASE_RESET_MS, DEFAULT_CALIBRATION_REPLAY_FREQ_MHZ, DEFAULT_CURRENT_SCALE,
    DEFAULT_POWER_SCALE, DEFAULT_VOLTAGE_SCALE, average_f32, env_csv_strings_any, env_flag_any,
    env_flag_default_any, env_var_any, parse_csv_numbers, parse_csv_numbers_any,
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
        }
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
        if !self.bringup_applied || !self.config.bringup.enabled {
            return Ok(());
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
        Ok(())
    }

    pub(super) async fn apply_saved_operating_point(
        &self,
        bus_layouts: &[Bzm2BusLayout],
        profile: &Bzm2PersistedCalibrationProfile,
    ) -> Result<(), BoardError> {
        self.apply_domain_voltage_map(&profile.saved_state.per_domain_voltage_mv)
            .await?;
        for bus in bus_layouts {
            if bus.asic_count == 0 {
                continue;
            }
            let initial_frequencies = [0usize, 1usize].map(|pll_index| {
                average_f32(
                    (bus.asic_start..bus.asic_start + bus.asic_count)
                        .filter_map(|asic_id| profile.saved_state.per_asic_pll_mhz.get(&asic_id))
                        .map(|frequencies| frequencies[pll_index]),
                )
                .unwrap_or(DEFAULT_CALIBRATION_REPLAY_FREQ_MHZ)
            });
            self.apply_bus_frequency_map(
                bus,
                initial_frequencies,
                &profile.saved_state.per_asic_pll_mhz,
            )
            .await?;
        }
        store_applied_operating_state(
            &self.applied_operating_state,
            &profile.saved_state.per_domain_voltage_mv,
            &profile.saved_state.per_asic_pll_mhz,
            Some(profile.saved_state.clone()),
            Some(Bzm2StartupPath::SavedReplay),
            Some(profile.saved_operating_point_status),
            &profile.saved_operating_point_reasons,
        );
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
        for (rail_index, voltage_mv) in rail_targets_mv {
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
        Ok(())
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
        let stream = SerialStream::new(&bus.serial_path, self.config.baud_rate).map_err(|err| {
            BoardError::InitializationFailed(format!(
                "Failed to open BZM2 calibration transport {}: {}",
                bus.serial_path, err
            ))
        })?;
        let (reader, writer, _control) = stream.split();
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
