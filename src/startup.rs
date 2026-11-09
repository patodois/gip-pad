//! Packet startup shared by the controller and diagnostics; no logging in the library.

use crate::connection::Clock;
use crate::{
    Gip, Header, CMD_AUTHENTICATE, CMD_LED, CMD_POWER, OPT_ACK_WANTED, OPT_INTERNAL, PDP_VENDOR,
};
use anyhow::{Context, Result};
use std::time::Duration;

pub(crate) const ANNOUNCE_TIMEOUT: Duration = Duration::from_millis(1500);

/// Outcome of the optional reset and Hello Device handshake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupStatus {
    Announced,
    TimedOut,
    ResetUnsupported,
    Skipped,
}

/// Packet operations used by GIP startup and the command-line diagnostic workflow.
pub trait Transport {
    fn read(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>>;
    fn send(&mut self, command: u8, options: u8, body: &[u8]) -> Result<()>;
    fn ack(&mut self, header: &Header, received: u16, remaining: u16) -> Result<()>;
}

impl Transport for Gip {
    fn read(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        Gip::read(self, timeout)
    }
    fn send(&mut self, command: u8, options: u8, body: &[u8]) -> Result<()> {
        Gip::send(self, command, options, body)
    }
    fn ack(&mut self, header: &Header, received: u16, remaining: u16) -> Result<()> {
        Gip::ack(self, header, received, remaining)
    }
}

/// MS-GIPUSB 3.1.5.5.1, Table 27 specifies flags 0x20, 28 payload bytes,
/// nonzero sequence, and Device ID 00:00:UU:VV:WW:XX:YY:ZZ (little endian).
/// Payload bytes 6 and 7 must be zero; an invalid Device ID must not be answered.
/// Accepting ACME or trailing Hello bytes is our compatibility tolerance, not a
/// Table 27 extension. An ACME reply uses the ACK mechanism in section 3.1.5.1.
/// Larger fragmented messages are covered by section 3.1.5.2 (MTU: 2.2.10.4);
/// this Hello parser rejects fragments and secondary devices.
pub(crate) fn is_announce(h: &Header, packet: &[u8]) -> bool {
    h.command == 0x02
        && h.options & !OPT_ACK_WANTED == OPT_INTERNAL
        && h.length >= 28
        && h.sequence != 0
        && packet
            .get(h.payload..)
            .and_then(|body| body.get(..h.length))
            .is_some_and(|body| body.get(6..8) == Some(&[0, 0][..]))
}

pub(crate) fn wait_for_announce(
    transport: &mut impl Transport,
    clock: &dyn Clock,
    timeout: Duration,
    mut on_packet: impl FnMut(&[u8]),
) -> Result<StartupStatus> {
    let until = clock.now() + timeout;
    loop {
        let remaining = until.saturating_sub(clock.now());
        if remaining.is_zero() {
            return Ok(StartupStatus::TimedOut);
        }
        let Some(packet) = transport
            .read(remaining.min(Duration::from_millis(100)))
            .context("waiting for the controller announce after USB reset")?
        else {
            continue;
        };
        on_packet(&packet);
        if let Some(h) = Header::parse(&packet).filter(|h| is_announce(h, &packet)) {
            if h.options & OPT_ACK_WANTED != 0 {
                transport.ack(&h, h.length as u16, 0)?;
            }
            return Ok(StartupStatus::Announced);
        }
    }
}

pub(crate) fn send_start(transport: &mut impl Transport, vendor: u16) -> Result<()> {
    // MS-GIPUSB 3.1.5.5.1 permits START as the Hello response without metadata.
    transport.send(CMD_POWER, OPT_INTERNAL, &[0x00])?;
    if vendor == PDP_VENDOR {
        // Existing hardware-confirmed PDP sequence; MS-GIPUSB does not specify its security payload.
        transport.send(CMD_LED, OPT_INTERNAL, &[0x00, 0x01, 0x14])?;
        transport.send(CMD_AUTHENTICATE, OPT_INTERNAL, &[0x01, 0x00])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Pad, CMD_INPUT};
    use std::collections::VecDeque;

    trait Reset {
        type Transport: Transport;
        fn reset_and_open(self) -> Result<Self::Transport>;
    }
    struct WallClock(std::time::Instant);
    impl Clock for WallClock {
        fn now(&self) -> Duration {
            self.0.elapsed()
        }
    }
    fn reset_and_wait<R: Reset>(
        reset: R,
        timeout: Duration,
        on_packet: impl FnMut(&[u8]),
    ) -> Result<R::Transport> {
        let mut transport = reset.reset_and_open()?;
        wait_for_announce(
            &mut transport,
            &WallClock(std::time::Instant::now()),
            timeout,
            on_packet,
        )?;
        Ok(transport)
    }
    fn start<R: Reset>(reset: R, vendor: u16, timeout: Duration) -> Result<R::Transport> {
        let mut transport = reset_and_wait(reset, timeout, |_| {})?;
        send_start(&mut transport, vendor)?;
        Ok(transport)
    }

    struct StalePad {
        reset: bool,
        started: bool,
        led: bool,
        authenticated: bool,
        emit_announce: bool,
        announced: bool,
        packets: VecDeque<Vec<u8>>,
        trace: Vec<&'static str>,
        timeouts: Vec<Duration>,
        continuous_traffic: bool,
        read_error: bool,
        reset_error: bool,
        acks: Vec<(u8, u16, u16)>,
    }

    impl Default for StalePad {
        fn default() -> Self {
            Self {
                reset: false,
                started: false,
                led: false,
                authenticated: false,
                emit_announce: true,
                announced: false,
                packets: VecDeque::new(),
                trace: Vec::new(),
                timeouts: Vec::new(),
                continuous_traffic: false,
                read_error: false,
                reset_error: false,
                acks: Vec::new(),
            }
        }
    }

    fn hello() -> Vec<u8> {
        // MS-GIPUSB 3.1.5.5.1, Table 27. Same ID/version layout as the repository's PDP fixture.
        let mut packet = vec![0x02, OPT_INTERNAL, 1, 28];
        packet.extend_from_slice(&[
            6, 5, 4, 3, 2, 1, 0, 0, 0x6f, 0x0e, 0x38, 3, 2, 0, 0, 0, 3, 0, 0, 0, 2, 0, 1, 0, 1, 0,
            1, 0,
        ]);
        packet
    }

    impl Reset for StalePad {
        type Transport = Self;

        fn reset_and_open(mut self) -> Result<Self> {
            if self.reset_error {
                anyhow::bail!("fake USB reset failed");
            }
            assert!(!self.reset, "startup must reset exactly once");
            self.reset = true;
            self.trace.push("reset");
            if self.emit_announce {
                self.packets.push_back(hello());
            }
            Ok(self)
        }
    }

    impl Transport for StalePad {
        fn ack(&mut self, h: &Header, received: u16, remaining: u16) -> Result<()> {
            self.acks.push((h.sequence, received, remaining));
            Ok(())
        }
        fn read(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
            self.timeouts.push(timeout);
            if self.read_error {
                anyhow::bail!("fake disconnected");
            }
            if let Some(packet) = self.packets.pop_front() {
                if packet == hello() {
                    self.announced = true;
                    self.trace.push("announce");
                }
                return Ok(Some(packet));
            }
            if self.reset && self.started && self.led && self.authenticated {
                // A, Up; LT=512, RT=1023, both sticks moved (MS-GIPUSB Table 57).
                let mut report = vec![CMD_INPUT, 0, 1, 14, 16, 1];
                report.extend_from_slice(&[0, 2, 255, 3, 1, 0, 2, 0, 3, 0, 4, 0]);
                Ok(Some(report))
            } else if self.continuous_traffic {
                Ok(Some(vec![0x03, OPT_INTERNAL, 1, 1, 0]))
            } else {
                std::thread::sleep(timeout);
                Ok(None)
            }
        }

        fn send(&mut self, command: u8, options: u8, body: &[u8]) -> Result<()> {
            assert_eq!(options, OPT_INTERNAL);
            match command {
                CMD_POWER => {
                    assert_eq!(body, [0]);
                    assert!(
                        !self.emit_announce || self.announced,
                        "START must follow announce"
                    );
                    self.trace.push("start");
                    self.started = true;
                }
                CMD_LED => {
                    assert_eq!(body, [0, 1, 20]);
                    assert!(self.started);
                    self.trace.push("led");
                    self.led = true;
                }
                CMD_AUTHENTICATE => {
                    assert_eq!(body, [1, 0]);
                    assert!(self.led);
                    self.trace.push("authenticate");
                    self.authenticated = true;
                }
                _ => panic!("unexpected command {command:#04x}"),
            }
            Ok(())
        }
    }

    #[test]
    fn stale_pad_delivers_input_after_open() {
        let mut transport = start(StalePad::default(), PDP_VENDOR, Duration::from_secs(1)).unwrap();
        let report = transport
            .read(Duration::from_millis(10))
            .unwrap()
            .expect("an opened stale pad must deliver input after reset and startup");
        let mut pad = Pad::default();
        assert!(pad.apply_input(&report));
        assert_eq!(pad.pressed(), ["A", "Up"]);
        assert_eq!((pad.left_trigger, pad.right_trigger), (512, 1023));
        assert_eq!((pad.left_stick, pad.right_stick), ((1, 2), (3, 4)));
        assert_eq!(
            transport.trace,
            ["reset", "announce", "start", "led", "authenticate"]
        );
    }

    #[test]
    fn no_announce_times_out_and_keeps_existing_pdp_startup() {
        let fake = StalePad {
            emit_announce: false,
            ..StalePad::default()
        };
        let timeout = Duration::from_millis(10);
        let mut transport = start(fake, PDP_VENDOR, timeout).unwrap();
        assert_eq!(transport.trace, ["reset", "start", "led", "authenticate"]);
        assert!(transport.timeouts.iter().all(|t| *t <= timeout));
        assert!(transport.read(timeout).unwrap().is_some());
    }

    #[test]
    fn non_pdp_gets_only_start_after_announce() {
        let transport = start(StalePad::default(), 0x1234, Duration::from_secs(1)).unwrap();
        assert_eq!(transport.trace, ["reset", "announce", "start"]);
    }

    #[test]
    fn diagnostic_wait_observes_packets_and_ignores_invalid_announces() {
        let mut fragmented = hello();
        fragmented[1] |= crate::OPT_CHUNK;
        fragmented.insert(4, 28);
        let mut secondary = hello();
        secondary[1] |= 1;
        let mut zero_sequence = hello();
        zero_sequence[2] = 0;
        let invalid = vec![
            zero_sequence,
            vec![0x02, OPT_INTERNAL, 1, 28, 0], // truncated payload
            vec![0x02, OPT_INTERNAL, 1, 0],     // wrong length
            fragmented,
            secondary,
            vec![0x03, OPT_INTERNAL, 1, 1, 0], // unrelated status
        ];
        let fake = StalePad {
            packets: invalid.iter().cloned().collect(),
            ..StalePad::default()
        };
        let mut seen = Vec::new();
        let transport =
            reset_and_wait(fake, Duration::from_secs(1), |p| seen.push(p.to_vec())).unwrap();
        assert_eq!(&seen[..invalid.len()], invalid);
        assert_eq!(seen.last(), Some(&hello()));
        assert_eq!(transport.trace, ["reset", "announce"]);
        assert!(
            !transport.started,
            "the caller chooses START or metadata request"
        );
    }

    #[test]
    fn unrelated_traffic_does_not_extend_the_announce_deadline() {
        let fake = StalePad {
            emit_announce: false,
            continuous_traffic: true,
            ..StalePad::default()
        };
        let transport = start(fake, 0x1234, Duration::from_millis(10)).unwrap();
        assert!(!transport.timeouts.is_empty());
        assert_eq!(transport.trace, ["reset", "start"]);
    }

    #[test]
    fn hello_with_invalid_device_id_is_ignored_without_acknowledgement() {
        for high_byte in [6, 7] {
            let mut invalid = hello();
            invalid[4 + high_byte] = 1;
            invalid[1] |= OPT_ACK_WANTED;
            let fake = StalePad {
                packets: [invalid].into(),
                ..StalePad::default()
            };
            let transport = reset_and_wait(fake, Duration::from_secs(1), |_| {}).unwrap();
            assert_eq!(
                transport.timeouts.len(),
                2,
                "invalid Device ID must not finish Hello wait"
            );
            assert!(
                transport.acks.is_empty(),
                "invalid Device ID must not be answered"
            );
        }
    }

    #[test]
    fn hello_with_acme_and_a_longer_payload_is_accepted_and_acknowledged() {
        let mut packet = hello();
        packet[1] |= OPT_ACK_WANTED;
        packet[3] = 30;
        packet.extend([99, 88]);
        let fake = StalePad {
            emit_announce: false,
            packets: [packet].into(),
            ..StalePad::default()
        };
        let transport = reset_and_wait(fake, Duration::from_secs(1), |_| {}).unwrap();
        assert_eq!(
            transport.timeouts.len(),
            1,
            "Hello must finish the wait immediately"
        );
        assert_eq!(transport.acks, [(1, 30, 0)]);
    }

    #[test]
    fn reset_and_read_errors_are_not_misreported_as_announce_timeouts() {
        for fake in [
            StalePad {
                reset_error: true,
                ..StalePad::default()
            },
            StalePad {
                read_error: true,
                ..StalePad::default()
            },
        ] {
            assert!(start(fake, PDP_VENDOR, Duration::from_millis(10)).is_err());
        }
    }
}
