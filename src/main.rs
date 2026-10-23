//! Command-line reader for GIP controllers; see the library docs for the protocol.

use std::io::Write;
use std::time::Duration;

use anyhow::{Context, Result};
use gip_pad::*;

/// Prints the USB descriptors, then resets the pad so it announces itself again (GIP 0x02 carries
/// its firmware and hardware versions) and asks for its metadata (0x04), dumping what comes back.
fn info(ctx: &rusb::Context) -> Result<()> {
    let (dev, desc, config, iface, ep_in, ep_out) = find_gip(ctx)?;
    let handle = dev.open().context("opening the controller")?;
    let s = |i: Option<u8>| {
        i.and_then(|i| handle.read_string_descriptor_ascii(i).ok())
            .unwrap_or_default()
    };
    println!(
        "vendor {:04x} product {:04x}",
        desc.vendor_id(),
        desc.product_id()
    );
    println!(
        "manufacturer {:?} product {:?} serial {:?}",
        s(desc.manufacturer_string_index()),
        s(desc.product_string_index()),
        s(desc.serial_number_string_index())
    );
    println!(
        "usb {} device version {} class {:02x}/{:02x}/{:02x} max packet {} configurations {}",
        desc.usb_version(),
        desc.device_version(),
        desc.class_code(),
        desc.sub_class_code(),
        desc.protocol_code(),
        desc.max_packet_size(),
        desc.num_configurations()
    );
    for c in 0..desc.num_configurations() {
        let cfg = dev.config_descriptor(c)?;
        println!(
            "configuration {} max power {} mA",
            cfg.number(),
            cfg.max_power()
        );
        for i in cfg.interfaces() {
            for alt in i.descriptors() {
                print!(
                    "  interface {} alt {} class {:02x}/{:02x}/{:02x}:",
                    alt.interface_number(),
                    alt.setting_number(),
                    alt.class_code(),
                    alt.sub_class_code(),
                    alt.protocol_code()
                );
                for ep in alt.endpoint_descriptors() {
                    print!(
                        " {:#04x} {:?} {}B/{}ms",
                        ep.address(),
                        ep.transfer_type(),
                        ep.max_packet_size(),
                        ep.interval()
                    );
                }
                println!();
            }
        }
    }
    // A reset makes the pad re-enumerate, which ends this handle; open the new device afterwards
    match handle.reset() {
        Ok(()) | Err(rusb::Error::NotFound) | Err(rusb::Error::NoDevice) => {}
        Err(e) => return Err(e).context("resetting the controller"),
    }
    drop(handle);
    let mut found = None;
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(100));
        if let Ok(f) = find_gip(ctx) {
            if let Ok(h) = f.0.open() {
                found = Some(h);
                break;
            }
        }
    }
    let handle = found.context("the controller did not come back after the reset")?;
    if handle.active_configuration().ok() != Some(config) {
        handle.set_active_configuration(config)?;
    }
    handle.claim_interface(iface)?;
    let mut gip = Gip {
        handle,
        ep_in,
        ep_out,
        seq: 0,
    };
    let raw = std::env::args().any(|a| a == "--raw");
    let mut buf = [0u8; 64];
    let mut metadata: Vec<u8> = Vec::new();
    let mut received = 0usize;
    let mut announced = false;
    // After a reset the pad announces itself until the host answers; wait for that first
    let until = std::time::Instant::now() + Duration::from_millis(1500);
    while !announced && std::time::Instant::now() < until {
        let n = match gip
            .handle
            .read_interrupt(gip.ep_in, &mut buf, Duration::from_millis(100))
        {
            Ok(n) => n,
            Err(rusb::Error::Timeout) => continue,
            Err(e) => return Err(e.into()),
        };
        if raw {
            println!("{:02x?}", &buf[..n]);
        }
        if let Some(h) = Header::parse(&buf[..n]) {
            if h.command == 0x02 {
                announced = true;
                if let Some(d) = describe_announce(h.body(&buf[..n])) {
                    println!("announce: {d}");
                }
            }
        }
    }
    gip.send(0x04, OPT_INTERNAL, &[])?;
    let until = std::time::Instant::now() + Duration::from_millis(2500);
    while std::time::Instant::now() < until {
        let n = match gip
            .handle
            .read_interrupt(gip.ep_in, &mut buf, Duration::from_millis(100))
        {
            Ok(n) => n,
            Err(rusb::Error::Timeout) => continue,
            Err(e) => return Err(e.into()),
        };
        let p = buf[..n].to_vec();
        if raw {
            println!("{:02x?}", p);
        }
        let Some(h) = Header::parse(&p) else { continue };
        let body = h.body(&p);
        match (h.command, h.chunk) {
            (0x02, _) if !announced => {
                announced = true;
                if let Some(d) = describe_announce(body) {
                    println!("announce: {d}");
                }
            }
            (0x04, Some(total)) if h.options & OPT_CHUNK_START != 0 => {
                metadata = vec![0; total];
                metadata[..body.len().min(total)].copy_from_slice(&body[..body.len().min(total)]);
                received = body.len();
            }
            (0x04, Some(offset)) if !metadata.is_empty() => {
                if offset + body.len() <= metadata.len() {
                    metadata[offset..offset + body.len()].copy_from_slice(body);
                    received = received.max(offset + body.len());
                }
            }
            _ => {}
        }
        if h.options & OPT_ACK_WANTED != 0 {
            let remaining = metadata.len().saturating_sub(received);
            gip.ack(
                &h,
                if h.chunk.is_some() {
                    received as u16
                } else {
                    h.length as u16
                },
                remaining as u16,
            )?;
        }
        if !metadata.is_empty() && received >= metadata.len() {
            break;
        }
    }
    if metadata.is_empty() {
        println!("no metadata received");
    } else {
        println!("metadata: {} of {} bytes", received, metadata.len());
        for s in strings(&metadata, 6) {
            println!("  {s}");
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--info") {
        return info(&rusb::Context::new()?);
    }
    let raw = args.iter().any(|a| a == "--raw");
    let seconds: Option<u64> = args
        .iter()
        .position(|a| a == "--seconds")
        .and_then(|i| args.get(i + 1)?.parse().ok());

    let mut c = Controller::open()?;
    println!(
        "found {} ({:04x}:{:04x}), endpoints in {:#04x} out {:#04x}",
        c.name, c.vendor, c.product, c.gip.ep_in, c.gip.ep_out
    );
    if let Err(e) = c.gip.rumble(40, 25) {
        eprintln!("rumble not accepted: {e:#}");
    }
    println!("reading; press buttons and move the sticks (Ctrl+C to stop)");

    let start = std::time::Instant::now();
    let mut last = String::new();
    let mut out = std::io::stdout();
    loop {
        if seconds.is_some_and(|s| start.elapsed().as_secs() >= s) {
            println!();
            return Ok(());
        }
        let Some(p) = c.poll(Duration::from_millis(200))? else {
            continue;
        };
        if raw {
            println!("{:02x?}", p);
            continue;
        }
        let line = c.state().line();
        if line != last {
            write!(out, "\r{line:<100}")?;
            out.flush()?;
            last = line;
        }
    }
}
