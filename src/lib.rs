//! Govee H5075 BLE decoding and history transfer logic.
//!
//! The binary lives in `main.rs`; everything here is Bluetooth-independent so
//! that protocol and formatting behaviour can be tested without hardware.

pub mod cli;
pub mod format;
pub mod history;
pub mod protocol;
