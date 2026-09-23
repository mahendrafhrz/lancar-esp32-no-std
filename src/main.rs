#![no_std]
#![no_main]

use core::fmt::Write;
use core::mem::MaybeUninit;

use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Config, IpAddress, Runner, Stack, StackResources};
use embassy_time::{Duration, Timer};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Level, OutputOpenDrain, Pull};
use esp_hal::rng::Rng;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::Config as HalConfig;
use esp_hal_embassy::main;
use esp_wifi::wifi::{ClientConfiguration, WifiController, WifiDevice, WifiStaDevice};
use esp_wifi::EspWifiController;
use heapless::String;
use tinymqtt::MqttClient;

const fn app_text<const N: usize>(value: &[u8]) -> [core::ffi::c_char; N] {
    let mut result = [0; N];
    let mut index = 0;
    while index < value.len() && index + 1 < N {
        result[index] = value[index] as core::ffi::c_char;
        index += 1;
    }
    result
}

#[repr(C)]
struct AppDescription {
    magic_word: u32,
    secure_version: u32,
    reserved1: [u32; 2],
    version: [core::ffi::c_char; 32],
    project_name: [core::ffi::c_char; 32],
    build_time: [core::ffi::c_char; 16],
    build_date: [core::ffi::c_char; 16],
    idf_version: [core::ffi::c_char; 32],
    elf_sha256: [u8; 32],
    min_efuse_revision: u16,
    max_efuse_revision: u16,
    mmu_page_size: u8,
    reserved3: [u8; 3],
    reserved2: [u32; 18],
}

#[used]
#[unsafe(link_section = ".rodata_desc.appdesc")]
#[unsafe(export_name = "esp_app_desc")]
static APP_DESCRIPTION: AppDescription = AppDescription {
    magic_word: 0xABCD5432,
    secure_version: 0,
    reserved1: [0; 2],
    version: app_text(b"0.1.0"),
    project_name: app_text(b"lancar-esp32-no-std"),
    build_time: app_text(b"00:00:00"),
    build_date: app_text(b"2026-09-22"),
    idf_version: app_text(b"no_std"),
    elf_sha256: [0; 32],
    min_efuse_revision: 0,
    max_efuse_revision: 0,
    mmu_page_size: 16,
    reserved3: [0; 3],
    reserved2: [0; 18],
};

const DEFAULT_DEVICE_ID: &str = "ESP32-001";
const DEFAULT_INTERVAL_SECONDS: u32 = 10;
const MAX_SAMPLE_ID: u32 = 18;
const MQTT_TOPIC: &str = "enose/ESP32-001/measurement";
const MQTT_HOST: &str = "broker.emqx.io";
const MQTT_PORT: u16 = 1883;
const HEAP_SIZE: usize = 128 * 1024;

static mut HEAP: MaybeUninit<[u8; HEAP_SIZE]> = MaybeUninit::uninit();
static NET_RESOURCES: static_cell::StaticCell<StackResources<3>> = static_cell::StaticCell::new();
static WIFI_INIT: static_cell::StaticCell<EspWifiController<'static>> =
    static_cell::StaticCell::new();

fn init_heap() {
    unsafe {
        esp_alloc::HEAP.add_region(esp_alloc::HeapRegion::new(
            core::ptr::addr_of_mut!(HEAP).cast::<u8>(),
            HEAP_SIZE,
            esp_alloc::MemoryCapability::Internal.into(),
        ));
    }
}

fn device_id() -> &'static str {
    option_env!("DEVICE_ID").unwrap_or(DEFAULT_DEVICE_ID)
}

fn interval_seconds() -> u32 {
    option_env!("INTERVAL_SECONDS")
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_INTERVAL_SECONDS)
}

fn wifi_ssid() -> &'static str {
    option_env!("WIFI_SSID").unwrap_or("CHANGE_ME")
}

fn wifi_password() -> &'static str {
    option_env!("WIFI_PASSWORD").unwrap_or("CHANGE_ME")
}

fn wait_level(pin: &OutputOpenDrain<'_>, level: bool, timeout_us: u32, delay: &mut Delay) -> bool {
    let mut elapsed = 0;
    while pin.is_high() != level {
        if elapsed >= timeout_us {
            return false;
        }
        delay.delay_micros(1);
        elapsed += 1;
    }
    true
}

fn read_dht22(pin: &mut OutputOpenDrain<'_>, delay: &mut Delay) -> Option<(i16, i16)> {
    pin.set_low();
    delay.delay_millis(2);
    pin.set_high();
    delay.delay_micros(30);

    if !wait_level(pin, false, 100, delay)
        || !wait_level(pin, true, 100, delay)
        || !wait_level(pin, false, 100, delay)
    {
        return None;
    }

    let mut data = [0u8; 5];
    for byte in &mut data {
        for bit in (0..8).rev() {
            if !wait_level(pin, true, 100, delay) {
                return None;
            }
            delay.delay_micros(40);
            if pin.is_high() {
                *byte |= 1 << bit;
            }
            if !wait_level(pin, false, 100, delay) {
                return None;
            }
        }
    }

    let checksum = data[0]
        .wrapping_add(data[1])
        .wrapping_add(data[2])
        .wrapping_add(data[3]);
    if checksum != data[4] {
        return None;
    }

    let humidity_x10 = u16::from_be_bytes([data[0], data[1]]) as i16;
    let raw_temperature = u16::from_be_bytes([data[2] & 0x7f, data[3]]) as i16;
    let temperature_x10 = if data[2] & 0x80 != 0 {
        -raw_temperature
    } else {
        raw_temperature
    };
    Some((temperature_x10, humidity_x10))
}

fn write_decimal_x10<const N: usize>(output: &mut String<N>, value: i16) {
    let negative = value < 0;
    let absolute = value.unsigned_abs();
    if negative {
        let _ = output.push('-');
    }
    let _ = write!(output, "{}.{:01}", absolute / 10, absolute % 10);
}

fn write_mqtt_publish<'a>(buffer: &'a mut [u8], topic: &str, payload: &[u8]) -> Option<&'a [u8]> {
    let remaining_length = 2usize
        .checked_add(topic.len())?
        .checked_add(1)?
        .checked_add(payload.len())?;
    if topic.len() > u16::MAX as usize || remaining_length > 268_435_455 {
        return None;
    }

    let mut cursor = 0;
    buffer[cursor] = 0x30;
    cursor += 1;

    let mut encoded_length = remaining_length as u32;
    loop {
        let mut byte = (encoded_length % 128) as u8;
        encoded_length /= 128;
        if encoded_length > 0 {
            byte |= 0x80;
        }
        if cursor >= buffer.len() {
            return None;
        }
        buffer[cursor] = byte;
        cursor += 1;
        if encoded_length == 0 {
            break;
        }
    }

    let end = cursor
        .checked_add(2)?
        .checked_add(topic.len())?
        .checked_add(1)?
        .checked_add(payload.len())?;
    if end > buffer.len() {
        return None;
    }

    buffer[cursor..cursor + 2].copy_from_slice(&(topic.len() as u16).to_be_bytes());
    cursor += 2;
    buffer[cursor..cursor + topic.len()].copy_from_slice(topic.as_bytes());
    cursor += topic.len();
    buffer[cursor] = 0;
    cursor += 1;
    buffer[cursor..end].copy_from_slice(payload);

    Some(&buffer[..end])
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static, WifiStaDevice>>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn mqtt_task(
    stack: Stack<'static>,
    mut wifi_controller: WifiController<'static>,
    mut sensor_pin: OutputOpenDrain<'static>,
    mut delay: Delay,
) -> ! {
    esp_println::println!("network task started");

    wifi_controller.start().unwrap();
    loop {
        esp_println::println!("wifi: waiting for connection to {}", wifi_ssid());
        match wifi_controller.connect_async().await {
            Ok(()) => {
                esp_println::println!("wifi: connected");
                break;
            }
            Err(_) => {
                esp_println::println!("wifi: connection failed; retrying");
                Timer::after(Duration::from_secs(5)).await;
            }
        }
    }

    stack.wait_config_up().await;
    esp_println::println!("dhcp ready");

    let addresses = match stack
        .dns_query(MQTT_HOST, embassy_net::dns::DnsQueryType::A)
        .await
    {
        Ok(addresses) => {
            esp_println::println!("broker DNS resolved");
            addresses
        }
        Err(_) => loop {
            esp_println::println!("broker DNS failed; retrying");
            Timer::after(Duration::from_secs(5)).await;
        },
    };

    let broker_ip = match addresses.first() {
        Some(IpAddress::Ipv4(address)) => IpAddress::Ipv4(*address),
        _ => loop {
            Timer::after(Duration::from_secs(5)).await;
        },
    };

    let mut socket_rx_buffer = [0u8; 2048];
    let mut tx_buffer = [0u8; 2048];
    let mut packet_buffer = [0u8; 2048];
    let mut publish_buffer = [0u8; 1024];
    let mut socket = TcpSocket::new(stack, &mut socket_rx_buffer, &mut tx_buffer);
    if socket.connect((broker_ip, MQTT_PORT)).await.is_err() {
        esp_println::println!("MQTT TCP connect failed");
        loop {
            Timer::after(Duration::from_secs(5)).await;
        }
    }
    esp_println::println!("MQTT TCP connected");

    let mut client: MqttClient<1024> = MqttClient::new();
    let connect_packet = match client.connect(device_id(), None) {
        Ok(packet) => packet,
        Err(_) => loop {
            Timer::after(Duration::from_secs(5)).await;
        },
    };
    let _ = socket.write(connect_packet).await;

    let response_length = match socket.read(&mut packet_buffer).await {
        Ok(length) => length,
        Err(_) => 0,
    };
    if response_length == 0
        || client
            .receive_packet(&packet_buffer[..response_length], |_, _, _| {})
            .is_err()
    {
        esp_println::println!("MQTT CONNACK failed");
        loop {
            Timer::after(Duration::from_secs(5)).await;
        }
    }
    esp_println::println!("MQTT connected");

    let mut sample_id = 1u32;
    loop {
        let (temperature_x10, humidity_x10) = match read_dht22(&mut sensor_pin, &mut delay) {
            Some(values) => values,
            None => {
                esp_println::println!("sensor: DHT22 read failed; publishing zero fallback");
                (0, 0)
            }
        };
        let mut payload: String<256> = String::new();
        let _ = payload.push_str("{\"sample_id\":");
        let _ = write!(payload, "{}", sample_id);
        let _ = payload.push_str(",\"score\":");
        write_decimal_x10(&mut payload, temperature_x10);
        let _ = payload.push_str(",\"accuracy\":");
        write_decimal_x10(&mut payload, humidity_x10);
        let _ = write!(
            payload,
            ",\"source\":\"dht22\",\"device_id\":\"{}\",\"features\":[{},{},0,0,0,0,0,0]}}",
            device_id(),
            temperature_x10,
            humidity_x10
        );

        if let Some(packet) =
            write_mqtt_publish(&mut publish_buffer, MQTT_TOPIC, payload.as_bytes())
        {
            if socket.write(packet).await.is_err() {
                esp_println::println!("MQTT publish failed");
                socket.abort();
                let _ = socket.connect((broker_ip, MQTT_PORT)).await;
            } else {
                esp_println::println!("measurement {} published", sample_id);
            }
        }

        sample_id = if sample_id >= MAX_SAMPLE_ID {
            1
        } else {
            sample_id + 1
        };
        Timer::after(Duration::from_secs(interval_seconds() as u64)).await;
    }
}

#[main]
async fn main(spawner: Spawner) -> ! {
    esp_println::println!("boot: starting application");
    init_heap();
    esp_println::println!("boot: heap initialized");

    let hal_config = HalConfig::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(hal_config);
    esp_println::println!("boot: HAL initialized");

    let embassy_timers = TimerGroup::new(peripherals.TIMG1);
    esp_hal_embassy::init(embassy_timers.timer0);
    esp_println::println!("boot: Embassy timer initialized");

    let wifi_timers = TimerGroup::new(peripherals.TIMG0);
    esp_println::println!("wifi: initializing controller");
    let wifi_init = WIFI_INIT.init(
        esp_wifi::init(
            wifi_timers.timer0,
            Rng::new(peripherals.RNG),
            peripherals.RADIO_CLK,
        )
        .unwrap(),
    );
    esp_println::println!("wifi: controller initialized");

    let wifi_config = ClientConfiguration {
        ssid: wifi_ssid().try_into().unwrap(),
        password: wifi_password().try_into().unwrap(),
        ..Default::default()
    };
    let (wifi_device, wifi_controller) =
        esp_wifi::wifi::new_with_config(wifi_init, peripherals.WIFI, wifi_config).unwrap();
    esp_println::println!("wifi: driver initialized");
    let (stack, runner) = embassy_net::new(
        wifi_device,
        Config::dhcpv4(Default::default()),
        NET_RESOURCES.init(StackResources::new()),
        0x1234_5678,
    );

    let sensor_pin = OutputOpenDrain::new(peripherals.GPIO18, Level::High, Pull::Up);
    let sensor_delay = Delay::new();

    spawner.spawn(net_task(runner)).unwrap();
    spawner
        .spawn(mqtt_task(stack, wifi_controller, sensor_pin, sensor_delay))
        .unwrap();
    esp_println::println!("boot: network tasks spawned");

    loop {
        Timer::after(Duration::from_secs(3600)).await;
    }
}
