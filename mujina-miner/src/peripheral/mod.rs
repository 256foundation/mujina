//! Peripheral chip drivers.
//!
//! This module contains drivers for non-mining peripheral chips such as
//! temperature sensors (TMP75), power monitors (INA260), fan controllers
//! (EMC2101), and other board management ICs. All drivers are generic
//! over the hw_trait interfaces.

pub mod emc2101;
pub mod pmbus;
pub mod tps546;
