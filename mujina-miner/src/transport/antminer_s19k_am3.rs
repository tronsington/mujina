//! Antminer S19K Pro (AM3/Amlogic control board) virtual transport.
//!
//! Like the CPU miner, this board isn't a USB peripheral discovered
//! at runtime -- it *is* the host control board, so its "connection"
//! event is synthesized once at daemon startup from configuration,
//! not discovered from hardware.

/// Transport events for the S19K Pro virtual device.
#[derive(Debug)]
pub enum TransportEvent {
    /// The board "connected" (enabled via environment at startup).
    DeviceConnected(DeviceInfo),

    /// The board "disconnected". Not currently triggered -- this
    /// board has no real hotplug -- but kept for symmetry with the
    /// other transports and for a future graceful-shutdown path.
    DeviceDisconnected { device_id: String },
}

/// Information about the S19K Pro virtual device.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// Unique identifier for this virtual device.
    pub device_id: String,
}
