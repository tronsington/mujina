//! APW12 PSU driver, generic over I2C implementation.
//!
//! Bitmain's APW12-family PSUs (used on the S19 lineup, including the
//! S19K Pro's APW121215d/e) speak a small framed protocol over I2C:
//! each frame byte is its own write transaction to a fixed register,
//! and the response is read back one byte at a time as current-address
//! reads. That shape maps directly onto plain [`I2c::write`]/[`I2c::read`]
//! calls, so this driver works unchanged whether `I2c` is backed by a
//! real Linux I2C controller or (as on the S19K Pro, where the PSU
//! isn't reachable from any real `/dev/i2c-N`) a bit-banged one.
//!
//! Protocol reverse-engineered from skot/amlogic-cb-tools'
//! `apw12-psu-tool` (not part of Mujina); this port and the DAC/ADC
//! calibration constants come from there. See
//! `docs/s19k-pro/hardware.md`'s "PSU communication" section for the
//! full protocol writeup and how the address/register were confirmed
//! against real APW12 firmware.

use crate::hw_trait::{HwError, i2c::I2c};

/// Default 7-bit I2C address, confirmed against real APW12 PIC
/// firmware (its SSPADD setup implies this exact address).
pub const DEFAULT_ADDRESS: u8 = 0x10;

/// Default outbound register: every frame byte is written here.
pub const DEFAULT_WRITE_REGISTER: u8 = 0x11;

const PREAMBLE_LSB: u8 = 0x55;
const PREAMBLE_MSB: u8 = 0xAA;
const NAK_BYTE: u8 = 0xF5;

const CMD_GET_FW_VERSION: u8 = 0x01;
const CMD_GET_HW_VERSION: u8 = 0x02;
const CMD_GET_VOLTAGE: u8 = 0x03;
const CMD_MEASURE_VOLTAGE: u8 = 0x04;
const CMD_READ_STATE: u8 = 0x05;
const CMD_WATCHDOG: u8 = 0x81;
const CMD_SET_VOLTAGE: u8 = 0x83;

const DAC_REF_VOLTS: f32 = 15.1084;
const DAC_OFFSET_VOLTS_PER_COUNT: f32 = -0.013046;

/// Driver error.
#[derive(Debug, thiserror::Error)]
pub enum Error<E> {
    #[error("I2C: {0}")]
    I2c(E),

    #[error("PSU returned NAK")]
    Nak,

    #[error("malformed response: {0}")]
    Malformed(&'static str),

    #[error("checksum mismatch: expected 0x{expected:02X}, got 0x{actual:02X}")]
    Checksum { expected: u8, actual: u8 },
}

type Result<T> = std::result::Result<T, Error<HwError>>;

/// PSU on/off state, from [`Apw12::read_state`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    On,
    Off,
}

/// APW12 driver, generic over I2C implementation.
pub struct Apw12<I> {
    i2c: I,
    address: u8,
    write_register: u8,
}

impl<I: I2c> Apw12<I> {
    /// Create a new driver instance at the default address/register.
    pub fn new(i2c: I) -> Self {
        Self {
            i2c,
            address: DEFAULT_ADDRESS,
            write_register: DEFAULT_WRITE_REGISTER,
        }
    }

    /// Measured output voltage.
    pub async fn measure_voltage(&mut self) -> Result<f32> {
        let payload = self.exchange(CMD_MEASURE_VOLTAGE, &[]).await?;
        let [lo, hi] = payload_pair(&payload)?;
        Ok(decode_measured_voltage(lo, hi))
    }

    /// Configured DAC setpoint, decoded to volts.
    pub async fn get_voltage(&mut self) -> Result<f32> {
        let payload = self.exchange(CMD_GET_VOLTAGE, &[]).await?;
        let dac = *payload
            .first()
            .ok_or(Error::Malformed("empty GET_VOLTAGE payload"))?;
        Ok(decode_dac_to_voltage(dac))
    }

    /// Set the output voltage setpoint.
    pub async fn set_voltage(&mut self, volts: f32) -> Result<()> {
        let dac = encode_voltage_to_dac(volts);
        self.exchange(CMD_SET_VOLTAGE, &[dac, 0x00]).await?;
        Ok(())
    }

    /// PSU on/off state.
    pub async fn read_state(&mut self) -> Result<State> {
        let payload = self.exchange(CMD_READ_STATE, &[]).await?;
        let [lo, hi] = payload_pair(&payload)?;
        let state = u16::from(lo) | (u16::from(hi) << 8);
        Ok(if state == 1 { State::On } else { State::Off })
    }

    /// Firmware version.
    pub async fn firmware_version(&mut self) -> Result<u16> {
        let payload = self.exchange(CMD_GET_FW_VERSION, &[]).await?;
        let [lo, hi] = payload_pair(&payload)?;
        Ok(u16::from(lo) | (u16::from(hi) << 8))
    }

    /// Hardware version.
    pub async fn hardware_version(&mut self) -> Result<u16> {
        let payload = self.exchange(CMD_GET_HW_VERSION, &[]).await?;
        let [lo, hi] = payload_pair(&payload)?;
        Ok(u16::from(lo) | (u16::from(hi) << 8))
    }

    /// Disable the PSU's comms watchdog (it otherwise expects periodic
    /// servicing and may fault without it).
    pub async fn disable_watchdog(&mut self) -> Result<()> {
        self.exchange(CMD_WATCHDOG, &[0x00, 0x00]).await?;
        Ok(())
    }

    /// Send a framed command and return the validated response payload.
    async fn exchange(&mut self, command: u8, payload: &[u8]) -> Result<Vec<u8>> {
        let frame = build_frame(command, payload);
        for byte in frame {
            self.i2c
                .write(self.address, &[self.write_register, byte])
                .await
                .map_err(Error::I2c)?;
        }

        let response = self.read_response_frame().await?;
        if response == [NAK_BYTE] {
            return Err(Error::Nak);
        }
        parse_frame(command, &response)
    }

    async fn read_response_frame(&mut self) -> Result<Vec<u8>> {
        let mut first = self.read_byte().await?;
        // A device that never syncs to the preamble would loop
        // forever; bail out after a generous number of stray bytes.
        for _ in 0..32 {
            if first == PREAMBLE_LSB || first == NAK_BYTE {
                break;
            }
            first = self.read_byte().await?;
        }
        if first == NAK_BYTE {
            return Ok(vec![NAK_BYTE]);
        }

        let second = self.read_byte().await?;
        if second != PREAMBLE_MSB {
            return Err(Error::Malformed("invalid preamble"));
        }

        let length = self.read_byte().await?;
        let mut response = vec![first, second, length];
        let remaining = usize::from(length)
            .checked_sub(1)
            .ok_or(Error::Malformed("length underflow"))?;
        for _ in 0..remaining {
            response.push(self.read_byte().await?);
        }
        Ok(response)
    }

    async fn read_byte(&mut self) -> Result<u8> {
        let mut buf = [0u8];
        self.i2c
            .read(self.address, &mut buf)
            .await
            .map_err(Error::I2c)?;
        Ok(buf[0])
    }
}

fn payload_pair(payload: &[u8]) -> Result<[u8; 2]> {
    payload
        .first_chunk::<2>()
        .copied()
        .ok_or(Error::Malformed("payload shorter than 2 bytes"))
}

fn checksum(length: u8, command: u8, payload: &[u8]) -> u16 {
    payload
        .iter()
        .fold(u16::from(length) + u16::from(command), |sum, b| {
            sum + u16::from(*b)
        })
}

fn build_frame(command: u8, payload: &[u8]) -> Vec<u8> {
    let length = (payload.len() + 4) as u8;
    let sum = checksum(length, command, payload);

    let mut frame = Vec::with_capacity(payload.len() + 6);
    frame.push(PREAMBLE_LSB);
    frame.push(PREAMBLE_MSB);
    frame.push(length);
    frame.push(command);
    frame.extend_from_slice(payload);
    frame.push((sum & 0x00FF) as u8);
    frame.push((sum >> 8) as u8);
    frame
}

fn parse_frame(expected_command: u8, raw: &[u8]) -> Result<Vec<u8>> {
    if raw.len() < 6 {
        return Err(Error::Malformed("frame shorter than 6 bytes"));
    }

    let declared_len = raw[2] as usize;
    if declared_len != raw.len() - 2 {
        return Err(Error::Malformed("declared length doesn't match frame size"));
    }

    let command = raw[3];
    let checksum_index = raw.len() - 2;
    let payload = &raw[4..checksum_index];
    let actual_checksum = raw[checksum_index];
    let expected_checksum = checksum(raw[2], command, payload) as u8;
    if actual_checksum != expected_checksum {
        return Err(Error::Checksum {
            expected: expected_checksum,
            actual: actual_checksum,
        });
    }
    if command != expected_command {
        return Err(Error::Malformed("response command doesn't match request"));
    }

    Ok(payload.to_vec())
}

fn decode_dac_to_voltage(dac: u8) -> f32 {
    DAC_REF_VOLTS + DAC_OFFSET_VOLTS_PER_COUNT * f32::from(dac)
}

fn encode_voltage_to_dac(voltage: f32) -> u8 {
    let code = ((voltage - DAC_REF_VOLTS) / DAC_OFFSET_VOLTS_PER_COUNT).round();
    code.clamp(0.0, 255.0) as u8
}

fn decode_measured_voltage(adc_lo: u8, adc_hi: u8) -> f32 {
    let raw = u16::from(adc_lo) | (u16::from(adc_hi) << 8);
    (raw as f32 + 0.8615) / 63.017
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_frame_matches_the_checksum_formula() {
        // length=4 (payload.len()+4), checksum=length+command=0x08.
        assert_eq!(
            build_frame(CMD_MEASURE_VOLTAGE, &[]),
            [0x55, 0xAA, 0x04, 0x04, 0x08, 0x00]
        );
    }

    #[test]
    fn parse_frame_round_trips_a_real_capture() {
        // "measure-voltage" response captured against real hardware:
        // adc_raw=0x03BB measured_voltage=15.1683 V
        let raw = [0x55, 0xAA, 0x06, 0x04, 0xBB, 0x03, 0xC8, 0x00];
        let payload = parse_frame(CMD_MEASURE_VOLTAGE, &raw).unwrap();
        let [lo, hi] = payload_pair(&payload).unwrap();
        assert!((decode_measured_voltage(lo, hi) - 15.1683).abs() < 0.001);
    }

    #[test]
    fn parse_frame_rejects_bad_checksum() {
        let mut raw = [0x55, 0xAA, 0x06, 0x04, 0xBB, 0x03, 0xC8, 0x00];
        raw[6] ^= 0xFF;
        assert!(matches!(
            parse_frame(CMD_MEASURE_VOLTAGE, &raw),
            Err(Error::Checksum { .. })
        ));
    }

    #[test]
    fn dac_voltage_round_trips_near_the_real_calibration() {
        // Real PSU readback: dac=0xE2 (226) -> 12.1600 V.
        assert!((decode_dac_to_voltage(0xE2) - 12.16).abs() < 0.01);
        assert_eq!(encode_voltage_to_dac(12.16), 0xE2);
    }
}
