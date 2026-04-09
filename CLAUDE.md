# Unicorn Display

Rust firmware for the **Pimoroni Galactic Unicorn** — a 53×11 RGB LED matrix with an embedded Raspberry Pi Pico W.
Subscribes to MQTT topics and renders scrolling/static text on the display.

## Hardware

- **MCU:** RP2040 (Cortex-M0+), target `thumbv6m-none-eabi`
- **Display:** 53×11 RGB LED matrix driven by PIO shift registers
  - DATA=GPIO8, CLK=GPIO9, LATCH=GPIO10, BLANK=GPIO11, row-select=GPIO19–22
- **WiFi:** CYW43439 chip via `cyw43` + `cyw43-pio` (Pico W)

## Stack

| Concern | Crate |
|---|---|
| Async runtime | `embassy-executor` |
| RP2040 HAL | `embassy-rp` |
| Networking | `embassy-net` |
| WiFi driver | `cyw43` + `cyw43-pio` |
| MQTT client | `minimq` |
| Logging | `defmt` + `defmt-rtt` |

## Build setup (one-time)

```sh
rustup target add thumbv6m-none-eabi
cargo install flip-link probe-rs-tools
cd firmware && sh fetch.sh   # downloads CYW43 firmware blobs
```

## Configuration

Edit `src/config.rs` before flashing:
- `WIFI_SSID` / `WIFI_PASSWORD`
- `MQTT_BROKER_IP` (as `[u8; 4]`)
- `TOPICS` — list of `(topic, label, (r, g, b))` tuples

## Build & flash

```sh
cargo run --release   # flashes via probe-rs (needs a debug probe)
```

## Source layout

```
src/
  main.rs        Embassy entrypoint; spawns net + MQTT tasks; display scroll loop
  config.rs      WiFi/MQTT credentials and topic list
  display.rs     53×11 framebuffer, 5×7 bitmap font, draw_str / measure_str
  wifi.rs        CYW43 init + async join
  mqtt.rs        minimq subscribe loop → Channel<Message, 4> → display task
```

## What is not yet implemented

- `flush()` in `main.rs` — PIO shift-register driver that pushes the framebuffer
  to the hardware GPIOs listed above. This is the main outstanding piece.

## Key constraints

- `no_std` / `no_main` — no heap allocator; use `heapless` collections
- All async tasks run on Embassy's single-threaded executor
- Font is 5px wide + 1px gap = 6px per character; display fits ~8 chars without scrolling
