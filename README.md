# govee5075

A small Rust CLI that reads Govee H5075 temperature/humidity sensors from their BLE advertisements on macOS (using CoreBluetooth through `btleplug`). No pairing is required.

By default it prints the first reading and exits. `live --watch` keeps printing readings until interrupted. Identical readings from the same sensor arriving within two seconds are collapsed.

`history` connects to a sensor and downloads the readings stored on the device
itself (one per minute, up to 20 days) over the GATT connection, no pairing
required.

## Install and run

```sh
# Print the first reading (the live subcommand is the default):
cargo run --release
cargo run --release -- live
# Keep printing readings until Ctrl-C:
cargo run --release -- live --watch
# Only one sensor, matched by name or Bluetooth ID:
cargo run --release -- live --filter GVH5075_50EE
# Scan for a few seconds and list the latest reading from each sensor:
cargo run --release -- list
# Download the readings stored on a sensor, oldest first:
cargo run --release -- history --filter GVH5075_50EE
# Just the last 6 hours, ending 30 minutes ago:
cargo run --release -- history --filter GVH5075_50EE --start 6:00 --end 0:30
# Everything the sensor keeps (20 days; takes a few minutes to transfer):
cargo run --release -- history --filter GVH5075_50EE --start 480:00 --end 0:00
# Machine-readable output:
cargo run --release -- live --watch --format jsonl
cargo run --release -- history --filter GVH5075_50EE --format tsv --header
```

`history` requires `--filter` so it can never connect to an arbitrary nearby sensor. Use `list` or `live --watch` to find the name/ID to pass.

On first run, macOS may ask the terminal/application for Bluetooth permission. If no devices are found, enable **System Settings → Privacy & Security → Bluetooth** for the terminal you are using and keep the sensor awake and nearby. H5075 advertisements are read passively, so the sensor does not need to be paired.

Use `--adapter N` to choose a Bluetooth adapter when more than one is present. Use `--debug` to log raw advertisement/notification bytes and history request frames to stderr.

## Output format

The default `fixed` format is one record per line, fixed width with exactly one space between fields, so `cut -c` can extract any column. Columns 1–72 keep the same meaning as earlier releases; the device ID is appended after column 72. A heading line in the same layout is written to **stderr** at startup (or after a history download), so it aligns with the records in a terminal but never mixes into the stdout stream that `cut` reads. Use `--header` to write the heading to stdout instead.

```
name                 timestamp            temp_c dew_pt rh_pct bat  rssi id
GVH5075_50EE         2026-10-26T19:55:00Z  21.00  12.60  58.70  86   -61 7C9E8A1B-2D3F-4E5A-9B6C-1D2E3F4A5B6C
```

| characters | field | heading | notes |
|------------|-------|---------|-------|
| 1–20 | name | `name` | truncated to 20 characters, control characters replaced |
| 22–41 | timestamp | `timestamp` | UTC, ISO 8601 |
| 43–48 | temperature | `temp_c` | °C, 2 decimals |
| 50–55 | dew point | `dew_pt` | °C, 2 decimals, or `n/a` |
| 57–62 | humidity | `rh_pct` | % RH, 2 decimals |
| 64–66 | battery | `bat` | % |
| 68–72 | RSSI | `rssi` | dBm, or `n/a` |
| 74–109 | device ID | `id` | CoreBluetooth UUID on macOS, MAC on other platforms |

Dew point uses the Magnus–Tetens formula and is `n/a` when relative humidity is zero.

### Other formats

- `--format tsv` emits tab-separated fields (same order as the fixed columns, with the ID last). Use `cut -f` or import directly into a spreadsheet.
- `--format jsonl` emits one JSON object per line with `name`, `id`, `timestamp`, `temp_c`, `dew_pt`, `rh_pct`, `battery`, and `rssi`; unavailable values are `null`.

Names are sanitized in fixed/TSV output so device-provided control characters cannot corrupt the line stream. JSONL escapes them instead.

```sh
# Just the temperature, one per line (heading goes to stderr, so is not included):
govee5075 live --watch | cut -c 43-48
# Just the device IDs:
govee5075 live --watch | cut -c 74-109
# Filter JSONL with jq:
govee5075 live --watch --format jsonl | jq 'select(.rh_pct > 60)'
```

## History

`history` connects to the sensor and asks it to replay the per-minute readings
it stores in flash (20 days = 28,800 records). `--start` and `--end` take
`hhh:mm` back from now; a plain number means minutes. `--start` is the older
boundary and `--end` the newer one; reversed ranges are rejected.

History rows use the same fixed-width layout as live readings, with `n/a` for
battery and RSSI, so the same `cut -c` extraction works. Progress goes to stderr.

The device streams its records as notification batches; the tool anchors each
record's timestamp to the start of the minute in which the request was written
and sorts/deduplicates the result. The device reports whole minutes back, so
timestamps have up to one minute of uncertainty. The tool also filters records
client-side to the requested window: the device can overshoot the newest
boundary by up to one batch (six minutes), and the rows are trimmed to exactly
`--start`/`--end`.

If the transfer stalls or hits `--timeout-secs` (default 600), or the device
reports that it sent more messages than were received, the rows collected so
far are still printed but the command exits with status 3. `--idle-timeout-secs`
(default 15) controls how long to wait for the next notification, and
`--scan-timeout-secs` (default 30) bounds the initial scan.

```sh
# Everything the device keeps, as JSONL:
govee5075 history --filter GVH5075_50EE --start 480:00 --end 0:00 --format jsonl
```

## Exit codes

| code | meaning |
|------|---------|
| 0 | success |
| 1 | runtime error, no adapter, no matching sensor, or no readings |
| 2 | command-line usage error (from clap) |
| 3 | history transfer incomplete; partial rows were printed |
| 130 | interrupted with Ctrl-C |

A closed stdout pipe (for example `govee5075 live --watch | head -1`) exits cleanly rather than panicking.

## Build

Requires a recent Rust toolchain and macOS with Bluetooth hardware:

```sh
cargo build --release
./target/release/govee5075 --help
```

The parser reads the H5075 measurement from BLE manufacturer data with ID `0xEC88` (not a service UUID), ignoring unrelated advertisements. That manufacturer ID is shared with the H5074/H505x/H5179, which use different payload layouts; advertisements whose names identify those models are ignored. On macOS the sensor name comes from the advertisement (`GVH5075_XXXX`); `--filter` also matches CoreBluetooth's per-Mac device UUID, because macOS does not expose BLE MAC addresses.

The history protocol uses the `494e5445-...-2012` command characteristic and the `494e5445-...-2013` data characteristic. Command frames are checksummed with XOR, and completion/acknowledgement frames are validated before use. The implementation targets the legacy unencrypted protocol used by the H5075; some newer Govee firmware revisions encrypt GATT history downloads, and those devices will fail the acknowledgement/completion checks rather than emitting garbage.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The protocol codec, output formatting, CLI parsing and the history transfer state machine live in `src/lib.rs` modules and are tested without Bluetooth hardware. `src/main.rs` contains only the `btleplug` event loop and I/O.
