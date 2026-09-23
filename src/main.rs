use std::{
    collections::HashMap,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use btleplug::{
    api::{Central, CentralEvent, Manager as _, Peripheral as _, ScanFilter},
    platform::{Manager, Peripheral},
};
use chrono::{DateTime, Utc};
use clap::Parser;
use futures_util::StreamExt;
use serde::Serialize;
use thiserror::Error;
use tokio::time::sleep;

#[derive(Parser, Debug)]
#[command(
    name = "govee5075",
    about = "Read Govee H5075 sensors over Bluetooth Low Energy"
)]
struct Args {
    /// Scan for this many seconds instead of exiting after the first measurement
    #[arg(short, long)]
    seconds: Option<u64>,
    /// Only show a sensor whose name or address contains this text
    #[arg(short, long)]
    filter: Option<String>,
    /// Print newline-delimited JSON instead of a table
    #[arg(long)]
    json: bool,
    /// Exit after the first reading from each sensor
    #[arg(long)]
    once: bool,
    /// Show 0xEC88 manufacturer advertisements and decoding (on stderr)
    #[arg(long)]
    debug: bool,
}

#[derive(Debug, Serialize, Clone)]
struct Reading {
    name: String,
    address: String,
    temperature_c: f32,
    dew_point_c: Option<f32>,
    humidity_percent: f32,
    battery_percent: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    rssi: Option<i16>,
    timestamp: u64,
}

#[derive(Debug, Error)]
enum Error {
    #[error("no Bluetooth adapter found")]
    NoAdapter,
    #[error("Bluetooth error: {0}")]
    Bluetooth(#[from] btleplug::Error),
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let args = Args::parse();
    let manager = Manager::new().await?;
    let adapters = manager.adapters().await?;
    let adapter = adapters.into_iter().next().ok_or(Error::NoAdapter)?;

    if let Some(seconds) = args.seconds {
        eprintln!("Scanning for Govee H5075 sensors for {seconds} seconds...");
    } else {
        eprintln!("Scanning until the first Govee H5075 measurement...");
    }
    let mut events = adapter.events().await?;
    adapter.start_scan(ScanFilter::default()).await?;
    let deadline = sleep(Duration::from_secs(args.seconds.unwrap_or(0)));
    tokio::pin!(deadline);
    let interrupted = tokio::signal::ctrl_c();
    tokio::pin!(interrupted);
    let mut seen: HashMap<String, (Reading, Instant)> = HashMap::new();

    loop {
        tokio::select! {
            _ = &mut deadline, if args.seconds.is_some() => break,
            _ = &mut interrupted => break,
            event = events.next() => {
                let Some(event) = event else { break };
                let CentralEvent::ManufacturerDataAdvertisement { id, manufacturer_data } = event else { continue };
                // The H5075 measurement is under manufacturer ID 0xEC88, not a service UUID.
                let Some(payload) = manufacturer_data.get(&0xEC88) else { continue };
                let peripheral = match adapter.peripheral(&id).await { Ok(p) => p, Err(_) => continue };
                if let Some(reading) = read_sensor(&peripheral, &args, payload).await {
                    let now = Instant::now();
                    if should_print(seen.get(&reading.address), &reading, now, args.once) {
                        seen.insert(reading.address.clone(), (reading.clone(), now));
                        print_reading(&reading, args.json);
                        if args.seconds.is_none() {
                            break;
                        }
                    }
                }
            }
        }
    }
    adapter.stop_scan().await?;
    if seen.is_empty() {
        eprintln!("No H5075 advertisements found. Check macOS Bluetooth permission and that the sensor is nearby.");
    }
    Ok(())
}

// Ignore repeated advertisements carrying the same measurement in a short burst.
// A later advertisement is still printed even when the values have not changed.
fn should_print(
    previous: Option<&(Reading, Instant)>,
    current: &Reading,
    now: Instant,
    once: bool,
) -> bool {
    match previous {
        None => true,
        Some(_) if once => false,
        Some((last, at)) => {
            now.duration_since(*at) >= Duration::from_secs(2)
                || last.temperature_c != current.temperature_c
                || last.humidity_percent != current.humidity_percent
                || last.battery_percent != current.battery_percent
        }
    }
}

async fn read_sensor(peripheral: &Peripheral, args: &Args, payload: &[u8]) -> Option<Reading> {
    let properties = peripheral.properties().await.ok().flatten()?;
    let name = properties.local_name.as_deref();
    // CoreBluetooth does not expose MAC addresses. Its stable per-Mac UUID is
    // available as the peripheral ID; Peripheral::address() is all zeroes.
    let address = peripheral.id().to_string();
    if let Some(filter) = &args.filter {
        let f = filter.to_lowercase();
        if !name.is_some_and(|n| n.to_lowercase().contains(&f))
            && !address.to_lowercase().contains(&f)
        {
            return None;
        }
    }
    let measurement = decode(payload);
    if args.debug {
        eprintln!(
            "[debug] {} ({address}) manufacturer 0xEC88: {payload:02X?} -> {measurement:?}",
            name.unwrap_or("<unnamed>")
        );
    }
    let (temperature_c, humidity, battery) = measurement?;
    let rssi = properties.rssi;
    Some(Reading {
        name: name.unwrap_or("Govee H5075").to_owned(),
        address,
        temperature_c,
        dew_point_c: dew_point_c(temperature_c, humidity),
        humidity_percent: humidity,
        battery_percent: battery,
        rssi,
        timestamp: SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs(),
    })
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

fn decode(data: &[u8]) -> Option<(f32, f32, u8)> {
    // 0xEC88 manufacturer payload: packet type, combined temperature/humidity
    // (24-bit big endian), battery. The low three decimal digits are humidity
    // in tenths of a percent; the remaining digits are temperature in tenths.
    if data.len() < 5 {
        return None;
    }
    let raw = u32::from_be_bytes([0, data[1], data[2], data[3]]);
    let negative = raw & 0x800000 != 0;
    let value = raw & 0x7FFFFF;
    let mut temperature = (value / 1000) as f32 / 10.0;
    if negative {
        temperature = -temperature;
    }
    let humidity = (value % 1000) as f32 / 10.0;
    let battery = data[4];
    if !(-40.0..=85.0).contains(&temperature) || humidity > 100.0 || battery > 100 {
        return None;
    }
    Some((temperature, humidity, battery))
}

#[cfg(test)]
mod tests {
    use super::{decode, dew_point_c, should_print, Reading};
    use std::time::{Duration, Instant};

    #[test]
    fn decodes_h5075_manufacturer_data() {
        // Combined value 235456 means 23.5 °C and 45.6% RH.
        assert_eq!(
            decode(&[0x00, 0x03, 0x97, 0xC0, 87, 0x00]),
            Some((23.5, 45.6, 87))
        );
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
    fn suppresses_only_short_bursts_of_identical_readings() {
        let reading = Reading {
            name: "GVH5075_50EE".into(),
            address: "device-1".into(),
            temperature_c: 21.0,
            dew_point_c: Some(12.6),
            humidity_percent: 58.7,
            battery_percent: 86,
            rssi: Some(-61),
            timestamp: 0,
        };
        let start = Instant::now();
        let previous = (reading.clone(), start);
        let soon = start + Duration::from_millis(100);
        assert!(should_print(None, &reading, start, false));
        assert!(!should_print(Some(&previous), &reading, soon, false));
        assert!(should_print(
            Some(&previous),
            &reading,
            start + Duration::from_secs(2),
            false
        ));
        let mut changed = reading.clone();
        changed.temperature_c = 21.1;
        assert!(should_print(Some(&previous), &changed, soon, false));
        assert!(!should_print(Some(&previous), &changed, soon, true));
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

fn print_reading(r: &Reading, json: bool) {
    if json {
        println!("{}", serde_json::to_string(r).unwrap());
    } else {
        let timestamp = DateTime::<Utc>::from_timestamp(r.timestamp as i64, 0)
            .map(|value| value.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
            .unwrap_or_else(|| r.timestamp.to_string());
        println!(
            "{:<22} {}   {:>7.2} °C   dew point {:>7} °C   {:>6.2}% RH   {:>3}% battery{}",
            r.name,
            timestamp,
            r.temperature_c,
            r.dew_point_c
                .map(|value| format!("{value:.2}"))
                .unwrap_or_else(|| "n/a".into()),
            r.humidity_percent,
            r.battery_percent,
            r.rssi.map(|v| format!("   ({v} dBm)")).unwrap_or_default()
        );
    }
}
