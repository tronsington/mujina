//! [`I2c`] backed by a real Linux `/dev/i2c-N` character device.
//!
//! Standard `i2c-dev` usage: `ioctl(I2C_SLAVE, addr)` selects the
//! target for subsequent `read`/`write` on the same file descriptor.
//! That per-descriptor state is shared across `dup`'d descriptors
//! (which is what `File::try_clone` produces), so re-selecting the
//! address before each transfer is safe even though every call clones
//! a fresh handle to hand into [`tokio::task::spawn_blocking`].

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;

use async_trait::async_trait;
use nix::libc;

use crate::hw_trait::{
    HwError, Result,
    i2c::{I2c, I2cError},
};

const I2C_SLAVE: libc::c_ulong = 0x0703;

/// A real Linux I2C bus, e.g. `/dev/i2c-1`.
pub struct LinuxI2c {
    file: File,
}

impl LinuxI2c {
    pub fn open(path: &str) -> std::io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(Self { file })
    }

    /// Clone the handle for use inside a blocking task, since the trait's
    /// `&mut self` can't cross an `.await` into `spawn_blocking`.
    fn cloned_handle(&self) -> Result<File> {
        self.file.try_clone().map_err(HwError::Io)
    }
}

#[async_trait]
impl I2c for LinuxI2c {
    async fn write(&mut self, addr: u8, data: &[u8]) -> Result<()> {
        let mut handle = self.cloned_handle()?;
        let data = data.to_vec();
        blocking(move || {
            select_address(&handle, addr)?;
            handle
                .write_all(&data)
                .map_err(|e| HwError::I2c(classify(addr, e)))
        })
        .await
    }

    async fn read(&mut self, addr: u8, buffer: &mut [u8]) -> Result<()> {
        let mut handle = self.cloned_handle()?;
        let len = buffer.len();
        let read = blocking(move || {
            select_address(&handle, addr)?;
            let mut buf = vec![0u8; len];
            handle
                .read_exact(&mut buf)
                .map_err(|e| HwError::I2c(classify(addr, e)))?;
            Ok(buf)
        })
        .await?;
        buffer.copy_from_slice(&read);
        Ok(())
    }

    async fn write_read(&mut self, addr: u8, write: &[u8], read: &mut [u8]) -> Result<()> {
        // Two separate transactions (STOP between them), not a true
        // repeated START. Fine for the register-addressed devices on
        // this bus (TMP75, EEPROM) -- a true repeated start would need
        // the combined I2C_RDWR ioctl instead of plain write()/read().
        self.write(addr, write).await?;
        self.read(addr, read).await
    }

    async fn set_frequency(&mut self, _hz: u32) -> Result<()> {
        // This bus's clock is fixed by the kernel driver/devicetree,
        // not runtime-adjustable via i2c-dev. Callers that always set
        // a frequency before use (matching the tunneled-transport
        // boards) still work; the request is just a no-op here.
        Ok(())
    }
}

fn select_address(file: &File, addr: u8) -> Result<()> {
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), I2C_SLAVE as _, libc::c_ulong::from(addr)) };
    if rc < 0 {
        return Err(HwError::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

fn classify(addr: u8, e: std::io::Error) -> I2cError {
    match e.raw_os_error() {
        // ENXIO/EREMOTEIO: no device acknowledged the address.
        Some(6) | Some(121) => I2cError::NoAck(addr),
        _ => I2cError::Other(e.to_string()),
    }
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
