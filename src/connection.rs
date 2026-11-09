//! USB boundary and connection lifecycle. Tests replace only OS I/O and time.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nusb::transfer::{Buffer, In, Interrupt, Out};
use nusb::{Endpoint, Interface, MaybeFuture};

use crate::{Controller, Found, Gip, Pad, StartupStatus};

pub(crate) trait PacketIo: Send {
    fn read(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>>;
    fn write(&mut self, packet: &[u8]) -> Result<()>;
}

pub(crate) struct UsbIo {
    pub _interface: Interface,
    pub reader: Endpoint<Interrupt, In>,
    pub writer: Endpoint<Interrupt, Out>,
}

impl PacketIo for UsbIo {
    fn read(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        if self.reader.pending() == 0 {
            self.reader
                .submit(Buffer::new(self.reader.max_packet_size().max(1)));
        }
        let Some(done) = self.reader.wait_next_complete(timeout) else {
            return Ok(None);
        };
        self.reader
            .submit(Buffer::new(self.reader.max_packet_size().max(1)));
        done.status?;
        Ok(Some(done.buffer[..done.actual_len].to_vec()))
    }

    fn write(&mut self, packet: &[u8]) -> Result<()> {
        let mut buf = Buffer::new(packet.len());
        buf.extend_from_slice(packet);
        self.writer
            .transfer_blocking(buf, Duration::from_millis(500))
            .status?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Port {
    pub bus: String,
    pub chain: Vec<u8>,
    pub vendor: u16,
    pub product: u16,
}

impl Port {
    fn from_info(info: &nusb::DeviceInfo) -> Self {
        Self {
            bus: info.bus_id().into(),
            chain: info.port_chain().into(),
            vendor: info.vendor_id(),
            product: info.product_id(),
        }
    }
}

struct DeviceEntry<T> {
    port: Port,
    listed_gip: bool,
    unconfigured: bool,
    data: T,
}

fn inspect_candidates<T>(
    entries: impl IntoIterator<Item = DeviceEntry<T>>,
    target: Option<&Port>,
    mut inspect: impl FnMut(T) -> Result<Candidate>,
) -> Result<Vec<Candidate>> {
    let mut candidates = Vec::new();
    let mut gip_error = None;
    let mut system_error = None;
    let mut other_error = None;
    for entry in entries {
        // Match USB enumeration metadata before opening any device for descriptors.
        if target.is_some_and(|port| !same_port(port, &entry.port)) {
            continue;
        }
        if !entry.listed_gip && !entry.unconfigured {
            continue;
        }
        match inspect(entry.data) {
            Ok(candidate) => candidates.push(candidate),
            // A pad macOS drives is reported only when no other pad explains the failure.
            Err(error) if error.is::<crate::owner::SystemOwned>() => {
                system_error.get_or_insert(error);
            }
            Err(error) if entry.listed_gip => {
                gip_error.get_or_insert(error);
            }
            Err(error) => {
                other_error.get_or_insert(error);
            }
        }
    }
    if candidates.is_empty() {
        if let Some(error) = gip_error.or(system_error).or(other_error) {
            return Err(error);
        }
    }
    Ok(candidates)
}

#[derive(Clone)]
pub(crate) struct Candidate {
    pub port: Port,
    pub instance: String,
    pub name: String,
    found: Option<Found>,
}

impl Candidate {
    pub fn from_found(found: Found) -> Self {
        Self {
            port: Port::from_info(&found.info),
            instance: format!("{:?}", found.info.id()),
            name: found.info.product_string().unwrap_or("?").into(),
            found: Some(found),
        }
    }
}

pub(crate) trait Clock {
    fn now(&self) -> Duration;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResetResult {
    Reset,
    Unsupported,
}

pub(crate) trait Backend: Clock + Send {
    fn devices(&mut self, target: Option<&Port>) -> Result<Vec<Candidate>>;
    fn reset(&mut self, candidate: &Candidate) -> Result<ResetResult>;
    fn open(&mut self, candidate: &Candidate) -> Result<Gip>;
    fn sleep(&mut self, duration: Duration);
}

pub(crate) struct UsbBackend {
    origin: Instant,
    handle: Option<nusb::Device>,
}

impl Default for UsbBackend {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
            handle: None,
        }
    }
}

impl UsbBackend {
    pub fn with_handle(handle: nusb::Device) -> Self {
        Self {
            handle: Some(handle),
            ..Self::default()
        }
    }
}

impl Clock for UsbBackend {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// A reset can invalidate its own handle; Unsupported preserves the old opening path.
fn reset_result(kind: nusb::ErrorKind) -> Option<ResetResult> {
    match kind {
        nusb::ErrorKind::NotFound | nusb::ErrorKind::Disconnected => Some(ResetResult::Reset),
        nusb::ErrorKind::Unsupported => Some(ResetResult::Unsupported),
        _ => None,
    }
}

impl Backend for UsbBackend {
    fn devices(&mut self, target: Option<&Port>) -> Result<Vec<Candidate>> {
        let devices = nusb::list_devices().wait().context("listing USB devices")?;
        let entries = devices.map(|info| {
            let listed: Vec<_> = info
                .interfaces()
                .map(|i| (i.class(), i.subclass(), i.protocol()))
                .collect();
            DeviceEntry {
                port: Port::from_info(&info),
                listed_gip: listed.contains(&crate::GIP_CLASS),
                unconfigured: crate::may_be_unconfigured_pad(&listed, info.class()),
                data: info,
            }
        });
        inspect_candidates(entries, target, |info| {
            crate::inspect_devices(false, [info]).map(|d| Candidate::from_found(d.found))
        })
    }

    fn reset(&mut self, candidate: &Candidate) -> Result<ResetResult> {
        let found = candidate.found.as_ref().context("missing USB device")?;
        if let Some(crate::owner::Owner::Driver(name)) = crate::owner::exclusive_owner(&found.info)
        {
            return Err(crate::system_owned(&found.info, name).into());
        }
        let device = match self.handle.take() {
            Some(device) => device,
            None => found
                .info
                .open()
                .wait()
                .map_err(|e| found.explain_error(e, "opening the controller".into()))?,
        };
        match device.reset().wait() {
            Ok(()) => Ok(ResetResult::Reset),
            Err(e) => match reset_result(e.kind()) {
                Some(result) => Ok(result),
                None => Err(found.explain_error(e, "resetting the controller".into())),
            },
        }
    }

    fn open(&mut self, candidate: &Candidate) -> Result<Gip> {
        candidate
            .found
            .as_ref()
            .context("missing USB device")?
            .open()
            .map(|(_, gip)| gip)
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

pub(crate) fn first(backend: &mut dyn Backend) -> Result<Candidate> {
    backend
        .devices(None)?
        .into_iter()
        .next()
        .context("no Xbox One / Series controller (GIP over USB) found; is it plugged in by cable?")
}

/// Physical topology and USB IDs survive USB reset; the OS instance ID does not.
fn same_port(a: &Port, b: &Port) -> bool {
    a.bus == b.bus && a.chain == b.chain && a.vendor == b.vendor && a.product == b.product
}

pub(crate) fn reset_and_wait(
    backend: &mut dyn Backend,
    candidate: &Candidate,
    on_packet: impl FnMut(&[u8]),
    performed: &mut bool,
) -> Result<(Gip, StartupStatus, String)> {
    // MS-GIPUSB 3.1.1: USB Reset is handled from all GIP states.
    let reset_result = backend.reset(candidate)?;
    if reset_result == ResetResult::Unsupported {
        return Ok((
            backend.open(candidate)?,
            StartupStatus::ResetUnsupported,
            candidate.instance.clone(),
        ));
    }
    *performed = true;
    let until = backend.now() + Duration::from_secs(3);
    let mut last_error = None;
    let (mut gip, instance) = 'opened: loop {
        if backend.now() >= until {
            return Err(last_error.unwrap_or_else(|| {
                anyhow::anyhow!("the controller did not come back after the reset")
            }))
            .context("reopening the controller after USB reset");
        }
        backend.sleep(
            until
                .saturating_sub(backend.now())
                .min(Duration::from_millis(100)),
        );
        match backend.devices(Some(&candidate.port)) {
            Ok(devices) => {
                for found in devices {
                    if !same_port(&candidate.port, &found.port) {
                        continue;
                    }
                    match backend.open(&found) {
                        Ok(gip) => break 'opened (gip, found.instance),
                        Err(e) => last_error = Some(e),
                    }
                }
            }
            Err(e) => last_error = Some(e),
        }
    };
    let status = crate::startup::wait_for_announce(
        &mut gip,
        backend,
        crate::startup::ANNOUNCE_TIMEOUT,
        on_packet,
    )?;
    Ok((gip, status, instance))
}

#[derive(Default)]
struct Attempts {
    connections: Vec<(Port, String, bool)>,
    delay: u64,
}

impl Attempts {
    fn reset_for(&mut self, candidate: &Candidate) -> bool {
        if let Some((_, id, pending)) = self
            .connections
            .iter_mut()
            .find(|(p, _, _)| same_port(p, &candidate.port))
        {
            if *pending || *id == candidate.instance {
                if *id != candidate.instance {
                    *id = candidate.instance.clone();
                    *pending = false;
                }
                return false;
            }
            *id = candidate.instance.clone();
            *pending = true;
            return true;
        }
        self.connections
            .push((candidate.port.clone(), candidate.instance.clone(), true));
        true
    }

    fn failed(&mut self) -> Duration {
        self.delay = if self.delay == 0 {
            1
        } else {
            (self.delay * 2).min(8)
        };
        Duration::from_secs(self.delay)
    }
}

pub(crate) fn read_loop(
    backend: &mut dyn Backend,
    latest: &Arc<Mutex<Option<Pad>>>,
    error: &Arc<Mutex<Option<String>>>,
    stop: &AtomicBool,
    reset_on_connect: bool,
) {
    let mut attempts = Attempts::default();
    while !stop.load(Ordering::Relaxed) {
        let result = first(backend).and_then(|found| {
            let reset = reset_on_connect && attempts.reset_for(&found);
            let mut performed = false;
            let result = Controller::open_candidate(backend, &found, reset, &mut performed);
            if reset && !performed {
                // A refused or unsupported reset did not change this physical connection.
                attempts
                    .connections
                    .retain(|(port, _, _)| !same_port(port, &found.port));
            }
            // Absence during our own reset is expected. Adopt the new instance once visible,
            // even on a failed claim, so retries cannot reset it again.
            if performed {
                let replacement = match &result {
                    Ok(controller) => Some(controller.instance.clone()),
                    Err(_) => backend.devices(Some(&found.port)).ok().and_then(|devices| {
                        devices
                            .into_iter()
                            .find(|d| same_port(&found.port, &d.port))
                            .map(|d| d.instance)
                    }),
                };
                if let Some(new) = replacement {
                    if let Some((_, id, pending)) = attempts
                        .connections
                        .iter_mut()
                        .find(|(p, _, _)| same_port(p, &found.port))
                    {
                        if *id != new || result.is_ok() {
                            *id = new;
                            *pending = false;
                        }
                    }
                }
            }
            result
        });
        let mut controller = match result {
            Ok(controller) => {
                attempts.delay = 0;
                controller
            }
            Err(e) => {
                *error.lock().unwrap() = Some(format!("{e:#}"));
                interruptible_sleep(backend, attempts.failed(), stop);
                continue;
            }
        };
        *error.lock().unwrap() = None;
        *latest.lock().unwrap() = Some(controller.state());
        while !stop.load(Ordering::Relaxed) {
            match controller.poll(Duration::from_millis(100)) {
                Ok(Some(_)) => {
                    *latest.lock().unwrap() = Some(controller.state());
                }
                Ok(None) => {}
                Err(e) => {
                    // Some platforms reuse an OS instance ID after unplug/replug. A confirmed
                    // disconnect ends the reset budget even when that identifier is reused.
                    if e.downcast_ref::<nusb::transfer::TransferError>()
                        == Some(&nusb::transfer::TransferError::Disconnected)
                    {
                        attempts
                            .connections
                            .retain(|(_, id, _)| id != &controller.instance);
                    }
                    *error.lock().unwrap() = Some(format!("{e:#}"));
                    break;
                }
            }
        }
        *latest.lock().unwrap() = None;
        drop(controller);
        interruptible_sleep(backend, attempts.failed(), stop);
    }
}

fn interruptible_sleep(backend: &mut dyn Backend, duration: Duration, stop: &AtomicBool) {
    let until = backend.now() + duration;
    while !stop.load(Ordering::Relaxed) && backend.now() < until {
        backend.sleep(
            until
                .saturating_sub(backend.now())
                .min(Duration::from_millis(100)),
        );
    }
}

#[cfg(test)]
thread_local! {
    static TEST_BACKEND: std::cell::RefCell<Option<Box<dyn Backend>>> = const { std::cell::RefCell::new(None) };
}

pub(crate) fn new_backend() -> Box<dyn Backend> {
    #[cfg(test)]
    return TEST_BACKEND.with(|slot| {
        slot.borrow_mut()
            .take()
            .expect("unit tests must install a fake USB backend")
    });
    #[cfg(not(test))]
    Box::<UsbBackend>::default()
}

pub(crate) fn with_backend<T>(f: impl FnOnce(&mut dyn Backend) -> T) -> T {
    f(&mut *new_backend())
}

#[cfg(test)]
#[path = "connection_tests.rs"]
mod tests;
