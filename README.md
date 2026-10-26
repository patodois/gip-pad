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
| Firmware / hardware (from the GIP announce packet) | 2.0.3.0 / 2.1.1.1 |
| Preferred types (from the GIP metadata) | `Windows.Xbox.Input.Gamepad`, `Victrix.Xbox.Gamepad.Gambit` |
| Input report | `0x20`, 50 bytes (payload `0x2e`); Share is bit 0 of byte 32 |
| Start-up | power on, then PDP's LED-on and authentication-done packets |
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
./target/release/gip-pad --info     # USB descriptors, firmware/hardware versions, metadata
```

`--info` resets the pad so it announces itself again, then reads its metadata. The reader finds
the first interface with the GIP class and its interrupt endpoints, so other GIP pads should work
too. Only PDP pads (vendor 0x0e6f) get the PDP start-up packets.

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

## How it works

The packet layouts follow the Linux [`xpad`](https://github.com/torvalds/linux/blob/master/drivers/input/joystick/xpad.c)
and [`xone`](https://github.com/dlundqvist/xone) drivers and Microsoft's published
[MS-GIPUSB](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-gipusb/e7c90904-5e21-426e-b9ad-d82adeee0dbc)
specification. No code was copied. `src/main.rs` documents what is sent and received.

## Disclaimer

Not affiliated with or endorsed by Microsoft, PDP or Victrix. Product names are used only to say
which hardware this was tested with.

## License

Licensed under either of MIT or Apache-2.0, at your option.
