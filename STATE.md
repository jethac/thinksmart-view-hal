# State fields

Every field of every `State` in this crate, with its unit and meaning. **Shared**
marks a field with the same name, unit and meaning as `hub-hal`'s (v0.11), so code
written against the Hub reads it unchanged on the View. Units are in field names:
`_ms` milliseconds, `_us` microseconds, `_mdegc` millidegrees Celsius, `_ms2` m/s²,
`_mhz` megahertz, `_hz` hertz, `_deg` degrees. `None` (JSON `null`) always means
"not known", never zero.

Every `State` has `updated_ms` (epoch milliseconds, `util::now_ms`, the last time
it was refreshed) and `error` (`None` when the last refresh succeeded). Events
carry `updated_ms` too: the kernel's timestamp for the event, on the same clock.

## `display::State`

| Field | Type | Unit / values | Meaning | Shared |
| --- | --- | --- | --- | --- |
| `connector` | `Option<String>` | `card0-DSI-1` | DRM connector, as sysfs names it | yes (Hub: `card0-DP-1`) |
| `status` | `Option<String>` | `connected` | connector `status` | |
| `dpms` | `Option<String>` | `On` / `Off` | connector `dpms`, read only | |
| `width`, `height` | `Option<u32>` | pixels | active CRTC mode (debugfs), else preferred mode (sysfs) | |
| `refresh_hz` | `Option<u32>` | Hz | active CRTC vrefresh; `None` without root (no debugfs) | |
| `backlight` | `Option<String>` | path | backlight class device | |
| `brightness` | `Option<u32>` | device units, 0..`brightness_max` | current backlight level | yes |
| `brightness_max` | `Option<u32>` | device units | 4095 here, 100 on the Hub | yes |
| `brightness_floor` | `u32` | device units | lowest level `set_brightness` writes (default 1400, not a hardware limit) | |
| `power` | `Option<String>` | `on` `standby` `suspend` `off` | backlight power (`bl_power`) | yes (Hub: DDC VCP D6) |
| `blanked_until` | `Option<u64>` | epoch ms | end of a `blank()` in progress | yes |
| `updated_ms` | `Option<u64>` | epoch ms | last refresh | |
| `error` | `Option<String>` | | | yes |

`hub-hal` also has `bus`, `last_op_ms` and `detect` (DDC/CI specifics) which have no
meaning here. Straylight uses `bus.is_some()` as "brightness available"; on the View
the equivalent is `brightness.is_some()`.

`display::Panel` (`connector`, `width`, `height`, `internal`, `powered`) and
`display::Shortfall` have `hub-hal`'s shape and meaning; the View's `Panel` adds
`status` and `dpms`, and `covers` accepts a rotated window.

## `orientation::State`

| Field | Type | Unit / values | Meaning |
| --- | --- | --- | --- |
| `path` | `Option<String>` | path | IIO device directory |
| `angle_deg` | `Option<f64>` | degrees, (−180, 180] | θ = atan2(a_x, −a_y): 0 upright portrait, positive clockwise; filtered; held while `flat` |
| `upright` | `Option<&str>` | `portrait` `landscape-cw` `portrait-flipped` `landscape-ccw` | quadrant with 10° hysteresis; held while `flat` |
| `flat` | `bool` | | in-plane gravity below 3 m/s² (clears above 3.5) |
| `in_plane_ms2` | `Option<f64>` | m/s² | filtered in-plane gravity magnitude |
| `gravity_ms2` | `Option<f64>` | m/s² | total magnitude of the last sample |
| `x_ms2`, `y_ms2`, `z_ms2` | `Option<f64>` | m/s² | last sample, device frame, after the mount matrix |
| `raw` | `Option<[i64; 3]>` | counts | last sample as the driver gave it |
| `scale` | `Option<f64>` | m/s² per count | `in_accel_scale` |
| `mount_matrix` | `Option<[f64; 9]>` | | `in_mount_matrix`, row major |
| `last_read_us` | `Option<u64>` | µs | wall time of the last three-axis read |
| `reads` | `u64` | count | samples taken by this handle |

## `switches::State` and `switches::Event`

| Field | Type | Unit / values | Meaning |
| --- | --- | --- | --- |
| `device` | `Option<String>` | `/dev/input/eventN` | `gpio-keys` node |
| `lens_covered` | `Option<bool>` | | camera cover closed |
| `mic_mute_fitted` | `bool` | always `false` | no mute switch on this unit |
| `mic_mute_note` | `&str` | | why |
| Event `switch` | `&str` | `lens_cover` | |
| Event `code` | `u16` | `SW_CAMERA_LENS_COVER` (9) | |
| Event `on` | `bool` | | cover closed |

## `buttons::State` and `buttons::Event`

| Field | Type | Unit / values | Meaning |
| --- | --- | --- | --- |
| `volume_up_device`, `volume_down_device` | `Option<String>` | `/dev/input/eventN` | `gpio-keys`, `pm8941_resin` |
| `volume_up`, `volume_down` | `Option<bool>` | | held down now |
| `power_key_fitted` | `bool` | always `false` | |
| Event `key` | `&str` | `volume_up` `volume_down` | |
| Event `code` | `u16` | 115 / 114 | `KEY_VOLUMEUP` / `KEY_VOLUMEDOWN` |
| Event `pressed` | `bool` | | down (value 1 or 2) |
| Event `repeat` | `bool` | | autorepeat (value 2) |

## `usb::State`

| Field | Type | Unit / values | Meaning |
| --- | --- | --- | --- |
| `gadget` | `Option<String>` | `tsv` | configfs gadget |
| `id` | `Option<String>` | `vid:pid` hex | `1d6b:0104` |
| `functions` | `Vec<String>` | | defined functions |
| `linked` | `Vec<String>` | | functions linked into a configuration |
| `ncm`, `acm`, `adb` | `bool` | | that function is linked |
| `udc` | `Option<String>` | `7000000.usb` | bound controller; `None` when unbound |
| `udc_state` | `Option<String>` | `configured`, `not attached`... | UDC `state` |
| `speed` | `Option<String>` | `high-speed` | UDC `current_speed` |
| `host_connected` | `Option<bool>` | | `udc_state == configured`; `None` with no UDC |
| `interface` | `Option<String>` | `usb0` | NCM interface |
| `operstate` | `Option<String>` | `up`, `down` | |
| `addresses` | `Vec<String>` | `a.b.c.d/prefix` | IPv4 only |

## `thermal::State`

| Field | Type | Unit / values | Meaning |
| --- | --- | --- | --- |
| `zones[].zone` | `String` | `thermal_zoneN` | probe order; prefer `type` |
| `zones[].type` | `String` | `cpu0-thermal`... | |
| `zones[].temp_mdegc` | `Option<i64>` | m°C | |
| `zones[].enabled` | `Option<bool>` | | zone `mode` |
| `zones[].trips[]` | `type`, `temp_mdegc` | m°C | trip points as the kernel has them |
| `max_temp_mdegc` | `Option<i64>` | m°C | hottest zone |
| `policies[].policy` | `String` | `policy0` | |
| `policies[].cpus` | `Vec<u32>` | | related CPUs |
| `policies[].driver`, `.governor` | `Option<String>` | | |
| `policies[].cur_mhz`, `.min_mhz`, `.max_mhz` | `Option<f64>` | MHz | current and governor limits |
| `policies[].hw_min_mhz`, `.hw_max_mhz` | `Option<f64>` | MHz | `cpuinfo_*_freq` |
| `policies[].available_mhz` | `Vec<f64>` | MHz | |
| `cpus[].cpu`, `.online`, `.cur_mhz` | | MHz | per core; `cur_mhz` `None` without cpufreq |

## Shared with `hub-hal`, for modules not in this crate yet

When audio and microphone support lands, it takes `hub-hal`'s names exactly:
`audio`: `volume`, `muted`, `source_volume`, `source_muted`, `sink`, `source`,
`playing`, `error`; `mic`: `channels[].dbfs`, `channels[].peak_dbfs`, `monitoring`,
`recording`, `error`. Presence (the VCNL4200, once it is safe to enable) takes the
shared meaning `present` / `kind = "proximity"` / `raw`, with device detail beside it.

## Timestamps: notes for aligning `hub-hal` later

This crate uses one name, `updated_ms`, everywhere. `hub-hal` v0.11 does not have a
general refresh timestamp, and its time fields are named by what they mark:

| `hub-hal` field | Module | Meaning |
| --- | --- | --- |
| `blanked_until` | `display` | epoch ms a blank ends (kept here with the same meaning) |
| `last_op_ms` | `display` | duration of the last ddcutil call, not a timestamp |
| `last_read_ms` | `prox` | duration of the last read, not a timestamp |
| `last_change` | `prox` | epoch ms of the last raw value change, no unit suffix |
| `stuck_since` | `prox` | epoch ms, no unit suffix |
| `history[].0` | `prox` | epoch ms |

Adding `updated_ms` to `hub-hal`'s `State`s, and `_ms` suffixes to `last_change` and
`stuck_since`, would make the two crates line up. `hub-hal` is not changed here.

## What the Straylight panel reads from `hub-hal` today

From `src/main.rs` of `straylight-panel` (pinned to `hub-hal` v0.9.0):

| Module | Used |
| --- | --- |
| `display` | `Display::new`, `start`, `state().bus`, `.brightness`, `.error`, `set_brightness(u32)`; `display::panel()` -> `.connector`, `.width`, `.height`, `Panel::shortfall` |
| `audio` | `state().volume`, `.muted`, `.source_volume`, `.source_muted`, `.error`; `set_volume`, `set_mute`, `set_source_volume`, `set_source_mute` |
| `mic` | `keep_alive`, `state().channels[].dbfs` |
| `media`, `video`, `preview` | device lists and camera previews (no View equivalent yet) |
| `util` | `run` |

Two differences a View port of the panel has to handle: brightness is out of
`brightness_max` (4095), not a fixed 100, so a 0-100 slider must scale by it; and
"brightness available" is `brightness.is_some()`, not `bus.is_some()`.
