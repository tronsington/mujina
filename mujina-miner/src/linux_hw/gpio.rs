//! Sysfs-backed [`Gpio`]/[`GpioPin`] for a single `periphs-banks`-style
//! GPIO controller.
//!
//! [`Gpio::pin`] takes a `u8` offset, but this SoC's mining-relevant
//! lines (chain enable, PSU enable, ...) are numbered well past 255
//! globally (e.g. GPIO 454). [`SysfsGpio`] binds to one controller's
//! base line number at construction, so callers address pins by their
//! offset *within that controller* (line 454 on the base-411
//! controller is offset 43) rather than the global sysfs number.

use async_trait::async_trait;
use tokio::fs;

use crate::hw_trait::{
    Result,
    gpio::{Gpio, GpioPin, PinMode, PinValue},
};

/// A `periphs-banks`-style sysfs GPIO controller, addressed by its base
/// line number (e.g. 411 for the S19K Pro's main controller).
pub struct SysfsGpio {
    base: u32,
}

impl SysfsGpio {
    /// Bind to the controller whose first line is sysfs GPIO `base`.
    pub fn new(base: u32) -> Self {
        Self { base }
    }
}

#[async_trait]
impl Gpio for SysfsGpio {
    type Pin = SysfsGpioPin;

    async fn pin(&mut self, offset: u8) -> Result<Self::Pin> {
        let number = self.base + u32::from(offset);
        export(number).await?;
        Ok(SysfsGpioPin { number })
    }
}

/// A single exported sysfs GPIO line.
pub struct SysfsGpioPin {
    number: u32,
}

#[async_trait]
impl GpioPin for SysfsGpioPin {
    async fn set_mode(&mut self, mode: PinMode) -> Result<()> {
        let direction = match mode {
            PinMode::Input => "in",
            PinMode::Output => "out",
        };
        fs::write(gpio_path(self.number, "direction"), direction).await?;
        Ok(())
    }

    async fn write(&mut self, value: PinValue) -> Result<()> {
        let value = match value {
            PinValue::Low => "0",
            PinValue::High => "1",
        };
        fs::write(gpio_path(self.number, "value"), value).await?;
        Ok(())
    }

    async fn read(&mut self) -> Result<PinValue> {
        let raw = fs::read_to_string(gpio_path(self.number, "value")).await?;
        Ok(PinValue::from(raw.trim() == "1"))
    }
}

fn gpio_path(number: u32, leaf: &str) -> String {
    format!("/sys/class/gpio/gpio{number}/{leaf}")
}

/// Export a sysfs GPIO line if it isn't already.
async fn export(number: u32) -> Result<()> {
    if fs::metadata(format!("/sys/class/gpio/gpio{number}"))
        .await
        .is_ok()
    {
        return Ok(());
    }
    match fs::write("/sys/class/gpio/export", number.to_string()).await {
        Ok(()) => Ok(()),
        // EBUSY: exported by a racing task between the metadata check
        // and this write. Fine either way.
        Err(e) if e.raw_os_error() == Some(16) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
