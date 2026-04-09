// WiFi credentials
pub const WIFI_SSID: &str = "your_wifi_ssid";
pub const WIFI_PASSWORD: &str = "your_wifi_password";

// MQTT broker
pub const MQTT_BROKER_IP: [u8; 4] = [192, 168, 1, 100];
pub const MQTT_PORT: u16 = 1883;
pub const MQTT_CLIENT_ID: &str = "galactic_unicorn";

// Topics to subscribe — (topic, label, color RGB)
// Color is used when rendering the label prefix on the display
pub const TOPICS: &[(&str, &str, (u8, u8, u8))] = &[
    ("home/temperature", "Temp", (255, 100,  50)),
    ("home/humidity",    "Humi", ( 50, 150, 255)),
    ("home/status",      "",     (100, 255, 100)),
];

// Display
pub const BRIGHTNESS: f32 = 0.4; // 0.0 – 1.0
pub const SCROLL_STEP_MS: u64 = 40; // ms per pixel shift
pub const SCROLL_PAUSE_MS: u64 = 1000; // pause at ends
