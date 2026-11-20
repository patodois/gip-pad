# Measurements

Every protocol fact the code uses should come from one of two places:

- Microsoft's published MS-GIPUSB specification, cited by section and table in the code and in the README.
- A measurement on a pad, recorded in this file.

The entries below are values the code uses today that are in neither place yet. Each one says what
the code does, what has to be measured to support it, and how. Each is marked **to measure** until a
record is added under it. If a measurement contradicts the code, the code changes, or the value is
dropped.

## Who measures, and how to record it

- Only the owner of the pad runs these. Agents never run `gip-pad` or open a USB device (see
  `AGENTS.md`).
- "Without the packet" means a local build with one line changed. That build is never committed.
- Unplug and replug the pad before each run, so that every run starts from the same state.
- Record, under the entry: the date, the pad (USB ID, and the firmware that `gip-pad --info` prints),
  the commit of `gip-pad` and the local change if there was one, the exact command, the relevant
  `--raw` lines, what was observed, and the conclusion.
- Mask the device ID before you paste raw lines: the first 6 payload bytes of the announce packet
  (`0x02`), that is packet bytes 4 to 9, become `xx`.

The pad measured so far is the PDP Afterglow Wave Wired Controller for Xbox (`0e6f:0338`, firmware
2.0.3.0).

## 1. Acknowledgement payload layout

- **Code:** `Gip::ack` in `src/lib.rs` sends `01 20 SS 09 | 00 CC 20 RL RH 00 00 ML MH`. `SS` is the
  sequence number of the packet being acknowledged and `CC` its command. `RL RH` is the number of
  bytes received (little endian): the payload length, or, for a fragmented transfer, the bytes
  received so far. `ML MH` is the number of bytes still missing.
- **In MS-GIPUSB:** the packet's type `0x01`, its flags and its length of 9 (Table 26, section
  3.1.5.1). The meaning of the 9 payload bytes is not.
- **To measure:**
  1. With the packet as it is: run `gip-pad --info --raw`. Observe whether the fragmented metadata
     transfer (`0x04`) completes ("metadata: N of N bytes"), and whether any fragment arrives twice.
  2. Without a correct payload: in a local build, make `Gip::ack` send nine zero bytes as its
     payload. Repeat step 1. Observe whether the pad repeats fragments, stops early or completes.
  3. Guide, which asks for an acknowledgement: run `gip-pad --raw --seconds 20` and press Guide three
     times, with the normal build and with the build of step 2. Observe whether the pad sends the
     same `0x07` packet (same sequence number) again when the acknowledgement is wrong.
- **Supports it if:** step 1 completes without repeats, and step 2 or 3 behaves differently.
- **Status:** to measure.

## 2. LED packet sent to PDP pads: `0A 20 SS 03 | 00 01 14`

- **Code:** `send_start` in `src/startup.rs` sends it after START (`0x05`) when the vendor is PDP
  (`0x0e6f`).
- **In MS-GIPUSB:** the layout of the LED command `0x0A` (command, pattern, intensity; Tables 41 and
  42, section 3.1.5.5.7). The values `00 01 14`, and whether this pad needs the packet before it
  sends input, are not.
- **To measure:**
  1. With the packet: run `gip-pad --raw --seconds 20`, press A a few times and watch the Guide LED.
     Observe whether input (`0x20`) arrives and what the LED does.
  2. Without the packet: in a local build, remove the LED line from `send_start`. Repeat step 1.
  3. With another intensity: change `0x14` to `0x05`. Repeat step 1 and compare the LED brightness.
- **Supports it if:** step 1 gives input, and steps 2 and 3 show what the packet and its last byte
  change (no input, or a different LED state).
- **Status:** to measure.

## 3. `0x06` packet sent to PDP pads: `06 20 SS 02 | 01 00`

- **Code:** `send_start` in `src/startup.rs` sends it after the LED packet when the vendor is PDP.
  The constant is named `CMD_AUTHENTICATE` in `src/lib.rs`.
- **In MS-GIPUSB:** not this packet, its payload, or that the pad waits for it.
- **To measure:**
  1. With the packet: run `gip-pad --raw --seconds 20` and press A a few times. Observe whether input
     (`0x20`) arrives.
  2. Without the packet: in a local build, remove the `0x06` line from `send_start`. Repeat step 1.
  3. With payload `00 00` instead of `01 00`. Repeat step 1.
- **Supports it if:** step 1 gives input and step 2 or 3 gives none.
- **Status:** to measure.

## 4. Guide button: bit 0 of the first payload byte of `0x07`

- **Code:** `Controller::poll` in `src/lib.rs` sets `guide` from bit 0 of the first payload byte of
  an unfragmented `0x07` packet. It does not check the system flag (`0x20`) in the options.
- **In MS-GIPUSB:** the `0x07` packet is listed with a 2-byte payload, and section 3.1.5.5.6
  describes its behaviour only, not its bits.
- **To measure:** run `gip-pad --raw --seconds 20`. Press and release Guide three times, then hold
  it for two seconds. Record every `0x07` packet: options byte, length and payload.
- **Supports it if:** bit 0 of the first payload byte is 1 exactly while Guide is held, and no other
  bit follows the button. Note the options byte too: if it always carries `0x20`, the code can
  require the system flag.
- **Status:** to measure.

## 5. Share button: bit 0 of payload byte 28 in the 46-byte report

- **Code:** `Pad::apply_input` in `src/lib.rs` reads Share from bit 0 of payload byte 28 (packet
  byte 32 with a 4-byte header) when the report payload is at least 46 bytes (`0x2e`), for any
  vendor. It does not check the system flag (`0x20`) in the options.
- **In MS-GIPUSB:** Table 62 puts the Console Function Map at byte 18 of a 32-byte payload. This
  pad sends 46 bytes, and the specification does not place Share there.
- **To measure:** run `gip-pad --raw --seconds 30`. Press and release Share alone three times, then
  press every other button once, one at a time. Record the `0x20` packets.
- **Supports it if:** bit 0 of payload byte 28 changes with Share and with no other button, and the
  report is 46 bytes. Note the options byte of the input reports too: if it never carries `0x20`,
  the code can refuse system-flagged input.
- **Status:** to measure.

## 6. Rumble payload: `09 00 SS 09 | 00 0F 00 00 SS SS DD 00 00`

- **What the code does:** `Controller::rumble` sends the Direct Motor command `0x09`. MS-GIPUSB
  (section 3.1.5.6.1, Table 56) names the command; the code's payload layout (motor mask, two trigger
  motors, two main motors, on time, off time, repeat) is not recorded there or measured here.
- **What to measure:** with `--info`, the short rumble at start. Then, with a build that changes one
  byte at a time (the mask to `0x03` and to `0x0C`, the main motors to `0`, the on time to `0x32`):
  which motors run, and for how long.
- **What to record:** for each payload, which motors ran and the duration, timed by ear or video.
- **Status:** to measure.

## Behaviour the code assumes

These are not protocol values, but the code relies on them and no run has shown them yet.

- **A pad that announces again during a session (since 0.4.0).** `Controller::poll` treats a valid
  Hello (`0x02`) after startup as a restart: it sends the start packets again and reports every
  input as released until the pad reports again. To measure: run `gip-pad --raw --seconds 300`,
  partly idle and partly pressing buttons, and count the `0x02` packets after startup. None are
  expected from a pad that keeps working. If one does appear, check that input resumes after it.
  **Status:** to measure.
- **A refused reset (since 0.4.0).** If macOS refuses the USB reset because the device is open
  exclusively elsewhere, `gip-pad` warns "USB reset refused" and opens the pad without a reset. To
  measure: note whether that warning ever appears with this pad, for instance while its headset
  audio interface is in use, and whether input still arrives after it. **Status:** to measure.
