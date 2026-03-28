use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Result, anyhow};
use ergot::interface_manager::profiles::direct_edge::{DirectEdge, EDGE_NODE_ID, EdgeFrameProcessor};
use ergot::interface_manager::utils::framed_stream;
use ergot::interface_manager::InterfaceState;
use ergot::toolkits::nusb_v0_1::{EdgeStack, find_new_devices, register_edge_interface};
use ergot::well_known::ErgotPingEndpoint;
use std::sync::Arc;

const MTU: u16 = 2048;
const OUT_BUFFER_SIZE: usize = 8192;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,ergot=debug"))
        .init();

    log::info!("ESP-NOW Host — searching for USB device...");

    let state_notify = Arc::new(ergot::toolkits::tokio_stream::WaitQueue::new());

    // Find ergot USB device (the bridge)
    let devices = find_new_devices(&HashSet::new()).await;
    let device = devices
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("No ergot USB device found. Is the bridge plugged in?"))?;

    log::info!(
        "Found: {:?}",
        device.info.usb_product.as_deref().unwrap_or("unknown")
    );

    // Create edge stack
    let queue = ergot::interface_manager::utils::std::new_std_queue(OUT_BUFFER_SIZE);
    let stack = EdgeStack::new_with_profile(DirectEdge::new_target(
        framed_stream::Sink::new_from_handle(queue.clone(), MTU),
    ));

    // Register USB interface with link-local addressing (net_id=0).
    // The router rewrites net_id=0 to the real net_id on both src and dst.
    register_edge_interface(
        &stack,
        device,
        &queue,
        EdgeFrameProcessor::new(),
        InterfaceState::Active {
            net_id: 0,
            node_id: EDGE_NODE_ID,
        },
        MTU,
        Some(state_notify.clone()),
    )
    .await
    .map_err(|e| anyhow!("USB registration failed: {:?}", e))?;

    log::info!("USB connected (link-local), running discovery...");

    // Discover all devices
    let devices = stack.discovery().discover(10, Duration::from_secs(3)).await;
    log::info!("Found {} devices:", devices.len());
    for dev in &devices {
        log::info!(
            "  {:?} — name: {:?}, desc: {:?}",
            dev.addr,
            dev.info.name,
            dev.info.description
        );
    }

    // Ping all discovered devices
    log::info!("Pinging all discovered devices...");
    for dev in &devices {
        let ping_addr = dev.addr;
        log::info!("Pinging {:?}...", ping_addr);
        for attempt in 1..=3 {
            match tokio::time::timeout(
                Duration::from_secs(3),
                stack
                    .endpoints()
                    .request::<ErgotPingEndpoint>(ping_addr, &42u32, None),
            )
            .await
            {
                Ok(Ok(val)) => {
                    log::info!("  Reply: {} ✓ (attempt {})", val, attempt);
                    break;
                }
                Ok(Err(e)) => {
                    log::warn!("  Error (attempt {}): {:?}", attempt, e);
                }
                Err(_) => {
                    log::warn!("  Timeout (attempt {})", attempt);
                }
            }
        }
    }

    log::info!("Demo complete. Press Ctrl+C to exit.");
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}
