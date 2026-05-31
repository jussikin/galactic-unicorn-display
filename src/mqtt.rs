/// Minimal MQTT 3.1.1 subscriber over a raw embassy-net TcpSocket.
///
/// Implements only what is needed:
///   - CONNECT → CONNACK
///   - SUBSCRIBE → SUBACK
///   - Receive PUBLISH (QoS 0)
///   - PINGREQ / PINGRESP keepalive

use defmt::{info, warn};
use embassy_net::tcp::TcpSocket;
use embassy_net::Stack;
use embassy_time::{Duration, Timer};
use heapless::String;

use crate::config::{MQTT_BROKER_IP, MQTT_CLIENT_ID, MQTT_PORT, TOPICS};
use crate::wifi::NetDriver;

/// A received MQTT message (topic index into TOPICS + payload string).
#[derive(Clone)]
pub struct Message {
    pub topic_idx: usize,
    pub payload: String<128>,
}

pub async fn run(
    stack: &'static Stack<NetDriver>,
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

        let [a, b, c, d] = MQTT_BROKER_IP;
        let addr = embassy_net::IpAddress::Ipv4(embassy_net::Ipv4Address::new(a, b, c, d));
        info!("Connecting to MQTT broker");

        if socket.connect((addr, MQTT_PORT)).await.is_err() {
            warn!("TCP connect failed — retry in 5s");
            Timer::after(Duration::from_secs(5)).await;
            continue;
        }

        if connect(&mut socket, MQTT_CLIENT_ID).await.is_err() {
            warn!("MQTT CONNECT failed — retry in 5s");
            Timer::after(Duration::from_secs(5)).await;
            continue;
        }
        info!("MQTT connected");

        let topic_list: heapless::Vec<(&str, u8), 8> = TOPICS
            .iter()
            .map(|(t, _, _)| (*t, 0u8))
            .collect();

        if subscribe(&mut socket, &topic_list).await.is_err() {
            warn!("MQTT SUBSCRIBE failed — reconnecting");
            Timer::after(Duration::from_secs(2)).await;
            continue;
        }
        info!("MQTT subscribed to {} topics", TOPICS.len());

        // Poll loop
        loop {
            match recv_packet(&mut socket).await {
                Ok(PacketType::Publish { topic, payload }) => {
                    if let Some(idx) = TOPICS.iter().position(|(t, _, _)| *t == topic.as_str()) {
                        let _ = sender.try_send(Message { topic_idx: idx, payload });
                    }
                }
                Ok(PacketType::PingReq) => {
                    // Broker sent a PINGREQ (unusual, but handle it)
                    let _ = write_all(&mut socket, &[0xD0, 0x00]).await;
                }
                Ok(PacketType::PingResp) => {} // expected response to our PINGREQ
                Ok(PacketType::Other) => {}
                Err(_) => {
                    warn!("MQTT recv error — reconnecting");
                    break;
                }
            }
        }

        Timer::after(Duration::from_secs(2)).await;
    }
}

// ---------------------------------------------------------------------------
// Packet types we care about
// ---------------------------------------------------------------------------

enum PacketType {
    Publish { topic: String<128>, payload: String<128> },
    PingReq,
    PingResp,
    Other,
}

// ---------------------------------------------------------------------------
// MQTT CONNECT
// ---------------------------------------------------------------------------

async fn connect(socket: &mut TcpSocket<'_>, client_id: &str) -> Result<(), ()> {
    // Build variable header + payload
    let mut buf = [0u8; 128];
    let mut pos = 0;

    // Protocol name "MQTT"
    buf[pos] = 0x00; buf[pos+1] = 0x04; pos += 2;
    buf[pos..pos+4].copy_from_slice(b"MQTT"); pos += 4;
    // Protocol level (3.1.1 = 4)
    buf[pos] = 4; pos += 1;
    // Connect flags: clean session
    buf[pos] = 0x02; pos += 1;
    // Keepalive: 60s
    buf[pos] = 0x00; buf[pos+1] = 60; pos += 2;
    // Client ID
    let id_bytes = client_id.as_bytes();
    buf[pos] = 0x00; buf[pos+1] = id_bytes.len() as u8; pos += 2;
    buf[pos..pos+id_bytes.len()].copy_from_slice(id_bytes); pos += id_bytes.len();

    // Fixed header
    let mut header = [0u8; 5];
    header[0] = 0x10; // CONNECT
    let len_bytes = encode_varlen(pos, &mut header[1..]);

    write_all(socket, &header[..1 + len_bytes]).await?;
    write_all(socket, &buf[..pos]).await?;

    // Expect CONNACK: 0x20 0x02 0x00 0x00
    let mut ack = [0u8; 4];
    read_exact(socket, &mut ack).await?;
    if ack[0] != 0x20 || ack[3] != 0x00 {
        return Err(());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// MQTT SUBSCRIBE
// ---------------------------------------------------------------------------

async fn subscribe(socket: &mut TcpSocket<'_>, topics: &heapless::Vec<(&str, u8), 8>) -> Result<(), ()> {
    let mut buf = [0u8; 256];
    let mut pos = 0;

    // Packet ID = 1
    buf[pos] = 0x00; buf[pos+1] = 0x01; pos += 2;

    for &(topic, qos) in topics {
        let t = topic.as_bytes();
        buf[pos] = 0x00; buf[pos+1] = t.len() as u8; pos += 2;
        buf[pos..pos+t.len()].copy_from_slice(t); pos += t.len();
        buf[pos] = qos; pos += 1;
    }

    let mut header = [0u8; 5];
    header[0] = 0x82; // SUBSCRIBE
    let len_bytes = encode_varlen(pos, &mut header[1..]);

    write_all(socket, &header[..1 + len_bytes]).await?;
    write_all(socket, &buf[..pos]).await?;

    // Expect SUBACK: 0x90 + length + packet_id(2) + return_codes(n)
    let mut fixed = [0u8; 2];
    read_exact(socket, &mut fixed).await?;
    if fixed[0] != 0x90 {
        return Err(());
    }
    let remaining = fixed[1] as usize;
    let mut skip = [0u8; 64];
    read_exact(socket, &mut skip[..remaining.min(64)]).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Receive one MQTT packet (minimal: PUBLISH QoS 0, PINGREQ, PINGRESP)
// ---------------------------------------------------------------------------

async fn recv_packet(socket: &mut TcpSocket<'_>) -> Result<PacketType, ()> {
    let mut fixed = [0u8; 2];
    read_exact(socket, &mut fixed).await?;

    let ptype = fixed[0] & 0xF0;
    let remaining = decode_varlen(socket, fixed[1]).await?;

    match ptype {
        0x30 => {
            // PUBLISH QoS 0 (no packet ID)
            let mut buf = [0u8; 256];
            let len = remaining.min(256);
            read_exact(socket, &mut buf[..len]).await?;
            // Skip any extra bytes beyond our buffer
            skip_bytes(socket, remaining.saturating_sub(256)).await?;

            // Parse topic length (first 2 bytes)
            if len < 2 {
                return Ok(PacketType::Other);
            }
            let tlen = ((buf[0] as usize) << 8) | (buf[1] as usize);
            if tlen + 2 > len {
                return Ok(PacketType::Other);
            }
            let mut topic: String<128> = String::new();
            if let Ok(s) = core::str::from_utf8(&buf[2..2+tlen]) {
                let _ = topic.push_str(&s[..s.len().min(127)]);
            }
            let mut payload: String<128> = String::new();
            if let Ok(s) = core::str::from_utf8(&buf[2+tlen..len]) {
                let _ = payload.push_str(&s[..s.len().min(127)]);
            }
            Ok(PacketType::Publish { topic, payload })
        }
        0xC0 => Ok(PacketType::PingReq),
        0xD0 => Ok(PacketType::PingResp),
        _ => {
            // Unknown/unsupported packet — skip payload and continue
            skip_bytes(socket, remaining).await?;
            Ok(PacketType::Other)
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Encode MQTT variable-length integer into `buf`. Returns number of bytes written.
fn encode_varlen(mut n: usize, buf: &mut [u8]) -> usize {
    let mut i = 0;
    loop {
        let mut byte = (n & 0x7F) as u8;
        n >>= 7;
        if n > 0 { byte |= 0x80; }
        buf[i] = byte;
        i += 1;
        if n == 0 { break; }
    }
    i
}

/// Decode MQTT variable-length integer, consuming more bytes from socket if needed.
/// `first` is the already-read first length byte.
async fn decode_varlen(socket: &mut TcpSocket<'_>, first: u8) -> Result<usize, ()> {
    let mut val = (first & 0x7F) as usize;
    if first & 0x80 == 0 {
        return Ok(val);
    }
    let mut shift = 7;
    for _ in 0..3 {
        let mut b = [0u8; 1];
        read_exact(socket, &mut b).await?;
        val |= ((b[0] & 0x7F) as usize) << shift;
        if b[0] & 0x80 == 0 {
            return Ok(val);
        }
        shift += 7;
    }
    Err(()) // malformed
}

async fn skip_bytes(socket: &mut TcpSocket<'_>, mut n: usize) -> Result<(), ()> {
    let mut buf = [0u8; 64];
    while n > 0 {
        let chunk = n.min(64);
        read_exact(socket, &mut buf[..chunk]).await?;
        n -= chunk;
    }
    Ok(())
}

async fn write_all(socket: &mut TcpSocket<'_>, mut data: &[u8]) -> Result<(), ()> {
    while !data.is_empty() {
        match socket.write(data).await {
            Ok(n) => data = &data[n..],
            Err(_) => return Err(()),
        }
    }
    Ok(())
}

async fn read_exact(socket: &mut TcpSocket<'_>, buf: &mut [u8]) -> Result<(), ()> {
    let mut pos = 0;
    while pos < buf.len() {
        match socket.read(&mut buf[pos..]).await {
            Ok(0) | Err(_) => return Err(()),
            Ok(n) => pos += n,
        }
    }
    Ok(())
}
