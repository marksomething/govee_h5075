//! Govee H5075 wire formats.
//!
//! The advertisement layout and the GATT history protocol are reverse
//! engineered (see Heckie75/govee-h5075-thermo-hygrometer). This module is
//! deliberately independent of `btleplug` so the codecs can be tested without
//! Bluetooth hardware.

use uuid::Uuid;

/// H5075 measurements live under this manufacturer ID, not a service UUID.
pub const MANUFACTURER_ID: u16 = 0xEC88;

pub const UUID_COMMAND: Uuid = Uuid::from_u128(0x494e_5445_4c4c_495f_524f_434b_535f_2012);
pub const UUID_DATA: Uuid = Uuid::from_u128(0x494e_5445_4c4c_495f_524f_434b_535f_2013);

pub const HISTORY_REQUEST: [u8; 2] = [0x33, 0x01];
pub const HISTORY_ACK: [u8; 2] = [0x33, 0x01];
pub const HISTORY_COMPLETE: [u8; 2] = [0xEE, 0x01];

/// The device stores one reading per minute for 20 days.
pub const HISTORY_MAX_MINUTES: u32 = 28_800;
/// Each data notification carries up to this many 3-byte readings.
pub const HISTORY_RECORDS_PER_MESSAGE: usize = 6;

/// A temperature/humidity pair in the device's native units (tenths).
///
/// Keeping tenths avoids floating-point equality problems when collapsing
/// duplicate advertisements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measurement {
    pub temperature_tenths: i16,
    pub humidity_tenths: u16,
}

impl Measurement {
    pub fn temperature_c(self) -> f64 {
        f64::from(self.temperature_tenths) / 10.0
    }

    pub fn humidity_percent(self) -> f64 {
        f64::from(self.humidity_tenths) / 10.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Advertisement {
    pub measurement: Measurement,
    pub battery_percent: u8,
}

/// Decode the 24-bit packed temperature/humidity value.
///
/// The low three decimal digits are humidity in tenths of a percent; the
/// remaining digits are temperature in tenths. Bit 23 flags a negative
/// temperature.
pub fn decode_combined(value: u32) -> Option<Measurement> {
    let value = value & 0x00FF_FFFF;
    let negative = value & 0x0080_0000 != 0;
    let raw = value & 0x007F_FFFF;
    let magnitude = (raw / 1000) as i32;
    let humidity_tenths = (raw % 1000) as u16;
    let temperature_tenths = if negative { -magnitude } else { magnitude };
    if !(-400..=850).contains(&temperature_tenths) {
        return None;
    }
    Some(Measurement {
        temperature_tenths: temperature_tenths as i16,
        humidity_tenths,
    })
}

/// Decode the H5075 `0xEC88` manufacturer payload:
/// packet type, combined temperature/humidity (24-bit big endian), battery.
///
/// `data[0]` is a packet-type/flags byte. H5075-family parsers decode bytes
/// 1..4 regardless, so unknown packet types are not rejected here. Models that
/// share `0xEC88` but use a different layout (H5074, H505x) are filtered by
/// name in the event loop.
pub fn decode_advertisement(data: &[u8]) -> Option<Advertisement> {
    if data.len() < 5 {
        return None;
    }
    let measurement = decode_combined(u32::from_be_bytes([0, data[1], data[2], data[3]]))?;
    let battery_raw = data[4];
    // Bit 7 is the sensor error flag on some H5075-family firmware.
    if battery_raw & 0x80 != 0 {
        return None;
    }
    let battery_percent = battery_raw & 0x7F;
    if battery_percent > 100 {
        return None;
    }
    Some(Advertisement {
        measurement,
        battery_percent,
    })
}

/// Magnus-Tetens dew point, or `None` when humidity is zero/non-finite.
pub fn dew_point_c(temperature_c: f64, humidity_percent: f64) -> Option<f64> {
    if humidity_percent <= 0.0 || !humidity_percent.is_finite() || !temperature_c.is_finite() {
        return None;
    }
    const A: f64 = 17.27;
    const B: f64 = 237.7;
    let gamma = (humidity_percent / 100.0).ln() + A * temperature_c / (B + temperature_c);
    let dew_point = B * gamma / (A - gamma);
    dew_point.is_finite().then_some(dew_point)
}

pub fn xor_checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |sum, byte| sum ^ byte)
}

/// True when the last byte is the XOR of every preceding byte.
pub fn is_valid_frame(frame: &[u8]) -> bool {
    frame.len() >= 2 && xor_checksum(&frame[..frame.len() - 1]) == frame[frame.len() - 1]
}

/// The 20-byte write that asks the device for stored readings.
///
/// Command, start and end (minutes back, big endian), zero padding to 19 bytes,
/// then an XOR checksum.
pub fn history_request_frame(start_minutes: u16, end_minutes: u16) -> [u8; 20] {
    let mut frame = [0u8; 20];
    frame[0] = HISTORY_REQUEST[0];
    frame[1] = HISTORY_REQUEST[1];
    frame[2..4].copy_from_slice(&start_minutes.to_be_bytes());
    frame[4..6].copy_from_slice(&end_minutes.to_be_bytes());
    frame[19] = xor_checksum(&frame[..19]);
    frame
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlFrame {
    /// `33 01` on the command characteristic: request accepted.
    Ack,
    /// `ee 01` on the command characteristic: transfer finished.
    Complete { expected_messages: u16 },
}

/// Parse and checksum a command-characteristic notification.
pub fn parse_control_frame(data: &[u8]) -> Option<ControlFrame> {
    if data.len() < 6 || !is_valid_frame(data) {
        return None;
    }
    if data[..2] == HISTORY_ACK && data[2] == 0 && data[3] == 0 {
        Some(ControlFrame::Ack)
    } else if data[..2] == HISTORY_COMPLETE {
        Some(ControlFrame::Complete {
            expected_messages: u16::from_be_bytes([data[2], data[3]]),
        })
    } else {
        None
    }
}

/// One decoded history reading within a notification batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchRecord {
    /// Slot within the message; the record's minute is minutes-back minus this.
    pub offset: usize,
    pub measurement: Measurement,
}

/// One history notification: minutes-back of its first reading (big endian),
/// then up to six 3-byte readings; a leading `0xff` marks an empty slot.
pub fn decode_history_message(data: &[u8]) -> Option<(u16, Vec<BatchRecord>)> {
    if data.len() < 2 {
        return None;
    }
    let minutes_back = u16::from_be_bytes([data[0], data[1]]);
    let mut records = Vec::new();
    for offset in 0..HISTORY_RECORDS_PER_MESSAGE {
        let at = 2 + 3 * offset;
        let Some(record) = data.get(at..at + 3) else {
            break;
        };
        if record[0] == 0xFF {
            continue;
        }
        if let Some(measurement) =
            decode_combined(u32::from_be_bytes([0, record[0], record[1], record[2]]))
        {
            records.push(BatchRecord {
                offset,
                measurement,
            });
        }
    }
    Some((minutes_back, records))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        assert_eq!(s.len() % 2, 0);
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn decodes_h5075_manufacturer_data() {
        // Combined value 235456 means 23.5 C and 45.6% RH.
        let advertisement = decode_advertisement(&[0x00, 0x03, 0x97, 0xC0, 87, 0x00]);
        assert_eq!(
            advertisement,
            Some(Advertisement {
                measurement: Measurement {
                    temperature_tenths: 235,
                    humidity_tenths: 456,
                },
                battery_percent: 87,
            })
        );
    }

    #[test]
    fn decodes_captured_manufacturer_data_with_trailing_bytes() {
        // btleplug may report additional bytes after the five measurement bytes.
        let advertisement = decode_advertisement(&[0x00, 0x03, 0x36, 0x9B, 0x56, 0x00, 0x4C, 0x00]);
        assert_eq!(
            advertisement,
            Some(Advertisement {
                measurement: Measurement {
                    temperature_tenths: 210,
                    humidity_tenths: 587,
                },
                battery_percent: 86,
            })
        );
    }

    #[test]
    fn decodes_combined_values() {
        assert_eq!(
            decode_combined(0x0003_97C0),
            Some(Measurement {
                temperature_tenths: 235,
                humidity_tenths: 456,
            })
        );
        // Top bit flags a negative temperature.
        assert_eq!(
            decode_combined(0x81_D4C0),
            Some(Measurement {
                temperature_tenths: -120,
                humidity_tenths: 0,
            })
        );
        // Out of the supported temperature range.
        assert_eq!(decode_combined(0x9999999), None);
        assert_eq!(decode_combined(0x7FFFFF), None);
    }

    #[test]
    fn masks_values_wider_than_24_bits() {
        // Only the low 24 bits are meaningful; the high byte must not leak in.
        assert_eq!(decode_combined(0xAB03_97C0), decode_combined(0x0003_97C0));
    }

    #[test]
    fn rejects_battery_error_flag_and_invalid_battery() {
        // Bit 7 is the error flag.
        assert_eq!(decode_advertisement(&[0x00, 0x03, 0x97, 0xC0, 0xE4]), None);
        // 0x65 = 101%, outside the valid range.
        assert_eq!(decode_advertisement(&[0x00, 0x03, 0x97, 0xC0, 0x65]), None);
        // Battery of exactly 100% is valid.
        assert_eq!(
            decode_advertisement(&[0x00, 0x03, 0x97, 0xC0, 0x64]).map(|a| a.battery_percent),
            Some(100)
        );
    }

    #[test]
    fn builds_history_request_frames() {
        // Captured request for all available data: start 0x7080 (28800 minutes)
        // and end 1 minute back.
        let frame = history_request_frame(28_800, 1);
        assert_eq!(
            frame.to_vec(),
            hex("33017080000100000000000000000000000000c3")
        );
        assert!(is_valid_frame(&frame));

        // The checksum is the XOR of the first 19 bytes.
        assert_eq!(frame[19], xor_checksum(&frame[..19]));
    }

    #[test]
    fn parses_control_frames_only_with_a_valid_checksum() {
        let ack = hex("3301000000000000000000000000000000000032");
        assert_eq!(parse_control_frame(&ack), Some(ControlFrame::Ack));

        let complete = hex("ee010004000000000000000000000000000000eb");
        assert_eq!(
            parse_control_frame(&complete),
            Some(ControlFrame::Complete {
                expected_messages: 4
            })
        );

        let mut corrupt = complete.clone();
        corrupt[19] ^= 0xFF;
        assert_eq!(parse_control_frame(&corrupt), None);

        // A checked 0x33 frame with a non-zero start/end is an echoed request,
        // not an ack.
        let request = history_request_frame(21, 2);
        assert_eq!(parse_control_frame(&request), None);
    }

    #[test]
    fn decodes_history_messages() {
        // Captured batch: minutes-back 21, six valid readings.
        let data = hex("00150371e70375cf0371e70375cf0371e60375ce");
        let (minutes_back, records) = decode_history_message(&data).unwrap();
        assert_eq!(minutes_back, 21);
        assert_eq!(records.len(), 6);
        assert_eq!(records[0].offset, 0);
        assert_eq!(
            records[0].measurement,
            Measurement {
                temperature_tenths: 225,
                humidity_tenths: 767,
            }
        );

        // Empty slots are marked with 0xff.
        let data = hex("00030375cf0375cf0375ceffffffffffffffffffff");
        let (minutes_back, records) = decode_history_message(&data).unwrap();
        assert_eq!(minutes_back, 3);
        assert_eq!(records.len(), 3);

        assert!(decode_history_message(&[0x00]).is_none());
    }

    #[test]
    fn calculates_dew_point_with_magnus_tetens() {
        let dew_point = dew_point_c(20.0, 50.0).unwrap();
        assert!((dew_point - 9.26).abs() < 0.02);
        assert!((dew_point_c(20.0, 100.0).unwrap() - 20.0).abs() < 0.01);
    }

    #[test]
    fn zero_humidity_has_no_finite_dew_point() {
        assert_eq!(dew_point_c(20.0, 0.0), None);
    }
}
