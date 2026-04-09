/// MQTT subscriber task.
///
/// Connects to the broker, subscribes to all configured topics, and
/// forwards received payloads through a channel to the display task.

use defmt::{error, info, warn};
use embassy_net::tcp::TcpSocket;
use embassy_net::Stack;
use embassy_time::{Duration, Timer};
use heapless::String;
use minimq::{
    embedded_nal::IpAddr, ConfigBuilder, DeferredPublication, Minimq, Publication,
    QoS, Retain,
};

use crate::config::{MQTT_BROKER_IP, MQTT_CLIENT_ID, MQTT_PORT, TOPICS};

/// A received MQTT message (topic index into TOPICS + payload string).
#[derive(Clone)]
pub struct Message {
    pub topic_idx: usize,
    pub payload: String<128>,
}

/// Subscribe to all configured topics and feed messages into `sender`.
pub async fn run(
    stack: &'static Stack<crate::wifi::NetDriver>,
    sender: embassy_sync::channel::Sender<
        'static,
        embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
        Message,
        4,
    >,
) -> ! {
    let mut rx_buf = [0u8; 1024];
    let mut tx_buf = [0u8; 1024];

    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
        socket.set_timeout(Some(Duration::from_secs(30)));

        let broker = IpAddr::from(MQTT_BROKER_IP);

        info!("Connecting to MQTT broker {}:{}", MQTT_BROKER_IP, MQTT_PORT);

        if let Err(e) = socket
            .connect((broker, MQTT_PORT).into())
            .await
        {
            warn!("TCP connect failed: {:?} — retry in 5s", e);
            Timer::after(Duration::from_secs(5)).await;
            continue;
        }

        let config = ConfigBuilder::new(MQTT_CLIENT_ID.as_bytes(), &mut [])
            .keepalive_interval(60)
            .build();

        let mut mqtt: Minimq<_, _, 256, 8> =
            Minimq::new(socket, embassy_net::dns::DnsSocket::new(stack), config);

        // Wait until MQTT is ready
        loop {
            if mqtt.poll(|_client, _topic, _payload, _props| {}).is_ok() {
                if mqtt.client().is_connected() {
                    break;
                }
            }
            Timer::after(Duration::from_millis(100)).await;
        }

        info!("MQTT connected — subscribing to {} topics", TOPICS.len());

        for (topic, _label, _color) in TOPICS {
            if let Err(e) = mqtt.client().subscribe(topic, &[]) {
                warn!("Subscribe failed for {}: {:?}", topic, e);
            } else {
                info!("Subscribed to {}", topic);
            }
        }

        // Poll loop
        loop {
            let result = mqtt.poll(|_client, topic, payload, _props| {
                let topic_str = core::str::from_utf8(topic).unwrap_or("");
                let payload_str = core::str::from_utf8(payload).unwrap_or("?");

                // Find matching topic index
                if let Some(idx) = TOPICS
                    .iter()
                    .position(|(t, _, _)| *t == topic_str)
                {
                    let mut s: String<128> = String::new();
                    let _ = s.push_str(payload_str);
                    let msg = Message { topic_idx: idx, payload: s };
                    // Best-effort send — drop if channel full
                    let _ = sender.try_send(msg);
                }
            });

            if result.is_err() {
                warn!("MQTT poll error — reconnecting");
                break;
            }

            Timer::after(Duration::from_millis(10)).await;
        }

        Timer::after(Duration::from_secs(2)).await;
    }
}
