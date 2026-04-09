#![no_std]
#![no_main]

use defmt::info;
use embassy_executor::Spawner;
use embassy_net::{Config as NetConfig, Stack, StackResources};
use embassy_rp::bind_interrupts;
use embassy_rp::peripherals::USB;
use embassy_rp::usb::{Driver, InterruptHandler as UsbInterruptHandler};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Timer};
use heapless::String;
use static_cell::StaticCell;

use {defmt_rtt as _, panic_probe as _};

mod config;
mod display;
mod mqtt;
mod wifi;

use config::{BRIGHTNESS, SCROLL_PAUSE_MS, SCROLL_STEP_MS, TOPICS};
use display::{Display, HEIGHT, WIDTH};
use mqtt::Message;

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => UsbInterruptHandler<USB>;
});

// Channel from MQTT task → display task (capacity 4)
static CHANNEL: Channel<CriticalSectionRawMutex, Message, 4> = Channel::new();

static STACK: StaticCell<Stack<wifi::NetDriver>> = StaticCell::new();
static RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    info!("Unicorn Display starting");

    // --- WiFi init ---
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
            info!("IP: {:?}", stack.config_v4());
            break;
        }
        Timer::after(Duration::from_millis(100)).await;
    }

    // --- MQTT task ---
    spawner.spawn(mqtt_task(stack)).unwrap();

    // --- Display loop ---
    display_loop().await;
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<wifi::NetDriver>) -> ! {
    stack.run().await
}

#[embassy_executor::task]
async fn mqtt_task(stack: &'static Stack<wifi::NetDriver>) -> ! {
    mqtt::run(stack, CHANNEL.sender()).await
}

/// Main display loop: shows scrolling text for each received MQTT message.
async fn display_loop() -> ! {
    let mut disp = Display::new(BRIGHTNESS);

    // Last known value per topic
    let mut values: [Option<String<128>>; 8] = Default::default();
    let mut current_topic: usize = 0;

    loop {
        // Drain any new messages
        while let Ok(msg) = CHANNEL.try_receive() {
            if msg.topic_idx < values.len() {
                values[msg.topic_idx] = Some(msg.payload);
            }
        }

        // Build display string for current topic
        let (_, label, (r, g, b)) = TOPICS[current_topic];
        let value = values[current_topic]
            .as_deref()
            .unwrap_or("---");

        // Compose: "Label: value" or just "value" if label is empty
        let mut text: String<160> = String::new();
        if !label.is_empty() {
            let _ = text.push_str(label);
            let _ = text.push_str(": ");
        }
        let _ = text.push_str(value);

        let total_px = Display::measure_str(&text);
        let needs_scroll = total_px > WIDTH as i32;

        if needs_scroll {
            scroll_text(&mut disp, &text, *r, *g, *b).await;
        } else {
            // Center the text vertically (font is 7px tall, display is 11px)
            let y = (HEIGHT as i32 - 7) / 2;
            disp.clear();
            disp.draw_str(0, y, &text, *r, *g, *b);
            // Flush (placeholder — actual PIO flush goes here)
            flush(&disp);
            Timer::after(Duration::from_millis(2000)).await;
        }

        // Cycle to next topic that has data, or just advance
        current_topic = (current_topic + 1) % TOPICS.len();
    }
}

/// Scroll `text` across the display from right to left.
async fn scroll_text(disp: &mut Display, text: &str, r: u8, g: u8, b: u8) {
    let total_px = Display::measure_str(text);
    let y = (HEIGHT as i32 - 7) / 2;

    // Start: text begins just off the right edge; end: last char exits left edge
    let start_x = WIDTH as i32;
    let end_x = -total_px;

    // Pause at start
    disp.clear();
    disp.draw_str(start_x, y, text, r, g, b);
    flush(disp);
    Timer::after(Duration::from_millis(SCROLL_PAUSE_MS)).await;

    let mut x = start_x;
    while x >= end_x {
        // Absorb any new MQTT messages while scrolling (non-blocking)
        let _ = crate::CHANNEL.try_receive();

        disp.clear();
        disp.draw_str(x, y, text, r, g, b);
        flush(disp);
        x -= 1;
        Timer::after(Duration::from_millis(SCROLL_STEP_MS)).await;
    }

    // Pause at end before moving to next topic
    Timer::after(Duration::from_millis(SCROLL_PAUSE_MS)).await;
}

/// Push the current display frame to the hardware.
///
/// TODO: wire up to the actual PIO shift-register driver.
/// The frame buffer is available via `disp.frame()`.
#[inline]
fn flush(_disp: &Display) {
    // PIO write will go here.
    // For now this is a stub so the rest of the code compiles cleanly.
}
