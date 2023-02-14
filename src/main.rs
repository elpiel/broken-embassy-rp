#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

use {defmt_rtt as _, panic_probe as _};

use core::convert::Infallible;

use defmt::{debug, info, unwrap, Display2Format, Format};
use embassy_executor::Executor;
use embassy_rp::{
    adc::{Adc, Config as AdcConfig},
    gpio::{Level, Output},
    interrupt,
    multicore::{spawn_core1, Stack},
    peripherals::PIN_25,
    spi::Error as SpiError,
};
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex,
    mutex::Mutex,
    pubsub::{PubSubChannel, Publisher, Subscriber, WaitResult},
};
use embassy_time::{Duration, Timer};
use heapless::String;

use static_cell::StaticCell;

/// Core 0
///
/// The core which will handle incoming data and pass it to the Netbox from:
///
/// - w5500 - receive message over network
/// - I2C
/// - UART
/// - SPI? possibly in the future
static APPLICATION_EXECUTOR: StaticCell<Executor> = StaticCell::new();

/// Core 1
///
/// Main core which will handle the Netbox Pub/Sub logic with `broadcaste-rs`
static NETBOX_EXECUTOR: StaticCell<Executor> = StaticCell::new();
/// Core 1 stack of 4KiB
static mut CORE1_STACK: Stack<4096> = Stack::new();

#[derive(Clone, Copy, Debug, Format)]
pub enum Message {
    Led(LedState),
    /// Placeholder for writing a log message to SD card
    Log(&'static str),
    /// In Degrees
    Temperature(f32),
    /// Placeholder for handling networking messages from w5500 network stack
    NetworkSend(&'static str),
    NetworkReceived(([u8; 256], usize)),
}

/// The channel to be used when devices receive and send messages
static PUB_SUB_CHANNEL: PubSubChannel<CriticalSectionRawMutex, Message, 256, 10, 10> =
    PubSubChannel::new();

#[derive(Clone, Copy, Debug, Format)]
pub enum LedState {
    On,
    Off,
}

#[cortex_m_rt::entry]
fn main() -> ! {
    let p = embassy_rp::init(Default::default());
    let led = Output::new(p.PIN_25, Level::Low);
    let irq = interrupt::take!(ADC_IRQ_FIFO);
    let adc = Adc::new(p.ADC, irq, AdcConfig::default());

    spawn_core1(p.CORE1, unsafe { &mut CORE1_STACK }, move || {
        let netbox_executor = NETBOX_EXECUTOR.init(Executor::new());

        netbox_executor.run(move |spawner| {
            let netbox_subscriber = unwrap!(PUB_SUB_CHANNEL.subscriber());
            info!("here");

            unwrap!(spawner.spawn(core1_netbox_task(led, netbox_subscriber,)));

            info!("here after");
        });
    });

    let application_executor = APPLICATION_EXECUTOR.init(Executor::new());
    application_executor.run(|spawner| {
        // SD card logging
        {
            let logging_publisher = unwrap!(PUB_SUB_CHANNEL.publisher());
            spawner.must_spawn(mock_sd_logging_publisher_task(logging_publisher));
        }

        // LED
        {
            let led_publisher = unwrap!(PUB_SUB_CHANNEL.publisher());

            // spawn LED publisher task
            spawner.must_spawn(led_publisher_task(led_publisher));
        }
    });
}

#[embassy_executor::task]
async fn led_publisher_task(
    publisher: Publisher<'static, CriticalSectionRawMutex, Message, 256, 10, 10>,
) {
    debug!("Spawned LED publisher task");
    loop {
        info!("before publish ON");
        publisher.publish(Message::Led(LedState::On)).await;
        info!("published ON");
        Timer::after(Duration::from_millis(700)).await;
        publisher.publish(Message::Led(LedState::Off)).await;
        info!("published OFF");
        Timer::after(Duration::from_millis(700)).await;
    }
}

#[embassy_executor::task]
async fn mock_sd_logging_publisher_task(
    publisher: Publisher<'static, CriticalSectionRawMutex, Message, 256, 10, 10>,
) {
    debug!("Spawned mock micro sd logging publisher task");
    let message = "Hello world from Raspberry Pi Pico and length of bytes - 100 in order to benchmark SD card writing..\n";
    assert_eq!(
        101,
        message.as_bytes().len(),
        "Should have 101 bytes because of the new line (\\n)"
    );

    loop {
        publisher.publish(Message::Log(message)).await;
        Timer::after(Duration::from_millis(300)).await;
    }
}

#[embassy_executor::task]
async fn core1_netbox_task(
    mut led: Output<'static, PIN_25>,
    mut subscriber: Subscriber<'static, CriticalSectionRawMutex, Message, 256, 10, 10>,
) {
    debug!("Spawned netbox task");

    loop {
        info!("before next_message");
        match subscriber.next_message().await {
            WaitResult::Lagged(missed_count) => {
                info!("Subscriber is lagging, {} missed messages", missed_count)
            }
            WaitResult::Message(message) => match message {
                Message::Led(led_status) => {
                    info!("Receiving message: {:?}", led_status);
                    match led_status {
                        LedState::On => {
                            info!("Led ON!");
                            led.set_high()
                        }
                        LedState::Off => {
                            info!("Led OFF!");
                            led.set_low()
                        }
                    }
                }
                Message::Log(log_message) => {
                    info!("Logging: {}", log_message);
                }
                Message::Temperature(degrees) => {
                    // Used only for logging the temperature!
                    let log_message = {
                        let mut string_buf: String<256> = String::new();
                        core::fmt::write(
                            &mut string_buf,
                            format_args!("Logging temperature: {} degrees", degrees),
                        )
                        .expect("Should not overflow String");

                        string_buf
                    };

                    let display_degrees = Display2Format(&log_message);
                    info!("{}", display_degrees);
                }
                Message::NetworkSend(packet) => {
                    info!("received packet for sending over the network");

                    // NETWORK_CHANNEL.send(packet).await;
                }
                Message::NetworkReceived((buf, received_bytes)) => {
                    info!("Received bytes from network '{:?}'", buf[..received_bytes],);
                }
            },
        }
        info!("message handled")
    }
}
