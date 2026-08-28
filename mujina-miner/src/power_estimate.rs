//! Forward power model for S19K Pro (BHB56902 / BM1366) on APW12.
//!
//! The APW12 does not report current or power (Braiins and LuxOS both
//! treat APW12 watts as *estimates*). This module is the same class of
//! model those firmwares bake in: `P ∝ N · f · V²`, anchored on the
//! nameplate operating point.
//!
//! See `docs/s19k-pro/power-controls.md` for the derivation. Numbers
//! here are **estimates** until a wall meter (or a PSU that reports
//! current) calibrates them. Call sites must label the result as such.

/// Nameplate: 120 TH/s @ 2760 W AC ≈ 23.0 J/TH.
pub const NAMEPLATE_HASHRATE_THS: f32 = 120.0;
pub const NAMEPLATE_POWER_W: f32 = 2760.0;
pub const NAMEPLATE_J_PER_TH: f32 = NAMEPLATE_POWER_W / NAMEPLATE_HASHRATE_THS;

/// Factory ATE setpoint used as the model anchor.
pub const NOMINAL_FREQUENCY_MHZ: f32 = 645.0;
pub const NOMINAL_RAIL_VOLTS: f32 = 13.9;
pub const NOMINAL_CHIP_COUNT: f32 = 231.0; // 77 × 3 boards

/// Rough non-ASIC AC loads subtracted when deriving DC ASIC power at
/// the nameplate point (fans ≈ 135 W, control board ≈ 15 W, PSU eff ≈ 93%).
const FANS_AND_CONTROL_W: f32 = 150.0;
const PSU_EFFICIENCY: f32 = 0.93;

/// Inputs for a single power estimate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PowerEstimateInput {
    /// Chip frequency in MHz.
    pub frequency_mhz: f32,
    /// Shared APW12 rail voltage in volts.
    pub rail_volts: f32,
    /// Number of chips actively hashing.
    pub chip_count: u32,
}

/// Result of the forward model.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PowerEstimate {
    /// Estimated AC wall draw in watts.
    pub ac_watts: f32,
    /// Estimated DC ASIC power in watts (before PSU / fan overhead).
    pub asic_dc_watts: f32,
    /// Implied efficiency in J/TH at the estimated hashrate.
    pub j_per_th: f32,
    /// Model hashrate in TH/s (linear in N·f from nameplate).
    pub hashrate_ths: f32,
}

impl PowerEstimateInput {
    /// Estimate AC power from the CMOS-style forward model.
    ///
    /// ```text
    /// P_asic_dc(nominal) ≈ NAMEPLATE_POWER_W * PSU_EFFICIENCY - FANS_AND_CONTROL_W
    /// P_asic(f, V, N)    ≈ P_asic_dc(nominal) · (N/231) · (f/645) · (V/13.9)²
    /// P_ac               ≈ (P_asic + FANS_AND_CONTROL_W) / PSU_EFFICIENCY
    /// ```
    pub fn estimate(self) -> Option<PowerEstimate> {
        if self.frequency_mhz <= 0.0
            || self.rail_volts <= 0.0
            || self.chip_count == 0
            || !self.frequency_mhz.is_finite()
            || !self.rail_volts.is_finite()
        {
            return None;
        }

        let p_asic_dc_nominal = NAMEPLATE_POWER_W * PSU_EFFICIENCY - FANS_AND_CONTROL_W;
        let n_scale = self.chip_count as f32 / NOMINAL_CHIP_COUNT;
        let f_scale = self.frequency_mhz / NOMINAL_FREQUENCY_MHZ;
        let v_scale = self.rail_volts / NOMINAL_RAIL_VOLTS;
        let asic_dc = p_asic_dc_nominal * n_scale * f_scale * v_scale * v_scale;
        let ac_watts = (asic_dc + FANS_AND_CONTROL_W) / PSU_EFFICIENCY;
        let hashrate_ths =
            NAMEPLATE_HASHRATE_THS * n_scale * (self.frequency_mhz / NOMINAL_FREQUENCY_MHZ);
        let j_per_th = if hashrate_ths > 0.0 {
            ac_watts / hashrate_ths
        } else {
            0.0
        };

        Some(PowerEstimate {
            ac_watts,
            asic_dc_watts: asic_dc,
            j_per_th,
            hashrate_ths,
        })
    }
}

/// Convenience: estimate from rail volts at the default S19K operating
/// point (575 MHz, all 231 chips) — useful when only voltage is known.
pub fn estimate_at_default_frequency(rail_volts: f32, chip_count: u32) -> Option<PowerEstimate> {
    PowerEstimateInput {
        frequency_mhz: 575.0,
        rail_volts,
        chip_count,
    }
    .estimate()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nameplate_point_is_near_2760w() {
        let est = PowerEstimateInput {
            frequency_mhz: NOMINAL_FREQUENCY_MHZ,
            rail_volts: NOMINAL_RAIL_VOLTS,
            chip_count: NOMINAL_CHIP_COUNT as u32,
        }
        .estimate()
        .unwrap();
        // Model reconstructs nameplate AC by construction.
        assert!(
            (est.ac_watts - NAMEPLATE_POWER_W).abs() < 1.0,
            "ac_watts={} expected ~{}",
            est.ac_watts,
            NAMEPLATE_POWER_W
        );
        assert!((est.hashrate_ths - NAMEPLATE_HASHRATE_THS).abs() < 0.1);
    }

    #[test]
    fn measured_operating_point_is_near_2270w() {
        // From docs/s19k-pro/power-controls.md sanity check:
        // 575 MHz, 14.04 V, 206 chips ≈ 2,270 W AC.
        let est = PowerEstimateInput {
            frequency_mhz: 575.0,
            rail_volts: 14.04,
            chip_count: 206,
        }
        .estimate()
        .unwrap();
        assert!(
            (est.ac_watts - 2270.0).abs() < 40.0,
            "ac_watts={} expected ~2270",
            est.ac_watts
        );
    }

    #[test]
    fn half_frequency_roughly_halves_asic_power() {
        let full = PowerEstimateInput {
            frequency_mhz: 600.0,
            rail_volts: 13.9,
            chip_count: 231,
        }
        .estimate()
        .unwrap();
        let half = PowerEstimateInput {
            frequency_mhz: 300.0,
            rail_volts: 13.9,
            chip_count: 231,
        }
        .estimate()
        .unwrap();
        let ratio = half.asic_dc_watts / full.asic_dc_watts;
        assert!(
            (ratio - 0.5).abs() < 0.01,
            "asic power ratio at half f = {ratio}"
        );
    }

    #[test]
    fn rejects_non_physical_inputs() {
        assert!(
            PowerEstimateInput {
                frequency_mhz: 0.0,
                rail_volts: 13.9,
                chip_count: 231,
            }
            .estimate()
            .is_none()
        );
        assert!(
            PowerEstimateInput {
                frequency_mhz: 575.0,
                rail_volts: 13.9,
                chip_count: 0,
            }
            .estimate()
            .is_none()
        );
    }
}
