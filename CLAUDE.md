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

## Message markup

MQTT payloads may carry inline tags that change colour and font size mid-string.
Parsed in `src/display.rs` (`walk_markup`); tags consume no horizontal space.

```sh
mosquitto_pub -t makkari/lampo -m '{red}m: {white}{big}21.5'
mosquitto_pub -t kello         -m '{#ff8800}13:52'
```

| Tag | Alias | Effect |
|---|---|---|
| `{red}` `{green}` `{blue}` `{white}` | `{r}` `{g}` `{b}` `{w}` | colour |
| `{yellow}` `{cyan}` `{magenta}` `{orange}` | `{y}` `{c}` `{m}` `{o}` | colour |
| `{#ff8800}` | — | arbitrary RGB hex |
| `{big}` | `{B}` | 11px font (default) |
| `{small}` | `{S}` | 7px font, centred vertically |
| `{reset}` | — | back to the topic's own colour, big font |
| `{{` | — | a literal `{` |

Aliases are case-sensitive: lowercase letters are colours, `B`/`S` are sizes.
Text starts in the topic's colour from `TOPICS`, which is also what `{reset}`
returns to. An unknown or unterminated tag is drawn as literal text rather than
swallowed, so a typo shows up on the panel.

Spacing is proportional: `glyph_metrics` in `display.rs` derives each advance
from the glyph's inked columns, so `:` and `.` take 3px where `M` takes 6, and a
space is 3px. Both sizes use the same widths — `{small}` changes only the
height. How much fits therefore depends on the text: `13:52-12.5` is 52px of the
panel's 53, while eight capitals do not fit.

Widths are derived from the bitmaps rather than a table, so `measure_markup`
and `draw_markup` cannot disagree — the scroll decision depends on them
matching.

## Build & flash

The on-board debug header is broken, so flash over USB in BOOTSEL mode (not probe-rs).
The cargo runner is set to `elf2uf2-rs -d`, so flashing is one step:

```sh
# Hold BOOTSEL and plug in the board → it mounts as RPI-RP2.
cargo run --release   # builds, converts to UF2, deploys; board reboots into firmware
```

> The firmware exposes no USB reset interface, so there is no auto-reboot into
> BOOTSEL — manually enter BOOTSEL before each flash. If the board is not in
> BOOTSEL, `elf2uf2-rs` exits with "Unable to find mounted pico".
>
> Manual fallback (equivalent): `elf2uf2-rs target/thumbv6m-none-eabi/release/unicorn-display /tmp/unicorn.uf2 && cp /tmp/unicorn.uf2 /Volumes/RPI-RP2/`

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
  display.rs     53×11 framebuffer, 5×11 + 5×7 fonts, markup parser, draw_markup / measure_markup
  wifi.rs        CYW43 init + async join
  mqtt.rs        Minimal MQTT 3.1.1 subscribe loop → Channel<Message, 4> → display task
  pio_display.rs PIO/DMA driver: FM6047 init + BCD framebuffer → shift registers (the flush path)
tools/
  genfont.py     glyph source (ASCII art) for both fonts; --preview / --write
```

The font tables in `display.rs` are generated — edit `tools/genfont.py` and run
`python3 tools/genfont.py --write` rather than hand-editing hex.

## Status

Full pipeline works end to end: boot2 → PIO display → WiFi join → DHCP → MQTT
subscribe → render (static + scrolling text). The PIO shift-register driver lives
in `src/pio_display.rs` (`PioDisplay::flush` / `flush_for_ms`), driven from the
display loop in `main.rs`.

## Key constraints

- `no_std` / `no_main` — no heap allocator; use `heapless` collections
- All async tasks run on Embassy's single-threaded executor
- Font is 5px wide + 1px gap = 6px per character; display fits ~8 chars without scrolling
