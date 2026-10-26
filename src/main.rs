use std::collections::HashMap;
use std::io::{self, BufWriter, Write};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use btleplug::api::{Central, CentralEvent, Manager as _, Peripheral as _, ScanFilter, WriteType};
use btleplug::platform::{Adapter, Manager, Peripheral};
use clap::Parser;
use futures_util::StreamExt;
use thiserror::Error;

use govee5075::cli::{Args, Command, HistoryArgs, ListArgs, LiveArgs};
use govee5075::format::{self, OutputFormat, Row};
use govee5075::history::{HistoryRecord, NotificationKind, Transfer, TransferSummary};
use govee5075::protocol::{self, Measurement};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(20);
const SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const FIRST_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const COMPLETION_GRACE: Duration = Duration::from_millis(500);

macro_rules! progress {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let mut stderr = std::io::stderr();
        let _ = writeln!(stderr, $($arg)*);
    }};
}

#[derive(Debug, Error)]
enum Error {
    #[error("no Bluetooth adapter found")]
    NoAdapter,
    #[error("no matching H5075 sensor found")]
    NotFound,
    #[error(
        "no H5075 readings found; check macOS Bluetooth permission and that the sensor is nearby"
    )]
    NoReadings,
    #[error("no history records received from the sensor")]
    NoHistory,
    #[error("device did not respond to the history request")]
    NoResponse,
    #[error("device does not expose the Govee history characteristics")]
    MissingCharacteristics,
    #[error("start ({start}) must be older than end ({end})")]
    BadRange { start: String, end: String },
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),
    #[error("scan timed out after {0} seconds")]
    ScanTimeout(u64),
    #[error("history transfer incomplete: {0}")]
    Incomplete(String),
    #[error("interrupted")]
    Interrupted,
    #[error("Bluetooth error: {0}")]
    Bluetooth(#[from] btleplug::Error),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

impl Error {
    fn exit_code(&self) -> u8 {
        match self {
            Self::Interrupted => 130,
            Self::Incomplete(_) => 3,
            _ => 1,
        }
    }
}

#[derive(Debug, Clone)]
struct Reading {
    name: String,
    address: String,
    measurement: Measurement,
    battery_percent: u8,
    rssi: Option<i16>,
    timestamp: u64,
}

impl Reading {
    fn to_row(&self) -> Row {
        Row {
            name: self.name.clone(),
            id: self.address.clone(),
            timestamp: self.timestamp,
            measurement: self.measurement,
            battery_percent: Some(self.battery_percent),
            rssi: self.rssi,
        }
    }
}

struct Output {
    format: OutputFormat,
    header: bool,
    out: BufWriter<io::Stdout>,
}

impl Output {
    fn new(format: OutputFormat, header: bool) -> Self {
        Self {
            format,
            header,
            out: BufWriter::new(io::stdout()),
        }
    }

    fn write_header(&mut self) -> io::Result<()> {
        if self.header {
            if let Some(heading) = format::heading(self.format) {
                writeln!(self.out, "{heading}")?;
            }
        }
        Ok(())
    }

    fn write_row(&mut self, row: &Row, flush: bool) -> io::Result<()> {
        writeln!(self.out, "{}", format::format_row(self.format, row))?;
        if flush {
            self.out.flush()?;
        }
        Ok(())
    }

    fn finish(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(Error::Io(error)) if error.kind() == io::ErrorKind::BrokenPipe => {
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            progress!("error: {error}");
            std::process::ExitCode::from(error.exit_code())
        }
    }
}

async fn run() -> Result<(), Error> {
    let args = Args::parse();
    let command = args
        .command
        .unwrap_or_else(|| Command::Live(LiveArgs::default()));

    let manager = Manager::new().await?;
    let adapters = manager.adapters().await?;
    let adapter = adapters
        .into_iter()
        .nth(args.adapter)
        .ok_or(Error::NoAdapter)?;

    let debug = args.debug;
    match command {
        Command::Live(args) => run_live(&adapter, &args, debug).await,
        Command::History(args) => run_history(&adapter, &args, debug).await,
        Command::List(args) => run_list(&adapter, &args, debug).await,
    }
}

async fn run_live(adapter: &Adapter, args: &LiveArgs, debug: bool) -> Result<(), Error> {
    let filter = args.filter.as_deref().map(str::to_lowercase);
    let mut events = adapter.events().await?;
    adapter.start_scan(ScanFilter::default()).await?;

    emit_stderr_heading(args.format, args.header);
    let mut output = Output::new(args.format, args.header);
    output.write_header()?;

    let interrupted = tokio::signal::ctrl_c();
    tokio::pin!(interrupted);
    let mut peripherals: HashMap<String, Peripheral> = HashMap::new();
    let mut seen: HashMap<String, (Reading, Instant)> = HashMap::new();
    let mut found = false;
    let mut was_interrupted = false;

    loop {
        tokio::select! {
            _ = &mut interrupted => {
                was_interrupted = true;
                break;
            }
            event = events.next() => {
                let Some(event) = event else { break };
                let CentralEvent::ManufacturerDataAdvertisement { id, manufacturer_data } = event else {
                    continue;
                };
                let Some(payload) = manufacturer_data.get(&protocol::MANUFACTURER_ID) else {
                    continue;
                };
                if debug {
                    progress!("advertisement {} {}", id, hex_bytes(payload));
                }
                // Timestamp at event receipt, before the async property lookup.
                let received_at = unix_now();
                let key = id.to_string();
                let peripheral = if let Some(peripheral) = peripherals.get(&key) {
                    peripheral.clone()
                } else {
                    let Ok(peripheral) = adapter.peripheral(&id).await else {
                        continue;
                    };
                    peripherals.insert(key, peripheral.clone());
                    peripheral
                };
                let Some(reading) =
                    read_sensor(&peripheral, filter.as_deref(), payload, received_at).await
                else {
                    continue;
                };

                if !args.watch {
                    output.write_row(&reading.to_row(), false)?;
                    found = true;
                    break;
                }

                let now = Instant::now();
                if should_print(seen.get(&reading.address), &reading, now) {
                    seen.insert(reading.address.clone(), (reading.clone(), now));
                    output.write_row(&reading.to_row(), true)?;
                    found = true;
                }
            }
        }
    }

    if let Err(error) = adapter.stop_scan().await {
        progress!("warning: failed to stop scan: {error}");
    }
    output.finish()?;
    if !found {
        if was_interrupted {
            return Err(Error::Interrupted);
        }
        return Err(Error::NoReadings);
    }
    Ok(())
}

async fn read_sensor(
    peripheral: &Peripheral,
    filter: Option<&str>,
    payload: &[u8],
    timestamp: u64,
) -> Option<Reading> {
    let properties = peripheral.properties().await.ok().flatten()?;
    let name = properties.local_name.as_deref();
    // CoreBluetooth does not expose MAC addresses. Its stable per-Mac UUID is
    // available as the peripheral ID; Peripheral::address() is all zeroes.
    let address = peripheral.id().to_string();
    if !label_matches(name, &address, filter) || is_unsupported_model(name) {
        return None;
    }
    let advertisement = protocol::decode_advertisement(payload)?;
    Some(Reading {
        name: name.unwrap_or("Govee H5075").to_owned(),
        address,
        measurement: advertisement.measurement,
        battery_percent: advertisement.battery_percent,
        rssi: properties.rssi,
        timestamp,
    })
}

/// `0xEC88` is shared with H5074/H505x/H5179, which use different payload
/// layouts. Reject those model names rather than decoding their bytes as an
/// H5075 measurement.
fn is_unsupported_model(name: Option<&str>) -> bool {
    const MARKERS: [&str; 5] = ["H5074", "H5051", "H5052", "H5071", "H5179"];
    name.is_some_and(|name| {
        let upper = name.to_ascii_uppercase();
        MARKERS.iter().any(|marker| upper.contains(marker))
    })
}

fn label_matches(name: Option<&str>, address: &str, filter: Option<&str>) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    name.is_some_and(|name| name.to_lowercase().contains(filter))
        || address.to_lowercase().contains(filter)
}

// Collapse repeated advertisements carrying the same measurement in a short
// burst. A later advertisement is printed even when the values are unchanged.
fn should_print(previous: Option<&(Reading, Instant)>, current: &Reading, now: Instant) -> bool {
    match previous {
        None => true,
        Some((last, at)) => {
            now.duration_since(*at) >= Duration::from_secs(2)
                || last.measurement != current.measurement
                || last.battery_percent != current.battery_percent
        }
    }
}

async fn run_history(adapter: &Adapter, args: &HistoryArgs, debug: bool) -> Result<(), Error> {
    if args.start.0 < args.end.0 {
        return Err(Error::BadRange {
            start: args.start.to_string(),
            end: args.end.to_string(),
        });
    }
    let filter = args.filter.to_lowercase();

    progress!(
        "scanning for {} (up to {}s) ...",
        args.filter,
        args.scan_timeout_secs
    );
    let mut events = adapter.events().await?;
    adapter.start_scan(ScanFilter::default()).await?;

    let scan_deadline =
        tokio::time::Instant::now() + Duration::from_secs(args.scan_timeout_secs.max(1));
    let interrupted = tokio::signal::ctrl_c();
    tokio::pin!(interrupted);

    let peripheral = loop {
        let event = tokio::select! {
            _ = &mut interrupted => {
                let _ = adapter.stop_scan().await;
                return Err(Error::Interrupted);
            }
            result = tokio::time::timeout_at(scan_deadline, events.next()) => {
                match result {
                    Ok(Some(event)) => event,
                    Ok(None) => {
                        let _ = adapter.stop_scan().await;
                        return Err(Error::NotFound);
                    }
                    Err(_) => {
                        let _ = adapter.stop_scan().await;
                        return Err(Error::ScanTimeout(args.scan_timeout_secs));
                    }
                }
            }
        };

        let CentralEvent::ManufacturerDataAdvertisement {
            id,
            manufacturer_data,
        } = event
        else {
            continue;
        };
        if !manufacturer_data.contains_key(&protocol::MANUFACTURER_ID) {
            continue;
        }
        let Ok(candidate) = adapter.peripheral(&id).await else {
            continue;
        };
        let name = candidate
            .properties()
            .await
            .ok()
            .flatten()
            .and_then(|properties| properties.local_name);
        let address = candidate.id().to_string();
        if is_unsupported_model(name.as_deref()) {
            continue;
        }
        if label_matches(name.as_deref(), &address, Some(&filter)) {
            progress!("found {address}, connecting ...");
            break candidate;
        }
    };
    if let Err(error) = adapter.stop_scan().await {
        progress!("warning: failed to stop scan: {error}");
    }

    let address = peripheral.id().to_string();
    let name = peripheral
        .properties()
        .await
        .ok()
        .flatten()
        .and_then(|properties| properties.local_name)
        .unwrap_or_else(|| "Govee H5075".to_owned());
    progress!("connecting to {name} ...");
    tokio::time::timeout(CONNECT_TIMEOUT, peripheral.connect())
        .await
        .map_err(|_| Error::Timeout("connect"))??;

    progress!("connected, discovering services ...");
    tokio::time::timeout(DISCOVER_TIMEOUT, peripheral.discover_services())
        .await
        .map_err(|_| Error::Timeout("service discovery"))??;

    let characteristics = peripheral.characteristics();
    let Some(command) = characteristics
        .iter()
        .find(|characteristic| characteristic.uuid == protocol::UUID_COMMAND)
        .cloned()
    else {
        return Err(Error::MissingCharacteristics);
    };
    let Some(data) = characteristics
        .iter()
        .find(|characteristic| characteristic.uuid == protocol::UUID_DATA)
        .cloned()
    else {
        return Err(Error::MissingCharacteristics);
    };

    // Create the stream first, then subscribe, so no notification can be lost
    // in between; the stream stays valid across both subscriptions.
    progress!("subscribing to notifications ...");
    let mut notifications = peripheral.notifications().await?;
    tokio::time::timeout(SUBSCRIBE_TIMEOUT, peripheral.subscribe(&command))
        .await
        .map_err(|_| Error::Timeout("subscribe (command)"))??;
    tokio::time::timeout(SUBSCRIBE_TIMEOUT, peripheral.subscribe(&data))
        .await
        .map_err(|_| Error::Timeout("subscribe (data)"))??;

    let frame = protocol::history_request_frame(args.start.0 as u16, args.end.0 as u16);
    if debug {
        progress!("request frame {}", hex_bytes(&frame));
    }
    let request_started = unix_now();
    tokio::time::timeout(
        WRITE_TIMEOUT,
        peripheral.write(&command, &frame, WriteType::WithResponse),
    )
    .await
    .map_err(|_| Error::Timeout("history request write"))??;
    progress!(
        "requested history from {} to {} minutes back; downloading ...",
        args.start,
        args.end
    );

    let mut transfer = Transfer::new(args.start.0, args.end.0, request_started);
    let idle_timeout = Duration::from_secs(args.idle_timeout_secs.max(1));
    let total_deadline = Instant::now() + Duration::from_secs(args.timeout_secs.max(1));
    let mut first_response_deadline = Some(Instant::now() + FIRST_RESPONSE_TIMEOUT);
    let mut grace_deadline: Option<Instant> = None;

    loop {
        let now = Instant::now();
        if transfer.is_done() {
            break;
        }
        if grace_deadline.is_some_and(|deadline| now >= deadline) {
            break;
        }
        if now >= total_deadline {
            transfer.on_total_timeout();
            break;
        }
        if !transfer.saw_response()
            && first_response_deadline.is_some_and(|deadline| now >= deadline)
        {
            break;
        }

        let mut wait = idle_timeout;
        if !transfer.saw_response() {
            if let Some(deadline) = first_response_deadline {
                wait = wait.min(deadline.saturating_duration_since(now));
            }
        }
        if let Some(deadline) = grace_deadline {
            wait = wait.min(deadline.saturating_duration_since(now));
        }
        wait = wait.min(total_deadline.saturating_duration_since(now));
        if wait.is_zero() {
            transfer.on_idle();
            break;
        }

        let notification = tokio::select! {
            _ = &mut interrupted => {
                transfer.on_interrupt();
                break;
            }
            result = tokio::time::timeout(wait, notifications.next()) => {
                match result {
                    Ok(Some(notification)) => notification,
                    Ok(None) => {
                        transfer.on_idle();
                        break;
                    }
                    Err(_) => {
                        let now = Instant::now();
                        if grace_deadline.is_some_and(|deadline| now >= deadline) {
                            break;
                        }
                        if !transfer.saw_response()
                            && first_response_deadline.is_some_and(|deadline| now >= deadline)
                        {
                            break;
                        }
                        if now >= total_deadline {
                            transfer.on_total_timeout();
                            break;
                        }
                        transfer.on_idle();
                        break;
                    }
                }
            }
        };

        let kind = if notification.uuid == data.uuid {
            NotificationKind::Data
        } else if notification.uuid == command.uuid {
            NotificationKind::Command
        } else {
            continue;
        };
        if debug {
            progress!(
                "notification {} {}",
                match kind {
                    NotificationKind::Data => "data",
                    NotificationKind::Command => "command",
                },
                hex_bytes(&notification.value)
            );
        }
        transfer.observe(kind, &notification.value);
        if transfer.saw_response() {
            first_response_deadline = None;
        }
        if transfer.completed() && grace_deadline.is_none() {
            // The completion frame and the last data notifications travel on
            // different characteristics; drain briefly after completion rather
            // than dropping in-flight records.
            grace_deadline = Some(Instant::now() + COMPLETION_GRACE);
        }
    }

    if let Err(error) = tokio::time::timeout(DISCONNECT_TIMEOUT, peripheral.disconnect()).await {
        progress!("warning: disconnect failed: {error}");
    }

    let summary = transfer.finish();
    write_history_output(&summary, &name, &address, args)
}

fn write_history_output(
    summary: &TransferSummary,
    name: &str,
    address: &str,
    args: &HistoryArgs,
) -> Result<(), Error> {
    if summary.records.is_empty() {
        if summary.interrupted {
            return Err(Error::Interrupted);
        }
        if !summary.saw_response {
            return Err(Error::NoResponse);
        }
        return Err(Error::NoHistory);
    }

    if !summary.acknowledged {
        progress!("warning: device did not acknowledge the history request");
    }
    if summary.timed_out {
        progress!("warning: history transfer timed out");
    }
    if let Some(expected) = summary.expected {
        if summary.received < expected {
            progress!(
                "warning: history incomplete: device sent {expected} messages, {} received",
                summary.received
            );
        }
    } else if !summary.complete {
        progress!("warning: history transfer did not report completion");
    }
    progress!(
        "received {} records in {} messages",
        summary.records.len(),
        summary.received
    );

    emit_stderr_heading(args.format, args.header);
    let mut output = Output::new(args.format, args.header);
    output.write_header()?;
    for record in &summary.records {
        output.write_row(&history_row(name, address, record), false)?;
    }
    output.finish()?;

    if summary.interrupted {
        return Err(Error::Interrupted);
    }
    if !summary.complete {
        let detail = if let Some(expected) = summary.expected {
            format!(
                "device sent {expected} messages, {} received",
                summary.received
            )
        } else if summary.timed_out {
            format!("timed out after {} messages", summary.received)
        } else {
            format!(
                "device did not report completion after {} messages",
                summary.received
            )
        };
        return Err(Error::Incomplete(detail));
    }
    Ok(())
}

fn history_row(name: &str, address: &str, record: &HistoryRecord) -> Row {
    Row {
        name: name.to_owned(),
        id: address.to_owned(),
        timestamp: record.timestamp,
        measurement: record.measurement,
        // The history records do not carry battery or RSSI.
        battery_percent: None,
        rssi: None,
    }
}

async fn run_list(adapter: &Adapter, args: &ListArgs, debug: bool) -> Result<(), Error> {
    let filter = args.filter.as_deref().map(str::to_lowercase);
    let mut events = adapter.events().await?;
    adapter.start_scan(ScanFilter::default()).await?;

    emit_stderr_heading(args.format, args.header);
    let mut output = Output::new(args.format, args.header);
    output.write_header()?;

    let deadline = Instant::now() + Duration::from_secs(args.seconds.max(1));
    let interrupted = tokio::signal::ctrl_c();
    tokio::pin!(interrupted);
    let mut peripherals: HashMap<String, Peripheral> = HashMap::new();
    let mut sensors: HashMap<String, Row> = HashMap::new();
    let mut was_interrupted = false;

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let wait = deadline.saturating_duration_since(now);
        let event = tokio::select! {
            _ = &mut interrupted => {
                was_interrupted = true;
                break;
            }
            result = tokio::time::timeout(wait, events.next()) => {
                match result {
                    Ok(Some(event)) => event,
                    _ => break,
                }
            }
        };

        let CentralEvent::ManufacturerDataAdvertisement {
            id,
            manufacturer_data,
        } = event
        else {
            continue;
        };
        let Some(payload) = manufacturer_data.get(&protocol::MANUFACTURER_ID) else {
            continue;
        };
        if debug {
            progress!("advertisement {} {}", id, hex_bytes(payload));
        }
        let received_at = unix_now();
        let key = id.to_string();
        let peripheral = if let Some(peripheral) = peripherals.get(&key) {
            peripheral.clone()
        } else {
            let Ok(peripheral) = adapter.peripheral(&id).await else {
                continue;
            };
            peripherals.insert(key, peripheral.clone());
            peripheral
        };
        let Some(reading) = read_sensor(&peripheral, filter.as_deref(), payload, received_at).await
        else {
            continue;
        };
        sensors.insert(reading.address.clone(), reading.to_row());
    }

    if let Err(error) = adapter.stop_scan().await {
        progress!("warning: failed to stop scan: {error}");
    }

    let mut rows: Vec<Row> = sensors.into_values().collect();
    rows.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.id.cmp(&right.id))
    });
    for row in &rows {
        output.write_row(row, false)?;
    }
    output.finish()?;

    if rows.is_empty() {
        if was_interrupted {
            return Err(Error::Interrupted);
        }
        return Err(Error::NoReadings);
    }
    Ok(())
}

fn emit_stderr_heading(format: OutputFormat, header: bool) {
    if !header && format == OutputFormat::Fixed {
        if let Some(heading) = format::heading(format) {
            progress!("{heading}");
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_reading() -> Reading {
        Reading {
            name: "GVH5075_50EE".into(),
            address: "device-1".into(),
            measurement: Measurement {
                temperature_tenths: 210,
                humidity_tenths: 587,
            },
            battery_percent: 86,
            rssi: Some(-61),
            timestamp: 0,
        }
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
        changed.measurement.temperature_tenths = 211;
        assert!(should_print(Some(&previous), &changed, soon));
    }

    #[test]
    fn ignores_models_that_share_the_manufacturer_id() {
        assert!(is_unsupported_model(Some("GVH5074_50EE")));
        assert!(is_unsupported_model(Some("H5051")));
        assert!(!is_unsupported_model(Some("GVH5075_50EE")));
        assert!(!is_unsupported_model(None));
    }

    #[test]
    fn matches_names_and_ids_case_insensitively() {
        assert!(label_matches(Some("gvh5075_50ee"), "uuid", None));
        assert!(label_matches(Some("gvh5075_50ee"), "uuid", Some("gvh5075")));
        assert!(label_matches(None, "ABCD-UUID", Some("abcd")));
        assert!(!label_matches(Some("other"), "uuid", Some("gvh5075")));
    }
}
