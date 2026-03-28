#![no_std]
#![no_main]
#![deny(clippy::mem_forget)]
#![deny(clippy::large_stack_frames)]

use esp_hal::clock::CpuClock;
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::otg_fs::{Usb, asynch::Driver as EspUsbDriver};
use esp_hal::timer::timg::TimerGroup;
use esp_radio::esp_now::{EspNowReceiver, EspNowSender, BROADCAST_ADDRESS};

use embassy_executor::Spawner;
use embassy_usb::UsbDevice;
use embassy_usb::driver::Driver;

use bbqueue::BBQueue;
use bbqueue::traits::coordination::cas::AtomicCoord;
use bbqueue::traits::notifier::maitake::MaiNotSpsc;
use bbqueue::traits::storage::Inline;

use ergot::interface_manager::profiles::router::{Router, RouterFrameProcessor, UPSTREAM_IDENT};
use ergot::interface_manager::utils::framed_stream;
use ergot::interface_manager::{FrameProcessor, InterfaceState, Profile};
use ergot::net_stack::services::{SeedLease, bridge_seed_assign, bridge_seed_refresh};
use ergot::toolkits::embassy_usb_v0_6 as usb_kit;
use ergot::NetStack;
use mutex::raw_impls::cs::CriticalSectionRawMutex;

use log::{debug, error, info, warn};
use static_cell::{ConstStaticCell, StaticCell};

#[panic_handler]
fn panic(panic_info: &core::panic::PanicInfo) -> ! {
    error!("{}", panic_info);
    loop {}
}

extern crate alloc;

// ========== Constants ==========

const ESP_NOW_MTU: u16 = 250;
const USB_MTU: u16 = 2048;
const ESP_NOW_QUEUE_SIZE: usize = 2048;
const USB_QUEUE_SIZE: usize = 8192;

// ========== Types ==========

type EspNowQueue = BBQueue<Inline<ESP_NOW_QUEUE_SIZE>, AtomicCoord, MaiNotSpsc>;
type EspNowQueueRef = &'static EspNowQueue;
type UsbQueue = usb_kit::Queue<USB_QUEUE_SIZE, AtomicCoord>;
type UsbQueueRef = &'static UsbQueue;
type AppDriver = EspUsbDriver<'static>;

struct EspNowInterface;
impl ergot::interface_manager::Interface for EspNowInterface {
    type Sink = framed_stream::Sink<EspNowQueueRef>;
}

ergot::multi_interface! {
    pub enum BridgeSink for BridgeInterface {
        Usb(ergot::interface_manager::interface_impls::embassy_usb::EmbassyInterface<UsbQueueRef>),
        EspNow(EspNowInterface),
    }
}

type Rng = esp_hal::rng::Rng;
type BridgeRouter = Router<BridgeInterface, Rng, 2, 0>;
type Stack = NetStack<CriticalSectionRawMutex, BridgeRouter>;

type UsbRxWorker = ergot::interface_manager::transports::eusb_0_6::RxWorker<
    &'static Stack,
    AppDriver,
    RouterFrameProcessor,
>;

// ========== Statics ==========

static ESP_NOW_OUTQ: EspNowQueue = BBQueue::new();
static USB_OUTQ: UsbQueue = usb_kit::Queue::new();
static USB_STORAGE: usb_kit::WireStorage<256, 256, 64, 256> = usb_kit::WireStorage::new();
static STATE_NOTIFY: ergot::exports::maitake_sync::WaitQueue =
    ergot::exports::maitake_sync::WaitQueue::new();

// ========== Entry ==========

esp_bootloader_esp_idf::esp_app_desc!();

fn usb_config(serial: &'static str) -> embassy_usb::Config<'static> {
    let mut config = embassy_usb::Config::new(0x1209, 0x0001);
    config.manufacturer = Some("okhsunrog");
    config.product = Some("ESP-NOW Bridge");
    config.serial_number = Some(serial);
    config.device_class = 0xEF;
    config.device_sub_class = 0x02;
    config.device_protocol = 0x01;
    config.composite_with_iads = true;
    config
}

#[allow(clippy::large_stack_frames)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);

    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    info!("ESP-NOW Bridge (S3) starting");

    // ========== ESP-NOW init ==========
    let (_wifi_controller, interfaces) =
        esp_radio::wifi::new(peripherals.WIFI, Default::default()).expect("WiFi init failed");
    let (_manager, sender, receiver) = interfaces.esp_now.split();

    info!("ESP-NOW initialized");

    // ========== USB OTG init ==========
    static SERIAL_BUF: StaticCell<[u8; 16]> = StaticCell::new();
    let ser_buf = SERIAL_BUF.init([b'B'; 16]);
    let ser_str = core::str::from_utf8(ser_buf.as_slice()).unwrap();

    let usb = Usb::new(peripherals.USB0, peripherals.GPIO20, peripherals.GPIO19);
    static EP_OUT_BUFFER: ConstStaticCell<[u8; 1024]> = ConstStaticCell::new([0u8; 1024]);
    let driver = EspUsbDriver::new(
        usb,
        EP_OUT_BUFFER.take(),
        esp_hal::otg_fs::asynch::Config::default(),
    );
    let usb_config = usb_config(ser_str);
    let (usb_device, tx_ep, rx_ep) = USB_STORAGE.init_ergot(driver, usb_config);

    info!("USB OTG initialized");

    // ========== Ergot stack (Bridge Router) ==========
    let rng = esp_hal::rng::Rng::new();
    let upstream_sink = BridgeSink::EspNow(framed_stream::Sink::new(
        ESP_NOW_OUTQ.framed_producer(),
        ESP_NOW_MTU,
    ));
    let router = Router::new_bridge(rng, upstream_sink);
    let stack: &'static Stack = {
        static STACK_CELL: StaticCell<Stack> = StaticCell::new();
        STACK_CELL.init(NetStack::new_with_profile(router))
    };

    // Register USB downstream as pending (no net_id yet).
    // The seed router on the root device will assign a globally-routable net_id.
    let usb_sink = BridgeSink::Usb(framed_stream::Sink::new(
        USB_OUTQ.framed_producer(),
        USB_MTU,
    ));
    let usb_ident = stack.manage_profile(|router| {
        router
            .register_interface_pending(usb_sink)
            .expect("USB interface registration failed")
    });

    info!("Bridge: usb ident={} (pending seed assignment)", usb_ident);

    // Manually activate upstream with root's net_id=1 so bridge can send seed request.
    // In a COBS stream setup the root would initiate contact, but with ESP-NOW broadcast
    // there's no auto-bootstrap — we must activate manually.
    stack.manage_profile(|router| {
        router
            .set_interface_state(
                UPSTREAM_IDENT,
                InterfaceState::Active {
                    net_id: 1,
                    node_id: ergot::interface_manager::profiles::direct_edge::EDGE_NODE_ID,
                },
            )
            .expect("failed to activate upstream")
    });
    info!("Bridge: upstream activated with net_id=1");

    // ========== Spawn all transport tasks ==========
    spawner.must_spawn(usb_task(usb_device));
    spawner.must_spawn(usb_tx_task(tx_ep, USB_OUTQ.framed_consumer()));
    spawner.must_spawn(esp_now_tx_task(sender));
    spawner.must_spawn(esp_now_rx_task(receiver, stack));

    // Spawn USB RX worker immediately so incoming packets aren't lost
    let usb_rx_worker = UsbRxWorker::new(
        stack,
        rx_ep,
        RouterFrameProcessor::new(0),
        usb_ident,
    )
    .with_state_notify(&STATE_NOTIFY);
    static RX_BUF: ConstStaticCell<[u8; 2048]> = ConstStaticCell::new([0u8; 2048]);
    spawner.must_spawn(usb_rx_task(usb_rx_worker, RX_BUF.take()));

    // ========== Request seed net_id from root router ==========
    info!("Bridge: requesting seed net_id...");
    let lease = match bridge_seed_assign(&stack, UPSTREAM_IDENT, usb_ident).await {
        Ok(lease) => {
            info!("Bridge: seed assigned net_id={}", lease.net_id);
            Some(lease)
        }
        Err(e) => {
            error!("Bridge: seed assign failed: {:?}", e);
            None
        }
    };

    if let Some(lease) = lease {
        spawner.must_spawn(seed_refresh_task(stack, lease));
    }

    let usb_net_id = stack
        .manage_profile(|router| router.net_id_of(usb_ident))
        .unwrap_or(0);
    info!("Bridge: usb net_id={}, all interfaces ready", usb_net_id);

    // ========== Run services ==========
    let _ = embassy_futures::join::join(
        stack.services().ping_handler::<2>(),
        stack.services().device_info_handler::<2>(&ergot::well_known::DeviceInfo {
            name: Some("ESP-NOW Bridge".try_into().unwrap_or_default()),
            description: Some("S3 ESP-NOW+USB".try_into().unwrap_or_default()),
            unique_id: 0,
        }),
    )
    .await;

    unreachable!()
}

// ========== USB tasks ==========

#[embassy_executor::task]
async fn usb_task(mut usb: UsbDevice<'static, AppDriver>) {
    usb.run().await;
}

#[embassy_executor::task]
async fn usb_rx_task(rx_worker: UsbRxWorker, rx_buf: &'static mut [u8]) {
    rx_worker
        .run(rx_buf, usb_kit::USB_FS_MAX_PACKET_SIZE)
        .await;
}

#[embassy_executor::task]
async fn usb_tx_task(
    mut ep_in: <AppDriver as Driver<'static>>::EndpointIn,
    consumer: bbqueue::prod_cons::framed::FramedConsumer<UsbQueueRef>,
) {
    usb_kit::tx_worker::<AppDriver, USB_QUEUE_SIZE, AtomicCoord>(
        &mut ep_in,
        consumer,
        usb_kit::DEFAULT_TIMEOUT_MS_PER_FRAME,
        usb_kit::USB_FS_MAX_PACKET_SIZE,
    )
    .await;
}

// ========== ESP-NOW tasks ==========

#[embassy_executor::task]
async fn esp_now_rx_task(mut receiver: EspNowReceiver<'static>, stack: &'static Stack) {
    let upstream_net_id = stack
        .manage_profile(|router| {
            router
                .interface_state(UPSTREAM_IDENT)
                .and_then(|s| match s {
                    InterfaceState::Active { net_id, .. } => Some(net_id),
                    _ => None,
                })
        })
        .unwrap_or(1);

    let mut processor = RouterFrameProcessor::new(upstream_net_id);
    info!("[esp-now rx] running, net_id={}", upstream_net_id);

    loop {
        let data = receiver.receive_async().await;
        let frame = data.data();
        debug!("[esp-now rx] got {} bytes: {:02x?}", frame.len(), &frame[..frame.len().min(24)]);
        let changed = processor.process_frame(frame, &stack, UPSTREAM_IDENT);
        if changed {
            STATE_NOTIFY.wake_all();
        }
    }
}

#[embassy_executor::task]
async fn esp_now_tx_task(mut sender: EspNowSender<'static>) {
    let consumer = ESP_NOW_OUTQ.framed_consumer();
    info!("[esp-now tx] running");

    loop {
        let grant = consumer.wait_read().await;
        debug!("[esp-now tx] sending {} bytes: {:02x?}", grant.len(), &grant[..grant.len().min(24)]);
        if let Err(e) = sender.send_async(&BROADCAST_ADDRESS, &grant).await {
            warn!("[esp-now tx] send error: {:?}", e);
        }
        grant.release();
    }
}

// ========== Seed lease refresh ==========

#[embassy_executor::task]
async fn seed_refresh_task(stack: &'static Stack, initial_lease: SeedLease) {
    use embassy_time::{Duration, Timer};

    let mut lease = initial_lease;
    loop {
        // Refresh when half the remaining time has passed, but not before min_refresh window
        let delay_secs = (lease.expires_seconds as u64)
            .saturating_sub(lease.min_refresh_seconds as u64 / 2)
            .max(5);
        Timer::after(Duration::from_secs(delay_secs)).await;

        match bridge_seed_refresh(&stack, &lease).await {
            Ok(new_lease) => {
                info!("[seed] refreshed net_id={}, expires in {}s", new_lease.net_id, new_lease.expires_seconds);
                lease = new_lease;
            }
            Err(e) => {
                warn!("[seed] refresh failed: {:?}", e);
                // Wait a bit and retry
                Timer::after(Duration::from_secs(5)).await;
            }
        }
    }
}
