//! Command-line interface.

use std::fmt;
use std::str::FromStr;

use clap::{Args as ClapArgs, Parser, Subcommand};

use crate::format::OutputFormat;
use crate::protocol::HISTORY_MAX_MINUTES;

#[derive(Debug, Parser)]
#[command(
    name = "govee5075",
    version,
    about = "Read Govee H5075 sensors over Bluetooth Low Energy"
)]
pub struct Args {
    /// Bluetooth adapter index to use
    #[arg(long, global = true, default_value_t = 0)]
    pub adapter: usize,
    /// Log raw advertisement/notification bytes to stderr
    #[arg(long, global = true)]
    pub debug: bool,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print live readings from advertisements (the default)
    Live(LiveArgs),
    /// Download the readings stored on a sensor
    History(HistoryArgs),
    /// Scan for nearby sensors and print the latest reading from each
    List(ListArgs),
}

#[derive(Debug, ClapArgs)]
pub struct LiveArgs {
    /// Keep printing readings until interrupted instead of exiting after the first
    #[arg(short, long)]
    pub watch: bool,
    /// Only use a sensor whose name or Bluetooth ID contains this text
    #[arg(short, long)]
    pub filter: Option<String>,
    #[arg(long, value_enum, default_value_t = OutputFormat::Fixed)]
    pub format: OutputFormat,
    /// Write the heading to stdout instead of stderr
    #[arg(long)]
    pub header: bool,
}

impl Default for LiveArgs {
    fn default() -> Self {
        Self {
            watch: false,
            filter: None,
            format: OutputFormat::Fixed,
            header: false,
        }
    }
}

#[derive(Debug, ClapArgs)]
pub struct HistoryArgs {
    /// Sensor to download from, matched by name or Bluetooth ID
    #[arg(short, long)]
    pub filter: String,
    /// Oldest reading to fetch, as hhh:mm back from now
    /// (max 480:00 = the 20 days the device stores)
    #[arg(long, default_value_t = MinutesBack(60))]
    pub start: MinutesBack,
    /// Newest reading to fetch, as hhh:mm back from now
    #[arg(long, default_value_t = MinutesBack(0))]
    pub end: MinutesBack,
    #[arg(long, value_enum, default_value_t = OutputFormat::Fixed)]
    pub format: OutputFormat,
    /// Write the heading to stdout instead of stderr
    #[arg(long)]
    pub header: bool,
    /// Give up when no notification arrives for this many seconds
    #[arg(long, default_value_t = 15)]
    pub idle_timeout_secs: u64,
    /// Overall transfer deadline in seconds
    #[arg(long, default_value_t = 600)]
    pub timeout_secs: u64,
    /// How long to scan for the sensor before giving up
    #[arg(long, default_value_t = 30)]
    pub scan_timeout_secs: u64,
}

#[derive(Debug, ClapArgs)]
pub struct ListArgs {
    /// Only list sensors whose name or Bluetooth ID contains this text
    #[arg(short, long)]
    pub filter: Option<String>,
    /// How long to scan
    #[arg(long, default_value_t = 5)]
    pub seconds: u64,
    #[arg(long, value_enum, default_value_t = OutputFormat::Fixed)]
    pub format: OutputFormat,
    /// Write the heading to stdout instead of stderr
    #[arg(long)]
    pub header: bool,
}

/// Minutes back from now, parsed from `hhh:mm` or a plain number of minutes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MinutesBack(pub u32);

impl FromStr for MinutesBack {
    type Err = String;

    fn from_str(spec: &str) -> Result<Self, Self::Err> {
        let spec = spec.trim();
        let (hours, minutes, explicit_minutes) = match spec.split_once(':') {
            Some((hours, minutes)) => {
                let hours = hours
                    .parse::<u32>()
                    .map_err(|_| format!("invalid hours in \"{spec}\""))?;
                let minutes = minutes
                    .parse::<u32>()
                    .map_err(|_| format!("invalid minutes in \"{spec}\""))?;
                (hours, minutes, true)
            }
            None => (
                0,
                spec.parse::<u32>()
                    .map_err(|_| format!("invalid minutes in \"{spec}\""))?,
                false,
            ),
        };
        if explicit_minutes && minutes >= 60 {
            return Err(format!("minutes must be less than 60 in \"{spec}\""));
        }
        let total = hours
            .checked_mul(60)
            .and_then(|value| value.checked_add(minutes))
            .ok_or_else(|| format!("time range \"{spec}\" is too large"))?;
        if total > HISTORY_MAX_MINUTES {
            return Err(format!(
                "time range \"{spec}\" exceeds the device maximum of 480:00"
            ));
        }
        Ok(Self(total))
    }
}

impl fmt::Display for MinutesBack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{:02}", self.0 / 60, self.0 % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_back_from_now_expressions() {
        assert_eq!("0:00".parse::<MinutesBack>(), Ok(MinutesBack(0)));
        assert_eq!("1:30".parse::<MinutesBack>(), Ok(MinutesBack(90)));
        assert_eq!("480:00".parse::<MinutesBack>(), Ok(MinutesBack(28_800)));
        assert_eq!("90".parse::<MinutesBack>(), Ok(MinutesBack(90)));
        assert!("481:00".parse::<MinutesBack>().is_err());
        assert!("nonsense".parse::<MinutesBack>().is_err());
    }

    #[test]
    fn rejects_minutes_over_59_in_explicit_hh_mm() {
        assert!("1:90".parse::<MinutesBack>().is_err());
        assert!("1:60".parse::<MinutesBack>().is_err());
        // A plain number is minutes, so 90 is valid.
        assert!("90".parse::<MinutesBack>().is_ok());
    }

    #[test]
    fn displays_a_round_trippable_value() {
        assert_eq!(MinutesBack(90).to_string(), "1:30");
        assert_eq!(MinutesBack(0).to_string(), "0:00");
        assert_eq!("1:30".parse::<MinutesBack>().unwrap().to_string(), "1:30");
    }
}
