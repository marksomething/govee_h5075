# govee5075

A small Rust CLI that reads Govee H5075 temperature/humidity sensors from their BLE advertisements on macOS (using CoreBluetooth through `btleplug`). No pairing is required. By default, it scans until the first measurement and then exits. During timed scans, identical readings from the same sensor within two seconds are collapsed; later readings and changed values are still printed.

## Install and run

```sh
# Wait for and print the first measurement:
cargo run --release
# Scan for a fixed period:
cargo run --release -- --seconds 15
# JSON, useful for scripts:
cargo run --release -- --json
# One reading per sensor during a fixed scan:
cargo run --release -- --once --seconds 30
# Select one sensor by name or Bluetooth address:
cargo run --release -- --filter ABC123
# Inspect 0xEC88 manufacturer advertisements and decoding:
cargo run -- --debug --seconds 15
```

On first run, macOS may ask the terminal/application for Bluetooth permission. If no devices are found, enable **System Settings → Privacy & Security → Bluetooth** for the terminal you are using and keep the sensor awake and nearby. H5075 advertisements are read passively, so the sensor does not need to be paired.

The JSON output contains `temperature_c`, `dew_point_c`, `humidity_percent`, `battery_percent`, `address`, `name`, RSSI when available, and a Unix `timestamp`. Dew point is calculated in Celsius with the Magnus–Tetens formula; it is `null` when relative humidity is zero. On macOS, `address` is CoreBluetooth's stable per-Mac device UUID because macOS does not expose BLE MAC addresses.

## Build

Requires a recent Rust toolchain and macOS with Bluetooth hardware:

```sh
cargo build --release
./target/release/govee5075 --help
```

The parser reads the H5075 measurement from BLE manufacturer data with ID `0xEC88` (not a service UUID), ignoring unrelated advertisements. `--debug` shows the raw matching payload and its decoded measurement on stderr. Devices without an advertised name are shown as `<unnamed>` in diagnostics.
