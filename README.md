# Glinux Hub - GProX

A native Linux replacement for Logitech G HUB's audio features on the **Logitech G PRO X Wireless** headset (USB dongle
`046d:0aba`). It runs G HUB's headphone EQ and the Blue VO!CE microphone chain as PipeWire nodes and controls the headset
itself (sidetone, auto-off, battery, mic mute), with the aim of sounding and behaving on Linux as it does under G HUB on Windows.

Unofficial and not affiliated with Logitech. No Logitech code, binaries, preset files or battery model
are included; presets and the battery model are read at run time from your own G HUB installation. The source does contain
numeric constants of G HUB's algorithms, its default Blue VO!CE parameter values, and the values of one preset (Broadcaster 1)
that the tests use.

## Features

- **Headphone EQ**: G HUB's 10-band "advanced" and 2-band "basic" EQ with its presets. The output is bit-exact with
  G HUB's in the two recorded test sequences (48 kHz), including the 100 ms gain ramps.
- **Blue VO!CE mic chain**: voice EQ, de-popper, noise reduction, expander/gate, de-esser, compressor and limiter, with all of
  G HUB's Blue VO!CE presets and your own. See [How close is it?](#how-close-is-it) for measurements.
- **Your G HUB presets**: imports every headphone EQ and Blue VO!CE preset from G HUB's settings database, including ones you
  made or downloaded.
- **Headset controls**: sidetone level, auto-off timer, battery level and remaining time, mic mute state, and recovery when the
  dongle is re-plugged or the headset is switched off and on.
- **Tray widget for KDE Plasma 6**, a small GUI and a command-line tool. Changes apply live, without dropouts.
- **Packaged for Arch / CachyOS**: a systemd user service starts it at login and makes it the default output and input.

![The tray widget in Plasma's light and dark themes](docs/tray.png)

## How it works

`gproxd` is a user daemon. It creates a virtual sink and source in PipeWire, processes audio in real time (no allocations, locks
or system calls on the audio thread), talks HID++ to the headset dongle, and serves a local control socket
(`$XDG_RUNTIME_DIR/gprox/control.sock`, your user only). The CLI, the GUI and the tray widget are clients of that socket.

| Crate | Purpose |
|---|---|
| `gprox-dsp` | The DSP: headphone EQ, Blue VO!CE blocks, noise reduction |
| `gprox-pw` | PipeWire virtual sink/source |
| `gprox-hid` | HID++ headset control, with an allow-list of the commands G HUB sends (firmware commands are blocked in code) |
| `gprox-ghub` | Reading G HUB's preset files and settings database |
| `gprox-core` | Preset formats and the control protocol |
| `gprox-daemon` | `gproxd` |
| `gprox-cli`, `gprox-gui` | `gprox` and `gprox-gui` |

## Install (Arch / CachyOS)

An AUR package is coming. Until then, pick one of these.

**Prebuilt package** (fastest, no compiling). Download it from the
[latest release](https://github.com/Unackn0wledged/Glinux-Hub-GProX/releases/latest) and install the downloaded file:

```bash
curl -LO https://github.com/Unackn0wledged/Glinux-Hub-GProX/releases/download/v0.1.0/gprox-0.1.0-1-x86_64.pkg.tar.zst
```

```bash
sudo pacman -U gprox-0.1.0-1-x86_64.pkg.tar.zst
```

The release notes list the package's SHA-256 checksum, so you can check the download with `sha256sum` first.

**Build the release from source.** Needs `rustup`, `clang` and `pkgconf`. The PKGBUILD downloads the tagged release
and verifies its checksum:

```bash
git clone https://github.com/Unackn0wledged/Glinux-Hub-GProX.git
```

```bash
cd Glinux-Hub-GProX/packaging/aur && makepkg -si
```

To build the latest development version instead, use `packaging/arch` the same way. It builds the commit you have
checked out.

Either way, this installs `gproxd`, `gprox`, `gprox-gui`, the systemd user service (enabled for all users, so it starts at login), a udev
rule that lets you access the headset dongle, and the Plasma tray widget. Start it now without logging out:

```bash
systemctl --user start gproxd
```

Other distributions: `cargo build --release`, then install the files under `packaging/` by hand.

## Setup

gprox needs two files from your own G HUB installation: the Blue VO!CE presets and the battery model. Copy the folder
`C:\ProgramData\LGHUB\depots\<number>\pro_headset` from Windows, then:

```bash
gprox setup /path/to/pro_headset
```

```bash
systemctl --user restart gproxd
```

This copies the two files to `~/.config/gprox/` and makes **PRO X (gprox)** and **PRO X Mic (gprox)** your default output
and input. Without it, the headphone EQ and headset controls still work, the mic is passed through unprocessed, and the
battery shows in volts.

To bring over your own presets, copy `settings.db` and `settings.db-wal` from `%LOCALAPPDATA%\LGHUB\` (with G HUB closed):

```bash
gprox import ghub /path/to/folder-with-settings.db
```

A preset with a value outside the range G HUB's own sliders allow is listed under "not imported" with the reason; the rest
are imported.

## Usage

- **Tray widget**: right-click the system tray → **Configure System Tray…** → **Entries** → **gprox Headset** → **Always
  shown**. The popup shows the battery, output and mic volume, the EQ and Blue VO!CE presets, sidetone (with the always-on
  switch), auto-off and the mute state. If `~/.config/gprox/headset.png` exists, it is shown as the headset picture.
- **GUI**: `gprox-gui` ("gprox" in the application menu) has the preset pickers, the headphone EQ curve and the headset
  controls.
- **CLI**:

```bash
gprox status
```

```bash
gprox preset list
```

```bash
gprox preset set bass
```

```bash
gprox bluevoice set "broadcaster 1"
```

```bash
gprox bluevoice off
```

```bash
gprox sidetone 30
```

```bash
gprox sidetone always-on on
```

```bash
gprox auto-off 15
```

Headphone EQ presets are TOML files in `~/.config/gprox/hp-eq/` (the file name is the preset name; `flat` is always there,
and `gprox import ghub` adds G HUB's and your own). For example `~/.config/gprox/hp-eq/bass.toml`:

```toml
mode = "basic"        # or "advanced"
basic = [6.0, -3.0]   # bass and treble shelves, dB
# advanced = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]   # 32 Hz … 16 kHz, ±12 dB
```

**Always-on sidetone.** The headset only plays sidetone while the computer records from its mic. With nothing recording,
PipeWire stops the headset's mic stream after a few seconds and sidetone goes quiet. `gprox sidetone always-on on` (or the
**Always on** switch in the GUI and the tray widget) keeps the stream open: gproxd records from the headset mic itself
(`gprox_sidetone` in mixers) and throws the audio away. Apps still only get mic audio when they record from **PRO X Mic
(gprox)**, and nothing is sent to the headset. Off by default; it is saved and follows dongle re-plugs and headset power
cycles. While it is on, desktop "microphone in use" indicators (KDE's included) show permanently, and the headset streams its
mic all the time, which may cost some battery (not measured).

Notes:
- The mic mute state is best effort: the headset doesn't report it, so gprox counts presses of the mute button and treats any
  mic audio as live. It shows "unknown" until the mic has been used once (or always-on sidetone is on). The dongle sends the
  same signal when the headset's mic or playback stream starts or is closed; gproxd watches the headset's devices in
  PipeWire and ignores it at those moments, so a real press within a second of one is missed until the next press or speech.
- Only one gproxd runs per user; a second one stops at once. If PipeWire goes away or a stream fails, gproxd exits with an
  error and the service restarts it.
- While the headset is missing, **PRO X Mic (gprox)** records silence instead of switching to another microphone, and **PRO X
  (gprox)** plays to your default output (with the headset EQ still applied).
- Logs: `journalctl --user -u gproxd`. Turn gprox off for your user: `systemctl --user mask --now gproxd`. When gproxd stops,
  audio falls back to the headset directly.

## How it was built

G HUB was analysed in a Windows 11 virtual machine and never run on Linux. Each part was studied, written up as a
specification in our own words, re-implemented from scratch in Rust, and then checked against reference recordings of G HUB's
own output:

1. **USB traffic** between G HUB and the dongle was captured with `usbmon` and Wireshark. This gave the HID++ commands for
   sidetone, auto-off and battery, and the audio formats.
2. **Static analysis** of G HUB's audio processing objects (the Windows APOs that run inside `audiodg.exe`) in Ghidra showed
   the structure of the EQ and the Blue VO!CE engine: filter designs, dynamics, noise reduction and parameter mapping. The
   exact arithmetic of the EQ's filter routine was worked out from its disassembly.
3. **Dynamic analysis** with Frida: hooks inside the Windows audio engine logged parameters, filter coefficients and the audio
   going in and out. A small harness drove the Blue VO!CE engine on its own with test signals. These recordings became the
   golden tests (see [How close is it?](#how-close-is-it)).
4. G HUB's preset files and settings database (SQLite holding protobuf-style JSON) were decoded to import presets.

Tools: Ghidra, Frida, Wireshark/usbmon, QEMU/KVM with virt-manager, Python and NumPy for analysis, Rust for the
implementation, Qt/QML for the tray widget. Analysis and implementation were done with AI assistance (Claude Code).

The purpose of the reverse engineering was to make this independent program work with the headset and with G HUB's presets.
The specifications, recordings and G HUB files used during development are
not part of this repository.

## How close is it?

- **Headphone EQ:** bit-exact with G HUB's recorded output in two sequences (a stream start, and a preset change while playing),
  at 48 kHz and several buffer sizes.
- **Blue VO!CE, single modules:** driven with the same test signals as G HUB's own engine, most are bit-exact; the rest differ by
  −102 dB or less.
- **Blue VO!CE, full chain:** on 8 seconds of recorded speech, −89, −83 and −101 dB from G HUB's engine for Broadcaster 1, FM Station
  and AM Radio. Other presets have not been measured.
- **Against G HUB running live on Windows:** one recording, Broadcaster 1, the same 8 seconds of speech: −68 dB overall and −54 dB in
  the worst second. Driving G HUB's engine on its own differs from the live recording by the same amount; the cause is not known.
- **Not measured:** switching modules on and off mid-stream, and sample rates other than 48 kHz.

## Development

```bash
bash tools/ci.sh
```

This runs `cargo fmt`, `clippy` and the tests. The golden tests compare against reference recordings of G HUB's output, which
are not distributed; without them those tests skip and the rest run normally. With the recordings present,
`GPROX_REQUIRE_VECTORS=1` turns every skip into a failure.

## License

GPL-3.0-or-later, see [LICENSE](LICENSE).

Logitech, G HUB, PRO X and Blue VO!CE are trademarks of Logitech. They are used here only to describe what this software
works with.
