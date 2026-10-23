//! Reads an Xbox One / Series style controller that speaks GIP (Game Input Protocol) over USB.
//!
//! macOS has no driver for wired third-party Xbox pads such as the PDP Afterglow Wave: they show up as
//! a vendor-class USB device (class 0xFF, interface subclass 0x47, protocol 0xD0) and no HID device is
//! created, so games and the Game Controller framework never see them. This talks to the pad directly
//! through libusb. The packet layouts follow the Linux `xpad` driver.
//!
//! GIP packets start with a 4-byte header: command, options, sequence number, payload length.
//! - Host to pad, to start input: power on (0x05), and for PDP pads also LED on (0x0A) and the
//!   authentication-done notice (0x06).
//! - Pad to host: input (0x20) with buttons, triggers and sticks; the guide button (0x07), which asks
//!   for an acknowledgement when its options carry 0x10.

use std::io::Write;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rusb::{Direction, TransferType, UsbContext};

const GIP_CLASS: (u8, u8, u8) = (0xFF, 0x47, 0xD0);
const CMD_ACK: u8 = 0x01;
const CMD_POWER: u8 = 0x05;
const CMD_AUTHENTICATE: u8 = 0x06;
const CMD_GUIDE: u8 = 0x07;
const CMD_LED: u8 = 0x0A;
const CMD_RUMBLE: u8 = 0x09;
const CMD_INPUT: u8 = 0x20;
const OPT_INTERNAL: u8 = 0x20;
const OPT_ACK_WANTED: u8 = 0x10;
const PDP_VENDOR: u16 = 0x0E6F;

/// The decoded state of one input report.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Pad {
    buttons: u16,
    guide: bool,
    share: bool,
    /// 0..=1023
    left_trigger: u16,
    right_trigger: u16,
    /// -32768..=32767, up is positive
    left_stick: (i16, i16),
    right_stick: (i16, i16),
}

/// Button names in report order: byte 4 bits 2..7, then byte 5 bits 0..7.
const BUTTONS: [(u16, &str); 14] = [
    (1 << 2, "Menu"),
    (1 << 3, "View"),
    (1 << 4, "A"),
    (1 << 5, "B"),
    (1 << 6, "X"),
    (1 << 7, "Y"),
    (1 << 8, "Up"),
    (1 << 9, "Down"),
    (1 << 10, "Left"),
    (1 << 11, "Right"),
    (1 << 12, "LB"),
    (1 << 13, "RB"),
    (1 << 14, "LS"),
    (1 << 15, "RS"),
];

impl Pad {
    /// Applies an input report (command 0x20); returns false if it is too short.
    fn apply_input(&mut self, p: &[u8]) -> bool {
        if p.len() < 18 {
            return false;
        }
        let u16le = |i: usize| u16::from_le_bytes([p[i], p[i + 1]]);
        let i16le = |i: usize| i16::from_le_bytes([p[i], p[i + 1]]);
        self.buttons = u16le(4);
        self.left_trigger = u16le(6);
        self.right_trigger = u16le(8);
        self.left_stick = (i16le(10), i16le(12));
        self.right_stick = (i16le(14), i16le(16));
        // Series-era pads send a longer report with the Share button; on the PDP Afterglow Wave
        // (payload 0x2E) it is bit 0 of byte 32
        self.share = p.len() > 32 && p[3] >= 0x2E && p[32] & 1 != 0;
        true
    }

    fn pressed(&self) -> Vec<&'static str> {
        let mut names: Vec<&str> = BUTTONS
            .iter()
            .filter(|(bit, _)| self.buttons & bit != 0)
            .map(|(_, n)| *n)
            .collect();
        if self.guide {
            names.push("Guide");
        }
        if self.share {
            names.push("Share");
        }
        names
    }

    fn line(&self) -> String {
        let pressed = self.pressed();
        format!(
            "LS {:>6} {:>6}  RS {:>6} {:>6}  LT {:>4}  RT {:>4}  {}",
            self.left_stick.0,
            self.left_stick.1,
            self.right_stick.0,
            self.right_stick.1,
            self.left_trigger,
            self.right_trigger,
            if pressed.is_empty() {
                "-".to_string()
            } else {
                pressed.join(" ")
            }
        )
    }
}

const OPT_CHUNK_START: u8 = 0x40;
const OPT_CHUNK: u8 = 0x80;

/// A GIP packet header. Lengths are LEB128 varints; chunked packets add a second varint, which is
/// the transfer's total length in the first chunk and the chunk's offset in the others.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Header {
    command: u8,
    options: u8,
    sequence: u8,
    length: usize,
    /// Total length for the first chunk, offset for later chunks
    chunk: Option<usize>,
    /// Where the payload starts
    payload: usize,
}

fn varint(p: &[u8], pos: &mut usize) -> Option<usize> {
    let mut v = 0usize;
    for shift in (0..21).step_by(7) {
        let b = *p.get(*pos)?;
        *pos += 1;
        v |= usize::from(b & 0x7F) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

impl Header {
    fn parse(p: &[u8]) -> Option<Self> {
        let (&command, &options, &sequence) = (p.first()?, p.get(1)?, p.get(2)?);
        let mut pos = 3;
        let length = varint(p, &mut pos)?;
        let chunk = if options & OPT_CHUNK != 0 {
            Some(varint(p, &mut pos)?)
        } else {
            None
        };
        (pos + length <= p.len()).then_some(Self {
            command,
            options,
            sequence,
            length,
            chunk,
            payload: pos,
        })
    }

    fn body<'a>(&self, p: &'a [u8]) -> &'a [u8] {
        &p[self.payload..self.payload + self.length]
    }
}

/// The pad's announce packet (0x02): who it is and its firmware and hardware versions.
fn describe_announce(body: &[u8]) -> Option<String> {
    if body.len() < 28 {
        return None;
    }
    let u16le = |i: usize| u16::from_le_bytes([body[i], body[i + 1]]);
    let version = |i: usize| {
        format!(
            "{}.{}.{}.{}",
            u16le(i),
            u16le(i + 2),
            u16le(i + 4),
            u16le(i + 6)
        )
    };
    let address: Vec<String> = body[..6].iter().rev().map(|b| format!("{b:02x}")).collect();
    Some(format!(
        "address {} vendor {:04x} product {:04x} firmware {} hardware {}",
        address.join(":"),
        u16le(8),
        u16le(10),
        version(12),
        version(20)
    ))
}

/// Printable strings of at least `min` characters, as the metadata names its interfaces with them.
fn strings(data: &[u8], min: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for &b in data.iter().chain([0u8].iter()) {
        if b.is_ascii_graphic() {
            cur.push(b as char);
        } else {
            if cur.len() >= min && !out.contains(&cur) {
                out.push(cur.clone());
            }
            cur.clear();
        }
    }
    out
}

struct Gip {
    handle: rusb::DeviceHandle<rusb::Context>,
    ep_in: u8,
    ep_out: u8,
    seq: u8,
}

impl Gip {
    fn send(&mut self, cmd: u8, options: u8, payload: &[u8]) -> Result<()> {
        self.seq = self.seq.wrapping_add(1).max(1);
        let mut packet = vec![cmd, options, self.seq, payload.len() as u8];
        packet.extend_from_slice(payload);
        self.handle
            .write_interrupt(self.ep_out, &packet, Duration::from_millis(500))
            .with_context(|| format!("sending command {cmd:#04x}"))?;
        Ok(())
    }

    /// Acknowledges a packet whose options ask for it. The ack carries the acknowledged packet's
    /// sequence number, and for chunked transfers how many bytes have arrived and how many remain
    /// (xone's `gip_pkt_acknowledge`).
    fn ack(&mut self, h: &Header, received: u16, remaining: u16) -> Result<()> {
        let [r0, r1] = received.to_le_bytes();
        let [m0, m1] = remaining.to_le_bytes();
        let packet = [
            CMD_ACK,
            OPT_INTERNAL,
            h.sequence,
            9,
            0x00,
            h.command,
            OPT_INTERNAL,
            r0,
            r1,
            0x00,
            0x00,
            m0,
            m1,
        ];
        self.handle
            .write_interrupt(self.ep_out, &packet, Duration::from_millis(500))
            .context("sending an acknowledgement")?;
        Ok(())
    }

    /// Rumble for a moment, so the pad shows it is listening. Strength 0..=100 per motor.
    fn rumble(&mut self, strength: u8, duration_10ms: u8) -> Result<()> {
        // Motor mask (all four), left/right trigger motors, left/right main motors, on time, off time, repeat
        self.send(
            CMD_RUMBLE,
            0x00,
            &[
                0x00,
                0x0F,
                0x00,
                0x00,
                strength,
                strength,
                duration_10ms,
                0x00,
                0x00,
            ],
        )
    }
}

fn find_gip(
    ctx: &rusb::Context,
) -> Result<(
    rusb::Device<rusb::Context>,
    rusb::DeviceDescriptor,
    u8,
    u8,
    u8,
    u8,
)> {
    for dev in ctx.devices()?.iter() {
        let desc = dev.device_descriptor()?;
        for c in 0..desc.num_configurations() {
            let Ok(config) = dev.config_descriptor(c) else {
                continue;
            };
            for iface in config.interfaces() {
                for alt in iface.descriptors() {
                    if (alt.class_code(), alt.sub_class_code(), alt.protocol_code()) != GIP_CLASS {
                        continue;
                    }
                    let mut ep_in = None;
                    let mut ep_out = None;
                    for ep in alt.endpoint_descriptors() {
                        if ep.transfer_type() == TransferType::Interrupt {
                            match ep.direction() {
                                Direction::In => ep_in = ep_in.or(Some(ep.address())),
                                Direction::Out => ep_out = ep_out.or(Some(ep.address())),
                            }
                        }
                    }
                    if let (Some(i), Some(o)) = (ep_in, ep_out) {
                        return Ok((dev, desc, config.number(), alt.interface_number(), i, o));
                    }
                }
            }
        }
    }
    bail!("no Xbox One / Series controller (GIP over USB) found; is it plugged in by cable?")
}

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

    let ctx = rusb::Context::new()?;
    let (dev, desc, config, iface, ep_in, ep_out) = find_gip(&ctx)?;
    let handle = dev
        .open()
        .context("opening the controller (is another program, e.g. Chrome, holding it?)")?;
    let product = handle
        .read_product_string_ascii(&desc)
        .unwrap_or_else(|_| "?".into());
    println!(
        "found {product} ({:04x}:{:04x}), interface {iface}, endpoints in {ep_in:#04x} out {ep_out:#04x}",
        desc.vendor_id(),
        desc.product_id()
    );
    if handle.active_configuration().ok() != Some(config) {
        handle
            .set_active_configuration(config)
            .context("configuring the controller")?;
    }
    handle
        .claim_interface(iface)
        .context("claiming the controller (is another program, e.g. Chrome, holding it?)")?;
    let mut gip = Gip {
        handle,
        ep_in,
        ep_out,
        seq: 0,
    };

    gip.send(CMD_POWER, OPT_INTERNAL, &[0x00])?;
    if desc.vendor_id() == PDP_VENDOR {
        // PDP pads stay silent until they get these (xpad: xboxone_pdp_led_on, xboxone_pdp_auth)
        gip.send(CMD_LED, OPT_INTERNAL, &[0x00, 0x01, 0x14])?;
        gip.send(CMD_AUTHENTICATE, OPT_INTERNAL, &[0x01, 0x00])?;
    }
    if let Err(e) = gip.rumble(40, 25) {
        eprintln!("rumble not accepted: {e:#}");
    }
    println!("reading; press buttons and move the sticks (Ctrl+C to stop)");

    let start = std::time::Instant::now();
    let mut pad = Pad::default();
    let mut last = String::new();
    let mut buf = [0u8; 64];
    let mut out = std::io::stdout();
    loop {
        if seconds.is_some_and(|s| start.elapsed().as_secs() >= s) {
            println!();
            return Ok(());
        }
        let n = match gip
            .handle
            .read_interrupt(gip.ep_in, &mut buf, Duration::from_millis(200))
        {
            Ok(n) => n,
            Err(rusb::Error::Timeout) => continue,
            Err(e) => return Err(e).context("reading from the controller"),
        };
        let p = &buf[..n];
        if p.len() < 4 {
            continue;
        }
        if raw {
            println!("{:02x?}", p);
        }
        if p[1] & OPT_ACK_WANTED != 0 {
            if let Some(h) = Header::parse(p) {
                gip.ack(&h, h.length as u16, 0)?;
            }
        }
        match p[0] {
            CMD_INPUT => {
                pad.apply_input(p);
            }
            CMD_GUIDE if p.len() > 4 => pad.guide = p[4] & 1 != 0,
            _ => continue,
        }
        let line = pad.line();
        if line != last && !raw {
            write!(out, "\r{line:<100}")?;
            out.flush()?;
            last = line;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_report_decodes() {
        // A pressed, Up, RB; LT half, RT full; left stick right, right stick down
        let mut p = vec![CMD_INPUT, 0x00, 0x05, 0x0E, 1 << 4, (1 << 0) | (1 << 5)];
        p.extend_from_slice(&512u16.to_le_bytes());
        p.extend_from_slice(&1023u16.to_le_bytes());
        for v in [32767i16, 0, 0, -32768] {
            p.extend_from_slice(&v.to_le_bytes());
        }
        let mut pad = Pad::default();
        assert!(pad.apply_input(&p));
        assert_eq!(pad.pressed(), ["A", "Up", "RB"]);
        assert_eq!((pad.left_trigger, pad.right_trigger), (512, 1023));
        assert_eq!((pad.left_stick, pad.right_stick), ((32767, 0), (0, -32768)));
        assert!(!pad.apply_input(&p[..17]));
    }

    #[test]
    fn headers_with_varint_lengths_and_chunks() {
        // First chunk: length 0x3a, total 289 (a1 02)
        let mut p = vec![0x04, 0xF0, 0x7C, 0x3A, 0xA1, 0x02];
        p.extend([0u8; 0x3A]);
        let h = Header::parse(&p).unwrap();
        assert_eq!(
            (h.command, h.length, h.chunk, h.payload),
            (0x04, 0x3A, Some(289), 6)
        );
        // A later chunk with a two-byte length (ba 00) at offset 0x3a
        let mut p = vec![0x04, 0xA0, 0x7C, 0xBA, 0x00, 0x3A];
        p.extend([0u8; 0x3A]);
        let h = Header::parse(&p).unwrap();
        assert_eq!((h.length, h.chunk, h.payload), (0x3A, Some(0x3A), 6));
        // A length past the end of the packet is rejected
        assert!(Header::parse(&[0x20, 0x00, 0x01, 0x10, 0x00]).is_none());
    }

    #[test]
    fn announce_names_the_firmware() {
        let body = [
            0x06, 0x05, 0x04, 0x03, 0x02, 0x01, 0, 0, 0x6F, 0x0E, 0x38, 0x03, 2, 0, 0, 0, 3, 0, 0,
            0, 2, 0, 1, 0, 1, 0, 1, 0,
        ];
        assert_eq!(
            describe_announce(&body).unwrap(),
            "address 01:02:03:04:05:06 vendor 0e6f product 0338 firmware 2.0.3.0 hardware 2.1.1.1"
        );
        assert_eq!(
            strings(b"\x1a\x00Windows.Xbox.Input.Gamepad\x1b\x00ab", 6),
            ["Windows.Xbox.Input.Gamepad"]
        );
    }

    #[test]
    fn share_button_in_the_long_report() {
        let mut p = vec![0u8; 50];
        p[0] = CMD_INPUT;
        p[3] = 0x2E;
        p[32] = 1;
        let mut pad = Pad::default();
        assert!(pad.apply_input(&p));
        assert_eq!(pad.pressed(), ["Share"]);
        // The short report has no Share button, whatever its byte 32 holds
        p[3] = 0x0E;
        assert!(pad.apply_input(&p));
        assert!(pad.pressed().is_empty());
    }
}
