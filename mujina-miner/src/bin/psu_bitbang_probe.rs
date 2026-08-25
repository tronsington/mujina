//! Bit-banged I2C probe for the Antminer S19K Pro's PSU bus.
//!
//! Standard kernel I2C (/dev/i2c-0, /dev/i2c-1) never shows any
//! traffic to the PSU (confirmed via ftrace across multiple capture
//! windows up to 5 minutes). Bitmain's own boot script
//! (/etc/init.d/S37board_setup) labels GPIO 476 "I2C_SCL" and GPIO
//! 477 "I2C_SDA" -- a bus distinct from both hardware I2C
//! controllers. This is a from-scratch software (bit-banged) I2C
//! master over those two lines via sysfs GPIO, to test whether the
//! PSU (address 0x10, per the real APW12 PIC firmware's SSPADD
//! setup) ACKs there.
//!
//! Usage: psu-bitbang-probe [address-hex, default 10]
//!
//! Only ever does a START + address-byte(write) + STOP -- checks for
//! an ACK and immediately stops. No data is written to the PSU.

use std::fs;
use std::io;
use std::thread::sleep;
use std::time::Duration;

const SCL: u32 = 476;
const SDA: u32 = 477;

// ~50us half-period => ~10kHz. Deliberately slow and conservative
// for a first attempt (real bus is probably faster, but slow is
// always a valid, if inefficient, I2C bus speed).
const HALF_PERIOD: Duration = Duration::from_micros(50);

fn gpio_path(gpio: u32, leaf: &str) -> String {
    format!("/sys/class/gpio/gpio{gpio}/{leaf}")
}

fn export(gpio: u32) -> io::Result<()> {
    if fs::metadata(format!("/sys/class/gpio/gpio{gpio}")).is_ok() {
        return Ok(());
    }
    match fs::write("/sys/class/gpio/export", gpio.to_string()) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(16) => Ok(()), // EBUSY: already exported
        Err(e) => Err(e),
    }
}

fn set_direction(gpio: u32, dir: &str) -> io::Result<()> {
    fs::write(gpio_path(gpio, "direction"), dir)
}

fn set_value(gpio: u32, high: bool) -> io::Result<()> {
    fs::write(gpio_path(gpio, "value"), if high { "1" } else { "0" })
}

fn get_value(gpio: u32) -> io::Result<bool> {
    let s = fs::read_to_string(gpio_path(gpio, "value"))?;
    Ok(s.trim() == "1")
}

/// Open-drain emulation: "high" = release (input, pulled up by the
/// bus); "low" = drive low (output, value 0).
fn line_release(gpio: u32) -> io::Result<()> {
    set_direction(gpio, "in")
}

fn line_drive_low(gpio: u32) -> io::Result<()> {
    set_direction(gpio, "out")?;
    set_value(gpio, false)
}

fn delay() {
    sleep(HALF_PERIOD);
}

struct BitBangI2c;

impl BitBangI2c {
    fn init() -> io::Result<Self> {
        export(SCL)?;
        export(SDA)?;
        line_release(SCL)?;
        line_release(SDA)?;
        delay();
        Ok(Self)
    }

    fn start(&self) -> io::Result<()> {
        // Both idle high, then SDA falls while SCL is high.
        line_release(SDA)?;
        line_release(SCL)?;
        delay();
        line_drive_low(SDA)?;
        delay();
        line_drive_low(SCL)?;
        delay();
        Ok(())
    }

    fn stop(&self) -> io::Result<()> {
        // SDA rises while SCL is high.
        line_drive_low(SDA)?;
        delay();
        line_release(SCL)?;
        delay();
        line_release(SDA)?;
        delay();
        Ok(())
    }

    /// Clock out one bit (MSB-first convention handled by caller).
    fn write_bit(&self, high: bool) -> io::Result<()> {
        if high {
            line_release(SDA)?;
        } else {
            line_drive_low(SDA)?;
        }
        delay();
        line_release(SCL)?; // clock high: bit is sampled by slave
        delay();
        line_drive_low(SCL)?; // clock low: safe to change SDA next
        delay();
        Ok(())
    }

    /// Clock in one bit, releasing SDA first (so the slave can drive it).
    fn read_bit(&self) -> io::Result<bool> {
        line_release(SDA)?;
        delay();
        line_release(SCL)?;
        delay();
        let bit = get_value(SDA)?;
        line_drive_low(SCL)?;
        delay();
        Ok(bit)
    }

    /// Write a byte MSB-first, then clock in and return the ACK bit
    /// (true = ACK/low, false = NAK/high).
    fn write_byte(&self, byte: u8) -> io::Result<bool> {
        for i in (0..8).rev() {
            self.write_bit((byte >> i) & 1 != 0)?;
        }
        let nak = self.read_bit()?;
        Ok(!nak)
    }
}

fn main() -> io::Result<()> {
    let addr: u8 = std::env::args()
        .nth(1)
        .map(|s| u8::from_str_radix(s.trim_start_matches("0x"), 16).expect("bad hex address"))
        .unwrap_or(0x10);

    println!("Bit-banged I2C probe on SCL=gpio{SCL} SDA=gpio{SDA}, address 0x{addr:02x}");

    let bus = BitBangI2c::init()?;

    println!("Sending START + address (write) + STOP, checking for ACK...");
    bus.start()?;
    let write_addr_byte = addr << 1; // R/W bit = 0 (write)
    let acked = bus.write_byte(write_addr_byte)?;
    bus.stop()?;

    if acked {
        println!("ACK received at address 0x{addr:02x} -- a device is present and responding.");
    } else {
        println!("NAK (no response) at address 0x{addr:02x}.");
    }

    Ok(())
}
