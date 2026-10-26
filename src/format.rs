//! Output formatting.
//!
//! The fixed-width layout is intentionally stable: columns 1-72 keep the same
//! meaning as earlier releases, and a device ID is appended after column 72.
//! TSV and JSONL are available for consumers that would rather not parse
//! fixed-width text.

use std::fmt;

use chrono::{DateTime, Utc};
use clap::ValueEnum;

use crate::protocol::{dew_point_c, Measurement};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Fixed,
    Tsv,
    Jsonl,
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Fixed => "fixed",
            Self::Tsv => "tsv",
            Self::Jsonl => "jsonl",
        })
    }
}

/// One row, independent of whether it came from an advertisement or history.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub name: String,
    pub id: String,
    pub timestamp: u64,
    pub measurement: Measurement,
    pub battery_percent: Option<u8>,
    pub rssi: Option<i16>,
}

impl Row {
    pub fn dew_point_c(&self) -> Option<f64> {
        dew_point_c(
            self.measurement.temperature_c(),
            self.measurement.humidity_percent(),
        )
    }
}

/// Replace control characters and non-ASCII text so a device-provided name
/// cannot break the line-oriented/fixed-width output.
pub fn sanitize_label(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub fn format_timestamp(secs: u64) -> String {
    DateTime::<Utc>::from_timestamp(secs as i64, 0)
        .map(|value| value.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

fn format_optional(value: Option<f64>, width: usize) -> String {
    value
        .map(|value| format!("{value:>width$.2}"))
        .unwrap_or_else(|| format!("{:>width$}", "n/a"))
}

fn json_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn heading_fields() -> [&'static str; 8] {
    [
        "name",
        "timestamp",
        "temp_c",
        "dew_pt",
        "rh_pct",
        "bat",
        "rssi",
        "id",
    ]
}

/// A heading line, or `None` for JSONL (which is self-describing).
pub fn heading(format: OutputFormat) -> Option<String> {
    match format {
        OutputFormat::Fixed => Some(format!(
            "{:<20.20} {:<20.20} {:>6} {:>6} {:>6} {:>3} {:>5} {:<36.36}",
            "name", "timestamp", "temp_c", "dew_pt", "rh_pct", "bat", "rssi", "id"
        )),
        OutputFormat::Tsv => Some(heading_fields().join("\t")),
        OutputFormat::Jsonl => None,
    }
}

pub fn format_row(format: OutputFormat, row: &Row) -> String {
    match format {
        OutputFormat::Fixed => format_fixed(row),
        OutputFormat::Tsv => format_tsv(row),
        OutputFormat::Jsonl => format_jsonl(row),
    }
}

fn format_fixed(row: &Row) -> String {
    let name = sanitize_label(&row.name);
    let id = sanitize_label(&row.id);
    let dew_point = format_optional(row.dew_point_c(), 6);
    let battery = row
        .battery_percent
        .map(|value| format!("{value:>3}"))
        .unwrap_or_else(|| "n/a".to_owned());
    let rssi = row
        .rssi
        .map(|value| format!("{value:>5}"))
        .unwrap_or_else(|| "  n/a".to_owned());
    format!(
        "{:<20.20} {:<20.20} {:>6.2} {} {:>6.2} {} {} {:<36.36}",
        name,
        format_timestamp(row.timestamp),
        row.measurement.temperature_c(),
        dew_point,
        row.measurement.humidity_percent(),
        battery,
        rssi,
        id
    )
}

fn format_tsv(row: &Row) -> String {
    let name = sanitize_label(&row.name);
    let id = sanitize_label(&row.id);
    let battery = row
        .battery_percent
        .map(|value| value.to_string())
        .unwrap_or_else(|| "n/a".to_owned());
    let rssi = row
        .rssi
        .map(|value| value.to_string())
        .unwrap_or_else(|| "n/a".to_owned());
    let dew_point = row
        .dew_point_c()
        .map(|value| format!("{value:.2}"))
        .unwrap_or_else(|| "n/a".to_owned());
    format!(
        "{}\t{}\t{:.2}\t{}\t{:.2}\t{}\t{}\t{}",
        name,
        format_timestamp(row.timestamp),
        row.measurement.temperature_c(),
        dew_point,
        row.measurement.humidity_percent(),
        battery,
        rssi,
        id
    )
}

fn format_jsonl(row: &Row) -> String {
    let dew_point = row
        .dew_point_c()
        .map(|value| format!("{value:.2}"))
        .unwrap_or_else(|| "null".to_owned());
    let battery = row
        .battery_percent
        .map(|value| value.to_string())
        .unwrap_or_else(|| "null".to_owned());
    let rssi = row
        .rssi
        .map(|value| value.to_string())
        .unwrap_or_else(|| "null".to_owned());
    format!(
        "{{\"name\":\"{}\",\"id\":\"{}\",\"timestamp\":\"{}\",\"temp_c\":{:.2},\"dew_pt\":{},\"rh_pct\":{:.2},\"battery\":{},\"rssi\":{}}}",
        json_escape(&row.name),
        json_escape(&row.id),
        format_timestamp(row.timestamp),
        row.measurement.temperature_c(),
        dew_point,
        row.measurement.humidity_percent(),
        battery,
        rssi
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row() -> Row {
        Row {
            name: "GVH5075_50EE".into(),
            id: "device-1".into(),
            timestamp: 0,
            measurement: Measurement {
                temperature_tenths: 210,
                humidity_tenths: 587,
            },
            battery_percent: Some(86),
            rssi: Some(-61),
        }
    }

    #[test]
    fn fixed_reading_uses_stable_cut_columns() {
        let line = format_row(OutputFormat::Fixed, &sample_row());
        assert_eq!(line.len(), 109);
        assert_eq!(&line[0..20], "GVH5075_50EE        ");
        assert_eq!(&line[21..41], "1970-01-01T00:00:00Z");
        assert_eq!(&line[42..48], " 21.00");
        assert_eq!(&line[49..55], " 12.60");
        assert_eq!(&line[56..62], " 58.70");
        assert_eq!(&line[63..66], " 86");
        assert_eq!(&line[67..72], "  -61");
        assert_eq!(&line[73..81], "device-1");
    }

    #[test]
    fn heading_reuses_the_data_columns() {
        let line = heading(OutputFormat::Fixed).unwrap();
        assert_eq!(line.len(), 109);
        assert_eq!(&line[0..20], "name                ");
        assert_eq!(&line[21..41], "timestamp           ");
        assert_eq!(&line[42..48], "temp_c");
        assert_eq!(&line[49..55], "dew_pt");
        assert_eq!(&line[56..62], "rh_pct");
        assert_eq!(&line[63..66], "bat");
        assert_eq!(&line[67..72], " rssi");
        assert_eq!(&line[73..75], "id");
    }

    #[test]
    fn missing_values_keep_their_column_width() {
        let mut row = sample_row();
        // Zero humidity means no finite dew point.
        row.measurement.humidity_tenths = 0;
        row.battery_percent = None;
        row.rssi = None;
        let line = format_row(OutputFormat::Fixed, &row);
        assert_eq!(line.len(), 109);
        assert_eq!(&line[49..55], "   n/a");
        assert_eq!(&line[63..66], "n/a");
        assert_eq!(&line[67..72], "  n/a");
    }

    #[test]
    fn tsv_has_no_padding_and_keeps_the_device_id() {
        assert_eq!(
            format_row(OutputFormat::Tsv, &sample_row()),
            "GVH5075_50EE\t1970-01-01T00:00:00Z\t21.00\t12.60\t58.70\t86\t-61\tdevice-1"
        );
        assert_eq!(
            heading(OutputFormat::Tsv).unwrap(),
            "name\ttimestamp\ttemp_c\tdew_pt\trh_pct\tbat\trssi\tid"
        );
    }

    #[test]
    fn jsonl_is_machine_readable_and_escapes_names() {
        let mut row = sample_row();
        row.name = "name\twith\"quotes\\and\nnewline".into();
        let line = format_row(OutputFormat::Jsonl, &row);
        assert!(line.starts_with("{\"name\":\"name\\twith\\\"quotes\\\\and\\nnewline\""));
        assert!(line.contains("\"id\":\"device-1\""));
        assert!(line.contains("\"temp_c\":21.00"));
        assert!(line.contains("\"battery\":86"));
        assert!(line.contains("\"rssi\":-61"));

        row.battery_percent = None;
        row.rssi = None;
        let line = format_row(OutputFormat::Jsonl, &row);
        assert!(line.contains("\"battery\":null"));
        assert!(line.contains("\"rssi\":null"));
    }

    #[test]
    fn sanitizes_device_provided_labels() {
        assert_eq!(sanitize_label("a\nb\tc\u{7f}d"), "a_b_c_d");
        assert_eq!(sanitize_label("caf\u{e9}"), "caf_");
    }
}
