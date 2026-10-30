# thinksmart-view-hal

Hardware support for the **Lenovo ThinkSmart View** (CD-18781Y) on Arch Linux ARM
with mainline Linux 7.3. One Rust crate, one module per device, no UI and no
policy. The sibling of `thinksmart-hub-hal` for the ThinkSmart Hub 500, and built
the same way.

These were Zoom Rooms and Teams tabletop displays: Qualcomm APQ8053 (eight
Cortex-A53, Snapdragon 625 family with no modem), 2 GB LPDDR3, an 8" 800x1280
portrait DSI panel, a camera with a sliding cover, two volume keys, mic array and
speaker. The Android they shipped with is gone and Linux 7.3 runs on them, but
several pieces of hardware do not behave the way their interfaces suggest.

Every fact below says where it came from. "Measured" means on this unit, on the
7.3 image, unless it says stock (the as-received Android).

## Modules

| Module | Hardware | Quirk |
|---|---|---|
| `display` | DSI panel `card0-DSI-1`, PMI8950 WLED backlight `/sys/class/backlight/backlight` (0..4095) | The low-end cutoff is unsettled, so brightness is clamped to a configurable floor (default 1400). Blanking is `bl_power` only: DRM DPMS-off did not recover on the previous port. The mode comes from debugfs, never from opening `/dev/dri/card0` |
| `orientation` | Bosch BMA253 over IIO (`name` = `bma253`) | Bit-banged I2C: about 1 ms of CPU per axis read. No mount matrix in the DT. Slope interrupt never seen to fire |
| `switches` | Camera lens cover, `SW_CAMERA_LENS_COVER` on `gpio-keys` | No mic-mute switch is fitted, though the DT and the input device advertise one. The cover mutes nothing |
| `buttons` | Volume Up (GPIO 85, `gpio-keys`), Volume Down (PM8953 `resin`, `pm8941_resin`) | Two different input devices. No power key. Volume Down is the PMIC reset gesture |
| `usb` | DWC3 gadget `tsv`: NCM, ACM, adb | Device mode only, no CC termination: works from a USB-A host, not from a Type-C host port that waits for CC |
| `thermal` | TSENS zones, PMIC zone, cpufreq | cpufreq has been broken on 7.3 and is being fixed; read as found, `None` when absent |
| `inventory` | One-shot hardware description | Leaves out the SoC serial number and everything derived from it |
| `evdev` | `ioctl`/`read` on `/dev/input/event*` | Devices are found by name; numbers move between boots |
| `util` | Bounded-child-process primitive, file helpers | Same API as `hub-hal`'s |

Every handle follows `hub-hal`'s shape: `new()`, `state()` returning a cloned
`#[derive(Serialize)] State`, `refresh()` or `poll()`, setters returning
`Result<(), String>`, `None` for unknown, `error: Option<String>`, and one
timestamp name, `updated_ms`. Inputs also offer `events()`, a blocking iterator.
[`STATE.md`](STATE.md) lists every field, its unit, and which ones are shared with
`hub-hal`.

## The backlight

- PMI8950 WLED, four strings at 20 mA. `max_brightness` 4095, `type raw`
  (measured).
- **Where it goes dark is not known for mainline.** The previous port (rickx,
  Plasma 6) said below about 1200 turns the panel off, and the image's idle
  screen never goes below 1400 on that basis. A sweep on stock found it readable
  down to about 1 of 255 and dark only at the bottom, which makes 1200 look like a
  policy artefact (`devices/backlight/ANALYSIS.md` §4). Mainline also still runs
  the WLED with OVP and boost limits above stock (§3). This crate's default floor
  is the idle screen's 1400; `Display::set_floor` changes it. Re-sweep on
  mainline before lowering it.
- **Blank with `bl_power`, not DPMS.** Turning the DRM connector off was
  unrecoverable on the previous port without restarting the compositor
  (`devices/display-panel/ANALYSIS.md` §7). The panel is a TDDI: display and touch
  are one FocalTech FT8201 die sharing a reset line, so a panel power-down also
  resets touch. Nothing here writes `dpms` or does a modeset.
- **Opening `/dev/dri/card0` is not free.** With no DRM master present, the first
  opener becomes master. The idle screen takes master only while its wordmark
  turns, so a stray open can make it step aside. The mode and refresh rate are
  read from `/sys/kernel/debug/dri/0/state` instead (root only; otherwise
  `refresh_hz` is `None`).
- The idle screen (`tsv-idle-screen`) lowers the backlight to 2000 when it starts
  and restores the old value when it stops.

## The accelerometer

- θ = atan2(a_x, −a_y), degrees, 0 upright in native portrait, positive clockwise
  as the viewer sees it. The direction was calibrated by hand (upright x +0.6,
  y −9.5 m/s²; turned clockwise x +6.4, y −3.4) and the reference pose on the
  stand reads x +0.30, y −9.54, θ +1.8° (`streams/idle-rotate/NOTES.md`). Same
  formula, filter (one pole, τ 0.12 s) and flat rule (below 3 m/s² in-plane, clear
  above 3.5) as the idle screen, so the two agree.
- **About 1 ms of CPU per sysfs read.** The sensor bus is `i2c-gpio` on GPIO 6/7
  because on stock the ADSP owned it (`CHIP.md`). A poll reads three axes, about
  2 ms wall time measured (`last_read_us` 2162). The idle screen already polls it
  at 2 Hz, so a second reader doubles the cost.
- `in_mount_matrix` is identity: the 7.3 DT has no `mount-matrix`
  (`devices/accelerometer/ANALYSIS.md` §5). It is applied anyway.
- Data-ready interrupts on GPIO 42 work; the slope (motion) interrupt has not been
  seen to fire. This crate polls and never writes the sensor's attributes.
- The IIO device number is probe order (`iio:device2` on one boot, `iio:device0`
  on the next, measured); it is found by name.

## Switches and buttons

- Lens cover: `SW_CAMERA_LENS_COVER`, GPIO 87, on `gpio-keys`. On stock the
  kernel delivered it as `KEY_POWER`, which nothing consumed
  (`devices/privacy-switches/ANALYSIS.md` §2). Which value means "covered" follows
  the kernel's convention; it has not yet been confirmed by flipping the cover on
  mainline.
- **No microphone mute.** `gpio-keys` advertises `SW_MICROPHONE_INSERT` on
  GPIO 86 ("Mic Mute" in `/proc/interrupts`), but nothing is connected, and stock
  recordings either side of a cover flip were the same level (§4). A crashed
  audio stack leaves the microphones live with no physical fallback.
- Volume Up is GPIO 85 on `gpio-keys`; Volume Down is the PM8953 PON `resin`
  input, named `pm8941_resin` by the driver. There is no power key: `kpdpwr` is
  unwired and disabled in the DT (`devices/buttons/ANALYSIS.md`).
- The nodes are `root:input 0660`. The image's `jethac` user is not in `input`,
  so as that user `state()` reports "permission denied" rather than "no buttons"
  (measured). Add the user to `input`, or run as root.

## USB

- DWC3 at `7000000`, USB 2.0 high speed, peripheral by default
  (`role-switch-default-mode`). The board has no Rd on CC: stock's Type-C driver
  read CC as OPEN with a cable attached (`devices/usb/ANALYSIS.md` §4). **Use a
  USB-A host port or a powered hub**, with an A-to-C cable.
- The image's `tsv-usb-gadget` builds gadget `tsv` (`1d6b:0104`) with NCM, ACM and
  adb, and puts `172.16.42.1/24` on `usb0` (`rootfs/README.md`). adbd closing
  FunctionFS unbinds the whole gadget, which the supervisor rebinds; a brief
  `udc: null` is that.
- Not reported: the USB serial string and NCM MACs (derived from the SoC serial)
  and IPv6 link-local addresses (which embed the MAC).

## Thermal and clocks

- Ten zones: `cpu0`..`cpu7-thermal`, `gpu-thermal`, `pm8953-thermal`, in m°C.
- On the baseline kernel on 2026-10-30 the CPU passive trips were 50 and 56 °C,
  the CPUs idled at 47-51 °C, and the cpufreq policy was capped at 1612.8 MHz
  against a 1804.8 MHz hardware maximum (measured). The trips are reported as the
  kernel has them, not assumed.
- cpufreq: one policy for all eight cores (`cpufreq-dt`, `schedutil`) on that
  kernel. Earlier 7.3 boots had none (`Failed to set OPP config`, about 1.23 GHz,
  `CHIP.md`), in which case the clocks are `None`.

## Building

Cross-built on an x86_64 laptop into one static binary:

```
rustup target add aarch64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl
cargo test
```

`.cargo/config.toml` links with `clang` driving `ld.lld`; rustc supplies musl's
crt objects and libc, so no aarch64 sysroot is needed. Dependencies are `libc`,
`serde` and `serde_json`: no C libraries, no libevdev, no libdrm.

`view-hal-dump` prints `inventory::collect()` and every module's `state()` as
JSON; `--events` streams button and switch events one JSON object per line;
`--set-brightness N [--floor N]` and `--power on|off` write the backlight.
`tests/fixtures/view-hal-dump.json` is its output on this unit (as root), and
`view-hal-dump-unprivileged.json` the same as the `jethac` user.

## Using it

Not published to crates.io. Add it as a git or path dependency and pin a
revision.

## Scope

One machine, one image. Not a cross-platform abstraction. Hardware facts and
their provenance live in `jethac/lenovo-thinksmart-view` (`CHIP.md`,
`devices/*/ANALYSIS.md`, `rootfs/README.md`).

Not here yet: audio and microphones (TAS5782M speaker driver and DSP patch in
progress), the VCNL4200 proximity sensor (disabled in the DT: its IR LED is driven
harder than stock), Wi-Fi (QCA9379), camera, touch.

## License

MIT or Apache-2.0, at your option.
