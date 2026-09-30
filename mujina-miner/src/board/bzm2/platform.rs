//! Which machine the BZM2 hashboards are plugged into.
//!
//! The ASIC protocol, the per-ASIC calibration and the hashboard MCU's opcode
//! set are properties of the **silicon and the board**. They travel wherever
//! the board does. What does not travel is how a particular chassis wires them
//! up: how many boards it holds, which I²C adapter reaches board 0, and what
//! the chain UART is called.
//!
//! Those three facts were constants in [`super::board_mcu`], which is fine
//! while exactly one machine exists and is a trap the moment a second one
//! does: the second platform arrives as a patch to a file full of the first
//! platform's assumptions, and the line between "universal to BZM2" and
//! "true of the RDS" stops being checkable. Named here instead, so a new
//! machine is a `const` and a test, not a diff.
//!
//! # What is deliberately NOT here
//!
//! - **`MCU_I2C_ADDRESS` (0x76)** — every hashboard MCU answers there. It is a
//!   property of the board, not of the chassis holding it.
//! - **ASICs per chain (100), engines per ASIC, series depth** — the board's
//!   own population.
//! - **Anything about calibration.** The stored per-ASIC tuning is the
//!   hashboard's silicon lottery and references nothing about the host.
//!
//! Put another way: if swapping the chassis would change it, it belongs here;
//! if swapping the hashboard would change it, it does not.

use std::path::PathBuf;

/// How one machine wires its BZM2 hashboards up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bzm2Platform {
    /// What this machine is called, for logs and for the API to report.
    pub name: &'static str,
    /// Hashboards the chassis holds.
    pub board_count: usize,
    /// Linux I²C adapter number carrying board 0. Boards are consecutive.
    pub first_i2c_adapter: usize,
    /// Where a PWM channel is exported into existence.
    ///
    /// MEASURED, 2026-09-22: all four chips report `npwm=1` and NONE are
    /// exported on a fresh boot. `pwmchip{n}/pwm0/` -- which every other fan
    /// path here assumes -- does not exist until `0` is written here. Every
    /// run we had taken inherited the export from the vendor stack, so nothing
    /// ever needed it; the first boot without that stack has no fan control at
    /// all and no path to write to.
    pub fan_pwm_export_pattern: &'static str,
    /// PWM period to program after exporting, in nanoseconds.
    ///
    /// A freshly exported channel reads `period=0`, and a duty cycle cannot
    /// exceed its period, so until this is written no duty can be set and the
    /// fan cannot be commanded at all. 40,000 ns is what this chassis runs at,
    /// read off the vendor stack's own configuration in our captures.
    pub fan_pwm_period_ns: u64,
    /// The PWM channel's own enable, distinct from the hwmon gate.
    ///
    /// MEASURED, 2026-09-22, and it is the operative control: with duty at
    /// 100 % and this at `0`, fan 0 turned at 34 tacho counts. Writing `1`
    /// took it to 224 -- 1,020 rpm to 6,720 -- while the three channels left
    /// unenabled stayed put. Opening the hwmon gate afterwards changed nothing
    /// (223 against 224), so on this chassis the gate is not what starts the
    /// fan; this is.
    ///
    /// A freshly exported channel reads `0` here. Every duty we wrote before
    /// this existed went into a disabled channel, and appeared to work only
    /// because the vendor stack had enabled it first.
    pub fan_pwm_enable_pattern: &'static str,
    /// Tacho-derived rpm a healthy fan reaches at 100 % duty, measured.
    ///
    /// Used by the POST, which commands FULL: a preflight that only asks
    /// "is it turning" passes a fan delivering 15 % of what was asked, which
    /// is exactly what happened before the enable above was written. The
    /// question worth answering before a kilowatt goes in is whether the fan
    /// can reach the speed the failure path will demand of it.
    pub fan_rpm_at_full: u32,
    /// Chain UART device for a board, as a `printf`-style pattern taking the
    /// board index. Held as a pattern rather than a list because the boards
    /// are uniform; a machine whose ports are not uniform wants a slice, and
    /// the day one exists this becomes one.
    pub chain_port_pattern: &'static str,
    /// Chain UART baud. 9-bit framing is the ASIC's, but the rate the host
    /// can actually drive is the host's.
    pub chain_baud: u32,

    /// How many chassis fans this machine has, and where their control lives.
    ///
    /// Two nodes per fan and NEITHER is sufficient alone: the duty sets the
    /// level, and a separate gate decides whether that duty drives anything.
    /// Measured on the RDS -- a duty written with the gate shut reads back
    /// perfectly and moves no air.
    pub fan_count: usize,
    /// `{}` is the fan index. The commanded level.
    pub fan_duty_pattern: &'static str,
    /// `{}` is the fan index. Full scale for the duty above.
    pub fan_period_pattern: &'static str,
    /// `{}` is the fan index. The control gate. WRITE-ONLY on the RDS --
    /// reading it returns an I/O error, which is why the tacho is the only
    /// confirmation available.
    pub fan_gate_pattern: &'static str,
    /// `{}` is the fan index. A raw pulse count, not rpm.
    pub fan_tacho_pattern: &'static str,
    /// Revolutions per minute per tacho count.
    ///
    /// EXACT BY CONSTRUCTION rather than by calibration: the node counts
    /// pulses over a one-second window across a two-pulse-per-revolution
    /// wheel. Confirmed against the vendor stack's own console, which printed
    /// 5430 rpm while we read 181 counts.
    pub fan_rpm_per_count: u32,
}

impl Bzm2Platform {
    /// The I²C bus reaching one board's MCU, or `None` past the last board.
    pub fn i2c_bus_path(&self, board_index: usize) -> Option<PathBuf> {
        (board_index < self.board_count)
            .then(|| PathBuf::from(format!("/dev/i2c-{}", self.first_i2c_adapter + board_index)))
    }

    /// Which board a chain UART belongs to, or `None` if it is not one of ours.
    ///
    /// The inverse of [`Self::chain_port`], and it lives here for the same
    /// reason that does: the mapping between a chain device and a board index
    /// is a property of the machine. A caller that parsed the digit out of the
    /// device name itself would be a second copy of this platform's naming
    /// convention, and the second platform is what makes such a copy wrong.
    pub fn board_index_for_chain(&self, path: &std::path::Path) -> Option<usize> {
        (0..self.board_count).find(|&index| self.chain_port(index).as_deref() == Some(path))
    }

    /// Paths for one fan, or `None` past the last fan.
    pub fn fan_paths(&self, fan: usize) -> Option<FanPaths> {
        (fan < self.fan_count).then(|| {
            let at = |pat: &str| PathBuf::from(pat.replace("{}", &fan.to_string()));
            FanPaths {
                duty: at(self.fan_duty_pattern),
                period: at(self.fan_period_pattern),
                gate: at(self.fan_gate_pattern),
                tacho: at(self.fan_tacho_pattern),
            }
        })
    }

    /// The chain UART for one board, or `None` past the last board.
    pub fn chain_port(&self, board_index: usize) -> Option<PathBuf> {
        (board_index < self.board_count).then(|| {
            PathBuf::from(
                self.chain_port_pattern
                    .replace("{}", &board_index.to_string()),
            )
        })
    }
}

/// Where one fan's four control nodes live.
#[derive(Debug, Clone)]
pub struct FanPaths {
    pub duty: PathBuf,
    pub period: PathBuf,
    pub gate: PathBuf,
    pub tacho: PathBuf,
}

/// RDS 2.0 control board: Cyclone V, three hashboards.
///
/// Every value MEASURED on the unit rather than read from a document:
/// adapters 2, 3 and 4 answer at 0x76 for boards 0, 1 and 2, and the chain
/// devices are `/dev/tty9bit00`, `10` and `20`.
pub const RDS_2_0: Bzm2Platform = Bzm2Platform {
    name: "rds-2.0",
    board_count: 3,
    first_i2c_adapter: 2,
    chain_port_pattern: "/dev/tty9bit{}0",
    fan_count: 4,
    fan_pwm_export_pattern: "/sys/class/pwm/pwmchip{}/export",
    fan_pwm_period_ns: 40_000,
    fan_pwm_enable_pattern: "/sys/class/pwm/pwmchip{}/pwm0/enable",
    fan_rpm_at_full: 6_800,
    fan_duty_pattern: "/sys/class/pwm/pwmchip{}/pwm0/duty_cycle",
    fan_period_pattern: "/sys/class/pwm/pwmchip{}/pwm0/period",
    fan_gate_pattern: "/sys/class/hwmon/hwmon{}/enable",
    fan_tacho_pattern: "/sys/class/hwmon/hwmon{}/speed",
    fan_rpm_per_count: 30,
    chain_baud: 5_000_000,
};

/// Every platform this build knows. A machine absent from here is a machine
/// this binary cannot address, which is a better failure than addressing it
/// with another machine's numbers.
#[cfg(test)]
pub const KNOWN: &[Bzm2Platform] = &[RDS_2_0];

/// What to use when nothing selected a platform.
///
/// The RDS is the reference platform and the only one that has ever run, so it
/// is the default *and it is named* — a default that is not stated is an
/// assumption, and this one would be an assumption about which machine is on
/// the other end of an I²C bus.
pub const DEFAULT: Bzm2Platform = RDS_2_0;

/// Look a platform up by name.
#[cfg(test)]
pub fn by_name(name: &str) -> Option<Bzm2Platform> {
    KNOWN.iter().copied().find(|p| p.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chain_device_maps_back_to_its_board() {
        // The inverse must agree with the forward mapping for every board, or
        // the heartbeat arms one board's watchdog while another board's chain
        // is the one being driven -- which would shed a working board and
        // leave the unattended one powered.
        for index in 0..RDS_2_0.board_count {
            let port = RDS_2_0.chain_port(index).unwrap();
            assert_eq!(
                RDS_2_0.board_index_for_chain(&port),
                Some(index),
                "chain {} did not map back to board {index}",
                port.display()
            );
        }
    }

    #[test]
    fn a_device_that_is_not_ours_maps_to_nothing() {
        // Guessing an index from an unrecognised name is how a heartbeat ends
        // up addressed at a board that is not there.
        for path in ["/dev/ttyUSB0", "/dev/tty9bit30", "/dev/null", ""] {
            assert_eq!(
                RDS_2_0.board_index_for_chain(std::path::Path::new(path)),
                None,
                "{path} should not resolve to a board"
            );
        }
    }

    use super::*;

    #[test]
    fn rds_bus_paths_are_the_measured_ones() {
        assert_eq!(
            RDS_2_0.i2c_bus_path(0).unwrap().to_str(),
            Some("/dev/i2c-2")
        );
        assert_eq!(
            RDS_2_0.i2c_bus_path(1).unwrap().to_str(),
            Some("/dev/i2c-3")
        );
        assert_eq!(
            RDS_2_0.i2c_bus_path(2).unwrap().to_str(),
            Some("/dev/i2c-4")
        );
    }

    #[test]
    fn past_the_last_board_is_none_not_a_wrong_path() {
        assert_eq!(RDS_2_0.i2c_bus_path(3), None);
        assert_eq!(RDS_2_0.chain_port(3), None);
    }

    #[test]
    fn rds_chain_ports_are_the_measured_ones() {
        assert_eq!(
            RDS_2_0.chain_port(0).unwrap().to_str(),
            Some("/dev/tty9bit00")
        );
        assert_eq!(
            RDS_2_0.chain_port(1).unwrap().to_str(),
            Some("/dev/tty9bit10")
        );
        assert_eq!(
            RDS_2_0.chain_port(2).unwrap().to_str(),
            Some("/dev/tty9bit20")
        );
    }

    #[test]
    fn every_known_platform_is_reachable_by_its_own_name() {
        // A platform in KNOWN that by_name cannot find is one nobody can
        // select, which is the same as it not being there.
        for p in KNOWN {
            assert_eq!(by_name(p.name), Some(*p), "{} is unreachable", p.name);
        }
        assert_eq!(by_name("no-such-machine"), None);
    }

    #[test]
    fn the_default_is_one_of_the_known_ones() {
        assert!(KNOWN.contains(&DEFAULT));
    }

    #[test]
    fn no_two_platforms_share_a_name() {
        for (i, a) in KNOWN.iter().enumerate() {
            for b in &KNOWN[i + 1..] {
                assert_ne!(a.name, b.name, "duplicate platform name {}", a.name);
            }
        }
    }
}
