use cyw43::JoinOptions;
use cyw43_pio::PioSpi;
use defmt::{error, info, warn};
use embassy_rp::peripherals::{DMA_CH0, PIN_23, PIN_24, PIN_25, PIN_29, PIO1};
use embassy_rp::pio::Pio;
use embassy_rp::gpio::{Level, Output};
use embassy_time::{Duration, Timer};
use static_cell::StaticCell;

use crate::config::{WIFI_PASSWORD, WIFI_SSID};

// Firmware blobs must be included at build time from the cyw43-firmware crate
// (or placed manually in the project).
const FIRMWARE: &[u8] = include_bytes!("../firmware/43439A0.bin");
const CLM: &[u8] = include_bytes!("../firmware/43439A0_clm.bin");

pub type NetDriver = cyw43::NetDriver<'static>;

static STATE: StaticCell<cyw43::State> = StaticCell::new();

pub async fn init(
    pwr: PIN_23,
    cs: PIN_25,
    pio: PIO1,
    dio: PIN_24,
    clk: PIN_29,
    dma: DMA_CH0,
    spawner: &embassy_executor::Spawner,
) -> (NetDriver, cyw43::Control<'static>) {
    let pwr = Output::new(pwr, Level::Low);
    let cs = Output::new(cs, Level::High);
    let mut pio = Pio::new(pio, cyw43_pio::Irqs);
    let spi = PioSpi::new(&mut pio.common, pio.sm0, pio.irq0, cs, dio, clk, dma);

    let state = STATE.init(cyw43::State::new());
    let (net_device, mut control, runner) =
        cyw43::new(state, pwr, spi, FIRMWARE).await;

    spawner.spawn(cyw43_task(runner)).unwrap();

    control.init(CLM).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    (net_device, control)
}

pub async fn join(control: &mut cyw43::Control<'static>) {
    loop {
        info!("Joining WiFi \"{}\"", WIFI_SSID);
        match control
            .join(WIFI_SSID, JoinOptions::new(WIFI_PASSWORD.as_bytes()))
            .await
        {
            Ok(_) => {
                info!("WiFi joined");
                break;
            }
            Err(e) => {
                warn!("WiFi join failed: {:?} — retrying in 5s", e);
                Timer::after(Duration::from_secs(5)).await;
            }
        }
    }
}

#[embassy_executor::task]
async fn cyw43_task(runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO1, 0, DMA_CH0>>) -> ! {
    runner.run().await
}
