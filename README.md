# gip-pad

Use wired Xbox One / Series style controllers on macOS when macOS has no driver for them.

Many third-party wired pads "for Xbox" speak GIP (Game Input Protocol, Microsoft's Xbox One controller
protocol) over a vendor-specific USB interface instead of HID. macOS creates no HID device for them,
so games and the Game Controller framework never see them. This repository has two ways around that:

1. **SDL3 with libusb:** SDL3 3.4.12 and later drive GIP pads directly over USB, so any game or app
   built on SDL3 can use the pad. SDL just has to be built with libusb.
2. **`gip-pad`:** a small Rust reader that talks to the pad directly over USB (pure Rust, through the `nusb` crate). It prints buttons, triggers
   and sticks, and `--info` shows what the pad reports about itself.

Neither makes the pad visible to every app on the Mac. That needs a DriverKit driver, which Apple
only lets you build and sign with a paid developer account. See
[leetrout/macgip](https://github.com/leetrout/macgip) for that route.

## Is my controller one of these?

```sh
system_profiler SPUSBHostDataType
```

Look for your pad, then build and run `gip-pad --info` (below). It applies if the pad has an
interface with class `ff`, subclass `47`, protocol `d0`, which is GIP. Product names are not a
reliable guide: one name can cover several USB IDs and firmware versions.

## Example: PDP Afterglow Wave Wired Controller for Xbox

This is the pad the code was written and tested with:

| | |
|---|---|
| USB ID | `0e6f:0338` (vendor 0x0e6f is PDP) |
| USB | 2.0, device class `ff`, device version 3.00 |
| Interface 0 | GIP (`ff/47/d0`), interrupt IN `0x81`, OUT `0x01`, 64 bytes, 4 ms |
| Interface 1 | headset audio (isochronous), not used here |
| Versions (from the GIP announce packet) | firmware 2.0.3.0; hardware 2.0; RF, security and GIP protocol 1.0 each |
| Preferred types (from the GIP metadata) | `Windows.Xbox.Input.Gamepad`, `Victrix.Xbox.Gamepad.Gambit` |
| Input report | `0x20`, 50 bytes (payload `0x2e`); Share is bit 0 of byte 32 |
| Start-up | power on, then the LED-on and `0x06` security packets this pad waits for |
| Rear buttons | mirrored by the pad itself to D-pad Up and B; they send no bits of their own |

The metadata suggests the Afterglow Wave runs the same firmware platform as PDP's Victrix Gambit, so
other PDP pads on that platform will likely behave the same. Another product sold as "Afterglow Wave"
uses USB ID `0e6f:0316` and has not been tested.

## Option 1: SDL3 with libusb

```sh
brew install libusb cmake ninja
git clone --depth 1 --branch release-3.4.18 https://github.com/libsdl-org/SDL.git
cmake -S SDL -B SDL/build -G Ninja -DCMAKE_BUILD_TYPE=Release \
  -DSDL_HIDAPI_LIBUSB=ON -DSDL_HIDAPI_LIBUSB_SHARED=OFF
ninja -C SDL/build SDL3-shared
```

`SDL_HIDAPI_LIBUSB_SHARED=OFF` links libusb directly. SDL otherwise loads it at run time and does not
find Homebrew's copy unless `DYLD_FALLBACK_LIBRARY_PATH=/opt/homebrew/lib` is set.

Check that SDL sees the pad with `tools/sdl_padcheck.c`:

```sh
cc -O2 tools/sdl_padcheck.c -I SDL/include -L SDL/build -lSDL3 -Wl,-rpath,"$PWD/SDL/build" -o sdl_padcheck
./sdl_padcheck 30
```

It prints the pad's name and type (`xboxone`), tries a short rumble, then prints buttons and
near-full axis movements for 30 seconds. With the Afterglow Wave every button works, including
Share (`misc1`) and Guide.

## Option 2: the `gip-pad` reader

```sh
cargo build --release
./target/release/gip-pad            # live line: sticks, triggers, pressed buttons
./target/release/gip-pad --raw      # every packet in hex
./target/release/gip-pad --info     # USB descriptors, firmware, hardware and protocol versions, metadata
```

`Controller::open()` and `--info` reset USB and reopen the same bus/port/VID/PID, allowing
up to three seconds for re-enumeration, then up to 1500 ms for announce. If reset is unsupported,
opening continues without reset. If announce times out, the existing packets are still sent.
`--info` responds with Metadata Request; `Controller::open()` responds with START and, for PDP
pads (vendor 0x0e6f), the existing LED and security packets.

The library writes no startup diagnostics to stderr. Call `Controller::announced()` or
`Controller::startup_status()` to inspect the result; the CLI prints its own warnings.
Use `Controller::open_without_reset()` or `PadReader::spawn_without_reset()` for the 0.2.3
opening behavior, without reset or an announce wait, including after replug in the reader.
This behavior change is released as **0.3.0**.

`PadReader::spawn()` performs at most one reset per physical connection. A reset refused before
it takes place leaves that reset available for the next attempt. After a performed reset, failed
reopening retries without another reset. Retry delays are 1, 2, 4 and then at most 8 seconds;
a successful opening clears the backoff even when no input report has arrived. USB re-enumeration
caused by its own reset is kept in the same connection; a later physical reconnect allows a
new reset. Silence alone does not cause a reset, because unchanged input need not be sent.
If the device has not reappeared after reset, the reader conservatively treats its next
appearance as that pending reset until it can identify the replacement USB instance.
Reset reopening filters bus, port, VID and PID before descriptor inspection, so it only opens
the original device and retains that device's access error.

Closing an app while opening the pad, including dropping `PadReader`, can take several seconds:
USB reset/reopening, the announce wait and pending USB writes must finish before its thread
joins. Retry backoff sleeps check for shutdown every 100 ms.

The USB layer is the pure-Rust `nusb` crate (IOKit underneath, no C library), so nothing else needs installing. If
another program holds the pad, opening it may fail; close that program and try again.

## Controllers macOS already supports

`gip-pad` is for wired pads macOS has no driver for. Official Xbox controllers (an Xbox Series pad,
USB `045e:0b12`, for example) are different: macOS binds its own driver to them and games see them
through Apple's Game Controller framework, so they need no help from this tool. Such a driver holds
the pad open exclusively, so `gip-pad` could not claim it anyway.

`gip-pad` detects this by reading the `UsbExclusiveOwner` property that macOS puts on the pad's
IORegistry entry (it names the driver, such as `XboxUSBDevice`; a user-space program that holds a
device shows up there as `pid <n>, <name>` instead). A driver named there is a pad macOS drives. What
happens:

- **Normal mode and `--raw`** skip such pads and keep looking, so a third-party pad plugged in beside an
  official one is still found. If the only GIP pads present are driven by macOS, `gip-pad` exits with
  status 1 and says so, for example:

  ```
  Error: found Microsoft Controller (045e:0b12), but macOS already drives it with its own Xbox controller driver (XboxUSBDevice): games see it through Game Controller, so gip-pad is not needed for it. gip-pad is for wired pads macOS has no driver for.
  ```
- **`--info`** still prints the USB descriptors, which macOS lets anyone read, then prints the same
  explanation instead of resetting the pad and reading its GIP announce and metadata, and exits with
  status 0. That choice is deliberate: `--info` was asked to describe the pad and did, and nothing is wrong
  with a pad macOS drives. Normal mode exits with 1 because it could not do what it was asked, to read
  a pad.
- **Other failures to open or claim a pad** keep the hint that another program (Chrome, say) may be
  holding it, but only when the pad has no system driver as its owner. If the owner is a user-space
  process, its `pid` and name are shown instead. Where macOS reports an exclusive-access error
  (`0xe00002c5`) and names no owner, the hint stays general because it cannot tell whether a driver or a
  program holds the pad.

The check only reads the registry. Beyond the descriptor reads of `--info`, no reset, claim or GIP packet is sent to a pad that macOS drives. For library users,
`find_gip()` skips these pads and returns a `gip_pad::owner::SystemOwned` error (downcast it from the
`anyhow::Error`) when only such pads are present; `find_gip_any()` also returns them, marked, for
inspection. On other platforms no owner is ever reported.

## Changes

### 0.3.0

- Add `Controller::open_without_reset()`, `PadReader::spawn_without_reset()`, `announced()` and
  `startup_status()`. Keep startup
  diagnostics in the CLI, and continue with the legacy sequence when USB reset is unsupported.
- Count only performed resets against the connection budget, including failed or slow reopening.
  Retry refused resets with capped backoff; clear the backoff on successful opening and allow a
  new reset after physical reconnect. Check shutdown during retry waits.
- Filter the original bus/port/VID/PID before opening descriptors and preserve the first GIP error.
- Validate Hello's nonzero sequence and Device ID high bytes (MS-GIPUSB section 3.1.5.5.1,
  Table 27). That table specifies flags 0x20 and 28 payload bytes. Accepting ACME and extra
  trailing Hello bytes is a local compatibility tolerance; requested ACK uses section 3.1.5.1.
  Larger fragmented messages are described in section 3.1.5.2 (MTU sizes in 2.2.10.4);
  the Hello parser rejects fragments. The CLI prints each accepted announce once.
- Exercise controller opening, the reader thread and diagnostics through fake USB/time boundaries;
  add reproducible mutation checks for reset, port/VID/PID filtering, the reopen window and metadata.
- Package only crate sources, README, manifests and licenses.

### 0.2.4

- Reset and wait for announce on every controller open, recovering pads left in an old state after
  sleep or an earlier program. The library and `--info` share the reset/reopen/announce path.
- Keep startup working when announce times out; log the reason and send the existing start packets.
- Do not reset an already opened pad merely because input is quiet: MS-GIPUSB section 3.1.5.6.1.1
  recommends sending input only when a field changes. `PadReader` uses the reset on each open.
- Add hardware-free transport tests for stale pads, announce timeout, malformed packets and errors.

## How it works

`src/main.rs` documents what is sent and received.

### Provenance

The protocol code was written by an AI (Claude) from its general knowledge of GIP and of the Linux
[`xpad`](https://github.com/torvalds/linux/blob/master/drivers/input/joystick/xpad.c) and
[`xone`](https://github.com/dlundqvist/xone) drivers, without reading their source at the time. It was
then checked against a real PDP pad and afterwards against Microsoft's published
[MS-GIPUSB](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-gipusb/e7c90904-5e21-426e-b9ad-d82adeee0dbc)
specification. No source text from xpad, xone or the specification was copied.

What MS-GIPUSB states, and the code matches:

- Start: Set Device State, command `0x05`, flags `0x20`, state `0x00` (Tables 39 and 40, section 3.1.5.5.5).
- The LED command `0x0A` layout: command, pattern, intensity (Tables 41 and 42, section 3.1.5.5.7).
- Header: 4 bytes, with bit 7 of the length extending it into a varint; the flag bits for fragment,
  first fragment, system message and acknowledge request (Tables 12 and 14, section 2.2.10).
- Input report `0x20`: buttons in bytes 4 and 5, triggers 6 to 9 (0 to 1023), sticks 10 to 17
  (Table 57, section 3.1.5.6.1.1).
- Rumble: the Direct Motor command `0x09` (Table 56, section 3.1.5.6.1).
- Announce `0x02` ("Hello Device"): device ID, vendor, product, firmware major, minor, build and
  revision as 16-bit fields, then hardware, RF, security and GIP versions as one-byte major.minor pairs
  (Table 27, section 3.1.5.5.1).
- A packet with the acknowledge flag must be acknowledged with a type `0x01` packet of length 9
  (Table 26, section 3.1.5.1).

What the specification text does not state, and was confirmed only on the PDP pad:

- The payload layout of the acknowledgement.
- The `01 00` payload of the `0x06` security packet, and that this pad sends no input until it gets it
  and the LED packet.
- The Guide button byte: the `0x07` packet is listed with a 2-byte payload, and section 3.1.5.5.6
  describes only behaviour. Bit 0 of the first payload byte is what the pad sends.
- The position of the Share button. Table 62 puts the Console Function Map at byte 18 of a 32-byte
  payload; this pad sends a 46-byte payload (`0x2e`) with Share at bit 0 of byte 32.

## Disclaimer

Not affiliated with or endorsed by Microsoft, PDP or Victrix. Product names are used only to say
which hardware this was tested with.

## License

Licensed under either of MIT or Apache-2.0, at your option.
