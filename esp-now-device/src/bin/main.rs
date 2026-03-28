#![no_std]
#![no_main]
#![deny(clippy::mem_forget)]
#![deny(clippy::large_stack_frames)]

use esp_hal::clock::CpuClock;
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::timer::timg::TimerGroup;
use esp_radio::esp_now::{EspNowReceiver, EspNowSender, BROADCAST_ADDRESS};

use embassy_executor::Spawner;
use embassy_futures::join::join4;

use bbqueue::BBQueue;
use bbqueue::traits::coordination::cas::AtomicCoord;
use bbqueue::traits::notifier::maitake::MaiNotSpsc;
use bbqueue::traits::storage::Inline;

use ergot::prelude::*;
use ergot::interface_manager::utils::framed_stream;
use ergot::interface_manager::FrameProcessor;
use mutex::raw_impls::cs::CriticalSectionRawMutex;

use defmt::{debug, info, warn};
use panic_rtt_target as _;

extern crate alloc;

// ========== Constants ==========

const ESP_NOW_MTU: u16 = 250;
const QUEUE_SIZE: usize = 2048;

// ========== Types ==========

type Queue = BBQueue<Inline<QUEUE_SIZE>, AtomicCoord, MaiNotSpsc>;
type QueueRef = &'static Queue;

struct EspNowInterface;
impl ergot::interface_manager::Interface for EspNowInterface {
    type Sink = framed_stream::Sink<QueueRef>;
}

type Rng = esp_hal::rng::Rng;
// S=4: seed route slots for bridge downstream networks
type DeviceRouter = Router<EspNowInterface, Rng, 1, 4>;
type Stack = NetStack<CriticalSectionRawMutex, DeviceRouter>;

// ========== Statics ==========

static OUTQ: Queue = BBQueue::new();
static STATE_NOTIFY: ergot::exports::maitake_sync::WaitQueue =
    ergot::exports::maitake_sync::WaitQueue::new();

// ========== Entry ==========

esp_bootloader_esp_idf::esp_app_desc!();

#[allow(clippy::large_stack_frames)]
#[esp_rtos::main]
async fn main(_spawner: Spawner) -> ! {
    rtt_target::rtt_init_defmt!();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);

    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    info!("ESP-NOW Device (S3) starting");

    // ========== ESP-NOW init ==========
    let (_wifi_controller, interfaces) =
        esp_radio::wifi::new(peripherals.WIFI, Default::default()).expect("WiFi init failed");
    let (_manager, sender, receiver) = interfaces.esp_now.split();

    info!("ESP-NOW initialized");

    // ========== Ergot stack ==========
    let rng = esp_hal::rng::Rng::new();
    let sink = framed_stream::Sink::new(OUTQ.framed_producer(), ESP_NOW_MTU);
    let router = Router::new(rng);
    let stack: &'static Stack = {
        static STACK_CELL: static_cell::StaticCell<Stack> = static_cell::StaticCell::new();
        STACK_CELL.init(NetStack::new_with_profile(router))
    };

    // Register ESP-NOW interface
    let ident = stack
        .manage_profile(|router| {
            router.register_interface(sink).expect("register failed")
        });
    let net_id = stack
        .manage_profile(|router| router.net_id_of(ident))
        .unwrap();

    info!("Ergot Router initialized, ident={}, net_id={}", ident, net_id);

    // ========== Run ==========
    let _ = join4(
        // Ping server + device info handler
        async {
            let _ = embassy_futures::join::join(
                stack.services().ping_handler::<2>(),
                stack.services().device_info_handler::<2>(&ergot::well_known::DeviceInfo {
                    name: Some("ESP-NOW S3".try_into().unwrap_or_default()),
                    description: Some("Root Router".try_into().unwrap_or_default()),
                    unique_id: 0,
                }),
            )
            .await;
        },
        // Seed router: assigns globally-routable net_ids to bridge downstreams
        stack.services().seed_router_request_handler::<4>(),
        // ESP-NOW RX
        esp_now_rx(receiver, stack, ident, net_id),
        // ESP-NOW TX
        esp_now_tx(sender),
    )
    .await;

    unreachable!()
}

// ========== ESP-NOW RX worker ==========

async fn esp_now_rx(
    mut receiver: EspNowReceiver<'static>,
    stack: &'static Stack,
    ident: u8,
    net_id: u16,
) {
    let mut processor = RouterFrameProcessor::new(net_id);
    info!("[esp-now rx] running");

    loop {
        let data = receiver.receive_async().await;
        let frame = data.data();
        debug!("[esp-now rx] got {} bytes: {:02x}", frame.len(), &frame[..frame.len().min(24)]);
        let changed = processor.process_frame(frame, &stack, ident);
        if changed {
            debug!("[esp-now rx] state changed!");
            STATE_NOTIFY.wake_all();
        }
    }
}

// ========== ESP-NOW TX worker ==========

async fn esp_now_tx(mut sender: EspNowSender<'static>) {
    let consumer = OUTQ.framed_consumer();
    info!("[esp-now tx] running");

    loop {
        let grant = consumer.wait_read().await;
        debug!("[esp-now tx] sending {} bytes: {:02x}", grant.len(), &grant[..grant.len().min(24)]);
        if let Err(e) = sender.send_async(&BROADCAST_ADDRESS, &grant).await {
            warn!("[esp-now tx] send error: {:?}", e);
        }
        grant.release();
    }
}
