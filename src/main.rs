//! Command-line reader for GIP controllers; see the library docs for the protocol.

use std::io::Write;
use std::time::Duration;

use anyhow::{Context, Result};
use gip_pad::*;
use nusb::descriptors::language_id::US_ENGLISH;
use nusb::MaybeFuture;

/// A BCD-coded USB version such as 0x0200 as "2.0.0", like libusb prints it.
fn bcd(v: u16) -> String {
    let d = |n: u16| (n >> 4 & 0xF) * 10 + (n & 0xF);
    format!("{}.{}.{}", d(v >> 8), v >> 4 & 0xF, v & 0xF)
}

/// Prints the USB descriptors, then resets the pad so it announces itself again (GIP 0x02 carries
/// its firmware and hardware versions) and asks for its metadata (0x04), dumping what comes back.
fn info() -> Result<()> {
    let Detected {
        found,
        system_owned,
    } = find_gip_any()?;
    let device = found.info.open().wait().context("opening the controller")?;
    let desc = device.device_descriptor();
    let s = |i: Option<std::num::NonZeroU8>| {
        i.and_then(|i| {
            device
                .get_string_descriptor(i, US_ENGLISH, Duration::from_millis(500))
                .wait()
                .ok()
        })
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
        bcd(desc.usb_version()),
        bcd(desc.device_version()),
        desc.class(),
        desc.subclass(),
        desc.protocol(),
        desc.max_packet_size_0(),
        desc.num_configurations()
    );
    for cfg in device.configurations() {
        println!(
            "configuration {} max power {} mA",
            cfg.configuration_value(),
            u16::from(cfg.max_power()) * 2
        );
        for alt in cfg.interface_alt_settings() {
            print!(
                "  interface {} alt {} class {:02x}/{:02x}/{:02x}:",
                alt.interface_number(),
                alt.alternate_setting(),
                alt.class(),
                alt.subclass(),
                alt.protocol()
            );
            for ep in alt.endpoints() {
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
    // A pad that macOS drives cannot be reset or claimed, and needs no help: say so and stop. This is
    // not a failure, so the exit status is 0.
    if let Some(owned) = system_owned {
        println!();
        println!("{owned}");
        println!(
            "Not resetting it or reading its GIP announce and metadata: macOS holds the pad open exclusively."
        );
        return Ok(());
    }
    // A reset makes the pad re-enumerate, which ends this handle; open the new device afterwards
    match device.reset().wait() {
        Ok(()) => {}
        Err(e)
            if matches!(
                e.kind(),
                nusb::ErrorKind::NotFound | nusb::ErrorKind::Disconnected
            ) => {}
        Err(e) => return Err(found.explain_error(e, "resetting the controller".into())),
    }
    drop(device);
    let mut found_again = None;
    let mut last_error = None;
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(100));
        match find_gip().and_then(|f| f.open()) {
            Ok(opened) => {
                found_again = Some(opened);
                break;
            }
            Err(e) => last_error = Some(e),
        }
    }
    let (_device, mut gip) = match (found_again, last_error) {
        (Some(opened), _) => opened,
        (None, Some(e)) => {
            return Err(e).context("the controller did not come back after the reset")
        }
        (None, None) => anyhow::bail!("the controller did not come back after the reset"),
    };
    let raw = std::env::args().any(|a| a == "--raw");
    let mut metadata: Vec<u8> = Vec::new();
    let mut received = 0usize;
    let mut announced = false;
    // After a reset the pad announces itself until the host answers; wait for that first
    let until = std::time::Instant::now() + Duration::from_millis(1500);
    while !announced && std::time::Instant::now() < until {
        let Some(p) = gip.read(Duration::from_millis(100))? else {
            continue;
        };
        if raw {
            println!("{:02x?}", p);
        }
        if let Some(h) = Header::parse(&p) {
            if h.command == 0x02 {
                announced = true;
                if let Some(d) = describe_announce(h.body(&p)) {
                    println!("announce: {d}");
                }
            }
        }
    }
    gip.send(0x04, OPT_INTERNAL, &[])?;
    let until = std::time::Instant::now() + Duration::from_millis(2500);
    while std::time::Instant::now() < until {
        let Some(p) = gip.read(Duration::from_millis(100))? else {
            continue;
        };
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
            (0x04, Some(offset))
                if !metadata.is_empty() && offset + body.len() <= metadata.len() =>
            {
                metadata[offset..offset + body.len()].copy_from_slice(body);
                received = received.max(offset + body.len());
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
        return info();
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

#[cfg(test)]
mod tests {
    use super::bcd;

    #[test]
    fn usb_versions_print_like_libusb() {
        assert_eq!(bcd(0x0200), "2.0.0");
        assert_eq!(bcd(0x0110), "1.1.0");
        assert_eq!(bcd(0x0321), "3.2.1");
        assert_eq!(bcd(0x1234), "12.3.4");
    }
}
