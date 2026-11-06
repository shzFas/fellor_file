//! Enumerate USB devices, report MTP/PTP candidates and, if one is usable,
//! print its storages and the root of the first storage.
//!
//!     cargo run -p fellor-mtp --example mtp_probe            # one-shot probe
//!     cargo run -p fellor-mtp --example mtp_probe -- --watch 30   # discovery events for 30 s
//!
//! Set RUST_LOG=debug for protocol traces.

use fellor_mtp::discovery::{backend_id, classify_device, Candidate};
use fellor_mtp::MtpBackend;
use fellor_ptp::{EventSource, TransportError};
use fellor_vfs::{Backend, DeviceState, VPath};
use std::time::Duration;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .init();
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--watch") {
        let secs = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(30);
        watch(secs).await;
        return;
    }
    probe().await;
}

async fn watch(secs: u64) {
    println!("watching MTP discovery for {secs} s (plug / unplug / switch USB mode now)...");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let _h = fellor_mtp::spawn_discovery(tx);
    let deadline = tokio::time::sleep(Duration::from_secs(secs));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            ev = rx.recv() => match ev {
                Some(fellor_vfs::DeviceEvent::Attached(b)) => println!("Attached {} — {} [{:?}] {:?}", b.id(), b.info().name, b.info().kind, b.info().state),
                Some(fellor_vfs::DeviceEvent::Updated(b)) => println!("Updated  {} — {} [{:?}] {:?}", b.id(), b.info().name, b.info().kind, b.info().state),
                Some(fellor_vfs::DeviceEvent::Detached(id)) => println!("Detached {id}"),
                None => break,
            }
        }
    }
}

async fn probe() {
    let devices: Vec<nusb::DeviceInfo> = match nusb::list_devices().await {
        Ok(d) => d.collect(),
        Err(e) => {
            eprintln!("USB enumeration failed: {e}");
            std::process::exit(1);
        }
    };
    println!("{} USB device(s):", devices.len());
    let mut candidates = Vec::new();
    for d in &devices {
        let c = classify_device(d);
        println!(
            "  {:04x}:{:04x}  {:<32} serial={:<20} class={:02x} {}",
            d.vendor_id(),
            d.product_id(),
            d.product_string().unwrap_or("?"),
            d.serial_number().unwrap_or("-"),
            d.class(),
            match &c {
                Some(Candidate::Ptp(i)) => format!("<- MTP/PTP interface #{} ({:02x}/{:02x}/{:02x} {:?})", i.number, i.class, i.subclass, i.protocol, i.name),
                Some(Candidate::NeedsMtpMode) => "<- Android in charging-only mode (needs \"File transfer\")".into(),
                None => String::new(),
            }
        );
        for i in d.interfaces() {
            println!("        iface {} class {:02x}/{:02x}/{:02x} {:?}", i.interface_number(), i.class(), i.subclass(), i.protocol(), i.interface_string());
        }
        if let Some(c) = c {
            candidates.push((d.clone(), c));
        }
    }
    if candidates.is_empty() {
        println!("\nNo MTP/PTP devices found.");
        return;
    }
    let Some((dev, iface)) = candidates.iter().find_map(|(d, c)| match c {
        Candidate::Ptp(i) => Some((d.clone(), i.clone())),
        _ => None,
    }) else {
        println!("\nOnly phones without an MTP interface: on the phone pick USB mode \"File transfer\".");
        return;
    };
    let id = backend_id(&dev);
    println!("\nOpening {id} interface #{}...", iface.number);
    let (transport, events) = match fellor_ptp::usb::open(&dev, iface.number).await {
        Ok(t) => t,
        Err(TransportError::Busy) => {
            let by = fellor_mtp::busy::detect_holder(iface.is_still_image_class()).await;
            println!("Interface is busy (claimed by {}). Close that program and retry.", by.as_deref().unwrap_or("another program"));
            return;
        }
        Err(e) => {
            println!("Cannot open: {e}");
            return;
        }
    };
    let events = events.map(|e| Box::new(e) as Box<dyn EventSource>);
    let b = match MtpBackend::open_with_transport(id, Box::new(transport), events).await {
        Ok(b) => b,
        Err(e) => {
            println!("Session setup failed: {e}");
            return;
        }
    };
    let info = b.info();
    println!("Device: {} ({:?}), model {:?}, version {:?}, state {:?}", info.name, info.kind, info.model, info.os_version, info.state);
    if let Some(di) = b.device_info() {
        println!("  vendor extension: {:?}", di.vendor_extension_desc);
        println!("  operations: {}", di.operations_supported.iter().map(|o| format!("{o:04X}")).collect::<Vec<_>>().join(" "));
    }
    println!("  capabilities: {:?}", b.capabilities());
    if info.state == DeviceState::Locked {
        println!("Phone reports no storages: unlock the screen and run again.");
        return;
    }
    for s in b.storages() {
        println!(
            "  storage 0x{:08X} {:?}: {:.1} GB free of {:.1} GB{}",
            s.id,
            s.name,
            s.info.free_space_bytes as f64 / 1e9,
            s.info.max_capacity as f64 / 1e9,
            if s.info.read_only() { " (read-only)" } else { "" }
        );
    }
    let Some(first) = b.storages().into_iter().next() else { return };
    let path = VPath::new(b.id(), vec![first.name.clone()]);
    let t = std::time::Instant::now();
    match b.list(&path).await {
        Ok(entries) => {
            println!("\n{path} — {} entries in {:?}:", entries.len(), t.elapsed());
            for e in entries.iter().take(50) {
                println!("  {:<4} {:>12}  {}", if e.is_dir() { "dir" } else { "file" }, e.size.map(|s| s.to_string()).unwrap_or_default(), e.name);
            }
        }
        Err(e) => println!("listing failed: {e}"),
    }
}
