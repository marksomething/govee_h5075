# govee5075

A small Rust CLI that reads Govee H5075 temperature/humidity sensors from their BLE advertisements on macOS (using CoreBluetooth through `btleplug`). No pairing is required. By default, it scans until the first measurement and then exits.

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
```

On first run, macOS may ask the terminal/application for Bluetooth permission. If no devices are found, enable **System Settings → Privacy & Security → Bluetooth** for the terminal you are using and keep the sensor awake and nearby. H5075 advertisements are read passively, so the sensor does not need to be paired.

The JSON output contains `temperature_c`, `temperature_f`, `humidity_percent`, `battery_percent`, `address`, `name`, RSSI when available, and a Unix `timestamp`. On macOS, `address` is CoreBluetooth's stable per-Mac device UUID because macOS does not expose BLE MAC addresses.

## Build

Requires a recent Rust toolchain and macOS with Bluetooth hardware:

```sh
cargo build --release
./target/release/govee5075 --help
```

The parser targets the H5075 measurement advertisement carried in Govee BLE service UUID `0xEC88` and intentionally ignores unrelated BLE devices.
