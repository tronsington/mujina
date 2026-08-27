//! Software (bit-banged) [`I2c`] over two arbitrary sysfs GPIO lines.
//!
//! Some boards wire an I2C-shaped bus to plain GPIO instead of one of
//! the SoC's hardware I2C controllers -- on the S19K Pro, the PSU is
//! reachable this way (GPIO 476/477, labelled `I2C_SCL`/`I2C_SDA` in
//! the vendor's own boot script) and nowhere else; it never appears
//! on any real `/dev/i2c-N` device. [`BitBangI2c`] drives START/STOP
//! and clocks bits by hand, open-drain-emulated via sysfs GPIO
//! direction switching (`in` = released/pulled high, `out`+`0` =
//! driven low).
//!
//! Bit-banging needs real (if coarse) timing control between each
//! GPIO write, which async sleeps can't guarantee under executor
//! load. Each [`I2c`] method hands the whole transaction to
//! [`tokio::task::spawn_blocking`] and drives it with plain
//! synchronous file I/O and `std::thread::sleep`, so it can't be
//! delayed by other tasks mid-transaction.

use std::fs;
use std::thread::sleep;
use std::time::Duration;

use async_trait::async_trait;

use crate::hw_trait::{
    HwError, Result,
    i2c::{I2c, I2cError},
};

/// A bit-banged I2C bus over two GPIO lines.
pub struct BitBangI2c {
    scl: u32,
    sda: u32,
    half_period: Duration,
}

impl BitBangI2c {
    /// `scl`/`sda` are absolute sysfs GPIO numbers (not offsets --
    /// this doesn't share [`super::gpio::SysfsGpio`]'s single-controller
    /// binding, since a bit-banged bus's two lines could in principle
    /// span controllers).
    pub fn new(scl: u32, sda: u32) -> Self {
        Self {
            scl,
            sda,
            // ~50us half-period => ~10kHz. Deliberately slow and
            // conservative; set_frequency adjusts it.
            half_period: Duration::from_micros(50),
        }
    }
}

#[async_trait]
impl I2c for BitBangI2c {
    async fn write(&mut self, addr: u8, data: &[u8]) -> Result<()> {
        let (scl, sda, half_period) = (self.scl, self.sda, self.half_period);
        let data = data.to_vec();
        blocking(move || {
            let bus = Bus {
                scl,
                sda,
                half_period,
            };
            bus.init()?;
            bus.start()?;
            let result = (|| -> Result<()> {
                require_ack(addr, bus.write_byte(addr << 1)?)?;
                for &byte in &data {
                    require_ack(addr, bus.write_byte(byte)?)?;
                }
                Ok(())
            })();
            bus.stop()?;
            result
        })
        .await
    }

    async fn read(&mut self, addr: u8, buffer: &mut [u8]) -> Result<()> {
        let (scl, sda, half_period) = (self.scl, self.sda, self.half_period);
        let len = buffer.len();
        let read = blocking(move || {
            let bus = Bus {
                scl,
                sda,
                half_period,
            };
            bus.init()?;
            bus.start()?;
            let result = (|| -> Result<Vec<u8>> {
                require_ack(addr, bus.write_byte((addr << 1) | 1)?)?;
                bus.read_bytes(len)
            })();
            bus.stop()?;
            result
        })
        .await?;
        buffer.copy_from_slice(&read);
        Ok(())
    }

    async fn write_read(&mut self, addr: u8, write: &[u8], read: &mut [u8]) -> Result<()> {
        let (scl, sda, half_period) = (self.scl, self.sda, self.half_period);
        let write = write.to_vec();
        let len = read.len();
        let result = blocking(move || {
            let bus = Bus {
                scl,
                sda,
                half_period,
            };
            bus.init()?;
            bus.start()?;
            let result = (|| -> Result<Vec<u8>> {
                require_ack(addr, bus.write_byte(addr << 1)?)?;
                for &byte in &write {
                    require_ack(addr, bus.write_byte(byte)?)?;
                }
                // Repeated start: no STOP before re-addressing for the read.
                bus.start()?;
                require_ack(addr, bus.write_byte((addr << 1) | 1)?)?;
                bus.read_bytes(len)
            })();
            bus.stop()?;
            result
        })
        .await?;
        read.copy_from_slice(&result);
        Ok(())
    }

    async fn set_frequency(&mut self, hz: u32) -> Result<()> {
        let hz = hz.max(1);
        self.half_period = Duration::from_nanos(1_000_000_000 / u64::from(hz) / 2);
        Ok(())
    }
}

fn require_ack(addr: u8, acked: bool) -> Result<()> {
    if acked {
        Ok(())
    } else {
        Err(HwError::I2c(I2cError::NoAck(addr)))
    }
}

/// One bus transaction's worth of GPIO state, held for the duration of
/// a blocking closure.
struct Bus {
    scl: u32,
    sda: u32,
    half_period: Duration,
}

impl Bus {
    fn init(&self) -> Result<()> {
        export(self.scl)?;
        export(self.sda)?;
        self.release(self.scl)?;
        self.release(self.sda)?;
        self.delay();
        Ok(())
    }

    fn start(&self) -> Result<()> {
        self.release(self.sda)?;
        self.release(self.scl)?;
        self.delay();
        self.drive_low(self.sda)?;
        self.delay();
        self.drive_low(self.scl)?;
        self.delay();
        Ok(())
    }

    fn stop(&self) -> Result<()> {
        self.drive_low(self.sda)?;
        self.delay();
        self.release(self.scl)?;
        self.delay();
        self.release(self.sda)?;
        self.delay();
        Ok(())
    }

    /// Write a byte MSB-first; returns whether the slave ACKed.
    fn write_byte(&self, byte: u8) -> Result<bool> {
        for i in (0..8).rev() {
            self.write_bit((byte >> i) & 1 != 0)?;
        }
        let nak = self.read_bit()?;
        Ok(!nak)
    }

    /// Read `len` bytes, ACKing all but the last (which is NAKed to
    /// signal "no more", the standard I2C master-read convention).
    fn read_bytes(&self, len: usize) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            let is_last = i + 1 == len;
            let mut byte = 0u8;
            for _ in 0..8 {
                byte = (byte << 1) | u8::from(self.read_bit()?);
            }
            self.write_bit(is_last)?; // NAK (release/high) on the last byte
            out.push(byte);
        }
        Ok(out)
    }

    fn write_bit(&self, high: bool) -> Result<()> {
        if high {
            self.release(self.sda)?;
        } else {
            self.drive_low(self.sda)?;
        }
        self.delay();
        self.release(self.scl)?; // clock high: bit sampled by the slave
        self.delay();
        self.drive_low(self.scl)?; // clock low: safe to change SDA next
        self.delay();
        Ok(())
    }

    fn read_bit(&self) -> Result<bool> {
        self.release(self.sda)?;
        self.delay();
        self.release(self.scl)?;
        self.delay();
        let bit = get_value(self.sda)?;
        self.drive_low(self.scl)?;
        self.delay();
        Ok(bit)
    }

    fn delay(&self) {
        sleep(self.half_period);
    }

    /// Open-drain "high": release (input, pulled up by the bus).
    fn release(&self, gpio: u32) -> Result<()> {
        set_direction(gpio, "in")
    }

    /// Open-drain "low": drive (output, value 0).
    fn drive_low(&self, gpio: u32) -> Result<()> {
        set_direction(gpio, "out")?;
        set_value(gpio, false)
    }
}

fn gpio_path(gpio: u32, leaf: &str) -> String {
    format!("/sys/class/gpio/gpio{gpio}/{leaf}")
}

fn export(gpio: u32) -> Result<()> {
    if fs::metadata(format!("/sys/class/gpio/gpio{gpio}")).is_ok() {
        return Ok(());
    }
    match fs::write("/sys/class/gpio/export", gpio.to_string()) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(16) => Ok(()), // EBUSY: already exported
        Err(e) => Err(e.into()),
    }
}

fn set_direction(gpio: u32, dir: &str) -> Result<()> {
    fs::write(gpio_path(gpio, "direction"), dir)?;
    Ok(())
}

fn set_value(gpio: u32, high: bool) -> Result<()> {
    fs::write(gpio_path(gpio, "value"), if high { "1" } else { "0" })?;
    Ok(())
}

fn get_value(gpio: u32) -> Result<bool> {
    let s = fs::read_to_string(gpio_path(gpio, "value"))?;
    Ok(s.trim() == "1")
}

async fn blocking<F, T>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| HwError::Other(e.to_string()))?
}
