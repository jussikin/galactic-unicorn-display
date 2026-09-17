#![no_std]
#![no_main]

use defmt::info;
use embassy_executor::Spawner;
use embassy_net::{Config as NetConfig, Stack, StackResources};
use embassy_rp::bind_interrupts;
use embassy_rp::peripherals::{PIO0, USB};
use embassy_rp::pio::InterruptHandler as PioInterruptHandler;
use embassy_rp::usb::InterruptHandler as UsbInterruptHandler;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Timer};
use heapless::String;
use static_cell::StaticCell;

use {defmt_rtt as _, panic_probe as _};

mod config;
mod display;
mod mqtt;
mod pio_display;
mod wifi;

use config::{BRIGHTNESS, SCROLL_PAUSE_MS, SCROLL_STEP_MS, TOPICS, TOPIC_HOLD_MS};
use display::{Display, FONT_HEIGHT, HEIGHT, WIDTH};
use mqtt::Message;
use pio_display::PioDisplay;

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => UsbInterruptHandler<USB>;
    PIO0_IRQ_0  => PioInterruptHandler<PIO0>;
});

/// Vertical offset for drawn text. The 5×11 font fills the panel, so this is 0;
/// it stays computed so a shorter font would still be centred.
const TEXT_Y: i32 = (HEIGHT as i32 - FONT_HEIGHT as i32) / 2;

// Channel from MQTT task → display task (capacity 4)
static CHANNEL: Channel<CriticalSectionRawMutex, Message, 4> = Channel::new();

static STACK: StaticCell<Stack<wifi::NetDriver>> = StaticCell::new();
static RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    info!("Unicorn Display starting");

    // --- PIO display driver (PIO0, DMA_CH1) ---
    let mut pio_disp = PioDisplay::new(
        p.PIO0,
        Irqs,
        p.DMA_CH1,
        p.PIN_13, // CLK
        p.PIN_14, // DATA
        p.PIN_15, // LATCH
        p.PIN_16, // BLANK
        p.PIN_17, // ROW0
        p.PIN_18, // ROW1
        p.PIN_19, // ROW2
        p.PIN_20, // ROW3
    );

    // Boot indicator: flash all pixels white at FULL brightness.
    // Uses brightness=1.0 so gamma produces 16383 (all BCD frames lit),
    // giving ~9% row duty cycle — clearly visible even in ambient light.
    {
        let mut disp = Display::new(1.0);
        for y in 0..display::HEIGHT {
            for x in 0..display::WIDTH {
                disp.set_pixel(x, y, 255, 255, 255);
            }
        }
        pio_disp.flush_for_ms(&disp.frame(), 2000).await;
    }

    // --- WiFi init (PIO1, DMA_CH0) ---
    let (net_device, mut control) = wifi::init(
        p.PIN_23, p.PIN_25, p.PIO1, p.PIN_24, p.PIN_29, p.DMA_CH0, &spawner,
    )
    .await;

    wifi::join(&mut control).await;

    // --- Network stack ---
    let config = NetConfig::dhcpv4(Default::default());
    let seed = 0x8A3F_1C2E_5B7D_4F09u64; // TODO: derive from unique ID for better randomness
    let stack = STACK.init(Stack::new(
        net_device,
        config,
        RESOURCES.init(StackResources::new()),
        seed,
    ));

    spawner.spawn(net_task(stack)).unwrap();

    // Wait for DHCP
    info!("Waiting for DHCP...");
    loop {
        if stack.is_config_up() {
            info!("DHCP acquired");
            break;
        }
        Timer::after(Duration::from_millis(100)).await;
    }

    // --- MQTT task ---
    spawner.spawn(mqtt_task(stack)).unwrap();

    // --- Display loop (runs forever) ---
    display_loop(&mut pio_disp).await;
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<wifi::NetDriver>) -> ! {
    stack.run().await
}

#[embassy_executor::task]
async fn mqtt_task(stack: &'static Stack<wifi::NetDriver>) -> ! {
    mqtt::run(stack, CHANNEL.sender()).await
}

/// Drain the MQTT channel into `values`.
/// Returns a bitmask of the topic indices whose value changed.
fn drain_messages(values: &mut [Option<String<128>>; 8]) -> u8 {
    let mut updated = 0u8;
    while let Ok(msg) = CHANNEL.try_receive() {
        if msg.topic_idx < values.len() {
            values[msg.topic_idx] = Some(msg.payload);
            updated |= 1 << msg.topic_idx;
        }
    }
    updated
}

/// Show `frame` for `ms`, draining the channel as we go.
/// Returns early (true) as soon as `topic_idx`'s value is updated, so a fresh
/// value replaces the one on screen instead of waiting out the full duration.
async fn show_while_draining(
    driver: &mut PioDisplay<'_>,
    disp: &Display,
    ms: u64,
    topic_idx: usize,
    values: &mut [Option<String<128>>; 8],
) -> bool {
    let frame = disp.frame();
    let mut left = ms;
    while left > 0 {
        let chunk = left.min(50);
        driver.flush_for_ms(&frame, chunk).await;
        left -= chunk;
        if drain_messages(values) & (1 << topic_idx) != 0 {
            return true;
        }
    }
    false
}

/// Main display loop: shows scrolling text for each received MQTT message.
async fn display_loop(driver: &mut PioDisplay<'_>) -> ! {
    let mut disp = Display::new(BRIGHTNESS);

    // Last known value per topic
    let mut values: [Option<String<128>>; 8] = Default::default();
    let mut current_topic: usize = 0;

    loop {
        drain_messages(&mut values);

        // Build display string for current topic
        let (_, label, (r, g, b)) = TOPICS[current_topic];
        let value = values[current_topic].as_deref().unwrap_or("---");

        // Compose "Label: value" or just "value" if label is empty
        let mut text: String<160> = String::new();
        if !label.is_empty() {
            let _ = text.push_str(label);
            let _ = text.push_str(": ");
        }
        let _ = text.push_str(value);

        let total_px = Display::measure_str(&text);

        let interrupted = if total_px > WIDTH as i32 {
            scroll_text(driver, &mut disp, &text, r, g, b, current_topic, &mut values).await
        } else {
            let y = TEXT_Y;
            disp.clear();
            disp.draw_str(0, y, &text, r, g, b);
            show_while_draining(driver, &disp, TOPIC_HOLD_MS, current_topic, &mut values).await
        };

        // If the current topic just got a new value, redraw it instead of
        // moving on — otherwise the update would not be seen until the next
        // full rotation through the topic list.
        if !interrupted {
            current_topic = (current_topic + 1) % TOPICS.len();
        }
    }
}

/// Scroll `text` across the display from right to left.
/// Returns true if `topic_idx` received a new value and the scroll was aborted.
async fn scroll_text(
    driver: &mut PioDisplay<'_>,
    disp: &mut Display,
    text: &str,
    r: u8,
    g: u8,
    b: u8,
    topic_idx: usize,
    values: &mut [Option<String<128>>; 8],
) -> bool {
    let total_px = Display::measure_str(text);
    let y = TEXT_Y;

    let start_x = WIDTH as i32;
    let end_x   = -total_px;

    // Initial pause with text visible at start position
    disp.clear();
    disp.draw_str(start_x, y, text, r, g, b);
    if show_while_draining(driver, disp, SCROLL_PAUSE_MS, topic_idx, values).await {
        return true;
    }

    let mut x = start_x;
    while x >= end_x {
        disp.clear();
        disp.draw_str(x, y, text, r, g, b);
        let frame = disp.frame();
        driver.flush_for_ms(&frame, SCROLL_STEP_MS).await;
        if drain_messages(values) & (1 << topic_idx) != 0 {
            return true;
        }
        x -= 1;
    }

    disp.clear();
    show_while_draining(driver, disp, SCROLL_PAUSE_MS, topic_idx, values).await
}
