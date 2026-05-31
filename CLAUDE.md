# Unicorn Display

Rust firmware for the **Pimoroni Galactic Unicorn** — a 53×11 RGB LED matrix with an embedded Raspberry Pi Pico W.
Subscribes to MQTT topics and renders scrolling/static text on the display.

## Hardware

- **MCU:** RP2040 (Cortex-M0+), target `thumbv6m-none-eabi`
- **Display:** 53×11 RGB LED matrix (FM6047 drivers) driven by PIO shift registers
  - CLK=GPIO13, DATA=GPIO14, LATCH=GPIO15, BLANK=GPIO16, row-select=GPIO17–20
  - (matches Pimoroni `galactic_unicorn.cpp`; see `src/pio_display.rs`)
- **WiFi:** CYW43439 chip via `cyw43` + `cyw43-pio` (Pico W)

## Stack

| Concern | Crate |
|---|---|
| Async runtime | `embassy-executor` |
| RP2040 HAL | `embassy-rp` |
| Networking | `embassy-net` |
| WiFi driver | `cyw43` + `cyw43-pio` |
| MQTT client | hand-rolled minimal MQTT 3.1.1 over a raw `embassy-net` TCP socket (`src/mqtt.rs`) |
| Logging | `defmt` + `defmt-rtt` (RTT output requires a debug probe to read; see flashing note) |

## Build setup (one-time)

```sh
rustup target add thumbv6m-none-eabi
cargo install flip-link elf2uf2-rs   # flip-link = linker; elf2uf2-rs = UF2 packaging
cd firmware && sh fetch.sh           # downloads CYW43 firmware blobs
```

## Configuration

`src/config.rs` holds WiFi credentials and is **gitignored**. Create it from the
template, then edit:

```sh
cp src/config.example.rs src/config.rs
```

- `WIFI_SSID` / `WIFI_PASSWORD`
- `MQTT_BROKER_IP` (as `[u8; 4]`)
- `TOPICS` — list of `(topic, label, (r, g, b))` tuples

Keep real credentials only in `src/config.rs` (never committed). When config keys
change, update `src/config.example.rs` too.

## Build & flash

The on-board debug header is broken, so flash over USB in BOOTSEL mode (not probe-rs):

```sh
cargo build --release
# Hold BOOTSEL and plug in the board → it mounts as RPI-RP2.
elf2uf2-rs target/thumbv6m-none-eabi/release/unicorn-display /tmp/unicorn.uf2
cp /tmp/unicorn.uf2 /Volumes/RPI-RP2/   # board reflashes and reboots automatically
```

> The `runner` in `.cargo/config.toml` still points at `probe-rs run`, which only
> works with a debug probe. With no probe, ignore `cargo run` and use the steps above.

**Boot indicator:** on boot the whole panel flashes white for ~2 s (before WiFi).
If you see it, the boot2 loader and PIO display driver are working; a still-blank
panel afterward is a WiFi/MQTT issue, not a display one.

### `.boot2` is required — don't drop it

`.cargo/config.toml` **must** pass `-C link-arg=-Tlink-rp.x` (embassy-rp's linker
fragment) so the RP2040 second-stage bootloader lands at `0x10000000`. Without it
the boot2 static is garbage-collected, the bootrom rejects the image, and the board
appears completely dead (no white flash, no USB-serial — it silently drops back to
BOOTSEL). Verify with `llvm-objdump -h <elf> | grep boot2` → a 0x100-byte `.boot2`
section at VMA `0x10000000` must be present.

## Source layout

```
src/
  main.rs        Embassy entrypoint; spawns net + MQTT tasks; display scroll loop
  config.rs      WiFi/MQTT credentials and topic list
  display.rs     53×11 framebuffer, 5×7 bitmap font, draw_str / measure_str
  wifi.rs        CYW43 init + async join
  mqtt.rs        Minimal MQTT 3.1.1 subscribe loop → Channel<Message, 4> → display task
  pio_display.rs PIO/DMA driver: FM6047 init + BCD framebuffer → shift registers (the flush path)
```

## Status

Full pipeline works end to end: boot2 → PIO display → WiFi join → DHCP → MQTT
subscribe → render (static + scrolling text). The PIO shift-register driver lives
in `src/pio_display.rs` (`PioDisplay::flush` / `flush_for_ms`), driven from the
display loop in `main.rs`.

## Key constraints

- `no_std` / `no_main` — no heap allocator; use `heapless` collections
- All async tasks run on Embassy's single-threaded executor
- Font is 5px wide + 1px gap = 6px per character; display fits ~8 chars without scrolling
