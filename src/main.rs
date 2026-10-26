use std::{
    collections::HashMap,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use btleplug::{
    api::{
        Central, CentralEvent, Manager as _, Peripheral as _, ScanFilter, WriteType,
    },
    platform::{Adapter, Manager, Peripheral},
};
use uuid::Uuid;
use chrono::{DateTime, Utc};
use clap::Parser;
use futures_util::StreamExt;
use thiserror::Error;

#[derive(Parser, Debug)]
#[command(
    name = "govee5075",
    about = "Read Govee H5075 sensors over Bluetooth Low Energy"
)]
struct Args {
    /// Keep printing readings until interrupted instead of exiting after the first
    #[arg(short, long)]
    watch: bool,
    /// Connect to a sensor and download the readings stored on the device
    #[arg(short = 'H', long)]
    history: bool,
    /// With --history: oldest reading to fetch, as hhh:mm back from now
    /// (max 480:00 = the 20 days the device stores)
    #[arg(long, default_value = "1:00")]
    start: String,
    /// With --history: newest reading to fetch, as hhh:mm back from now
    #[arg(long, default_value = "0:00")]
    end: String,
    /// Only use a sensor whose name or address contains this text
    #[arg(short, long)]
    filter: Option<String>,
}

#[derive(Debug, Clone)]
struct Reading {
    name: String,
    address: String,
    temperature_c: f32,
    dew_point_c: Option<f32>,
    humidity_percent: f32,
    battery_percent: u8,
    rssi: Option<i16>,
    timestamp: u64,
}

#[derive(Debug, Error)]
enum Error {
    #[error("no Bluetooth adapter found")]
    NoAdapter,
    #[error("no matching H5075 sensor found")]
    NotFound,
    #[error("invalid time range \"{0}\"; use hhh:mm back from now, max 480:00")]
    BadTimeRange(String),
    #[error("device does not expose the Govee history characteristics")]
    MissingCharacteristics,
    #[error("Bluetooth error: {0}")]
    Bluetooth(#[from] btleplug::Error),
}

// The H5075 GATT protocol (reverse engineered, e.g. Heckie75/
// govee-h5075-thermo-hygrometer): commands are written to UUID_COMMAND and the
// device answers on UUID_COMMAND (control) and UUID_DATA (history records).
const UUID_COMMAND: &str = "494e5445-4c4c-495f-524f-434b535f2012";
const UUID_DATA: &str = "494e5445-4c4c-495f-524f-434b535f2013";
// Command [0x33, 0x01] requests history; [0xee, 0x01] announces completion.
const HISTORY_REQUEST: [u8; 2] = [0x33, 0x01];
const HISTORY_COMPLETE: [u8; 2] = [0xee, 0x01];
// The device stores one reading per minute for 20 days.
const HISTORY_MAX_MINUTES: u32 = 28800;
// Each data notification carries up to this many 3-byte readings.
const HISTORY_RECORDS_PER_MESSAGE: usize = 6;
// The device answers only while connected and only sends records as fast as
// the link allows; give up when nothing arrives for this long.
const HISTORY_IDLE_TIMEOUT: Duration = Duration::from_secs(15);
const HISTORY_TOTAL_TIMEOUT: Duration = Duration::from_secs(600);

#[tokio::main]
async fn main() -> Result<(), Error> {
    let args = Args::parse();
    let manager = Manager::new().await?;
    let adapters = manager.adapters().await?;
    let adapter = adapters.into_iter().next().ok_or(Error::NoAdapter)?;

    if args.history {
        return run_history(&adapter, &args).await;
    }

    let mut events = adapter.events().await?;
    adapter.start_scan(ScanFilter::default()).await?;
    // Column headings are cosmetic: stderr keeps stdout a pure record stream
    // that `cut -c` can parse.
    eprintln!("{}", format_heading());
    let interrupted = tokio::signal::ctrl_c();
    tokio::pin!(interrupted);
    let mut seen: HashMap<String, (Reading, Instant)> = HashMap::new();
    let mut found = false;

    loop {
        tokio::select! {
            _ = &mut interrupted => break,
            event = events.next() => {
                let Some(event) = event else { break };
                let CentralEvent::ManufacturerDataAdvertisement { id, manufacturer_data } = event else {
                    continue;
                };
                // The H5075 measurement is under manufacturer ID 0xEC88, not a service UUID.
                let Some(payload) = manufacturer_data.get(&0xEC88) else { continue };
                let Ok(peripheral) = adapter.peripheral(&id).await else { continue };
                let Some(reading) = read_sensor(&peripheral, &args, payload).await else { continue };

                if !args.watch {
                    println!("{}", format_reading(&reading));
                    found = true;
                    break;
                }
                let now = Instant::now();
                if should_print(seen.get(&reading.address), &reading, now) {
                    seen.insert(reading.address.clone(), (reading.clone(), now));
                    println!("{}", format_reading(&reading));
                    found = true;
                }
            }
        }
    }

    adapter.stop_scan().await?;
    if !found {
        eprintln!(
            "no H5075 readings found; check macOS Bluetooth permission and that the sensor is nearby"
        );
    }
    Ok(())
}

async fn read_sensor(peripheral: &Peripheral, args: &Args, payload: &[u8]) -> Option<Reading> {
    let properties = peripheral.properties().await.ok().flatten()?;
    let name = properties.local_name.as_deref();
    // CoreBluetooth does not expose MAC addresses. Its stable per-Mac UUID is
    // available as the peripheral ID; Peripheral::address() is all zeroes.
    let address = peripheral.id().to_string();
    if !label_matches(name, &address, &args.filter) {
        return None;
    }
    let (temperature_c, humidity, battery) = decode(payload)?;
    Some(Reading {
        name: name.unwrap_or("Govee H5075").to_owned(),
        address,
        temperature_c,
        dew_point_c: dew_point_c(temperature_c, humidity),
        humidity_percent: humidity,
        battery_percent: battery,
        rssi: properties.rssi,
        timestamp: SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs(),
    })
}

// Collapse repeated advertisements carrying the same measurement in a short
// burst. A later advertisement is printed even when the values are unchanged.
fn should_print(previous: Option<&(Reading, Instant)>, current: &Reading, now: Instant) -> bool {
    match previous {
        None => true,
        Some((last, at)) => {
            now.duration_since(*at) >= Duration::from_secs(2)
                || last.temperature_c != current.temperature_c
                || last.humidity_percent != current.humidity_percent
                || last.battery_percent != current.battery_percent
        }
    }
}

fn label_matches(name: Option<&str>, address: &str, filter: &Option<String>) -> bool {
    match filter {
        None => true,
        Some(filter) => {
            let f = filter.to_lowercase();
            name.is_some_and(|n| n.to_lowercase().contains(&f))
                || address.to_lowercase().contains(&f)
        }
    }
}

fn dew_point_c(temperature_c: f32, humidity_percent: f32) -> Option<f32> {
    if humidity_percent <= 0.0 {
        return None;
    }
    let a = 17.27;
    let b = 237.7;
    let gamma = (humidity_percent / 100.0).ln() + a * temperature_c / (b + temperature_c);
    Some(b * gamma / (a - gamma))
}

fn decode_combined(value: u32) -> Option<(f32, f32)> {
    // 24-bit combined temperature/humidity value: the low three decimal digits
    // are humidity in tenths of a percent; the remaining digits are temperature
    // in tenths. The top bit flags a negative temperature.
    let negative = value & 0x800000 != 0;
    let raw = value & 0x7FFFFF;
    let mut temperature = (raw / 1000) as f32 / 10.0;
    if negative {
        temperature = -temperature;
    }
    let humidity = (raw % 1000) as f32 / 10.0;
    if !(-40.0..=85.0).contains(&temperature) || humidity > 100.0 {
        return None;
    }
    Some((temperature, humidity))
}

fn decode(data: &[u8]) -> Option<(f32, f32, u8)> {
    // 0xEC88 manufacturer payload: packet type, combined temperature/humidity
    // (24-bit big endian), battery.
    if data.len() < 5 {
        return None;
    }
    let (temperature, humidity) =
        decode_combined(u32::from_be_bytes([0, data[1], data[2], data[3]]))?;
    let battery = data[4];
    if battery > 100 {
        return None;
    }
    Some((temperature, humidity, battery))
}

#[derive(Debug, Clone)]
struct HistoryRecord {
    timestamp: u64,
    temperature_c: f32,
    humidity_percent: f32,
}

/// Parse an `hhh:mm` back-from-now expression (a plain number means minutes).
fn parse_minutes(spec: &str) -> Option<u32> {
    let (hours, minutes) = match spec.trim().split_once(':') {
        Some((hours, minutes)) => (hours.parse::<u32>().ok()?, minutes.parse::<u32>().ok()?),
        None => (0, spec.trim().parse::<u32>().ok()?),
    };
    let total = hours.checked_mul(60)?.checked_add(minutes)?;
    if total > HISTORY_MAX_MINUTES {
        return None;
    }
    Some(total)
}

/// The 20-byte write that asks the device to send its stored readings between
/// `start_minutes` and `end_minutes` ago: command, start, end (both minutes-back
/// in big endian), zero padding to 19 bytes, then a checksum over the rest.
fn history_request_frame(start_minutes: u32, end_minutes: u32) -> Vec<u8> {
    let mut frame = Vec::with_capacity(20);
    frame.extend_from_slice(&HISTORY_REQUEST);
    frame.extend_from_slice(&(start_minutes as u16).to_be_bytes());
    frame.extend_from_slice(&(end_minutes as u16).to_be_bytes());
    frame.resize(19, 0);
    let checksum = frame.iter().fold(0u8, |sum, byte| sum ^ byte);
    frame.push(checksum);
    frame
}

/// One decoded history reading within a notification batch.
#[derive(Debug, Clone, PartialEq)]
struct BatchRecord {
    /// Slot within the message; the record's minute is minutes-back minus this.
    offset: usize,
    temperature_c: f32,
    humidity_percent: f32,
}

/// One history notification: the minutes-back of its first reading (big
/// endian), then up to six 3-byte readings; a leading 0xff marks an empty slot.
fn decode_history_message(data: &[u8]) -> Option<(u16, Vec<BatchRecord>)> {
    if data.len() < 2 {
        return None;
    }
    let minutes_back = u16::from_be_bytes([data[0], data[1]]);
    let mut records = Vec::new();
    for offset in 0..HISTORY_RECORDS_PER_MESSAGE {
        let at = 2 + 3 * offset;
        let Some(record) = data.get(at..at + 3) else { break };
        if record[0] == 0xff {
            continue;
        }
        if let Some((temperature, humidity)) =
            decode_combined(u32::from_be_bytes([0, record[0], record[1], record[2]]))
        {
            records.push(BatchRecord {
                offset,
                temperature_c: temperature,
                humidity_percent: humidity,
            });
        }
    }
    Some((minutes_back, records))
}

// Fixed-width columns, single spaces between fields, so `cut -c` can extract
// any one of them:
//   1-20 name | 22-41 timestamp | 43-48 temperature C | 50-55 dew point C
//   57-62 humidity % | 64-66 battery % | 68-72 RSSI dBm
// History rows reuse the same layout with `n/a` for battery and RSSI.
fn format_heading() -> String {
    format!(
        "{:<20.20} {:<20.20} {:>6} {:>6} {:>6} {:>3} {:>5}",
        "name", "timestamp", "temp_c", "dew_pt", "rh_pct", "bat", "rssi"
    )
}

fn format_timestamp(secs: u64) -> String {
    DateTime::<Utc>::from_timestamp(secs as i64, 0)
        .map(|value| value.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

fn format_reading(r: &Reading) -> String {
    let timestamp = format_timestamp(r.timestamp);
    let dew_point = r
        .dew_point_c
        .map(|value| format!("{value:>6.2}"))
        .unwrap_or_else(|| "   n/a".to_owned());
    let rssi = r
        .rssi
        .map(|value| format!("{value:>5}"))
        .unwrap_or_else(|| "  n/a".to_owned());
    format!(
        "{:<20.20} {:<20.20} {:>6.2} {} {:>6.2} {:>3} {}",
        r.name, timestamp, r.temperature_c, dew_point, r.humidity_percent, r.battery_percent, rssi
    )
}

fn format_history_record(name: &str, r: &HistoryRecord) -> String {
    let dew_point = dew_point_c(r.temperature_c, r.humidity_percent)
        .map(|value| format!("{value:>6.2}"))
        .unwrap_or_else(|| "   n/a".to_owned());
    format!(
        "{:<20.20} {} {:>6.2} {} {:>6.2} n/a   n/a",
        name,
        format_timestamp(r.timestamp),
        r.temperature_c,
        dew_point,
        r.humidity_percent
    )
}

/// Connect to a nearby H5075 and download the readings stored on the device.
async fn run_history(adapter: &Adapter, args: &Args) -> Result<(), Error> {
    let start_minutes =
        parse_minutes(&args.start).ok_or_else(|| Error::BadTimeRange(args.start.clone()))?;
    let end_minutes =
        parse_minutes(&args.end).ok_or_else(|| Error::BadTimeRange(args.end.clone()))?;
    // start must be the older point in time; swap in case the caller mixed them up
    let (start_minutes, end_minutes) = (start_minutes.max(end_minutes), start_minutes.min(end_minutes));

    eprintln!(
        "scanning for {} ...",
        args.filter.as_deref().unwrap_or("an H5075 sensor")
    );
    let mut events = adapter.events().await?;
    adapter.start_scan(ScanFilter::default()).await?;
    let peripheral = loop {
        let Some(event) = events.next().await else {
            adapter.stop_scan().await?;
            return Err(Error::NotFound);
        };
        let CentralEvent::ManufacturerDataAdvertisement { id, manufacturer_data } = event else {
            continue;
        };
        if !manufacturer_data.contains_key(&0xEC88) {
            continue;
        }
        let Ok(candidate) = adapter.peripheral(&id).await else { continue };
        let name = candidate
            .properties()
            .await
            .ok()
            .flatten()
            .and_then(|p| p.local_name);
        let address = candidate.id().to_string();
        if label_matches(name.as_deref(), &address, &args.filter) {
            eprintln!("found {address}, connecting ...");
            break candidate;
        }
    };
    adapter.stop_scan().await?;

    let name = peripheral
        .properties()
        .await
        .ok()
        .flatten()
        .and_then(|p| p.local_name)
        .unwrap_or_else(|| "Govee H5075".to_owned());
    eprintln!("connecting to {name} ...");
    peripheral.connect().await?;
    eprintln!("connected, discovering services ...");
    peripheral.discover_services().await?;
    eprintln!("  {} characteristics found", peripheral.characteristics().len());

    let characteristic = |uuid: &str| {
        peripheral
            .characteristics()
            .iter()
            .find(|c| c.uuid == Uuid::parse_str(uuid).unwrap())
            .cloned()
    };
    let Some(command) = characteristic(UUID_COMMAND) else {
        return Err(Error::MissingCharacteristics);
    };
    let Some(data) = characteristic(UUID_DATA) else {
        return Err(Error::MissingCharacteristics);
    };

    // One subscription stream serves both characteristics; each notification
    // carries the UUID it came from.
    eprintln!("subscribing to notifications ...");
    // Create the stream first, then subscribe, so no notification can be lost
    // in between; the stream stays valid across both subscriptions.
    let mut notifications = peripheral.notifications().await?;
    peripheral.subscribe(&command).await?;
    peripheral.subscribe(&data).await?;
    // Give the device a moment to enable notifications before writing.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let frame = history_request_frame(start_minutes, end_minutes);
    // Record timestamps come from the device as minutes-back, so anchor them
    // to the moment the request was sent.
    let request_started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    peripheral
        .write(&command, &frame, WriteType::WithResponse)
        .await?;
    eprintln!(
        "requested history from {} to {} minutes back; downloading ...",
        start_minutes, end_minutes
    );

    let mut records: Vec<HistoryRecord> = Vec::new();
    let mut messages_received: u16 = 0;
    let mut expected: Option<u16> = None;
    let deadline = Instant::now() + HISTORY_TOTAL_TIMEOUT;
    loop {
        let notification = match tokio::time::timeout(HISTORY_IDLE_TIMEOUT, notifications.next()).await {
            Ok(Some(notification)) => notification,
            // stream ended or nothing arrived for the idle timeout
            _ => break,
        };
        if notification.uuid == data.uuid {
            messages_received = messages_received.wrapping_add(1);
            if let Some((minutes_back, batch)) = decode_history_message(&notification.value) {
                for record in batch {
                    let age = i64::from(minutes_back) - record.offset as i64;
                    let timestamp = request_started.saturating_sub(age.max(0) as u64 * 60);
                    records.push(HistoryRecord {
                        timestamp,
                        temperature_c: record.temperature_c,
                        humidity_percent: record.humidity_percent,
                    });
                }
            }
            if messages_received.is_multiple_of(100) {
                eprintln!("  {} messages received", messages_received);
            }
        } else if notification.uuid == command.uuid {
            // HISTORY_REQUEST here only confirms the transfer started.
            if notification.value.starts_with(&HISTORY_COMPLETE) {
                expected = notification
                    .value
                    .get(2..4)
                    .map(|b| u16::from_be_bytes([b[0], b[1]]));
                break;
            }
        }
        if Instant::now() >= deadline {
            break;
        }
    }

    peripheral.disconnect().await?;

    if records.is_empty() {
        eprintln!("no history records received from {name}");
    } else {
        if let Some(expected) = expected {
            if expected != messages_received {
                eprintln!(
                    "history incomplete: device sent {expected} messages, {messages_received} received"
                );
            }
        }
        eprintln!("received {} records in {messages_received} messages", records.len());
        eprintln!("{}", format_heading());
    }
    records.sort_by_key(|r| r.timestamp);
    for record in &records {
        println!("{}", format_history_record(&name, record));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        decode, decode_combined, decode_history_message, dew_point_c, format_heading,
        format_history_record, format_reading, history_request_frame, parse_minutes, should_print,
        BatchRecord, HistoryRecord, Reading,
    };
    use std::time::{Duration, Instant};

    fn sample_reading() -> Reading {
        Reading {
            name: "GVH5075_50EE".into(),
            address: "device-1".into(),
            temperature_c: 21.0,
            dew_point_c: Some(12.6),
            humidity_percent: 58.7,
            battery_percent: 86,
            rssi: Some(-61),
            timestamp: 0,
        }
    }

    #[test]
    fn decodes_h5075_manufacturer_data() {
        // Combined value 235456 means 23.5 °C and 45.6% RH.
        assert_eq!(
            decode(&[0x00, 0x03, 0x97, 0xC0, 87, 0x00]),
            Some((23.5, 45.6, 87))
        );
    }

    #[test]
    fn decodes_combined_values() {
        assert_eq!(decode_combined(0x000397C0), Some((23.5, 45.6)));
        // Top bit flags a negative temperature.
        assert_eq!(decode_combined(0x81D4C0), Some((-12.0, 0.0)));
        assert_eq!(decode_combined(0x9999999), None);
    }

    #[test]
    fn decodes_captured_manufacturer_data_with_trailing_bytes() {
        // btleplug may report additional bytes after the five measurement bytes.
        assert_eq!(
            decode(&[0x00, 0x03, 0x36, 0x9B, 0x56, 0x00, 0x4C, 0x00]),
            Some((21.0, 58.7, 86))
        );
    }

    #[test]
    fn parses_back_from_now_expressions() {
        assert_eq!(parse_minutes("0:00"), Some(0));
        assert_eq!(parse_minutes("1:30"), Some(90));
        assert_eq!(parse_minutes("480:00"), Some(28800));
        assert_eq!(parse_minutes("90"), Some(90));
        assert_eq!(parse_minutes("481:00"), None);
        assert_eq!(parse_minutes("nonsense"), None);
    }

    #[test]
    fn builds_history_request_frames() {
        // Command 33 01, start/end minutes back in big endian, zero padding,
        // XOR checksum over the first 19 bytes.
        let frame = history_request_frame(0x1234, 0x0000);
        assert_eq!(frame.len(), 20);
        assert_eq!(&frame[..6], &[0x33, 0x01, 0x12, 0x34, 0x00, 0x00]);
        assert!(frame[6..19].iter().all(|b| *b == 0));
        let checksum = frame[..19].iter().fold(0u8, |sum, b| sum ^ b);
        assert_eq!(frame[19], checksum);
    }

    #[test]
    fn decodes_history_messages() {
        // minutes-back 9 (0x0009), one 0xff empty slot, then 23.5/45.6.
        let mut data = vec![0x00, 0x09, 0xff, 0xff, 0xff, 0x03, 0x97, 0xC0];
        data.extend(std::iter::repeat(0xff).take(3 * 5));
        let (minutes_back, records) = decode_history_message(&data).unwrap();
        assert_eq!(minutes_back, 9);
        assert_eq!(
            records,
            vec![BatchRecord {
                offset: 1,
                temperature_c: 23.5,
                humidity_percent: 45.6,
            }]
        );
        assert!(decode_history_message(&[0x00]).is_none());
    }

    #[test]
    fn formats_history_records_like_live_readings() {
        let record = HistoryRecord {
            timestamp: 0,
            temperature_c: 21.0,
            humidity_percent: 58.7,
        };
        assert_eq!(
            format_history_record("GVH5075_50EE", &record),
            "GVH5075_50EE         1970-01-01T00:00:00Z  21.00  12.60  58.70 n/a   n/a"
        );
    }

    #[test]
    fn suppresses_only_short_bursts_of_identical_readings() {
        let reading = sample_reading();
        let start = Instant::now();
        let previous = (reading.clone(), start);
        let soon = start + Duration::from_millis(100);
        assert!(should_print(None, &reading, start));
        assert!(!should_print(Some(&previous), &reading, soon));
        assert!(should_print(
            Some(&previous),
            &reading,
            start + Duration::from_secs(2)
        ));
        let mut changed = reading.clone();
        changed.temperature_c = 21.1;
        assert!(should_print(Some(&previous), &changed, soon));
    }

    #[test]
    fn reading_uses_stable_cut_columns() {
        let line = format_reading(&sample_reading());
        assert_eq!(line.len(), 72);
        assert_eq!(&line[0..20], "GVH5075_50EE        ");
        assert_eq!(&line[21..41], "1970-01-01T00:00:00Z");
        assert_eq!(&line[42..48], " 21.00");
        assert_eq!(&line[49..55], " 12.60");
        assert_eq!(&line[56..62], " 58.70");
        assert_eq!(&line[63..66], " 86");
        assert_eq!(&line[67..72], "  -61");
    }

    #[test]
    fn heading_reuses_the_data_columns() {
        let line = format_heading();
        assert_eq!(line.len(), 72);
        assert_eq!(&line[0..20], "name                ");
        assert_eq!(&line[21..41], "timestamp           ");
        assert_eq!(&line[42..48], "temp_c");
        assert_eq!(&line[49..55], "dew_pt");
        assert_eq!(&line[56..62], "rh_pct");
        assert_eq!(&line[63..66], "bat");
        assert_eq!(&line[67..72], " rssi");
    }

    #[test]
    fn missing_values_keep_their_column_width() {
        let mut reading = sample_reading();
        reading.dew_point_c = None;
        reading.rssi = None;
        let line = format_reading(&reading);
        assert_eq!(line.len(), 72);
        assert_eq!(&line[49..55], "   n/a");
        assert_eq!(&line[67..72], "  n/a");
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
