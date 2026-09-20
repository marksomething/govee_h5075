use std::{
    collections::HashMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use btleplug::{
    api::{bleuuid::BleUuid, Central, CentralEvent, Manager as _, Peripheral as _, ScanFilter},
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
}

#[derive(Debug, Serialize, Clone)]
struct Reading {
    name: String,
    address: String,
    temperature_c: f32,
    temperature_f: f32,
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
    let mut seen: HashMap<String, Reading> = HashMap::new();

    loop {
        tokio::select! {
            _ = &mut deadline, if args.seconds.is_some() => break,
            _ = &mut interrupted => break,
            event = events.next() => {
                let Some(event) = event else { break };
                let (id, advertisement) = match event {
                    CentralEvent::DeviceDiscovered(id) | CentralEvent::DeviceUpdated(id) => (id, None),
                    CentralEvent::ServiceDataAdvertisement { id, service_data } => {
                        let payload = service_data.into_iter()
                            .find(|(uuid, _)| uuid.to_ble_u16() == Some(0xEC88))
                            .map(|(_, data)| data);
                        (id, payload)
                    }
                    CentralEvent::ManufacturerDataAdvertisement { id, manufacturer_data } => {
                        let payload = manufacturer_data.into_iter()
                            .find(|(company, _)| *company == 0xEC88 || *company == 0x88EC)
                            .map(|(_, data)| data);
                        (id, payload)
                    }
                    _ => continue,
                };
                let peripheral = match adapter.peripheral(&id).await { Ok(p) => p, Err(_) => continue };
                if let Some(reading) = read_sensor(&peripheral, &args, advertisement.as_deref()).await {
                    let key = reading.address.clone();
                    let new = !seen.contains_key(&key);
                    seen.insert(key, reading.clone());
                    if args.once {
                        if new { print_reading(&reading, args.json); }
                    } else if args.json {
                        print_reading(&reading, true);
                    } else {
                        print_reading(&reading, false);
                    }
                    if args.seconds.is_none() {
                        break;
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

async fn read_sensor(
    peripheral: &Peripheral,
    args: &Args,
    advertisement: Option<&[u8]>,
) -> Option<Reading> {
    let properties = peripheral.properties().await.ok().flatten()?;
    let name = properties
        .local_name
        .unwrap_or_else(|| "Govee H5075".into());
    // CoreBluetooth does not expose MAC addresses. Its stable per-Mac UUID is
    // available as the peripheral ID; Peripheral::address() is all zeroes.
    let address = peripheral.id().to_string();
    if let Some(filter) = &args.filter {
        let f = filter.to_lowercase();
        if !name.to_lowercase().contains(&f) && !address.to_lowercase().contains(&f) {
            return None;
        }
    }
    // The H5075 puts measurements in service data for UUID 0xEC88. This is a
    // Govee service UUID, not a Bluetooth manufacturer/company identifier.
    let service_payload = properties
        .service_data
        .iter()
        .find(|(uuid, _)| uuid.to_ble_u16() == Some(0xEC88))
        .map(|(_, data)| data.as_slice());
    let manufacturer_payload = properties
        .manufacturer_data
        .iter()
        .find(|(company, _)| **company == 0xEC88 || **company == 0x88EC)
        .map(|(_, data)| data.as_slice());
    let payload = advertisement.or(service_payload).or(manufacturer_payload)?;
    let (temperature_c, humidity, battery) = decode(payload)?;
    let rssi = properties.rssi;
    Some(Reading {
        name,
        address,
        temperature_f: temperature_c * 9.0 / 5.0 + 32.0,
        temperature_c,
        humidity_percent: humidity,
        battery_percent: battery,
        rssi,
        timestamp: SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs(),
    })
}

fn decode(data: &[u8]) -> Option<(f32, f32, u8)> {
    // 0xEC88 service payload: packet type, combined temperature/humidity
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
    use super::decode;

    #[test]
    fn decodes_h5075_service_data() {
        // Combined value 235456 means 23.5 °C and 45.6% RH.
        assert_eq!(
            decode(&[0x00, 0x03, 0x97, 0xC0, 87]),
            Some((23.5, 45.6, 87))
        );
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
            "{:<22} {}   {:>7.2} °C / {:>7.2} °F   {:>6.2}% RH   {:>3}% battery{}",
            r.name,
            timestamp,
            r.temperature_c,
            r.temperature_f,
            r.humidity_percent,
            r.battery_percent,
            r.rssi.map(|v| format!("   ({v} dBm)")).unwrap_or_default()
        );
    }
}
