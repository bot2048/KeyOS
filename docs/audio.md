# Audio output over USB-C

Passport Prime has no audio codec, DAC or headphone jack. Its USB-C port does run in
host mode, so KeyOS plays audio on USB Audio Class (UAC) devices plugged into it:

- USB-C headphones and earbuds with a built-in DAC
- USB-C to 3.5 mm headphone adapters (Apple, Google, Samsung and the many C-Media and
  Realtek based ones)
- USB sound cards and DACs
- USB Bluetooth audio transmitters, which present themselves as a UAC device and pair
  with Bluetooth headphones on their own

## Why Bluetooth headphones need a transmitter

The Prime's radio is an nRF52805 running a BLE-only application on Nordic's SoftDevice.
Classic Bluetooth audio (A2DP) needs a BR/EDR radio, which the chip does not have. LE
Audio needs LE Isochronous Channels and an LC3 codec, which the SoftDevice for this chip
does not support and which would not fit its 24 KB of RAM. A USB Bluetooth transmitter
does the Bluetooth side itself and looks like any other USB sound card, so the device
uses it through the same path as a headphone adapter.

## Architecture

```
app ──Play (48 kHz s16 stereo)──▶ os/audio ──IsoOut / ControlTransfer──▶ os/usb
                                     │                                      │
                               uac (parse,                         EHCI (high speed)
                               choose, schedule,                   OHCI (full speed)
                               convert, resample)                         │
                                                                    USB-C ─▶ device
```

- `imports/uac`: no_std crate that parses the audio function out of the descriptors
  (UAC1 and UAC2), picks a playback alternate setting and rate (16-bit stereo 48 kHz,
  else 44.1 kHz, else the closest the device has that fits its endpoint), works out the
  frames in each isochronous packet (44 and 45 at 44.1 kHz), builds the class requests
  (UAC1 endpoint rate, UAC2 clock source rate and RANGE, feature unit mute and volume)
  and converts PCM into 16, 24 or 32-bit subslots. Its tests rebuild descriptor bytes
  from published `lsusb -v` dumps of seven real devices; run them with
  `cargo test -p uac`.
- `imports/ehci`: reads whole configuration descriptors (an audio function's run to
  hundreds of bytes; only 64 were read before), hands full and low-speed devices to the
  companion OHCI controller through PORT_OWNER, and runs a periodic frame list with iTDs
  for isochronous OUT on high-speed devices.
- `imports/ohci`: new driver for the companion controller: root ports, enumeration,
  control transfers and isochronous OUT. Most headphone adapters, including Apple's, and
  the Bluetooth transmitters are full-speed devices, so this is the path most audio
  takes. Bulk and interrupt endpoints are not implemented on it.
- `os/usb`: drives both controllers off the shared UHPHS interrupt and serves `IsoOut`, a
  deferred lend of a buffer holding the packet lengths and payloads. Consecutive
  transfers on an endpoint are scheduled back to back. `UsbEvent::Connect` reports
  whether a device is full speed. The power manager clocks the OHCI side with 48 MHz
  from the UTMI PLL while the host is enabled.
- `os/audio` (`api/audio`): claims the first UAC device with a playback stream, sets it
  up, and streams from a quarter second queue with two worker threads that take turns
  submitting 16 ms transfers. Apps send 16-bit stereo at 48 kHz; the service resamples
  for 44.1 kHz devices and applies the volume in software (hardware volume is set to
  0 dB and unmuted). After about 128 ms of silence the workers stop submitting until
  more audio arrives.
- `apps/gui-app-audio-test`: dev app that plays a 440 Hz tone at 25 to 100 % volume.

## Testing on hardware

Hardware testing has not been done yet. To try it:

1. Build a dev image (`just build`; dev builds include the dev apps) and flash it as
   described in `docs/flash.md`.
2. Enable USB in Settings, then plug a USB-C headphone adapter with headphones in, USB-C
   headphones, or a USB Bluetooth transmitter paired with headphones. The Apple adapter
   only shows its audio interfaces once headphones are plugged into it; before that it
   is a HID device and is ignored.
3. With developer logs (`just logs`), expect `Full-speed device on port N, handing it to
   the companion` (for a full-speed device), then `Playing on device N: ...` from
   `os/audio` with the format and rate it picked.
4. Open Audio Test and press Play 440 Hz. The status line shows the device rate and bus.

Things to check first if it stays silent: whether `os/usb` logs the device as
configured, whether `os/audio` logs `Isochronous transfer failed` (with debug logs on
`os/usb`, the drivers report missed packets), and whether the device sits behind a hub,
which is not supported.

## Known limitations and risks

- Nothing here has run on hardware yet: the OHCI driver, the iTD schedule, the
  PORT_OWNER handoff and the 48 MHz clock setup are written from the EHCI and OpenHCI
  specifications and the SAMA5D2 datasheet only.
- Asynchronous devices (many DACs, the AudioQuest DragonFly) report their real rate on a
  feedback endpoint, which is not read yet. They are fed at the nominal rate, so their
  buffer drifts and an occasional click every few seconds or minutes is likely.
  Adaptive and synchronous devices (C-Media adapters, the Apple adapter) are not
  affected.
- One stream, one device, one app at a time; no mixing, no capture.
- Configuration 1 is always selected, as before.
- The EHCI iTD pool is one page, enough for about 40 ms of queued high-speed audio,
  which the service stays well within.
- The two workers rely on each filling and submitting its buffer long before the
  other's transfer finishes. Starving them of CPU for more than a transfer could swap
  two transfers.
- Audio messages use the `peripherals.feedback` permission group. A dedicated audio
  group needs a new display string from Localazy.
- Bus-powered devices draw from the Prime's battery through the VBUS boost; headphone
  adapters take tens of mA, but larger DACs may ask for up to 500 mA.
