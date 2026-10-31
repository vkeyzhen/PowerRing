# PowerRing

> 中文版：[README.md](README.md)

Battery monitor for wireless peripherals on Windows. A ring in the tray tells you how much juice your mouse and headset have left — without installing one vendor app per brand.

- **Single ~1 MB exe**, portable, no admin rights needed
- **No background network, no account, no telemetry** — everything stays on your machine
- **Adding a device = adding a YAML file**, no code changes (see [Adding a device](#adding-a-device))

![Hovering the tray icon shows all three devices, lowest battery first](assets/tooltip.png)

![Clicking the icon opens the battery panel: full name and level per device, with a status dot on the left](assets/panel.png)

## Features

| Feature | Notes |
| --- | --- |
| Tray battery ring | Ring fill maps to remaining charge. Green → orange → red as it drains, blue while charging. **Unreadable is shown as a grey ring — never as 0%** |
| Hover readout | Shows percentage per device. With multiple devices it uses short names, sorted lowest first |
| Left click | Toggles the battery panel with every device's full name and level |
| Right click menu | Show panel / Refresh now / Settings… / Exit |
| Low battery alerts | Fires once when crossing a threshold, then stays quiet. Card is anchored next to the tray icon |
| Start with Windows | Writes `HKCU\...\Run`, no elevation. A named mutex keeps a second instance from spawning a second icon |
| Theme | Follow system / light / dark. You can force dark even when apps are light and only the taskbar is dark |
| Auto discovery | Rescans the bus every 60 s: a device you switch on mid-session shows up on its own — **and for three minutes after something vanishes the pace tightens to every 10 s**, so switching it back on brings it straight back. A device that stops answering is marked *disconnected* and stays in the tray: it never reports a stale number, but it also does not disappear just because it dozed off |
| Logs | `%APPDATA%\PowerRing\log.txt`, rotates above 1 MB |

PowerRing asks each device once per poll interval (60 s by default), so **any change shows up at most one round later**: unplug a receiver and it drops off the tray within roughly 30–60 s, plug it back in and it returns on the next scan. That is deliberate — probing more often just to notice sooner wakes sleeping devices, which spends the battery we are trying to report on. Shorten the interval in Settings if that feels slow.

Settings expose the refresh interval, warning and critical thresholds, the alert toggle (with a "Test once" button so you can verify it immediately), start-with-Windows, the theme, and the custom URL used by "Check for declaration updates" (only needed when GitHub is unreachable). The keyboard works too: Tab moves between controls, Enter saves, Esc closes without saving.

## Verified devices

| Device | Interface | Reading taken from | Status |
| --- | --- | --- | --- |
| Logitech G PRO Wireless | receiver `046D:C539`, usage page `FF00` / usage `0002`, slots 1–6 | HID++ 2.0 `0x1001` voltage (mV → discharge curve) | Verified. Within ±1% of G HUB |
| Logitech G PRO Wireless (plugged in) | **USB cable** `046D:C088`, usage page `FF00` / usage `0002` | same `0x1001` feature | Verified. 4101 mV reads as 93%, correctly shown as charging |
| HyperX Cloud Flight | receiver `0951:1723`, usage page `FF90` / usage `0303` | vendor report `21 FF 05` (direct mode) | Verified, stable and self-consistent (raw 231 reads as 60%); the vendor app shows no percentage, so there is no external reference |
| Xbox Wireless Controller (Series) | **Bluetooth**, VID `045E` / PID `0B13` | standard Battery Service `0x180F` / `0x2A19` | Verified. Matches Windows' Bluetooth settings (checked at 93% and at 12%) |
| AULA MK75 (mechanical keyboard) | receiver `3554:FA09`, usage page `FF02` / usage `0002` | vendor report `13 05` (two replies per query) | Verified. At 87% the keyboard's own battery LED (Fn+B) lights up to the 8th key; while charging the reading climbs 89% → 93% |

### How accurate are the numbers

Short version: **anything that reports a percentage itself reads exactly what the OS reads; only devices that report voltage and get converted carry about ±1% of jitter.**

| Kind | Who decides the number | Accuracy |
| --- | --- | --- |
| Bluetooth (Xbox controller) | the device reports a percentage | Same source and same value as Windows' Bluetooth settings. Checked at both ends of the range, identical both times |
| Devices reporting a percentage | the device itself | Same: what it reports is what you see |
| Voltage devices (GPW) | PowerRing converts via a discharge curve | **±1% against G HUB, and the sign flips** — 1 high near full, 1 low near empty |

That ±1% is left alone deliberately: **it is not an offset you can shift away.** The sign flipping says the difference does not come from the curve sitting 1% off, but from the two sides sampling at different moments and rounding differently. Nudging a shared curve to suit one device would spoil it for every other Logitech one. If you want to settle it: let the mouse sit still for a few minutes and read both sides on the same instant.

One caveat on the Cloud Flight: its `verified` **does not include "cross-checked against the vendor reading"** — HyperX's NGENUITY app does not show a percentage at all, so there is nothing to compare against. The same caveat is recorded in `devices/hyperx_cloud_flight.yaml`.

The AULA MK75 sits between the two cases: **there is a reference, but it only has 10-step resolution**. Pressing Fn+B lights up the number row to show the level; at 87% it lit up to the 8th key, which lines up. While charging the reading climbed 89% → 93% and dropped back the moment the cable was pulled, so the trend agrees too. That makes it more trustworthy than the Cloud Flight, but not as nailed down as a point-by-point comparison against G HUB. Its vendor driver **does not show a battery percentage at all** — this program is the only way to get an actual number out of this keyboard.

The same receiver is not exclusive to AULA: the AULA F75 / F99 Pro / X99 Pro share the exact same VID:PID (it is a Compx design). Those were not tested here, but the protocol is very likely identical — copy `devices/aula_mk75.yaml`, change `name`, and it should just work.

The **Xbox controller row** needs no reverse engineering at all: the Bluetooth SIG standardised how to read battery level, so any compliant device is read by UUID and its YAML carries not a single byte of a report. To list every such device on your machine:

```bash
cargo run --release --example ble_probe
```

It lists every Bluetooth device on your machine exposing the standard battery service, prints each one's **connection state**, and reads it two ways (the labels it prints are Chinese — `实时读` is the fresh read, `缓存读` the cached one):

- **fresh** — actually sends a GATT request to the device and reads it now
- **cached** — takes what the OS cached, possibly synced minutes ago

**The app prefers the fresh read** and only falls back to the cached one when the read fails at that instant. Comparing it against Windows' Bluetooth settings: both read the same GATT value, so they agree on substance; the difference is that the Settings page shows whatever it last synced, so the fresh read is newer whenever the level just changed.

> If it prints "0 devices", don't suspect the script first: it means there is **currently** no Bluetooth device that is both connected and exposing the standard battery service — controller switched off, not paired, or connected by USB cable only all count. Pair it over Bluetooth and run again.

Device declarations live in `devices/`. Every YAML carries `protocol_source` (where the constants came from) and `verification`. **Anything not tested on real hardware is marked `unverified`.**

## Usage

Download the zip from Releases, extract it anywhere (e.g. `C:\Tools\PowerRing`) and double-click `powerring.exe`. The exe must sit next to the `devices` folder:

```
PowerRing/
├─ powerring.exe
└─ devices/
   ├─ logitech_gpw.yaml           # wireless (over the receiver)
   ├─ logitech_gpw_wired.yaml     # while plugged in
   ├─ hyperx_cloud_flight.yaml
   └─ xbox_controller.yaml
```

> Windows 10 hides new tray icons inside the `^` overflow. If you don't see it, go to Taskbar settings → Notification area → Select which icons appear on the taskbar.

### First run: Windows will stop you once. That is expected

You will see the blue "Windows protected your PC" panel. **This is not a malware warning.** It is what Windows shows for any program that is not code-signed and does not yet have enough download history — nearly every individually maintained open-source project hits it.

Click **More info → Run anyway**. No developer can remove this prompt without buying a code signing certificate (and then accumulating reputation), so this project is currently unsigned.

If you would rather verify it yourself: the source is public at
[github.com/vkeyzhen/PowerRing](https://github.com/vkeyzhen/PowerRing), and the zip is built and published by GitHub Actions from that same repository. The program also **makes no network requests on its own** — the only two happen when you explicitly pick "Submit device info…" or "Check for device profile updates…" in the tray menu — and writes nothing outside `%APPDATA%\PowerRing`.

### How to upgrade

**First right-click the tray icon → Exit**, then extract the new zip **over the same folder** and start it again.

- Do not extract into a new folder: the autostart registry entry still points at the old path, so you end up with two copies running — and you will be looking at the old one
- Overwriting is safe: declarations you added or edited under `%APPDATA%\PowerRing\devices` are left alone

### New devices from other people, without upgrading

Device declarations (how to read the battery of a given device) used to ship only with the exe:
recognising a new device meant waiting for a new release. There is now a second path —
**right-click the tray icon → Check for device profile updates…** pulls the latest list
from the project, saves it locally, and it **takes effect immediately, no restart needed**.

- **If you do not click it, it does not touch the network.** This is the only place in the
  program that sends a request. No background network, no telemetry
- **If it cannot reach the server, nothing was updated** — devices already recognised keep
  working. On machines that cannot reach GitHub it simply never succeeds; that is the network
  environment, not a broken program. In that case new devices still require upgrading the exe
- The list is validated entry by entry; one broken entry is skipped. A bad entry upstream
  cannot leave you with no devices at all
- Files land in `%APPDATA%\PowerRing\devices\synced-*.yaml`. **Do not hand-edit these** —
  the next sync overwrites them. To customise one, save a copy under a different name in the
  same folder; that one is left alone
- When one device is described by several declarations (one inside the exe, one synced down,
  one you wrote), only one is used, in this order: **yours → synced → bundled → auto-detected**.
  That is why a synced correction can override the older copy bundled in the exe, which is what
  makes this feature useful even for devices that were already supported
- Cannot reach GitHub but still want this? Settings lets you enter an address you can reach;
  when set, it takes priority

## Adding a device

Copy the closest YAML in `devices/` and edit a few lines. **No Rust code changes required.** Three flavours:

**Bluetooth standard devices** (controllers, some keyboards) need nothing but VID/PID — no report bytes at all:

```yaml
protocol: "ble"
transport: "direct"
match:
  vendor_id: "0x045E"
  product_id: "0x0B13"   # product id in Bluetooth mode
battery:
  kind: "percent"        # Battery Level already is a percentage
```

**HID++ 2.0 devices** (Logitech family) use a feature index plus a reading mode:

```yaml
name: "Logitech G PRO Wireless"
short: "GPW"              # used in hover text; falls back to full name
protocol: "hid++2.0"
transport: "receiver"     # address via receiver slots; use "direct" for direct connection

match:                    # which HID interface to open
  vendor_id: "0x046D"
  product_id: "0xC539"
  usage_page: "0xFF00"
  usage: "0x0002"

slots: [1, 2, 3, 4, 5, 6] # receiver slots to probe, defaults to 1..6
battery:
  feature: "0x1001"       # battery feature index
  kind: "voltage"         # voltage = read mV then convert; percent = already a percentage
identify:                 # a receiver may carry other devices, this double-checks it
  name_feature: "0x0005"
  contains: "PRO WIRELESS"
protocol_source: "Logitech HID++ 2.0, constants referenced from Solaar (GPL-3.0) public docs"
verification: "verified"
```

**Everything else** uses `raw`: what to send, which bytes to read and how to convert are all declared. This is the real HyperX Cloud Flight config — it landed without touching a single line of Rust:

```yaml
protocol: "raw"
transport: "direct"
match:
  vendor_id: "0x0951"
  product_id: "0x1723"
  usage_page: "0xFF90"
  usage: "0x0303"
battery:
  request: "21 FF 05"     # request bytes, first byte is the report id
  reply_len: 20           # many devices are picky about length and stay silent otherwise
  state_index: 3          # index of the "level band" byte in the reply
  value_index: 4          # index of the value byte in the reply
  charging_states: ["0x10", "0x11"]
  table:                  # piecewise mapping: which range maps to which percentage
    "0x0e":
      - [89, 10]
      - [239, 60]
      - [255, 65]
    "0x0f":
      - [19, 70]
      - [129, 95]
      - [255, 100]
```

A lookup table rather than a formula: on these devices the raw value is **not linear** against percentage. A linear fit drifts by more than ten points in the upper range.

Full field reference and a step-by-step sniffing walkthrough live in [docs/adding-a-device.md](docs/adding-a-device.md). If you want your device supported, open an issue with the "New device request" template.

## Building

Needs a Rust toolchain (Windows defaults to `x86_64-pc-windows-msvc`, which requires VS 2022 build tools).

```bash
cargo build --release
```

Output: `target/release/powerring.exe` (~1 MB).

## Protocol notes worth knowing

**Logitech G PRO Wireless** (HID++ 2.0)

- No `0x1004` unified battery support; `0x1001` voltage reporting only
- Long reports must be exactly **20 bytes**, short ones **7 bytes**; a wrong length gets no reply or an `0x8F` error
- Probe receiver slots with `getFeature`, **not** ping
- Voltage jitters by about 1 mV — the app applies hysteresis and ignores sub-2-point changes
- Device names inside HID reports are NUL-padded; `trim()` does not strip those NULs

**HyperX Cloud Flight**

- The receiver exposes 4 interfaces and **only `FF90:0303` answers** battery requests. The other three (`000C:0001` / `FF00:0001` / `FFC0:0001`) stay silent — probe more than one
- With the headset powered off there is no reply, and the app correctly excludes it instead of showing 0%
- Its report id is `0x21`, higher than the usual `0x00`–`0x10` sweep, so it is easy to miss

## Known limitations

- **Bluetooth devices are supported, with a caveat**: they must expose the standard Battery Service (controllers, some keyboards). Anything on a vendor-proprietary protocol such as AirPods is out — Windows already surfaces those anyway
- Xbox controllers are readable **only over Bluetooth**: USB cable never routes through GATT and the wireless adapter speaks Microsoft's proprietary GIP
- Bluetooth Battery Level carries a single percentage byte with **no charging flag**, so these devices never show as charging
- **Bluetooth devices enter and leave the list by whether they are connected right now**: drop the link and they fall off, reconnect and they come back. A powered-off controller no longer sits in the tray forever. When a live read fails we fall back to the system cache (possibly stale) rather than dropping a device that is in use
- When a reading fails the ring turns grey instead of guessing
- **Every number says where it came from**: the panel and the tooltip mark readings as
  "system cache" or "last reading"; an unmarked number is one that was really just read.
  When a device cannot report a percentage, or the value means nothing while charging,
  it says "unknown" instead of inventing a number

## Your device is not in that table

Do not reach for the YAML template just yet — the app tries to recognise it on its own. Both of these paths are public standards, so nothing needs reversing:

| Your device | How it gets recognised |
| --- | --- |
| Bluetooth (controllers, some keyboards and headsets) | Anything exposing the standard Battery Service (`0x180F`) is readable, and the percentage is **reported by the device itself** |
| Logitech (mice, keyboards; receiver or cable) | We ask whether it supports a battery feature (`0x1001` voltage, `0x1004` unified). It counts only if the device answers **and we actually read one plausible value** |

Every slot on a receiver is probed, not just the first one that answers — if a mouse and a keyboard share one receiver and report battery differently (one in millivolts, one as a percentage), both show up.

Anything recognised is written out as a **standard YAML declaration** under `%APPDATA%\PowerRing\devices`, so the next launch hits it directly instead of probing again. You can delete that file (it will be re-discovered next time) or paste it straight into an issue — it is already a declaration that can be merged as-is.

Two rules who anyone touching this code should keep: **only send HID++ reports to Logitech devices** (it is their protocol; firing vendor-private requests at someone else's hardware wakes sleeping devices), and **only probe on the first round after launch and while no device has been found** — once something is in use, leave it alone.

What genuinely cannot be recognised (vendor-private protocols, the HyperX Cloud Flight among them) needs a human: either a public source or your own packet capture. That path starts at [docs/adding-a-device.md](docs/adding-a-device.md).

### When it is not recognised: export device info

**Right-click the tray icon → Export device info…** and a plain-text report lands on your clipboard. Two ways to hand it over, neither needs any coding:

- **Submit device info…** (in the same menu): opens a form page — paste, submit, done. **No account needed.**
  This is the route for people without GitHub, and the one the maintainer actually collects:
  [wj.qq.com/s2/28339156/r6r6](https://wj.qq.com/s2/28339156/r6r6)
- [Device request issue](https://github.com/vkeyzhen/PowerRing/issues/new?template=device-request.yml):
  if you already use GitHub, paste the same text there

It has three sections: what the app is currently showing, which Bluetooth devices expose the standard battery service, and the VID:PID plus usage of every USB HID interface on the machine. That is enough to decide whether a device can be added and how.

Three privacy limits are hard-coded: **no serial numbers** (unique per device), **no interface paths** (they carry a machine-specific session id), **no Bluetooth addresses**. Device names are included because that is how a model is identified — so glance over yours before pasting in case it holds a nickname.

Three doors if you want more devices recognised, or want to change the code:

| Goal | Start here |
| --- | --- |
| Add your own device | [docs/adding-a-device.md](docs/adding-a-device.md) — no coding required; supplying data alone is a contribution too, the issue template has a slot for it |
| Change code, open a PR | [CONTRIBUTING.md](CONTRIBUTING.md): how to run it, code requirements, PR format |
| Report a security problem | [SECURITY.md](SECURITY.md): use GitHub Security Advisory, not a public issue |

## Credits

- [@tsubasa597](https://github.com/tsubasa597): HID++ reply validation, the two-step
  0x1004 read, and reading-source labels (issues
  [#7](https://github.com/vkeyzhen/PowerRing/issues/7),
  [#8](https://github.com/vkeyzhen/PowerRing/issues/8),
  [#12](https://github.com/vkeyzhen/PowerRing/issues/12)).
  Worth recording specifically: he disproved the claim that such bugs can only be
  verified by replaying real traffic — extract the predicate into a pure function and
  synthetic packets are enough. That idea now lives in `src/hidpp.rs`

## License boundary

Protocol constants and report layouts are facts and may be carried over; **do not copy source code** from GPL projects like Solaar, HeadsetControl or OpenRazer. Every device declaration must fill in `protocol_source`. When citing a GPL project, take constants only — never code.

PowerRing itself is **MIT**, see [LICENSE](LICENSE).
