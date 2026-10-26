//! Reads an Xbox One / Series style controller that speaks GIP (Game Input Protocol) over USB.
//!
//! macOS has no driver for wired third-party Xbox pads such as the PDP Afterglow Wave: they show up as
//! a vendor-class USB device (class 0xFF, interface subclass 0x47, protocol 0xD0) and no HID device is
//! created, so games and the Game Controller framework never see them. This talks to the pad directly
//! through the pure-Rust `nusb` crate (IOKit underneath). The packet layouts follow the Linux `xpad` driver.
//!
//! GIP packets start with a 4-byte header: command, options, sequence number, payload length.
//! - Host to pad, to start input: power on (0x05), and for PDP pads also LED on (0x0A) and the
//!   authentication-done notice (0x06).
//! - Pad to host: input (0x20) with buttons, triggers and sticks; the guide button (0x07), which asks
//!   for an acknowledgement when its options carry 0x10.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use nusb::descriptors::TransferType;
use nusb::transfer::{Buffer, Direction, In, Interrupt, Out};
use nusb::{DeviceInfo, Endpoint, Interface, MaybeFuture};

pub const GIP_CLASS: (u8, u8, u8) = (0xFF, 0x47, 0xD0);
pub const CMD_ACK: u8 = 0x01;
pub const CMD_POWER: u8 = 0x05;
pub const CMD_AUTHENTICATE: u8 = 0x06;
pub const CMD_GUIDE: u8 = 0x07;
pub const CMD_LED: u8 = 0x0A;
pub const CMD_RUMBLE: u8 = 0x09;
pub const CMD_INPUT: u8 = 0x20;
pub const OPT_INTERNAL: u8 = 0x20;
pub const OPT_ACK_WANTED: u8 = 0x10;
pub const PDP_VENDOR: u16 = 0x0E6F;

/// The decoded state of one input report.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pad {
    pub buttons: u16,
    pub guide: bool,
    pub share: bool,
    /// 0..=1023
    pub left_trigger: u16,
    pub right_trigger: u16,
    /// -32768..=32767, up is positive
    pub left_stick: (i16, i16),
    pub right_stick: (i16, i16),
}

/// Button names in report order: byte 4 bits 2..7, then byte 5 bits 0..7.
pub const BUTTONS: [(u16, &str); 14] = [
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
    pub fn apply_input(&mut self, p: &[u8]) -> bool {
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

    pub fn pressed(&self) -> Vec<&'static str> {
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

    pub fn line(&self) -> String {
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

pub const OPT_CHUNK_START: u8 = 0x40;
pub const OPT_CHUNK: u8 = 0x80;

/// A GIP packet header. Lengths are LEB128 varints; chunked packets add a second varint, which is
/// the transfer's total length in the first chunk and the chunk's offset in the others.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Header {
    pub command: u8,
    pub options: u8,
    pub sequence: u8,
    pub length: usize,
    /// Total length for the first chunk, offset for later chunks
    pub chunk: Option<usize>,
    /// Where the payload starts
    pub payload: usize,
}

pub fn varint(p: &[u8], pos: &mut usize) -> Option<usize> {
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
    pub fn parse(p: &[u8]) -> Option<Self> {
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

    pub fn body<'a>(&self, p: &'a [u8]) -> &'a [u8] {
        &p[self.payload..self.payload + self.length]
    }
}

/// The pad's announce packet (0x02): who it is and its firmware and hardware versions.
pub fn describe_announce(body: &[u8]) -> Option<String> {
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
pub fn strings(data: &[u8], min: usize) -> Vec<String> {
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

pub struct Gip {
    // Held so the claimed interface outlives the endpoints
    _interface: Interface,
    reader: Endpoint<Interrupt, In>,
    writer: Endpoint<Interrupt, Out>,
    /// Endpoint addresses
    pub ep_in: u8,
    pub ep_out: u8,
    pub seq: u8,
}

impl Gip {
    pub fn send(&mut self, cmd: u8, options: u8, payload: &[u8]) -> Result<()> {
        self.seq = self.seq.wrapping_add(1).max(1);
        let mut packet = vec![cmd, options, self.seq, payload.len() as u8];
        packet.extend_from_slice(payload);
        self.write(&packet)
            .with_context(|| format!("sending command {cmd:#04x}"))
    }

    /// Writes one raw packet to the interrupt OUT endpoint, waiting up to 500 ms.
    pub fn write(&mut self, packet: &[u8]) -> Result<()> {
        let mut buf = Buffer::new(packet.len());
        buf.extend_from_slice(packet);
        let done = self
            .writer
            .transfer_blocking(buf, Duration::from_millis(500));
        done.status?;
        Ok(())
    }

    /// Waits up to `timeout` for one packet from the interrupt IN endpoint; `None` on a timeout.
    /// The next read is queued as soon as one arrives, so no report is missed between calls.
    pub fn read(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        if self.reader.pending() == 0 {
            let size = self.reader.max_packet_size().max(1);
            self.reader.submit(Buffer::new(size));
        }
        let Some(done) = self.reader.wait_next_complete(timeout) else {
            return Ok(None);
        };
        let size = self.reader.max_packet_size().max(1);
        self.reader.submit(Buffer::new(size));
        done.status?;
        Ok(Some(done.buffer[..done.actual_len].to_vec()))
    }

    /// Acknowledges a packet whose options ask for it. The ack carries the acknowledged packet's
    /// sequence number, and for chunked transfers how many bytes have arrived and how many remain
    /// (xone's `gip_pkt_acknowledge`).
    pub fn ack(&mut self, h: &Header, received: u16, remaining: u16) -> Result<()> {
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
        self.write(&packet).context("sending an acknowledgement")
    }

    /// Rumble for a moment, so the pad shows it is listening. Strength 0..=100 per motor.
    pub fn rumble(&mut self, strength: u8, duration_10ms: u8) -> Result<()> {
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

/// A GIP interface found on a USB device, not yet opened.
pub struct Found {
    pub info: DeviceInfo,
    pub vendor: u16,
    pub product: u16,
    pub config: u8,
    pub interface: u8,
    pub alt: u8,
    pub ep_in: u8,
    pub ep_out: u8,
}

/// Finds the first USB interface with the GIP class and interrupt IN and OUT endpoints.
pub fn find_gip() -> Result<Found> {
    for info in nusb::list_devices().wait()? {
        if !info
            .interfaces()
            .any(|i| (i.class(), i.subclass(), i.protocol()) == GIP_CLASS)
        {
            continue;
        }
        let Ok(device) = info.open().wait() else {
            continue;
        };
        for config in device.configurations() {
            for iface in config.interfaces() {
                for alt in iface.alt_settings() {
                    if (alt.class(), alt.subclass(), alt.protocol()) != GIP_CLASS {
                        continue;
                    }
                    let mut ep_in = None;
                    let mut ep_out = None;
                    for ep in alt.endpoints() {
                        if ep.transfer_type() == TransferType::Interrupt {
                            match ep.direction() {
                                Direction::In => ep_in = ep_in.or(Some(ep.address())),
                                Direction::Out => ep_out = ep_out.or(Some(ep.address())),
                            }
                        }
                    }
                    if let (Some(i), Some(o)) = (ep_in, ep_out) {
                        return Ok(Found {
                            vendor: info.vendor_id(),
                            product: info.product_id(),
                            config: config.configuration_value(),
                            interface: alt.interface_number(),
                            alt: alt.alternate_setting(),
                            ep_in: i,
                            ep_out: o,
                            info,
                        });
                    }
                }
            }
        }
    }
    bail!("no Xbox One / Series controller (GIP over USB) found; is it plugged in by cable?")
}

impl Found {
    /// Opens the device, selects its configuration and claims the GIP interface.
    pub fn open(&self) -> Result<(nusb::Device, Gip)> {
        const HELD: &str = "is another program, e.g. Chrome, holding it?";
        let device = self
            .info
            .open()
            .wait()
            .with_context(|| format!("opening the controller ({HELD})"))?;
        if device
            .active_configuration()
            .map(|c| c.configuration_value())
            .ok()
            != Some(self.config)
        {
            device
                .set_configuration(self.config)
                .wait()
                .context("configuring the controller")?;
        }
        let interface = device
            .claim_interface(self.interface)
            .wait()
            .with_context(|| format!("claiming the controller ({HELD})"))?;
        if self.alt != 0 {
            interface.set_alt_setting(self.alt).wait()?;
        }
        let reader = interface.endpoint::<Interrupt, In>(self.ep_in)?;
        let writer = interface.endpoint::<Interrupt, Out>(self.ep_out)?;
        let gip = Gip {
            _interface: interface,
            reader,
            writer,
            ep_in: self.ep_in,
            ep_out: self.ep_out,
            seq: 0,
        };
        Ok((device, gip))
    }
}

/// An opened and started controller.
pub struct Controller {
    pub gip: Gip,
    pub name: String,
    pub vendor: u16,
    pub product: u16,
    pad: Pad,
}

impl Controller {
    /// Finds the first GIP pad, claims it and sends the start packets (and the PDP ones for PDP
    /// pads), so it begins sending input.
    pub fn open() -> Result<Self> {
        let found = find_gip()?;
        let (_device, mut gip) = found.open()?;
        let name = found
            .info
            .product_string()
            .map_or_else(|| "?".into(), str::to_string);
        gip.send(CMD_POWER, OPT_INTERNAL, &[0x00])?;
        if found.vendor == PDP_VENDOR {
            // PDP pads stay silent until they get these (xpad: xboxone_pdp_led_on, xboxone_pdp_auth)
            gip.send(CMD_LED, OPT_INTERNAL, &[0x00, 0x01, 0x14])?;
            gip.send(CMD_AUTHENTICATE, OPT_INTERNAL, &[0x01, 0x00])?;
        }
        Ok(Self {
            gip,
            name,
            vendor: found.vendor,
            product: found.product,
            pad: Pad::default(),
        })
    }

    /// Waits up to `timeout` for one packet, acknowledges it if asked, and updates the state.
    /// Returns the raw packet, or `None` on a timeout.
    pub fn poll(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        let Some(p) = self
            .gip
            .read(timeout)
            .context("reading from the controller")?
        else {
            return Ok(None);
        };
        if p.len() >= 4 {
            if p[1] & OPT_ACK_WANTED != 0 {
                if let Some(h) = Header::parse(&p) {
                    self.gip.ack(&h, h.length as u16, 0)?;
                }
            }
            match p[0] {
                CMD_INPUT => {
                    self.pad.apply_input(&p);
                }
                CMD_GUIDE if p.len() > 4 => self.pad.guide = p[4] & 1 != 0,
                _ => {}
            }
        }
        Ok(Some(p))
    }

    /// The state after the last input report.
    pub fn state(&self) -> Pad {
        self.pad
    }
}

/// A stick axis as -1..=1 (up and right positive), with values inside `dead` treated as 0.
pub fn stick(v: i16, dead: f32) -> f32 {
    let x = (f32::from(v) / 32767.0).clamp(-1.0, 1.0);
    if x.abs() < dead {
        0.0
    } else {
        x.signum() * (x.abs() - dead) / (1.0 - dead)
    }
}

/// A trigger as 0..=1.
pub fn trigger(v: u16) -> f32 {
    (f32::from(v) / 1023.0).clamp(0.0, 1.0)
}

/// Reads a controller on its own thread, so a game loop can take the latest state without
/// blocking. If no pad is found or it is unplugged, it keeps retrying every second.
pub struct PadReader {
    latest: std::sync::Arc<std::sync::Mutex<Option<Pad>>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PadReader {
    pub fn spawn() -> Self {
        use std::sync::atomic::Ordering;
        let latest = std::sync::Arc::new(std::sync::Mutex::new(None));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (l, s) = (latest.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                let Ok(mut c) = Controller::open() else {
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                };
                *l.lock().unwrap() = Some(c.state());
                while !s.load(Ordering::Relaxed) {
                    match c.poll(Duration::from_millis(100)) {
                        Ok(Some(_)) => *l.lock().unwrap() = Some(c.state()),
                        Ok(None) => {}
                        Err(_) => break,
                    }
                }
                *l.lock().unwrap() = None;
            }
        });
        Self {
            latest,
            stop,
            thread: Some(thread),
        }
    }

    /// The latest state, or `None` while no pad is connected.
    pub fn latest(&self) -> Option<Pad> {
        *self.latest.lock().unwrap()
    }
}

impl Drop for PadReader {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
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
    fn sticks_and_triggers_normalise_with_a_dead_zone() {
        assert_eq!(stick(0, 0.15), 0.0);
        assert_eq!(stick(3000, 0.15), 0.0);
        assert_eq!(stick(32767, 0.15), 1.0);
        assert_eq!(stick(-32768, 0.15), -1.0);
        assert!((stick(16384, 0.0) - 0.5).abs() < 1e-3);
        assert_eq!((trigger(0), trigger(1023)), (0.0, 1.0));
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
