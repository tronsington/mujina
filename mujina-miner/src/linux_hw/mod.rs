//! Native Linux hardware backends for [`crate::hw_trait`].
//!
//! Boards that expose GPIO/I2C directly from an SoC to the host Linux
//! kernel (`/sys/class/gpio`, `/dev/i2c-N`) rather than tunneling
//! through a USB co-processor (as `bitaxe.rs`/`emberone00.rs` do)
//! implement `hw_trait` against these instead of a management-protocol
//! transport.

pub mod bitbang_i2c;
pub mod gpio;
pub mod i2c;

pub use bitbang_i2c::BitBangI2c;
pub use gpio::{SysfsGpio, SysfsGpioPin};
pub use i2c::LinuxI2c;
