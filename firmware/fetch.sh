#!/bin/sh
# Download CYW43439 WiFi firmware required by Pico W.
# Run this once before building.
set -e
BASE="https://github.com/embassy-rs/embassy/raw/main/cyw43-firmware"
curl -L -o 43439A0.bin     "$BASE/43439A0.bin"
curl -L -o 43439A0_clm.bin "$BASE/43439A0_clm.bin"
echo "Firmware downloaded."
