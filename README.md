# Glinux Hub - GProX

G HUB's headphone EQ, Blue VO!CE mic chain and headset controls for the **Logitech G PRO X Wireless** on Linux.

Unofficial and not affiliated with Logitech. **No G HUB presets are included.** They are copied from your own G HUB install
(see [Setup](#setup)).

![The tray widget in Plasma's light and dark themes](docs/tray.png)

## Features

- **Headphone EQ:** G HUB's 10-band and 2-band EQ.
- **Blue VO!CE:** the full mic chain (EQ, noise reduction, gate, de-esser, compressor, limiter) using the presets from your G HUB install.
- **Headset controls:** sidetone (optionally always on), auto-off timer, battery level and mic mute state.
- **Import** of your own presets from G HUB.
- A KDE Plasma 6 tray widget, a small GUI and a command-line tool.

## Before you start

This is **not a plug-and-play app**. You need:

- The **G PRO X Wireless** with its USB dongle (`046d:0aba`). No other headset is supported.
- **Arch Linux or an Arch-based distribution** (CachyOS, EndeavourOS, …) with PipeWire and WirePlumber.
- **A Windows PC or virtual machine with G HUB installed**, to copy two files from: the Blue VO!CE presets and the battery
  model. Without them, the headphone EQ and headset controls work, but the mic is passed through unprocessed and the
  battery shows in volts instead of percent.
- A few minutes in a terminal. Setup is done with commands.
- Optional: KDE Plasma 6 for the tray widget.

## Install

Download the package from the [latest release](https://github.com/Unackn0wledged/Glinux-Hub-GProX/releases/latest)
(the release notes list its SHA-256) and install it. pacman installs the dependencies:

```bash
curl -LO https://github.com/Unackn0wledged/Glinux-Hub-GProX/releases/download/v0.1.1/gprox-0.1.1-1-x86_64.pkg.tar.zst
```

```bash
sudo pacman -U gprox-0.1.1-1-x86_64.pkg.tar.zst
```

Or build it from source (needs `rustup`, `clang` and `pkgconf`): clone this repository, then run `makepkg -si` in
`packaging/aur`.

## Setup

1. **Start the service** (it starts by itself at every login after this):

   ```bash
   systemctl --user start gproxd
   ```

2. **Copy the G HUB files.** On Windows, copy the folder `C:\ProgramData\LGHUB\depots\<number>\pro_headset` to this
   computer, then:

   ```bash
   gprox setup /path/to/pro_headset
   ```

   ```bash
   systemctl --user restart gproxd
   ```

   This also makes **PRO X (gprox)** and **PRO X Mic (gprox)** your default output and input. If you skip this step, pick
   them yourself in your sound settings.

3. **Your own G HUB presets (optional).** With G HUB closed, copy `settings.db` and `settings.db-wal` from
   `%LOCALAPPDATA%\LGHUB\` into one folder, then:

   ```bash
   gprox import ghub /path/to/that-folder
   ```

4. **Tray widget (KDE).** Right-click the system tray → **Configure System Tray…** → **Entries** → **gprox Headset** →
   **Always shown**.

## Usage

Use the tray widget, `gprox-gui` (in the application menu), or the CLI:

```bash
gprox status
```

```bash
gprox preset list
```

```bash
gprox bluevoice set "broadcaster 1"
```

```bash
gprox sidetone 30
```

`gprox --help` lists the rest.

Good to know:

- **Always-on sidetone:** the headset only plays sidetone while something records from its mic. The **Always on** switch
  (or `gprox sidetone always-on on`) keeps the mic stream open. KDE then shows its "microphone in use" icon all the time.
- **Mute state** is a best guess. The headset doesn't report it, so gprox counts button presses.
- Without the headset, **PRO X Mic (gprox)** records silence and **PRO X (gprox)** plays to your other output.
- Logs: `journalctl --user -u gproxd`. To turn gprox off: `systemctl --user mask --now gproxd`.

## How it was built

G HUB was studied in a Windows 11 virtual machine (never run on Linux). Each part was written up as a specification in our
own words, re-implemented from scratch in Rust, and tested against recordings of G HUB's own output.

Tools: **Ghidra** (static analysis of G HUB's audio processing objects), **Frida** (hooks in the Windows audio engine to record
parameters and audio), **Wireshark/usbmon** (USB traffic to the dongle), **QEMU/KVM with virt-manager**, **Python/NumPy**,
**Rust**, **Qt/QML** (tray widget) and **Claude Code** (AI assistance with the analysis and the code).

**Accuracy:** the headphone EQ is bit-exact with G HUB's output in our recordings. The Blue VO!CE chain differs from G HUB's
engine by −83 to −101 dB on 8 seconds of speech (three presets), and by −68 dB from one live G HUB recording.

Tests: `bash tools/ci.sh`. The comparisons with G HUB's output need recordings that are not distributed, so they skip here.

## License

GPL-3.0-or-later, see [LICENSE](LICENSE). Logitech, G HUB, PRO X and Blue VO!CE are trademarks of Logitech, used only to
describe what this software works with.
