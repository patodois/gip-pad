# gip-pad

Use wired Xbox One / Series style controllers on macOS when macOS has no driver for them.

Many third-party wired pads "for Xbox" speak GIP (Game Input Protocol, Microsoft's Xbox One controller
protocol) over a vendor-specific USB interface instead of HID. macOS creates no HID device for them,
so games and the Game Controller framework never see them. This repository has two ways around that:

1. **SDL3 with libusb:** SDL3 3.4.12 and later drive GIP pads directly over USB, so any game or app
   built on SDL3 can use the pad. SDL just has to be built with libusb.
2. **`gip-pad`:** a small Rust reader that talks to the pad through libusb. It prints buttons, triggers
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

libusb is built in (the `rusb` crate's `vendored` feature), so nothing else needs installing. If
another program holds the pad, opening it may fail; close that program and try again.

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
