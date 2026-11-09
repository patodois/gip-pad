use super::*;
use crate::{PadReader, CMD_AUTHENTICATE, CMD_INPUT, CMD_LED, CMD_POWER, OPT_INTERNAL, PDP_VENDOR};

struct State {
    now: Duration,
    candidate: Candidate,
    wrong: Vec<Candidate>,
    ready_delay: Duration,
    ready_at: Duration,
    resets: usize,
    opens: Vec<(Port, Duration)>,
    sleeps: Vec<Duration>,
    writes: Vec<Vec<u8>>,
    stale: bool,
    unsupported: bool,
    emit_announce: bool,
    hello_sent: bool,
    input_sent: bool,
    power: bool,
    led: bool,
    auth: bool,
    fail_claims: usize,
    fail_writes: usize,
    buttons: u16,
    live_handles: usize,
    disconnect_once: bool,
    inspected: Vec<Port>,
}

fn candidate() -> Candidate {
    Candidate {
        port: Port {
            bus: "bus-1".into(),
            chain: vec![1, 2],
            vendor: PDP_VENDOR,
            product: 0x0338,
        },
        instance: "physical-1".into(),
        name: "fake PDP".into(),
        found: None,
    }
}

impl Default for State {
    fn default() -> Self {
        Self {
            now: Duration::ZERO,
            candidate: candidate(),
            wrong: Vec::new(),
            ready_delay: Duration::ZERO,
            ready_at: Duration::ZERO,
            resets: 0,
            opens: Vec::new(),
            sleeps: Vec::new(),
            writes: Vec::new(),
            stale: true,
            unsupported: false,
            emit_announce: true,
            hello_sent: false,
            input_sent: false,
            power: false,
            led: false,
            auth: false,
            fail_claims: 0,
            fail_writes: 0,
            buttons: 16,
            live_handles: 0,
            disconnect_once: false,
            inspected: Vec::new(),
        }
    }
}

struct FakeBackend(Arc<Mutex<State>>);
struct FakeIo {
    state: Arc<Mutex<State>>,
    instance: String,
    port: Port,
}

fn hello() -> Vec<u8> {
    let mut packet = vec![0x02, OPT_INTERNAL, 1, 28];
    packet.extend([0; 28]);
    packet[4..10].copy_from_slice(&[6, 5, 4, 3, 2, 1]);
    packet
}

impl Clock for FakeBackend {
    fn now(&self) -> Duration {
        self.0.lock().unwrap().now
    }
}

impl Backend for FakeBackend {
    fn devices(&mut self, target: Option<&Port>) -> Result<Vec<Candidate>> {
        let mut state = self.0.lock().unwrap();
        if state.live_handles > 0 {
            anyhow::bail!("fake device is exclusively claimed");
        }
        let mut devices = if state.resets > 0 {
            state.wrong.clone()
        } else {
            Vec::new()
        };
        if state.now >= state.ready_at {
            devices.push(state.candidate.clone());
        }
        let entries = devices.into_iter().map(|candidate| DeviceEntry {
            port: candidate.port.clone(),
            listed_gip: true,
            unconfigured: false,
            data: candidate,
        });
        inspect_candidates(entries, target, |candidate| {
            state.inspected.push(candidate.port.clone());
            Ok(candidate)
        })
    }
    fn reset(&mut self, _: &Candidate) -> Result<ResetResult> {
        let mut state = self.0.lock().unwrap();
        state.resets += 1;
        if state.unsupported {
            return Ok(ResetResult::Unsupported);
        }
        state.candidate.instance = format!("reset-{}", state.resets);
        state.ready_at = state.now + state.ready_delay;
        state.stale = false;
        state.hello_sent = false;
        state.input_sent = false;
        state.power = false;
        state.led = false;
        state.auth = false;
        Ok(ResetResult::Reset)
    }
    fn open(&mut self, candidate: &Candidate) -> Result<Gip> {
        let mut state = self.0.lock().unwrap();
        let now = state.now;
        state.opens.push((candidate.port.clone(), now));
        if state.now < state.ready_at && same_port(&state.candidate.port, &candidate.port) {
            anyhow::bail!("fake pad still rebooting");
        }
        if state.fail_claims > 0 {
            state.fail_claims -= 1;
            anyhow::bail!("fake claim failed");
        }
        state.live_handles += 1;
        Ok(Gip {
            io: Box::new(FakeIo {
                state: self.0.clone(),
                instance: candidate.instance.clone(),
                port: candidate.port.clone(),
            }),
            ep_in: 0x81,
            ep_out: 1,
            seq: 0,
        })
    }
    fn sleep(&mut self, duration: Duration) {
        let mut state = self.0.lock().unwrap();
        state.now += duration;
        state.sleeps.push(duration);
        drop(state);
        std::thread::yield_now();
    }
}

impl Drop for FakeIo {
    fn drop(&mut self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .live_handles -= 1;
    }
}

impl PacketIo for FakeIo {
    fn read(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        let mut state = self.state.lock().unwrap();
        if state.disconnect_once {
            state.disconnect_once = false;
            return Err(nusb::transfer::TransferError::Disconnected.into());
        }
        if same_port(&self.port, &state.candidate.port) && self.instance != state.candidate.instance
        {
            return Err(nusb::transfer::TransferError::Disconnected.into());
        }
        if !state.stale && state.emit_announce && !state.hello_sent {
            state.hello_sent = true;
            return Ok(Some(hello()));
        }
        if !state.stale && state.power && state.led && state.auth && !state.input_sent {
            state.input_sent = true;
            let buttons = if self.port == state.candidate.port {
                state.buttons
            } else {
                32
            };
            let mut report = vec![CMD_INPUT, 0, 1, 14];
            report.extend(buttons.to_le_bytes());
            report.extend([0, 2, 255, 3, 1, 0, 2, 0, 3, 0, 4, 0]);
            return Ok(Some(report));
        }
        state.now += timeout;
        drop(state);
        std::thread::yield_now();
        Ok(None)
    }
    fn write(&mut self, packet: &[u8]) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.fail_writes > 0 {
            state.fail_writes -= 1;
            anyhow::bail!("fake startup write failed");
        }
        state.writes.push(packet.to_vec());
        match packet[0] {
            CMD_POWER => state.power = true,
            CMD_LED => state.led = true,
            CMD_AUTHENTICATE => state.auth = true,
            _ => {}
        }
        Ok(())
    }
}

fn install(state: State) -> Arc<Mutex<State>> {
    let state = Arc::new(Mutex::new(state));
    TEST_BACKEND.with(|slot| *slot.borrow_mut() = Some(Box::new(FakeBackend(state.clone()))));
    state
}

fn assert_input(controller: &mut Controller) {
    controller
        .poll(Duration::from_millis(100))
        .unwrap()
        .expect("stale pad must deliver input");
    let pad = controller.state();
    assert_eq!(pad.pressed(), ["A"]);
    assert_eq!((pad.left_trigger, pad.right_trigger), (512, 1023));
    assert_eq!((pad.left_stick, pad.right_stick), ((1, 2), (3, 4)));
}

#[test]
fn controller_open_recovers_a_stale_pad_through_the_usb_boundary() {
    let state = install(State::default());
    let mut controller = Controller::open().unwrap();
    assert!(controller.announced());
    assert_input(&mut controller);
    let state = state.lock().unwrap();
    assert_eq!(state.resets, 1);
    assert_eq!(
        state.writes.iter().map(|p| p[0]).collect::<Vec<_>>(),
        [CMD_POWER, CMD_LED, CMD_AUTHENTICATE]
    );
}

#[test]
fn controller_reopens_only_the_original_bus_port_vendor_and_product() {
    let original = candidate();
    let mut wrong = Vec::new();
    for field in 0..4 {
        let mut other = original.clone();
        match field {
            0 => other.port.bus = "other-bus".into(),
            1 => other.port.chain = vec![1, 3],
            2 => other.port.vendor ^= 1,
            _ => other.port.product ^= 1,
        }
        wrong.push(other);
    }
    let state = install(State {
        wrong,
        ..State::default()
    });
    let mut controller = Controller::open().unwrap();
    assert_input(&mut controller);
    assert!(state
        .lock()
        .unwrap()
        .opens
        .iter()
        .all(|(p, _)| p == &original.port));
    assert!(
        state
            .lock()
            .unwrap()
            .inspected
            .iter()
            .all(|port| port == &original.port),
        "reopening must not inspect other devices"
    );
}

#[test]
fn controller_allows_nearly_three_seconds_for_reenumeration() {
    let state = install(State {
        ready_delay: Duration::from_millis(2800),
        ..State::default()
    });
    let mut controller = Controller::open().unwrap();
    assert_input(&mut controller);
    assert_eq!(
        state.lock().unwrap().opens[0].1,
        Duration::from_millis(2800)
    );
}

#[test]
fn explicit_legacy_open_neither_resets_nor_waits_for_announce() {
    let state = install(State {
        stale: false,
        emit_announce: false,
        ..State::default()
    });
    let mut controller = Controller::open_without_reset().unwrap();
    assert_eq!(controller.startup_status(), StartupStatus::Skipped);
    assert!(!controller.announced());
    assert_input(&mut controller);
    let state = state.lock().unwrap();
    assert_eq!(state.resets, 0);
    assert_eq!(state.now, Duration::ZERO);
}

#[test]
fn unsupported_reset_uses_legacy_startup_without_an_announce_wait() {
    let state = install(State {
        unsupported: true,
        stale: false,
        emit_announce: false,
        ..State::default()
    });
    let mut controller = Controller::open().unwrap();
    assert_eq!(controller.startup_status(), StartupStatus::ResetUnsupported);
    assert_input(&mut controller);
    assert_eq!(state.lock().unwrap().now, Duration::ZERO);
    assert_eq!(
        reset_result(nusb::ErrorKind::Unsupported),
        Some(ResetResult::Unsupported)
    );
    for kind in [nusb::ErrorKind::NotFound, nusb::ErrorKind::Disconnected] {
        assert_eq!(reset_result(kind), Some(ResetResult::Reset));
    }
    assert_eq!(reset_result(nusb::ErrorKind::PermissionDenied), None);
}

#[test]
fn announce_timeout_is_observable_without_a_fatal_error() {
    let state = install(State {
        emit_announce: false,
        ..State::default()
    });
    let mut controller = Controller::open().unwrap();
    assert_eq!(controller.startup_status(), StartupStatus::TimedOut);
    assert!(!controller.announced());
    assert_input(&mut controller);
    assert_eq!(state.lock().unwrap().now, Duration::from_millis(1600));
}

fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "fake reader did not make progress"
        );
        std::thread::yield_now();
    }
}

#[test]
fn pad_reader_recovers_slow_reenumeration_without_a_second_reset() {
    let state = install(State {
        ready_delay: Duration::from_secs(10),
        ..State::default()
    });
    let reader = PadReader::spawn();
    wait_until(|| {
        let resets = state.lock().unwrap().resets;
        assert!(
            resets <= 1,
            "slow reopening must not restart the physical pad"
        );
        reader.latest().is_some_and(|p| p.buttons == 16)
    });
    assert_eq!(state.lock().unwrap().resets, 1);
    drop(reader);
    let mut attempts = Attempts::default();
    assert_eq!(
        (0..6)
            .map(|_| attempts.failed().as_secs())
            .collect::<Vec<_>>(),
        [1, 2, 4, 8, 8, 8]
    );
}

#[test]
fn pad_reader_does_not_repeat_reset_after_a_startup_write_failure() {
    let state = install(State {
        fail_writes: 1,
        ..State::default()
    });
    let reader = PadReader::spawn();
    wait_until(|| reader.latest().is_some_and(|p| p.buttons == 16));
    assert_eq!(state.lock().unwrap().resets, 1);
    drop(reader);
}

#[test]
fn pad_reader_resets_again_after_physical_reconnection() {
    let state = install(State::default());
    let reader = PadReader::spawn();
    wait_until(|| reader.latest().is_some_and(|p| p.buttons == 16));
    {
        let mut state = state.lock().unwrap();
        state.candidate.instance = "physical-2".into();
        state.buttons = 32;
        state.input_sent = false;
    }
    wait_until(|| reader.latest().is_some_and(|p| p.buttons == 32));
    assert_eq!(state.lock().unwrap().resets, 2);
    drop(reader);
}

#[test]
fn pad_reader_resets_after_reconnection_even_if_the_os_reuses_its_id() {
    let state = install(State::default());
    let reader = PadReader::spawn();
    wait_until(|| reader.latest().is_some_and(|p| p.buttons == 16));
    {
        let mut state = state.lock().unwrap();
        state.disconnect_once = true;
        state.buttons = 32;
        state.input_sent = false;
    }
    wait_until(|| reader.latest().is_some_and(|p| p.buttons == 32));
    assert_eq!(state.lock().unwrap().resets, 2);
    drop(reader);
}

/// A stale pad that only sends input after a real USB reset. Its first reset attempt is refused
/// (another program still holds it, as kIOReturnExclusiveAccess would be), then the other
/// program exits and the pad is free but still stale.
struct Refusing {
    now: Duration,
    refusals_left: usize,
    resets: usize,
    refused: usize,
    opens: usize,
    instance: String,
    reset_done: bool,
    hello_sent: bool,
    started: bool,
    input_sent: bool,
}

struct RefusingBackend(Arc<Mutex<Refusing>>);
struct RefusingIo(Arc<Mutex<Refusing>>);

impl Clock for RefusingBackend {
    fn now(&self) -> Duration {
        self.0.lock().unwrap().now
    }
}

impl Backend for RefusingBackend {
    fn devices(&mut self, _target: Option<&Port>) -> Result<Vec<Candidate>> {
        let mut c = candidate();
        c.instance = self.0.lock().unwrap().instance.clone();
        Ok(vec![c])
    }
    fn reset(&mut self, _: &Candidate) -> Result<ResetResult> {
        let mut s = self.0.lock().unwrap();
        if s.refusals_left > 0 {
            s.refusals_left -= 1;
            s.refused += 1;
            anyhow::bail!("resetting the controller (held by pid 42, OtherApp)");
        }
        s.resets += 1;
        s.reset_done = true;
        s.hello_sent = false;
        s.instance = format!("after-reset-{}", s.resets);
        Ok(ResetResult::Reset)
    }
    fn open(&mut self, _: &Candidate) -> Result<Gip> {
        self.0.lock().unwrap().opens += 1;
        Ok(Gip {
            io: Box::new(RefusingIo(self.0.clone())),
            ep_in: 0x81,
            ep_out: 1,
            seq: 0,
        })
    }
    fn sleep(&mut self, d: Duration) {
        self.0.lock().unwrap().now += d;
        std::thread::yield_now();
    }
}

impl PacketIo for RefusingIo {
    fn read(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        let mut s = self.0.lock().unwrap();
        if s.reset_done && !s.hello_sent {
            s.hello_sent = true;
            return Ok(Some(hello()));
        }
        if s.reset_done && s.started && !s.input_sent {
            s.input_sent = true;
            let mut report = vec![CMD_INPUT, 0, 1, 14, 16, 0];
            report.extend([0; 12]);
            return Ok(Some(report));
        }
        s.now += timeout;
        drop(s);
        std::thread::yield_now();
        Ok(None)
    }
    fn write(&mut self, packet: &[u8]) -> Result<()> {
        if packet[0] == CMD_POWER {
            self.0.lock().unwrap().started = true;
        }
        Ok(())
    }
}

#[test]
fn pad_reader_retries_a_refused_reset_before_opening_a_stale_pad() {
    let state = Arc::new(Mutex::new(Refusing {
        now: Duration::ZERO,
        refusals_left: 1,
        resets: 0,
        refused: 0,
        opens: 0,
        instance: "physical-1".into(),
        reset_done: false,
        hello_sent: false,
        started: false,
        input_sent: false,
    }));
    TEST_BACKEND.with(|slot| *slot.borrow_mut() = Some(Box::new(RefusingBackend(state.clone()))));
    let reader = PadReader::spawn();
    wait_until(|| state.lock().unwrap().now >= Duration::from_secs(60));
    let buttons = reader.latest().map(|p| p.buttons);
    drop(reader);
    let state = state.lock().unwrap();
    assert_eq!(state.refused, 1);
    assert_eq!(
        state.resets, 1,
        "a refusal must leave the physical reset available"
    );
    assert!(state.opens >= 1);
    assert_eq!(buttons, Some(16), "input requires a performed reset");
}

struct QuietConnection {
    now: Duration,
    discovery_failures: usize,
    hello_sent: bool,
    disconnected_at: Option<Duration>,
}
struct QuietBackend {
    state: Arc<Mutex<QuietConnection>>,
    stop: Arc<AtomicBool>,
}
struct QuietIo(Arc<Mutex<QuietConnection>>);
impl Clock for QuietBackend {
    fn now(&self) -> Duration {
        self.state.lock().unwrap().now
    }
}
impl Backend for QuietBackend {
    fn devices(&mut self, _target: Option<&Port>) -> Result<Vec<Candidate>> {
        let mut state = self.state.lock().unwrap();
        if state.disconnected_at.is_some() {
            self.stop.store(true, Ordering::Relaxed);
            return Ok(Vec::new());
        }
        if state.discovery_failures > 0 {
            state.discovery_failures -= 1;
            anyhow::bail!("temporary discovery failure");
        }
        Ok(vec![candidate()])
    }
    fn reset(&mut self, _: &Candidate) -> Result<ResetResult> {
        Ok(ResetResult::Reset)
    }
    fn open(&mut self, _: &Candidate) -> Result<Gip> {
        Ok(Gip {
            io: Box::new(QuietIo(self.state.clone())),
            ep_in: 0x81,
            ep_out: 1,
            seq: 0,
        })
    }
    fn sleep(&mut self, duration: Duration) {
        self.state.lock().unwrap().now += duration;
    }
}
impl PacketIo for QuietIo {
    fn read(&mut self, _: Duration) -> Result<Option<Vec<u8>>> {
        let mut state = self.0.lock().unwrap();
        if !state.hello_sent {
            state.hello_sent = true;
            return Ok(Some(hello()));
        }
        state.disconnected_at = Some(state.now);
        Err(nusb::transfer::TransferError::Disconnected.into())
    }
    fn write(&mut self, _: &[u8]) -> Result<()> {
        Ok(())
    }
}

#[test]
fn successful_open_resets_backoff_even_without_an_input_packet() {
    let state = Arc::new(Mutex::new(QuietConnection {
        now: Duration::ZERO,
        discovery_failures: 4,
        hello_sent: false,
        disconnected_at: None,
    }));
    let stop = Arc::new(AtomicBool::new(false));
    let mut backend = QuietBackend {
        state: state.clone(),
        stop: stop.clone(),
    };
    read_loop(&mut backend, &Arc::default(), &Arc::default(), &stop, true);
    let state = state.lock().unwrap();
    let disconnected_at = state
        .disconnected_at
        .expect("quiet connection must have opened");
    assert!(
        disconnected_at >= Duration::from_secs(15),
        "setup must reach the backoff cap"
    );
    assert_eq!(state.now - disconnected_at, Duration::from_secs(1));
}

struct WaitingBackend {
    now: Arc<Mutex<Duration>>,
    entered: Option<std::sync::mpsc::Sender<()>>,
    release: std::sync::mpsc::Receiver<()>,
}
impl Clock for WaitingBackend {
    fn now(&self) -> Duration {
        *self.now.lock().unwrap()
    }
}
impl Backend for WaitingBackend {
    fn devices(&mut self, _target: Option<&Port>) -> Result<Vec<Candidate>> {
        anyhow::bail!("no device connected");
    }
    fn reset(&mut self, _: &Candidate) -> Result<ResetResult> {
        unreachable!()
    }
    fn open(&mut self, _: &Candidate) -> Result<Gip> {
        unreachable!()
    }
    fn sleep(&mut self, duration: Duration) {
        let mut now = self.now.lock().unwrap();
        *now += duration;
        if *now >= Duration::from_millis(7100) {
            if let Some(entered) = self.entered.take() {
                drop(now);
                entered.send(()).unwrap();
                self.release.recv().unwrap();
            }
        }
    }
}

#[test]
fn dropping_reader_interrupts_the_eight_second_retry_wait() {
    let now = Arc::new(Mutex::new(Duration::ZERO));
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let backend = WaitingBackend {
        now: now.clone(),
        entered: Some(entered_tx),
        release: release_rx,
    };
    TEST_BACKEND.with(|slot| *slot.borrow_mut() = Some(Box::new(backend)));
    let reader = PadReader::spawn();
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let stop = reader.stop.clone();
    let closing = std::thread::spawn(move || drop(reader));
    wait_until(|| stop.load(Ordering::Relaxed));
    release_tx.send(()).unwrap();
    closing.join().unwrap();
    assert_eq!(*now.lock().unwrap(), Duration::from_millis(7100));
}

#[test]
fn targeted_discovery_filters_before_inspecting_other_devices() {
    let origin = candidate().port;
    let mut other = origin.clone();
    other.chain.push(3);
    let entries = [
        DeviceEntry {
            port: other.clone(),
            listed_gip: true,
            unconfigured: false,
            data: "other pad",
        },
        DeviceEntry {
            port: origin.clone(),
            listed_gip: true,
            unconfigured: false,
            data: "origin pad",
        },
        DeviceEntry {
            port: other,
            listed_gip: false,
            unconfigured: true,
            data: "unrelated vendor device",
        },
    ];
    let mut inspected = Vec::new();
    let result = inspect_candidates(entries, Some(&origin), |name| {
        inspected.push(name);
        anyhow::bail!("{name}: held by pid 42");
    });
    assert_eq!(inspected, ["origin pad"]);
    let error = result.err().expect("origin access error must be preserved");
    assert!(error.to_string().starts_with("origin pad:"));
}

#[test]
fn discovery_preserves_the_first_listed_gip_device_error() {
    let port = candidate().port;
    let entries = [
        DeviceEntry {
            port: port.clone(),
            listed_gip: true,
            unconfigured: false,
            data: "first pad: access denied",
        },
        DeviceEntry {
            port: port.clone(),
            listed_gip: false,
            unconfigured: true,
            data: "unrelated descriptor error",
        },
        DeviceEntry {
            port,
            listed_gip: true,
            unconfigured: false,
            data: "second pad: busy",
        },
    ];
    let result = inspect_candidates(entries, None, |message| anyhow::bail!("{message}"));
    assert_eq!(
        result.err().unwrap().to_string(),
        "first pad: access denied"
    );
}

#[test]
fn reader_without_reset_preserves_legacy_opening_across_replug() {
    let state = install(State {
        stale: false,
        emit_announce: false,
        ..State::default()
    });
    let reader = PadReader::spawn_without_reset();
    wait_until(|| reader.latest().is_some_and(|pad| pad.buttons == 16));
    {
        let mut state = state.lock().unwrap();
        state.candidate.instance = "reconnected".into();
        state.buttons = 32;
        state.input_sent = false;
    }
    wait_until(|| reader.latest().is_some_and(|pad| pad.buttons == 32));
    drop(reader);
    let state = state.lock().unwrap();
    assert_eq!(state.resets, 0);
    assert!(state.opens.len() >= 2);
    assert!(!state.hello_sent);
}

struct QueuedIo {
    packets: std::collections::VecDeque<Vec<u8>>,
    writes: Arc<Mutex<Vec<Vec<u8>>>>,
}
impl PacketIo for QueuedIo {
    fn read(&mut self, _: Duration) -> Result<Option<Vec<u8>>> {
        Ok(self.packets.pop_front())
    }
    fn write(&mut self, packet: &[u8]) -> Result<()> {
        self.writes.lock().unwrap().push(packet.to_vec());
        Ok(())
    }
}

#[test]
fn polling_only_acknowledges_hello_with_a_valid_device_id() {
    install(State::default());
    let mut controller = Controller::open().unwrap();
    let mut valid = hello();
    valid[1] |= crate::OPT_ACK_WANTED;
    let mut invalid_high_six = valid.clone();
    invalid_high_six[4 + 6] = 1;
    let mut invalid_high_seven = valid.clone();
    invalid_high_seven[4 + 7] = 1;
    let writes = Arc::new(Mutex::new(Vec::new()));
    controller.gip.io = Box::new(QueuedIo {
        packets: [invalid_high_six, invalid_high_seven, valid].into(),
        writes: writes.clone(),
    });
    for _ in 0..3 {
        controller.poll(Duration::ZERO).unwrap().unwrap();
    }
    let writes = writes.lock().unwrap();
    assert_eq!(
        writes.len(),
        1,
        "invalid Hello IDs must not receive ACKs during input polling"
    );
    assert_eq!(writes[0][0], crate::CMD_ACK);
}

#[test]
fn a_system_driven_pad_listed_first_does_not_hide_a_blocked_pad() {
    // An official pad macOS drives enumerates first; a third-party GIP pad after it cannot be
    // opened because another program holds it. The caller must get the second pad's error.
    let xbox = Port {
        bus: "1".into(),
        chain: vec![1],
        vendor: 0x045e,
        product: 0x0b12,
    };
    let pdp = Port {
        bus: "1".into(),
        chain: vec![2],
        vendor: 0x0e6f,
        product: 0x0338,
    };
    let entries = [
        DeviceEntry {
            port: xbox,
            listed_gip: true,
            unconfigured: false,
            data: 0,
        },
        DeviceEntry {
            port: pdp,
            listed_gip: true,
            unconfigured: false,
            data: 1,
        },
    ];
    let result = inspect_candidates(entries, None, |which| {
        if which == 0 {
            Err(crate::owner::SystemOwned {
                vendor: 0x045e,
                product: 0x0b12,
                name: Some("Microsoft Controller".into()),
                owner: "XboxUSBDevice".into(),
            }
            .into())
        } else {
            Err(anyhow::anyhow!(
                "opening the controller 0e6f:0338 (held by pid 42, OtherApp)"
            ))
        }
    });
    let error = result.err().unwrap();
    assert!(!error.is::<crate::owner::SystemOwned>());
    assert!(error.to_string().contains("0e6f:0338"));
}
