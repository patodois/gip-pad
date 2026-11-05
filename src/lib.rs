//! Reads an Xbox One / Series style controller that speaks GIP (Game Input Protocol) over USB.
//!
//! macOS has no driver for wired third-party Xbox pads such as the PDP Afterglow Wave: they show up as
//! a vendor-class USB device (class 0xFF, interface subclass 0x47, protocol 0xD0) and no HID device is
//! created, so games and the Game Controller framework never see them. This talks to the pad directly
//! through the pure-Rust `nusb` crate (IOKit underneath).
//!
//! Provenance: this protocol code was written by an AI (Claude) from its general knowledge of GIP and of
//! the Linux `xpad` and `xone` drivers, without reading their source at the time, then checked against
//! a real PDP pad and afterwards against Microsoft's MS-GIPUSB specification. No source text was copied.
//! The README lists which facts the specification states and which were found only on hardware.
//!
//! GIP packets start with a 4-byte header: command, options, sequence number, payload length.
//! - Host to pad, to start input: power on (0x05), and for PDP pads also LED on (0x0A) and the
//!   security-state packet (0x06) that the pad waits for.
//! - Pad to host: input (0x20) with buttons, triggers and sticks; the guide button (0x07), which asks
//!   for an acknowledgement when its options carry 0x10.

pub mod owner;

use std::time::Duration;

use anyhow::{bail, Context, Result};
use nusb::descriptors::TransferType;
use nusb::transfer::{Buffer, Direction, In, Interrupt, Out};
use nusb::{DeviceInfo, Endpoint, Interface, MaybeFuture};

use owner::{blocker, Blocker, Owner, SystemOwned};

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
    /// Applies an input report (command 0x20); returns false for an incomplete or fragmented report.
    pub fn apply_input(&mut self, p: &[u8]) -> bool {
        let Some(h) = Header::parse(p) else {
            return false;
        };
        if h.command != CMD_INPUT || h.chunk.is_some() || h.length < 14 {
            return false;
        }
        let p = h.body(p);
        let u16le = |i: usize| u16::from_le_bytes([p[i], p[i + 1]]);
        let i16le = |i: usize| i16::from_le_bytes([p[i], p[i + 1]]);
        self.buttons = u16le(0);
        self.left_trigger = u16le(2);
        self.right_trigger = u16le(4);
        self.left_stick = (i16le(6), i16le(8));
        self.right_stick = (i16le(10), i16le(12));
        // The Share flag is payload byte 28 in Series-era reports.
        self.share = h.length >= 0x2E && p[28] & 1 != 0;
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

/// The pad's announce packet (0x02, "Hello Device"): who it is and its versions.
///
/// The 28-byte payload follows MS-GIPUSB Table 27 (section 3.1.5.5.1): an 8-byte device ID, vendor and
/// product IDs, four 16-bit firmware fields (major, minor, build, revision), then four 2-byte
/// major.minor pairs of one byte each: hardware, RF protocol, security protocol and GIP version.
pub fn describe_announce(body: &[u8]) -> Option<String> {
    if body.len() < 28 {
        return None;
    }
    let u16le = |i: usize| u16::from_le_bytes([body[i], body[i + 1]]);
    let firmware = format!("{}.{}.{}.{}", u16le(12), u16le(14), u16le(16), u16le(18));
    let pair = |i: usize| format!("{}.{}", body[i], body[i + 1]);
    let address: Vec<String> = body[..6].iter().rev().map(|b| format!("{b:02x}")).collect();
    Some(format!(
        "address {} vendor {:04x} product {:04x} firmware {} hardware {} rf {} security {} gip {}",
        address.join(":"),
        u16le(8),
        u16le(10),
        firmware,
        pair(20),
        pair(22),
        pair(24),
        pair(26)
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
    /// sequence number, and for chunked transfers how many bytes have arrived and how many remain.
    /// The payload layout was confirmed on hardware; MS-GIPUSB gives only the packet's type, flags
    /// and length (Table 26).
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

/// The GIP interface of an opened device: configuration, interface, alternate setting and the
/// interrupt endpoints, or `None` if the device has none.
fn gip_interface(device: &nusb::Device) -> Option<(u8, u8, u8, u8, u8)> {
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
                    return Some((
                        config.configuration_value(),
                        alt.interface_number(),
                        alt.alternate_setting(),
                        i,
                        o,
                    ));
                }
            }
        }
    }
    None
}

/// Whether a device that lists these interfaces might be a pad that macOS has not configured: it
/// lists none, and its class leaves the real class to the interfaces (0x00, 0xEF) or is vendor
/// specific (0xFF).
fn may_be_unconfigured_pad(listed: &[(u8, u8, u8)], device_class: u8) -> bool {
    listed.is_empty() && matches!(device_class, 0x00 | 0xEF | 0xFF)
}

/// A GIP pad found by [`find_gip_any`], and whether a macOS driver holds it.
pub struct Detected {
    pub found: Found,
    /// Set if a macOS driver already drives this pad: its descriptors can be read, but it cannot be
    /// reset or claimed, and it needs no help from gip-pad.
    pub system_owned: Option<SystemOwned>,
}

fn system_owned(info: &DeviceInfo, driver: String) -> SystemOwned {
    SystemOwned {
        vendor: info.vendor_id(),
        product: info.product_id(),
        name: owner::display_name(info.manufacturer_string(), info.product_string()),
        owner: driver,
    }
}

/// Finds the first USB interface with the GIP class and interrupt IN and OUT endpoints, skipping
/// pads that a macOS driver already holds (an official Xbox pad, say): those are used through Game
/// Controller and cannot be claimed anyway, and a third-party pad plugged in beside one is still
/// found. If the only GIP pads present are held by the system, the error is a [`SystemOwned`]
/// (downcast it from the `anyhow::Error` to tell it from the rest).
///
/// macOS lists a device's interfaces only while the device is configured, and nothing configures a
/// vendor-class pad: after a USB reset (which `--info` does) it stays unconfigured and lists no
/// interfaces at all. So a device that lists none is opened and its descriptors read instead of
/// being skipped. A device that is certainly a pad but cannot be opened is an error, not "not found".
pub fn find_gip() -> Result<Found> {
    scan(false).map(|d| d.found)
}

/// Like [`find_gip`], but a pad held by a macOS driver is returned too, with `system_owned` set, if
/// no other pad is present. For inspecting such a pad: its descriptors are readable.
pub fn find_gip_any() -> Result<Detected> {
    scan(true)
}

fn scan(include_system: bool) -> Result<Detected> {
    let mut unreadable: Vec<String> = Vec::new();
    // Pads a system driver holds; the first one decides the error if nothing else turns up
    let mut held: Vec<SystemOwned> = Vec::new();
    let mut held_found: Option<Detected> = None;
    for info in nusb::list_devices().wait().context("listing USB devices")? {
        let listed: Vec<(u8, u8, u8)> = info
            .interfaces()
            .map(|i| (i.class(), i.subclass(), i.protocol()))
            .collect();
        let listed_gip = listed.contains(&GIP_CLASS);
        if !listed_gip && !may_be_unconfigured_pad(&listed, info.class()) {
            continue;
        }
        let driver = match owner::exclusive_owner(&info) {
            Some(Owner::Driver(name)) => Some(name),
            _ => None,
        };
        if let (false, true, Some(name)) = (include_system, listed_gip, &driver) {
            // Certainly a GIP pad and certainly the system's: leave it alone
            held.push(system_owned(&info, name.clone()));
            continue;
        }
        let device = match info.open().wait() {
            Ok(device) => device,
            Err(e) if listed_gip => {
                if let Some(name) = driver {
                    held.push(system_owned(&info, name));
                    continue;
                }
                return Err(e).with_context(|| {
                    format!(
                        "opening the controller {:04x}:{:04x}",
                        info.vendor_id(),
                        info.product_id()
                    )
                });
            }
            Err(e) => {
                unreadable.push(format!(
                    "{:04x}:{:04x} ({e})",
                    info.vendor_id(),
                    info.product_id()
                ));
                continue;
            }
        };
        if let Some((config, interface, alt, ep_in, ep_out)) = gip_interface(&device) {
            let system_owned = driver.map(|name| system_owned(&info, name));
            let detected = Detected {
                found: Found {
                    vendor: info.vendor_id(),
                    product: info.product_id(),
                    config,
                    interface,
                    alt,
                    ep_in,
                    ep_out,
                    info,
                },
                system_owned,
            };
            match &detected.system_owned {
                None => return Ok(detected),
                Some(owned) => {
                    held.push(owned.clone());
                    if include_system && held_found.is_none() {
                        held_found = Some(detected);
                    }
                }
            }
        }
    }
    if let Some(detected) = held_found {
        return Ok(detected);
    }
    // Only the first of several held pads is named
    if let Some(owned) = held.into_iter().next() {
        return Err(owned.into());
    }
    if unreadable.is_empty() {
        bail!("no Xbox One / Series controller (GIP over USB) found; is it plugged in by cable?")
    }
    bail!(
        "no Xbox One / Series controller (GIP over USB) found; could not read these devices: {}",
        unreadable.join(", ")
    )
}

impl Found {
    /// Turns a failure to open or claim this pad into the error to report: [`SystemOwned`] if a macOS
    /// driver holds it, otherwise the error with a hint that fits what is known about the holder.
    pub fn explain_error(&self, e: nusb::Error, what: String) -> anyhow::Error {
        let owner = owner::exclusive_owner(&self.info);
        match blocker(owner.as_ref(), e.kind(), e.os_error()) {
            Blocker::SystemDriver(name) => system_owned(&self.info, name).into(),
            other => anyhow::Error::new(e).context(format!("{what}{}", other.hint())),
        }
    }

    /// Opens the device, selects its configuration and claims the GIP interface. A pad that a macOS
    /// driver holds is refused with a [`SystemOwned`] error.
    pub fn open(&self) -> Result<(nusb::Device, Gip)> {
        if let Some(Owner::Driver(name)) = owner::exclusive_owner(&self.info) {
            return Err(system_owned(&self.info, name).into());
        }
        let device = self
            .info
            .open()
            .wait()
            .map_err(|e| self.explain_error(e, "opening the controller".into()))?;
        if device
            .active_configuration()
            .map(|c| c.configuration_value())
            .ok()
            != Some(self.config)
        {
            device.set_configuration(self.config).wait().map_err(|e| {
                self.explain_error(e, format!("selecting configuration {}", self.config))
            })?;
        }
        let interface = device
            .claim_interface(self.interface)
            .wait()
            .map_err(|e| self.explain_error(e, format!("claiming interface {}", self.interface)))?;
        if self.alt != 0 {
            interface
                .set_alt_setting(self.alt)
                .wait()
                .with_context(|| format!("selecting alternate setting {}", self.alt))?;
        }
        let reader = interface
            .endpoint::<Interrupt, In>(self.ep_in)
            .with_context(|| format!("opening endpoint {:#04x}", self.ep_in))?;
        let writer = interface
            .endpoint::<Interrupt, Out>(self.ep_out)
            .with_context(|| format!("opening endpoint {:#04x}", self.ep_out))?;
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
            // This PDP pad stays silent until it gets these two (found on hardware)
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
/// blocking. If no pad is found or it is unplugged, it keeps retrying every second and keeps the
/// reason in [`PadReader::last_error`].
pub struct PadReader {
    latest: std::sync::Arc<std::sync::Mutex<Option<Pad>>>,
    error: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PadReader {
    pub fn spawn() -> Self {
        use std::sync::atomic::Ordering;
        let latest = std::sync::Arc::new(std::sync::Mutex::new(None));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let error = std::sync::Arc::new(std::sync::Mutex::new(None));
        let (l, s, er) = (latest.clone(), stop.clone(), error.clone());
        let thread = std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                let mut c = match Controller::open() {
                    Ok(c) => c,
                    Err(e) => {
                        *er.lock().unwrap() = Some(format!("{e:#}"));
                        std::thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                };
                *er.lock().unwrap() = None;
                *l.lock().unwrap() = Some(c.state());
                while !s.load(Ordering::Relaxed) {
                    match c.poll(Duration::from_millis(100)) {
                        Ok(Some(_)) => *l.lock().unwrap() = Some(c.state()),
                        Ok(None) => {}
                        Err(e) => {
                            *er.lock().unwrap() = Some(format!("{e:#}"));
                            break;
                        }
                    }
                }
                *l.lock().unwrap() = None;
            }
        });
        Self {
            latest,
            error,
            stop,
            thread: Some(thread),
        }
    }

    /// The latest state, or `None` while no pad is connected.
    pub fn latest(&self) -> Option<Pad> {
        *self.latest.lock().unwrap()
    }

    /// Why there is no pad right now (not found, held by another program, unplugged), or `None`
    /// while one is being read.
    pub fn last_error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
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
            "address 01:02:03:04:05:06 vendor 0e6f product 0338 firmware 2.0.3.0 hardware 2.0 rf 1.0 security 1.0 gip 1.0"
        );
        assert_eq!(
            strings(b"\x1a\x00Windows.Xbox.Input.Gamepad\x1b\x00ab", 6),
            ["Windows.Xbox.Input.Gamepad"]
        );
    }

    #[test]
    fn announce_from_the_pdp_pad_reads_hardware_and_protocol_versions() {
        // The hardware version bytes the connected PDP pad (0e6f:0338) sends: `02 00 01 00 01 00 01 00`.
        // MS-GIPUSB Table 27 makes them four major.minor byte pairs (hardware 2.0, RF 1.0, security 1.0,
        // GIP 1.0), not four 16-bit numbers.
        let mut p = vec![0x02, 0x20, 0x01, 0x1C];
        p.extend([
            0x06, 0x05, 0x04, 0x03, 0x02, 0x01, 0x00, 0x00, // device ID
            0x6F, 0x0E, 0x38, 0x03, // vendor 0e6f, product 0338
            0x02, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, // firmware 2.0.3.0
            0x02, 0x00, 0x01, 0x00, 0x01, 0x00, 0x01, 0x00, // hardware, RF, security, GIP
        ]);
        let h = Header::parse(&p).unwrap();
        let text = describe_announce(h.body(&p)).unwrap();
        assert!(
            text.ends_with("hardware 2.0 rf 1.0 security 1.0 gip 1.0"),
            "{text}"
        );
        assert!(!text.contains("2.1.1.1"));
        // Each of the eight bytes is its own field, so a minor version above 0 stays one byte
        let mut body = h.body(&p).to_vec();
        body[20..28].copy_from_slice(&[3, 7, 1, 0, 1, 2, 1, 3]);
        assert!(describe_announce(&body)
            .unwrap()
            .ends_with("hardware 3.7 rf 1.0 security 1.2 gip 1.3"));
        // Shorter than the 28-byte payload of Table 27
        assert!(describe_announce(&body[..27]).is_none());
    }

    #[test]
    fn unconfigured_vendor_devices_are_inspected() {
        // The pad after a USB reset: no interfaces listed, vendor-specific device class
        assert!(may_be_unconfigured_pad(&[], 0xFF));
        assert!(may_be_unconfigured_pad(&[], 0x00));
        // A hub, or a device whose interfaces are listed and are not GIP, is not worth opening
        assert!(!may_be_unconfigured_pad(&[], 0x09));
        assert!(!may_be_unconfigured_pad(&[(0x03, 0, 0)], 0x00));
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

#[cfg(test)]
mod input_header_regression {
    use super::*;
    #[test]
    fn incomplete_input_does_not_change_state() {
        let mut pad = Pad {
            buttons: 123,
            ..Pad::default()
        };
        let mut p = vec![0x20, 0, 1, 46];
        p.extend_from_slice(&[0; 14]);
        assert!(!pad.apply_input(&p));
        assert_eq!(pad.buttons, 123);
    }
    #[test]
    fn extended_input_length_keeps_payload_offsets() {
        let mut pad = Pad::default();
        let mut p = vec![0x20, 0, 1, 0x8e, 0];
        p.extend_from_slice(&[16, 0, 12, 0, 23, 0, 34, 0, 45, 0, 56, 0, 67, 0]);
        assert!(pad.apply_input(&p));
        assert_eq!(pad.buttons, 16);
        assert_eq!(pad.left_trigger, 12);
        assert_eq!(pad.right_trigger, 23);
        assert_eq!(pad.left_stick, (34, 45));
        assert_eq!(pad.right_stick, (56, 67));
        p[1] = OPT_CHUNK;
        p.insert(5, 0);
        assert!(Header::parse(&p).is_some());
        assert!(!pad.apply_input(&p));
    }
}

#[cfg(test)]
mod input_guard_coverage {
    use super::*;
    fn packet(command: u8, length: u8) -> Vec<u8> {
        let mut p = vec![command, 0, 1, length];
        p.resize(4 + usize::from(length), 0);
        p
    }
    #[test]
    fn parsed_non_input_commands_leave_state_unchanged() {
        let p = packet(0x21, 14);
        assert!(Header::parse(&p).is_some());
        let mut pad = Pad {
            buttons: 123,
            ..Pad::default()
        };
        assert!(!pad.apply_input(&p));
        assert_eq!(pad.buttons, 123);
    }
    #[test]
    fn parsed_short_payloads_are_refused_before_field_reads() {
        let p = packet(CMD_INPUT, 13);
        assert!(Header::parse(&p).is_some());
        let mut pad = Pad {
            buttons: 123,
            ..Pad::default()
        };
        assert!(!pad.apply_input(&p));
        assert_eq!(pad.buttons, 123);
    }
    #[test]
    fn parsed_chunks_are_refused_even_with_a_complete_payload() {
        let mut p = vec![CMD_INPUT, OPT_CHUNK, 1, 14, 0];
        p.resize(19, 0);
        assert_eq!(Header::parse(&p).unwrap().chunk, Some(0));
        let mut pad = Pad {
            buttons: 123,
            ..Pad::default()
        };
        assert!(!pad.apply_input(&p));
        assert_eq!(pad.buttons, 123);
    }
    #[test]
    fn share_requires_the_long_report_not_just_byte_28() {
        let mut pad = Pad::default();
        for (length, expected) in [(29, false), (46, true)] {
            let mut p = packet(CMD_INPUT, length);
            p[4 + 28] = 1;
            assert!(Header::parse(&p).is_some());
            assert!(pad.apply_input(&p));
            assert_eq!(pad.share, expected);
        }
    }
}
