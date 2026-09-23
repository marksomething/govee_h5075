# govee5075

A small Rust CLI that reads Govee H5075 temperature/humidity sensors from their BLE advertisements on macOS (using CoreBluetooth through `btleplug`). No pairing is required.

By default it prints the first reading and exits. `--watch` keeps printing readings until interrupted. Identical readings from the same sensor arriving within two seconds are collapsed.

## Install and run

```sh
# Print the first reading:
cargo run --release
# Keep printing readings until Ctrl-C:
cargo run --release -- --watch
# Only one sensor, matched by name or Bluetooth ID:
cargo run --release -- --filter GVH5075_50EE
```

On first run, macOS may ask the terminal/application for Bluetooth permission. If no devices are found, enable **System Settings → Privacy & Security → Bluetooth** for the terminal you are using and keep the sensor awake and nearby. H5075 advertisements are read passively, so the sensor does not need to be paired.

## Output format

One record per line, fixed width with exactly one space between fields, so `cut -c` can extract any column. A heading line in the same layout is written to **stderr** at startup, so it aligns with the records in a terminal but never mixes into the stdout stream that `cut` reads:

```
name                 timestamp            temp_c dew_pt rh_pct bat  rssi
GVH5075_50EE         1970-01-01T00:00:00Z  21.00  12.60  58.70  86   -61
```

| characters | field | heading | notes |
|------------|-------|---------|-------|
| 1–20 | name | `name` | truncated to 20 characters |
| 22–41 | timestamp | `timestamp` | UTC, ISO 8601 |
| 43–48 | temperature | `temp_c` | °C, 2 decimals |
| 50–55 | dew point | `dew_pt` | °C, 2 decimals, or `n/a` |
| 57–62 | humidity | `rh_pct` | % RH, 2 decimals |
| 64–66 | battery | `bat` | % |
| 68–72 | RSSI | `rssi` | dBm, or `n/a` |

Dew point uses the Magnus–Tetens formula and is `n/a` when relative humidity is zero.

```sh
# Just the temperature, one per line (heading goes to stderr, so is not included):
govee5075 --watch | cut -c 43-48
```

## Build

Requires a recent Rust toolchain and macOS with Bluetooth hardware:

```sh
cargo build --release
./target/release/govee5075 --help
```

The parser reads the H5075 measurement from BLE manufacturer data with ID `0xEC88` (not a service UUID), ignoring unrelated advertisements. On macOS the sensor name comes from the advertisement (`GVH5075_XXXX`); `--filter` also matches CoreBluetooth's per-Mac device UUID, because macOS does not expose BLE MAC addresses.
