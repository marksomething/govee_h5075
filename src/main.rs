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
    /// Only show a sensor whose name or address contains this text
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
    #[error("Bluetooth error: {0}")]
    Bluetooth(#[from] btleplug::Error),
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let args = Args::parse();
    let manager = Manager::new().await?;
    let adapters = manager.adapters().await?;
    let adapter = adapters.into_iter().next().ok_or(Error::NoAdapter)?;

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
    if let Some(filter) = &args.filter {
        let f = filter.to_lowercase();
        if !name.is_some_and(|n| n.to_lowercase().contains(&f))
            && !address.to_lowercase().contains(&f)
        {
            return None;
        }
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

// Fixed-width columns, single spaces between fields, so `cut -c` can extract
// any one of them:
//   1-20 name | 22-41 timestamp | 43-48 temperature C | 50-55 dew point C
//   57-62 humidity % | 64-66 battery % | 68-72 RSSI dBm
fn format_heading() -> String {
    format!(
        "{:<20.20} {:<20.20} {:>6} {:>6} {:>6} {:>3} {:>5}",
        "name", "timestamp", "temp_c", "dew_pt", "rh_pct", "bat", "rssi"
    )
}

fn format_reading(r: &Reading) -> String {
    let timestamp = DateTime::<Utc>::from_timestamp(r.timestamp as i64, 0)
        .map(|value| value.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default();
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

#[cfg(test)]
mod tests {
    use super::{decode, dew_point_c, format_heading, format_reading, should_print, Reading};
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
    fn decodes_captured_manufacturer_data_with_trailing_bytes() {
        // btleplug may report additional bytes after the five measurement bytes.
        assert_eq!(
            decode(&[0x00, 0x03, 0x36, 0x9B, 0x56, 0x00, 0x4C, 0x00]),
            Some((21.0, 58.7, 86))
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
