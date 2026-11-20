# AGENTS.md: gip-pad

User-space reader for wired GIP (Xbox One / Series protocol) pads over USB on macOS: a Rust library
(`gip_pad`) and a command-line tool (`gip-pad`). There is no C ABI and no C# binding.

## Toolchain and checks

- Stable Rust, edition 2021, Cargo. Python 3.9 or later, standard library only, for the tools.
  Nothing needs the network once the dependencies are in the Cargo cache (add `--offline`).
- Before every commit, run what CI runs and keep it green:
  - `cargo build --release --locked` and `cargo test --locked`. Every test is hardware-free: a fake
    USB backend and a fake clock.
  - `cargo clippy --all-targets --locked -- -D warnings` and `cargo fmt --check`.
  - `python3 tools/check_startup_mutations.py`: every mutant must be killed. When you add a rule to
    startup, reset, reconnection or packet handling, add a mutant for it (an exact text that occurs
    once, and its replacement), and a focused test in `FOCUSED_TESTS` if a full run is slow.
  - `python3 tools/check_public_tree.py --range BASE..HEAD`: no game files, decompiled or foreign
    code, binaries or secrets.
- CI is `.github/workflows/ci.yml`: build and test on macOS and Ubuntu; clippy, rustfmt and the
  mutation check on macOS; the public-tree gate on the pushed commits.

## Code map

- `src/lib.rs`: constants, `Header` and varint parsing, `Pad` decoding, `Gip` packet I/O (`send`
  refuses payloads that do not fit one packet), discovery (`find_gip`, `find_gip_any`, `first_pad`),
  `Controller` (open, `poll`, restart on a mid-session Hello) and `PadReader`.
- `src/connection.rs`: the USB boundary (`Backend`, `PacketIo`, `Clock`), reset, reopen and announce
  wait (a reset refused for exclusive access opens without reset), the reader loop and its
  one-reset-per-connection budget. Its tests are in `src/connection_tests.rs`.
- `src/startup.rs`: `StartupStatus`, the `Transport` trait, Hello validation, the announce wait and
  the START, LED and `0x06` sequence. Nothing is logged here.
- `src/owner.rs`: the `UsbExclusiveOwner` registry read (the only `unsafe`) and the error hints.
- `src/main.rs`: the command line (`parse_args`; normal mode, `--raw`, `--info`, `--seconds N`,
  `--no-reset`, `--help`) and the `--info` metadata flow.
- `tools/`: `sdl_padcheck.c` (SDL check program), `check_startup_mutations.py` (mutation check),
  `check_public_tree.py` (public-tree gate, shared with other repositories; configured by
  `.public-tree.cfg`).
- `docs/measurements.md`: protocol values the code uses that MS-GIPUSB does not state, and how to
  measure each on a pad.

## Rules

- A protocol fact comes only from Microsoft's MS-GIPUSB specification (cite the section and table in
  the code) or from a measurement on a pad the owner has, recorded in `docs/measurements.md`. Never
  open, quote or reconstruct the source of any other GIP or Xbox controller driver or library, not
  even from memory. A value with neither source gets an entry marked "to measure".
- Treat every byte from the device as untrusted: parse it with `Header::parse` and read the payload
  only through `h.body(p)`. Never index the raw packet.
- All USB access goes through `connection::Backend` and `PacketIo`, and all time through `Clock`.
  Every new behaviour gets a fake-backend test that fails without the change.
- The library never writes to stdout or stderr. Diagnostics belong in the command line.
- A change to startup, reset, reconnection or the command line is visible to users: update the
  "Changes" section of `README.md` and the version in `Cargo.toml` together.
- The public API exposes `nusb` types, and `StartupStatus` is matched by users: changing either is a
  semver change.
- No local user path, person's name, serial number or device ID from real hardware in tracked files.

## Hardware: never

- Never run `gip-pad` in any mode (`cargo run` included), `tools/sdl_padcheck` or any code that
  lists, opens, resets or claims a USB device, unless the owner asks in the conversation. Every mode
  other than `--help` opens the first GIP pad it finds and sends it packets, and resets it unless
  `--no-reset` is given. The pads attached to the machine belong to the owner.
- Measurements in `docs/measurements.md` are run by the owner. Write the procedure; do not run it.
- Never write a test that reaches `nusb` (`find_gip`, `find_gip_any`, `Controller::open` or
  `PadReader::spawn` without a fake installed, `nusb::list_devices`). Under `cargo test`,
  `connection::new_backend` panics when no fake backend is installed; keep it that way.
- Never kill or signal a process that holds the pad. Report it instead.
- Never commit captures with serial numbers or device IDs from the owner's hardware unmasked.
