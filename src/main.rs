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
trait InfoSource {
    type Transport: Transport;
    fn describe(&mut self) -> Result<bool>;
    fn reopen(
        &mut self,
        on_packet: &mut dyn FnMut(&[u8]),
    ) -> Result<(Self::Transport, StartupStatus)>;
    fn now(&self) -> Duration;
}

struct UsbInfo {
    session: Option<(Found, nusb::Device)>,
    origin: std::time::Instant,
}

impl InfoSource for UsbInfo {
    type Transport = Gip;
    fn describe(&mut self) -> Result<bool> {
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
            return Ok(false);
        }
        self.session = Some((found, device));
        Ok(true)
    }
    fn reopen(&mut self, on_packet: &mut dyn FnMut(&[u8])) -> Result<(Gip, StartupStatus)> {
        let (found, device) = self.session.take().context("missing diagnostic device")?;
        found.reset_and_wait(device, on_packet)
    }
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

fn startup_warning(status: StartupStatus) -> Option<&'static str> {
    match status {
        StartupStatus::TimedOut => {
            Some("gip-pad: no announce (0x02) within 1500 ms after USB reset; continuing")
        }
        StartupStatus::ResetUnsupported => {
            Some("gip-pad: USB reset unsupported; continuing without reset")
        }
        _ => None,
    }
}

fn info(
    source: &mut impl InfoSource,
    raw: bool,
    output: &mut impl Write,
    errors: &mut impl Write,
) -> Result<()> {
    if !source.describe()? {
        return Ok(());
    }
    let mut metadata: Vec<u8> = Vec::new();
    let mut received = 0usize;
    let mut announced = false;
    let mut callback_error = None;
    let (mut gip, status) = source.reopen(&mut |p| {
        let mut emit = || -> std::io::Result<()> {
            if raw {
                writeln!(output, "{:02x?}", p)?;
            }
            if !announced {
                if let Some(h) = Header::parse(p).filter(|h| h.is_announce(p)) {
                    if let Some(d) = describe_announce(h.body(p)) {
                        announced = true;
                        writeln!(output, "announce: {d}")?;
                    }
                }
            }
            Ok(())
        };
        if let Err(e) = emit() {
            callback_error = Some(e);
        }
    })?;
    if let Some(e) = callback_error {
        return Err(e.into());
    }
    if let Some(warning) = startup_warning(status) {
        writeln!(errors, "{warning}")?;
    }
    gip.send(0x04, OPT_INTERNAL, &[])?;
    let until = source.now() + Duration::from_millis(2500);
    while source.now() < until {
        let Some(p) = gip.read(Duration::from_millis(100))? else {
            continue;
        };
        if raw {
            writeln!(output, "{:02x?}", p)?;
        }
        let Some(h) = Header::parse(&p) else { continue };
        let body = h.body(&p);
        match (h.command, h.chunk) {
            (0x02, _) if !announced && h.is_announce(&p) => {
                announced = true;
                if let Some(d) = describe_announce(body) {
                    writeln!(output, "announce: {d}")?;
                }
            }
            (0x04, Some(total)) if h.options & OPT_CHUNK_START != 0 => {
                (metadata, received) = start_metadata(total, body);
            }
            (0x04, Some(offset))
                if !metadata.is_empty() && offset + body.len() <= metadata.len() =>
            {
                store_metadata_chunk(&mut metadata, &mut received, offset, body);
            }
            _ => {}
        }
        if h.options & OPT_ACK_WANTED != 0 && (h.command != 0x02 || h.is_announce(&p)) {
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
        writeln!(output, "no metadata received")?;
    } else {
        writeln!(output, "metadata: {} of {} bytes", received, metadata.len())?;
        for s in strings(&metadata, 6) {
            writeln!(output, "  {s}")?;
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--info") {
        let mut source = UsbInfo {
            session: None,
            origin: std::time::Instant::now(),
        };
        return info(
            &mut source,
            args.iter().any(|a| a == "--raw"),
            &mut std::io::stdout(),
            &mut std::io::stderr(),
        );
    }
    let raw = args.iter().any(|a| a == "--raw");
    let seconds: Option<u64> = args
        .iter()
        .position(|a| a == "--seconds")
        .and_then(|i| args.get(i + 1)?.parse().ok());

    let mut c = Controller::open()?;
    if let Some(warning) = startup_warning(c.startup_status()) {
        eprintln!("{warning}");
    }
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

fn store_metadata_chunk(metadata: &mut [u8], received: &mut usize, offset: usize, body: &[u8]) {
    if offset <= *received {
        metadata[offset..offset + body.len()].copy_from_slice(body);
        *received = (*received).max(offset + body.len());
    }
}

#[cfg(test)]
mod metadata_regression {
    use super::*;
    #[test]
    fn metadata_gaps_do_not_count_as_received() {
        let mut b = vec![b'A', b'B', 0, 0, 0, 0];
        let mut n = 2;
        store_metadata_chunk(&mut b, &mut n, 4, b"EF");
        assert_eq!(n, 2);
        store_metadata_chunk(&mut b, &mut n, 2, b"CD");
        assert_eq!(n, 4);
        store_metadata_chunk(&mut b, &mut n, 4, b"EF");
        assert_eq!(n, 6);
        assert_eq!(b, b"ABCDEF");
    }
}

fn start_metadata(total: usize, body: &[u8]) -> (Vec<u8>, usize) {
    let mut metadata = vec![0; total];
    metadata[..body.len().min(total)].copy_from_slice(&body[..body.len().min(total)]);
    let received = body.len().min(total);
    (metadata, received)
}
#[cfg(test)]
mod first_metadata_coverage {
    use super::*;
    #[test]
    fn first_chunk_count_is_clamped_to_the_announced_total() {
        let (bytes, received) = start_metadata(2, b"ABCDEF");
        assert_eq!(bytes, b"AB");
        assert_eq!(received, 2);
    }
}

#[cfg(test)]
mod info_flow_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct FakeState {
        now: Duration,
        requests: usize,
        returned: bool,
        invalid_hello: bool,
        returned_invalid: bool,
        acks: usize,
    }
    struct FakePackets(Arc<Mutex<FakeState>>);
    struct FakeInfo {
        state: Arc<Mutex<FakeState>>,
        status: StartupStatus,
        system_owned: bool,
        reopens: usize,
    }

    impl InfoSource for FakeInfo {
        type Transport = FakePackets;
        fn describe(&mut self) -> Result<bool> {
            Ok(!self.system_owned)
        }
        fn reopen(
            &mut self,
            on_packet: &mut dyn FnMut(&[u8]),
        ) -> Result<(FakePackets, StartupStatus)> {
            self.reopens += 1;
            if self.status == StartupStatus::Announced {
                let mut hello = vec![0x02, OPT_INTERNAL | OPT_ACK_WANTED, 1, 30];
                hello.resize(34, 0);
                on_packet(&hello);
                on_packet(&hello);
            }
            Ok((FakePackets(self.state.clone()), self.status))
        }
        fn now(&self) -> Duration {
            self.state.lock().unwrap().now
        }
    }

    impl Transport for FakePackets {
        fn read(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
            let mut state = self.0.lock().unwrap();
            if state.invalid_hello && !state.returned_invalid {
                state.returned_invalid = true;
                let mut packet = vec![0x02, OPT_INTERNAL | OPT_ACK_WANTED, 1, 28];
                packet.resize(32, 0);
                packet[4 + 6] = 1;
                return Ok(Some(packet));
            }
            if state.requests > 0 && !state.returned {
                state.returned = true;
                return Ok(Some(vec![
                    0x04,
                    OPT_INTERNAL | OPT_CHUNK | OPT_CHUNK_START,
                    1,
                    6,
                    6,
                    b'G',
                    b'a',
                    b'm',
                    b'e',
                    b'p',
                    b'a',
                ]));
            }
            state.now += timeout;
            Ok(None)
        }
        fn send(&mut self, command: u8, options: u8, body: &[u8]) -> Result<()> {
            assert_eq!((command, options, body), (0x04, OPT_INTERNAL, &[][..]));
            self.0.lock().unwrap().requests += 1;
            Ok(())
        }
        fn ack(&mut self, _: &Header, _: u16, _: u16) -> Result<()> {
            self.0.lock().unwrap().acks += 1;
            Ok(())
        }
    }

    fn source(status: StartupStatus) -> FakeInfo {
        FakeInfo {
            state: Arc::default(),
            status,
            system_owned: false,
            reopens: 0,
        }
    }

    #[test]
    fn info_requests_metadata_and_prints_only_one_announce() {
        let mut source = source(StartupStatus::Announced);
        let (mut output, mut errors) = (Vec::new(), Vec::new());
        info(&mut source, true, &mut output, &mut errors).unwrap();
        assert_eq!(source.state.lock().unwrap().requests, 1);
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("metadata: 6 of 6 bytes"));
        assert_eq!(output.matches("announce:").count(), 1);
        assert!(errors.is_empty());
        assert_eq!(source.reopens, 1);
    }

    #[test]
    fn info_timeout_warning_is_neutral_and_metadata_still_works() {
        let mut source = source(StartupStatus::TimedOut);
        let (mut output, mut errors) = (Vec::new(), Vec::new());
        info(&mut source, false, &mut output, &mut errors).unwrap();
        assert!(String::from_utf8(output)
            .unwrap()
            .contains("metadata: 6 of 6 bytes"));
        let errors = String::from_utf8(errors).unwrap();
        assert!(errors.contains("no announce"));
        assert!(!errors.contains("startup sequence"));
    }

    #[test]
    fn metadata_does_not_acknowledge_hello_with_an_invalid_device_id() {
        let mut source = source(StartupStatus::Announced);
        source.state.lock().unwrap().invalid_hello = true;
        let mut output = Vec::new();
        info(&mut source, false, &mut output, &mut Vec::new()).unwrap();
        let state = source.state.lock().unwrap();
        assert!(state.returned);
        assert_eq!(
            state.acks, 0,
            "invalid Hello IDs must not receive ACKs during metadata"
        );
        assert!(String::from_utf8(output)
            .unwrap()
            .contains("metadata: 6 of 6 bytes"));
    }

    #[test]
    fn info_leaves_a_system_owned_pad_alone() {
        let mut source = source(StartupStatus::Announced);
        source.system_owned = true;
        info(&mut source, false, &mut Vec::new(), &mut Vec::new()).unwrap();
        assert_eq!(source.reopens, 0);
        assert_eq!(source.state.lock().unwrap().requests, 0);
    }
}
