# ESP32 no_std variant

This is a separate `#![no_std]` port of `lancar-esp32`. The existing `lancar-esp32` project is not modified.

## Intended flow

`DHT22 -> Wi-Fi -> MQTT -> backend/cloud -> database`

The MQTT topic and measurement contract will remain compatible with the std firmware. The implementation will use the embedded Rust stack instead of ESP-IDF services:

- `esp-hal` for hardware
- `esp-wifi` and `embassy-net` for Wi-Fi/networking
- a `no_std` MQTT client
- fixed-size buffers instead of `std::thread`, heap-dependent helpers, and `std`

## Current status

The folder contains a minimal bootable `no_std` starting point. Sensor, Wi-Fi, MQTT, retry handling, and OTA are intentionally not wired yet. This keeps the current firmware as the known-good fallback while the port is developed and tested independently.

## Build target

```powershell
. $HOME\export-esp.ps1
cargo +esp build --release
```

The target is `xtensa-esp32s3-none-elf`, which is different from the ESP-IDF target used by the std project.

## Switching variants

Build and flash from one folder at a time:

- std firmware: `lancar-esp32`
- no_std firmware: `lancar-esp32-no-std`

Do not delete or overwrite the std project until the no_std variant has passed hardware tests for Wi-Fi, MQTT delivery, sensor readings, recovery, and OTA behavior.
