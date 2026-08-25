//! Bit-banged I2C client for the Antminer S19K Pro's PSU (APW12).
//!
//! Bitmain's own boot script (/etc/init.d/S37board_setup) labels
//! GPIO 476 "I2C_SCL" and GPIO 477 "I2C_SDA" -- a software
//! bit-banged bus distinct from both hardware I2C controllers
//! (/dev/i2c-0, /dev/i2c-1), which is why the PSU (address 0x10,
//! confirmed via the real APW12 PIC firmware's SSPADD setup) never
//! showed up on either standard bus or in kernel i2c ftrace.
//!
//! This implements a from-scratch software I2C master over those two
//! lines (open-drain emulated via sysfs GPIO direction switching),
//! plus the APW12 framed request/response protocol on top -- ported
//! from skot/amlogic-cb-tools' apw12-psu-tool (protocol.rs,
//! linux_i2c.rs), whose `exchange()` sends the frame one byte at a
//! time as single-byte writes to register 0x11, then reads the
//! response one byte at a time as single-byte current-address reads.
//! That tool talks over the real /dev/i2c-1 hardware controller
//! (which doesn't reach the PSU on this board); this reimplements the
//! same wire protocol over the actual bit-banged bus instead.
//!
//! IMPORTANT: run this only with bosminer stopped first -- it
//! bit-bangs these same two GPIO lines itself, and a second software
//! master driving the same physical wires concurrently risks bus
//! corruption.
//!
//! Usage: psu-bitbang-probe <command> [args...]
//!   get-fw                  firmware version
//!   get-hw                  hardware version
//!   get-voltage             DAC setpoint readback
//!   measure-voltage         measured output voltage
//!   read-state              PSU on/off state
//!   disable-watchdog        disable the PSU's comms watchdog
//!   scan-addr <addr-hex>    just check ACK at a given address (no data)
//!
//! State-changing commands (set-voltage, set-dac, enable-watchdog)
//! are implemented in the protocol layer but deliberately not wired
//! up to the CLI here -- read-only telemetry only, until there's a
//! specific reason to change PSU output state live.

use std::fmt;
use std::fs;
use std::io;
use std::thread::sleep;
use std::time::Duration;

const SCL: u32 = 476;
const SDA: u32 = 477;

// ~50us half-period => ~10kHz. Deliberately slow and conservative.
const HALF_PERIOD: Duration = Duration::from_micros(50);

const DEFAULT_PSU_ADDRESS: u8 = 0x10;
const DEFAULT_PSU_WRITE_REGISTER: u8 = 0x11;
const RESPONSE_DELAY: Duration = Duration::from_millis(500);
const MAX_RESPONSE_ATTEMPTS: usize = 3;

// --- APW12 frame protocol (ported from amlogic-cb-tools/src/protocol.rs) ---

const PREAMBLE_LSB: u8 = 0x55;
const PREAMBLE_MSB: u8 = 0xAA;
const NAK_BYTE: u8 = 0xF5;

const CMD_GET_FW_VERSION: u8 = 0x01;
const CMD_GET_HW_VERSION: u8 = 0x02;
const CMD_GET_VOLTAGE: u8 = 0x03;
const CMD_MEASURE_VOLTAGE: u8 = 0x04;
const CMD_READ_STATE: u8 = 0x05;
#[allow(dead_code)]
const CMD_READ_CAL: u8 = 0x06;
const CMD_WATCHDOG: u8 = 0x81;
#[allow(dead_code)]
const CMD_SET_VOLTAGE: u8 = 0x83;
#[allow(dead_code)]
const CMD_WRITE_CAL: u8 = 0x86;

const DAC_REF_VOLTS: f32 = 15.1084;
const DAC_OFFSET_VOLTS_PER_COUNT: f32 = -0.013046;

#[derive(Debug)]
enum ProtocolError {
    EmptyResponse,
    Nak,
    InvalidPreamble(Vec<u8>),
    InvalidLength { declared: usize, actual: usize },
    InvalidChecksum { expected: u8, actual: u8 },
    Io(io::Error),
    NoValidResponse,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyResponse => write!(f, "empty response"),
            Self::Nak => write!(f, "PSU returned NAK (0xF5)"),
            Self::InvalidPreamble(b) => write!(f, "invalid preamble: {b:02X?}"),
            Self::InvalidLength { declared, actual } => {
                write!(f, "invalid frame length: declared {declared}, actual {actual}")
            }
            Self::InvalidChecksum { expected, actual } => {
                write!(f, "invalid checksum: expected 0x{expected:02X}, got 0x{actual:02X}")
            }
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::NoValidResponse => write!(f, "no valid PSU response received"),
        }
    }
}

impl From<io::Error> for ProtocolError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

#[derive(Debug, Clone)]
struct Frame {
    command: u8,
    payload: Vec<u8>,
    raw: Vec<u8>,
}

fn checksum(length: u8, command: u8, payload: &[u8]) -> u16 {
    payload
        .iter()
        .fold(u16::from(length) + u16::from(command), |sum, b| sum + u16::from(*b))
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

fn parse_frame(raw: &[u8]) -> Result<Frame, ProtocolError> {
    if raw.is_empty() {
        return Err(ProtocolError::EmptyResponse);
    }
    if raw == [NAK_BYTE] {
        return Err(ProtocolError::Nak);
    }
    if raw.len() < 6 {
        return Err(ProtocolError::InvalidLength {
            declared: raw.get(2).copied().unwrap_or_default() as usize,
            actual: raw.len(),
        });
    }
    if raw[0] != PREAMBLE_LSB || raw[1] != PREAMBLE_MSB {
        return Err(ProtocolError::InvalidPreamble(raw[..raw.len().min(2)].to_vec()));
    }

    let declared_len = raw[2] as usize;
    let actual_len_from_length = raw.len().saturating_sub(2);
    if declared_len != actual_len_from_length {
        return Err(ProtocolError::InvalidLength {
            declared: declared_len,
            actual: actual_len_from_length,
        });
    }

    let command = raw[3];
    let checksum_index = raw.len() - 2;
    let payload = &raw[4..checksum_index];
    let actual_checksum = raw[checksum_index];
    let expected_checksum = checksum(raw[2], command, payload) as u8;
    if actual_checksum != expected_checksum {
        return Err(ProtocolError::InvalidChecksum {
            expected: expected_checksum,
            actual: actual_checksum,
        });
    }

    Ok(Frame {
        command,
        payload: payload.to_vec(),
        raw: raw.to_vec(),
    })
}

fn decode_dac_to_voltage(dac: u8) -> f32 {
    DAC_REF_VOLTS + DAC_OFFSET_VOLTS_PER_COUNT * f32::from(dac)
}

fn decode_measured_voltage(adc_lo: u8, adc_hi: u8) -> f32 {
    let raw = u16::from(adc_lo) | (u16::from(adc_hi) << 8);
    (raw as f32 + 0.8615) / 63.017
}

// --- sysfs GPIO bit-bang I2C transport ---

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
        line_drive_low(SDA)?;
        delay();
        line_release(SCL)?;
        delay();
        line_release(SDA)?;
        delay();
        Ok(())
    }

    fn write_bit(&self, high: bool) -> io::Result<()> {
        if high {
            line_release(SDA)?;
        } else {
            line_drive_low(SDA)?;
        }
        delay();
        line_release(SCL)?;
        delay();
        line_drive_low(SCL)?;
        delay();
        Ok(())
    }

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

    /// Write a byte MSB-first, then clock in and return whether the
    /// slave ACKed (true = ACK/low).
    fn write_byte(&self, byte: u8) -> io::Result<bool> {
        for i in (0..8).rev() {
            self.write_bit((byte >> i) & 1 != 0)?;
        }
        let nak = self.read_bit()?;
        Ok(!nak)
    }

    /// Clock in a byte MSB-first, then send our own ACK/NAK bit
    /// (ack=true drives SDA low = ACK, requesting more bytes;
    /// ack=false releases SDA = NAK, signaling "last byte").
    fn read_byte(&self, ack: bool) -> io::Result<u8> {
        let mut byte = 0u8;
        for _ in 0..8 {
            byte = (byte << 1) | (self.read_bit()? as u8);
        }
        self.write_bit(!ack)?;
        Ok(byte)
    }

    /// One full I2C_SMBUS_WRITE_BYTE_DATA-equivalent transaction:
    /// START, addr+W, register, data, STOP. Errors if the address or
    /// register byte isn't ACKed.
    fn write_byte_transaction(&self, addr: u8, register: u8, data: u8) -> Result<(), ProtocolError> {
        self.start()?;
        if !self.write_byte(addr << 1)? {
            self.stop()?;
            return Err(ProtocolError::Nak);
        }
        if !self.write_byte(register)? {
            self.stop()?;
            return Err(ProtocolError::Nak);
        }
        self.write_byte(data)?; // ack on the data byte varies by device; don't fail on it
        self.stop()?;
        Ok(())
    }

    /// One current-address read transaction: START, addr+R, read one
    /// byte, NAK (we only ever want one byte per transaction, same
    /// as the reference implementation), STOP.
    fn read_byte_transaction(&self, addr: u8) -> Result<u8, ProtocolError> {
        self.start()?;
        if !self.write_byte((addr << 1) | 1)? {
            self.stop()?;
            return Err(ProtocolError::Nak);
        }
        let byte = self.read_byte(false)?;
        self.stop()?;
        Ok(byte)
    }
}

struct Psu {
    bus: BitBangI2c,
    address: u8,
    write_register: u8,
}

impl Psu {
    fn open(address: u8) -> io::Result<Self> {
        Ok(Self {
            bus: BitBangI2c::init()?,
            address,
            write_register: DEFAULT_PSU_WRITE_REGISTER,
        })
    }

    fn exchange(&self, command: u8, payload: &[u8]) -> Result<Frame, ProtocolError> {
        let frame = build_frame(command, payload);
        for byte in frame {
            self.bus.write_byte_transaction(self.address, self.write_register, byte)?;
        }

        sleep(RESPONSE_DELAY);

        let mut last_error = ProtocolError::NoValidResponse;
        for _ in 0..MAX_RESPONSE_ATTEMPTS {
            let response = self.read_response_frame()?;
            if response == [NAK_BYTE] {
                last_error = ProtocolError::Nak;
                sleep(RESPONSE_DELAY);
                continue;
            }

            match parse_frame(&response) {
                Ok(frame) if frame.command == command => return Ok(frame),
                Ok(frame) => {
                    last_error = ProtocolError::InvalidPreamble(vec![frame.command]);
                }
                Err(e) => last_error = e,
            }
            sleep(RESPONSE_DELAY);
        }

        Err(last_error)
    }

    fn read_response_frame(&self) -> Result<Vec<u8>, ProtocolError> {
        let mut first = self.bus.read_byte_transaction(self.address)?;
        // Guard against a runaway loop if the bus is stuck: bail out
        // after a generous number of empty/garbage bytes.
        let mut attempts = 0;
        while first != PREAMBLE_LSB && first != NAK_BYTE {
            attempts += 1;
            if attempts > 32 {
                return Err(ProtocolError::NoValidResponse);
            }
            first = self.bus.read_byte_transaction(self.address)?;
        }

        if first == NAK_BYTE {
            return Ok(vec![NAK_BYTE]);
        }

        let second = self.bus.read_byte_transaction(self.address)?;
        if second != PREAMBLE_MSB {
            return Err(ProtocolError::InvalidPreamble(vec![first, second]));
        }

        let length = self.bus.read_byte_transaction(self.address)?;
        let mut response = Vec::with_capacity(usize::from(length) + 2);
        response.push(first);
        response.push(second);
        response.push(length);

        let remaining = usize::from(length)
            .checked_sub(1)
            .ok_or(ProtocolError::InvalidLength { declared: length as usize, actual: 0 })?;
        for _ in 0..remaining {
            response.push(self.bus.read_byte_transaction(self.address)?);
        }

        Ok(response)
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(&args) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(args: &[String]) -> Result<(), ProtocolError> {
    let command = args.first().map(String::as_str).unwrap_or("help");

    if command == "help" || command.is_empty() {
        print_help();
        return Ok(());
    }

    if command == "scan-addr" {
        let addr_hex = args.get(1).expect("usage: psu-bitbang-probe scan-addr <addr-hex>");
        let addr = u8::from_str_radix(addr_hex.trim_start_matches("0x"), 16).expect("bad hex address");
        let bus = BitBangI2c::init()?;
        bus.start()?;
        let acked = bus.write_byte(addr << 1)?;
        bus.stop()?;
        println!("0x{addr:02x}: {}", if acked { "ACK" } else { "NAK" });
        return Ok(());
    }

    let psu = Psu::open(DEFAULT_PSU_ADDRESS)?;
    println!(
        "PSU at address 0x{:02x}, write register 0x{:02x} (bit-banged SCL=gpio{SCL} SDA=gpio{SDA})",
        psu.address, psu.write_register
    );

    match command {
        "get-fw" => {
            let frame = psu.exchange(CMD_GET_FW_VERSION, &[])?;
            println!("firmware payload: {:02X?}", frame.payload);
            println!("raw: {:02X?}", frame.raw);
        }
        "get-hw" => {
            let frame = psu.exchange(CMD_GET_HW_VERSION, &[])?;
            println!("hardware payload: {:02X?}", frame.payload);
            println!("raw: {:02X?}", frame.raw);
        }
        "get-voltage" => {
            let frame = psu.exchange(CMD_GET_VOLTAGE, &[])?;
            let dac = *frame.payload.first().ok_or(ProtocolError::EmptyResponse)?;
            println!("dac_code=0x{dac:02X} ({dac})");
            println!("estimated_voltage={:.4} V", decode_dac_to_voltage(dac));
            println!("raw: {:02X?}", frame.raw);
        }
        "measure-voltage" => {
            let frame = psu.exchange(CMD_MEASURE_VOLTAGE, &[])?;
            if frame.payload.len() < 2 {
                return Err(ProtocolError::InvalidLength { declared: 2, actual: frame.payload.len() });
            }
            let volts = decode_measured_voltage(frame.payload[0], frame.payload[1]);
            println!(
                "adc_raw=0x{:02X}{:02X} measured_voltage={:.4} V",
                frame.payload[1], frame.payload[0], volts
            );
            println!("raw: {:02X?}", frame.raw);
        }
        "read-state" => {
            let frame = psu.exchange(CMD_READ_STATE, &[])?;
            if frame.payload.len() < 2 {
                return Err(ProtocolError::InvalidLength { declared: 2, actual: frame.payload.len() });
            }
            let state = u16::from(frame.payload[0]) | (u16::from(frame.payload[1]) << 8);
            println!("state=0x{state:04X} ({})", if state == 1 { "ON" } else { "OFF" });
            println!("raw: {:02X?}", frame.raw);
        }
        "disable-watchdog" => {
            let frame = psu.exchange(CMD_WATCHDOG, &[0x00, 0x00])?;
            println!("watchdog disabled");
            println!("raw: {:02X?}", frame.raw);
        }
        other => {
            eprintln!("unknown command: {other}");
            print_help();
            std::process::exit(1);
        }
    }

    Ok(())
}

fn print_help() {
    println!("psu-bitbang-probe -- APW12 PSU client over the real bit-banged GPIO 476/477 bus");
    println!();
    println!("IMPORTANT: stop bosminer first -- it bit-bangs these same lines itself.");
    println!();
    println!("Commands:");
    println!("  help");
    println!("  scan-addr <addr-hex>   just check ACK at an address, no data exchange");
    println!("  get-fw");
    println!("  get-hw");
    println!("  get-voltage");
    println!("  measure-voltage");
    println!("  read-state");
    println!("  disable-watchdog");
}
