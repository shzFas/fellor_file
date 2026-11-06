//! Manual smoke test against a real device:
//!
//! ```sh
//! RUST_LOG=debug cargo run -p fellor-ios --example ios_probe
//! ```
//!
//! Lists devices via usbmuxd; for the first USB device prints lockdown
//! values, lists /DCIM via AFC, free space, File-Sharing apps and the
//! IosBackend view of /Media.

use fellor_afc::{installation_proxy, AfcClient, AFC_SERVICE};
use fellor_lockdown::Device;
use fellor_usbmux::{ConnectionType, UsbmuxClient};
use fellor_vfs::{Backend, VPath};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    if std::env::args().any(|a| a == "--watch") {
        // Run the real discovery loop and print events (Ctrl-C to stop).
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        fellor_ios::spawn_discovery(tx);
        println!("watching usbmuxd for iOS devices (Ctrl-C to stop)…");
        while let Some(ev) = rx.recv().await {
            match &ev {
                fellor_vfs::DeviceEvent::Attached(b) | fellor_vfs::DeviceEvent::Updated(b) => println!("{ev:?}: {:?}", b.info()),
                fellor_vfs::DeviceEvent::Detached(_) => println!("{ev:?}"),
            }
        }
        return;
    }
    if let Err(e) = run().await {
        eprintln!("error: {e} ({e:?})");
        std::process::exit(1);
    }
}

async fn run() -> fellor_vfs::Result<()> {
    println!("usbmuxd socket: {}", fellor_usbmux::socket_path().display());
    let devices = UsbmuxClient::connect().await?.list_devices().await?;
    println!("{} device(s)", devices.len());
    for d in &devices {
        println!("  id={} udid={} type={:?} product_id={:?}", d.device_id, d.udid, d.connection_type, d.product_id);
    }
    let Some(dev) = devices.iter().find(|d| d.connection_type == ConnectionType::Usb) else {
        println!("no USB iOS device attached");
        return Ok(());
    };

    let device = match Device::open(dev.device_id, &dev.udid).await {
        Ok(d) => d,
        Err(e) => {
            println!("pair record: {e} — unlock the phone and tap «Trust»");
            if let Ok(v) = fellor_ios::public_values(dev.device_id).await {
                println!("public lockdown values: {v:?}");
            }
            return Err(e);
        }
    };
    println!("pair record: {:?}", device.pair);

    let mut ld = device.lockdown().await?;
    println!("lockdown session: {:?}", ld.session());
    for key in ["DeviceName", "ProductType", "ProductVersion", "DeviceClass", "UniqueDeviceID", "BuildVersion"] {
        match ld.get_value(None, Some(key)).await {
            Ok(v) => println!("  {key}: {v:?}"),
            Err(e) => println!("  {key}: <{e}>"),
        }
    }
    let svc = ld.start_service(AFC_SERVICE, None).await?;
    println!("afc service: {svc:?}");
    drop(ld);

    let mut afc = AfcClient::new(device.connect_service(&svc).await?);
    let di = afc.device_info().await?;
    println!("fs: total={:?} free={:?} model={:?}", di.total_bytes, di.free_bytes, di.model);
    match afc.read_dir("/DCIM").await {
        Ok(names) => {
            println!("/DCIM: {names:?}");
            if let Some(first) = names.first() {
                let sub = format!("/DCIM/{first}");
                let files = afc.read_dir(&sub).await?;
                println!("{sub}: {} entries", files.len());
                for f in files.iter().take(5) {
                    let p = format!("{sub}/{f}");
                    println!("  {f}: {:?}", afc.file_info(&p).await.map(|i| (i.size, i.kind, i.mtime())));
                }
            }
        }
        Err(e) => println!("/DCIM: {e}"),
    }

    match device.start_service(installation_proxy::SERVICE).await {
        Ok(io) => match installation_proxy::file_sharing_apps(io).await {
            Ok(apps) => {
                println!("{} File-Sharing app(s):", apps.len());
                for a in &apps {
                    println!("  {} ({})", a.display_name, a.bundle_id);
                }
            }
            Err(e) => println!("installation_proxy Browse: {e}"),
        },
        Err(e) => println!("installation_proxy: {e}"),
    }

    let backend = fellor_ios::IosBackend::connect(dev.device_id, &dev.udid).await?;
    println!("backend: {:?}", backend.info());
    let media = VPath::from_str_path(backend.id(), "/Media");
    for e in backend.list(&media).await? {
        println!("  {:?} {} {:?}", e.kind, e.name, e.size);
    }
    let apps = VPath::from_str_path(backend.id(), "/Apps");
    match backend.list(&apps).await {
        Ok(list) => println!("/Apps: {:?}", list.iter().map(|e| &e.name).collect::<Vec<_>>()),
        Err(e) => println!("/Apps: {e}"),
    }
    Ok(())
}
