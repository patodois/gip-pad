#!/usr/bin/env python3
"""Exercise startup regressions in a temporary source copy; never access USB hardware.

A mutant counts only if it compiles and a Rust test fails. Originals are
restored in the copy, and the work tree is untouched.
Run: python3 tools/check_startup_mutations.py
"""

import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

SOURCE = Path(__file__).resolve().parents[1]
MUTANTS = [
    ("M1 open uses the 0.2.3 sequence", "src/lib.rs",
     "connection::with_backend(|backend| Self::open_from(backend, true))",
     "connection::with_backend(|backend| Self::open_from(backend, false))"),
    ("M2 reset is omitted", "src/connection.rs",
     "let reset_result = backend.reset(candidate)?;",
     "let reset_result = ResetResult::Reset;"),
    ("M3 descriptor inspection ignores the identity filter", "src/connection.rs",
     "if target.is_some_and(|port| !same_port(port, &entry.port)) {",
     "if false && target.is_some_and(|port| !same_port(port, &entry.port)) {"),
    ("M3b reopen ignores bus", "src/connection.rs", "a.bus == b.bus && ", ""),
    ("M3c reopen ignores port", "src/connection.rs", "a.chain == b.chain && ", ""),
    ("M3d reopen ignores VID", "src/connection.rs", " && a.vendor == b.vendor", ""),
    ("M3e reopen ignores PID", "src/connection.rs", " && a.product == b.product", ""),
    ("M4 reopen window shortened from 3 s to 2 s", "src/connection.rs",
     "let until = backend.now() + Duration::from_secs(3);",
     "let until = backend.now() + Duration::from_secs(2);"),
    ("M5 info omits metadata request", "src/main.rs",
     "gip.send(0x04, OPT_INTERNAL, &[])?;", ""),
    ("M6 reader resets on every retry", "src/connection.rs",
     "let reset = reset_on_connect && attempts.reset_for(&found);", "let reset = true;"),
    ("M7 Hello rejects ACME", "src/startup.rs",
     "h.options & !OPT_ACK_WANTED == OPT_INTERNAL", "h.options == OPT_INTERNAL"),
    ("M8 Hello rejects larger payloads", "src/startup.rs",
     "h.length >= 28", "h.length == 28"),
    ("M9 Hello omits requested ACK", "src/startup.rs",
     "transport.ack(&h, crate::ack_count(h.length)?, 0)?;", ""),
    ("M10 reset Unsupported becomes fatal", "src/connection.rs",
     "nusb::ErrorKind::Unsupported => Some(ResetResult::Unsupported),",
     "nusb::ErrorKind::Unsupported => None,"),
    ("M15 refused reset consumes the connection budget", "src/connection.rs",
     "if reset && !performed {", "if false && reset && !performed {"),
    ("M16 successful opening keeps the previous backoff", "src/connection.rs",
     "attempts.delay = 0;", ""),
    ("M17 retry wait ignores shutdown", "src/connection.rs",
     "while !stop.load(Ordering::Relaxed) && backend.now() < until {",
     "while backend.now() < until {"),
    ("M18 Hello accepts an invalid Device ID", "src/startup.rs",
     ".is_some_and(|body| body.get(6..8) == Some(&[0, 0][..]))",
     ".is_some_and(|_body| true)"),
    ("M19 legacy reader resets the device", "src/lib.rs",
     "Self::spawn_with_reset(false)", "Self::spawn_with_reset(true)"),
    ("M20 reopening omits the descriptor discovery target", "src/connection.rs",
     "match backend.devices(Some(&candidate.port)) {", "match backend.devices(None) {"),
    ("M21 later GIP errors overwrite the first device error", "src/connection.rs",
     "gip_error.get_or_insert(error);", "gip_error = Some(error);"),
    ("M22 input polling acknowledges invalid Hello IDs", "src/lib.rs",
     "&& (h.command != 0x02 || announce)", ""),
    ("M23 metadata acknowledges invalid Hello IDs", "src/main.rs",
     "&& (h.command != 0x02 || h.is_announce(&p))", ""),
    ("M24 a reset refused for exclusive access becomes fatal", "src/connection.rs",
     "_ if crate::owner::is_exclusive_access(kind, os_error) => Some(ResetResult::Refused),", ""),
    ("M25 a refused reset waits for Hello instead of opening as 0.2.3 did", "src/connection.rs",
     "ResetResult::Refused => Some(StartupStatus::ResetRefused),",
     "ResetResult::Refused => None,"),
    ("M26 a Hello during polling does not restart the pad", "src/lib.rs",
     "startup::send_start(&mut self.gip, self.vendor)\n                .context(", "Ok::<(), anyhow::Error>(())\n                .context("),
    ("M27 a Hello during polling keeps the last state", "src/lib.rs",
     "self.pad = Pad::default();\n", ""),
    ("M28 send accepts a length byte with bit 7 set", "src/lib.rs",
     ".filter(|&n| n <= 0x7F && 4 + usize::from(n) <= max)",
     ".filter(|&n| 4 + usize::from(n) <= max)"),
    ("M29 send ignores the packet size", "src/lib.rs",
     ".filter(|&n| n <= 0x7F && 4 + usize::from(n) <= max)", ".filter(|&n| n <= 0x7F)"),
    ("M30 acknowledgement counts are truncated", "src/lib.rs",
     "u16::try_from(n).with_context(", "Ok::<u16, anyhow::Error>(n as u16).with_context("),
    ("M31 Guide is read from a fragment", "src/lib.rs",
     "CMD_GUIDE if h.chunk.is_none() => {", "CMD_GUIDE => {"),
]


FOCUSED_TESTS = {
    "M15": "pad_reader_keeps_the_reset_available_after_a_refusal",
    "M16": "successful_open_resets_backoff_even_without_an_input_packet",
    "M17": "dropping_reader_interrupts_the_eight_second_retry_wait",
    "M22": "polling_only_acknowledges_hello_with_a_valid_device_id",
    "M23": "metadata_does_not_acknowledge_hello_with_an_invalid_device_id",
}


def run(copy, target, *args):
    env = dict(os.environ, CARGO_TARGET_DIR=str(target))
    return subprocess.run(["cargo", "test", "--offline", "-q", *args], cwd=copy,
                          env=env, capture_output=True, text=True, timeout=60)


def main():
    workspace = Path(tempfile.mkdtemp(prefix="gip-pad-mutations-"))
    copy = workspace / "source"
    target = workspace / "target"
    (copy / "src").mkdir(parents=True)
    print(f"working directory: {workspace}", flush=True)
    for name in ["Cargo.toml", "Cargo.lock", "README.md", "LICENSE-MIT", "LICENSE-APACHE"]:
        shutil.copy2(SOURCE / name, copy / name)
    for source in (SOURCE / "src").glob("*.rs"):
        shutil.copy2(source, copy / "src" / source.name)
    baseline = run(copy, target)
    if baseline.returncode:
        print("BASELINE FAILED\n" + baseline.stdout + baseline.stderr, flush=True)
        return 1
    print("baseline: passed", flush=True)
    failed = 0
    for label, relative, before, after in MUTANTS:
        path = copy / relative
        original = path.read_text()
        if original.count(before) != 1:
            print(f"{label}: INVALID (anchor count {original.count(before)})", flush=True)
            failed += 1
            continue
        try:
            path.write_text(original.replace(before, after))
            compilation = run(copy, target, "--no-run")
            if compilation.returncode:
                print(f"{label}: INVALID (did not compile)\n{compilation.stderr}", flush=True)
                failed += 1
                continue
            test = FOCUSED_TESTS.get(label.split()[0])
            result = run(copy, target, *([test] if test else []))
            failures = [line for line in result.stdout.splitlines() if "FAILED" in line and "::" in line]
            if result.returncode and failures:
                print(f"{label}: killed; {failures[0].strip()}", flush=True)
            elif not result.returncode:
                print(f"{label}: SURVIVED", flush=True)
                failed += 1
            else:
                print(f"{label}: INVALID (exit {result.returncode} without a test assertion)\n{result.stderr[-3000:]}", flush=True)
                failed += 1
        finally:
            path.write_text(original)
    restored = run(copy, target)
    if restored.returncode:
        print("RESTORED BASELINE FAILED", flush=True)
        failed += 1
    else:
        print("restored baseline: passed", flush=True)
    return bool(failed)


if __name__ == "__main__":
    sys.exit(main())
