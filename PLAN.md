# bevy_iroh — what is still to do

Everything the original plan set out (rooms, replication, messages, voice, video, browsers,
screen sharing) is in the crate; the README says what is there and how it works. This is the
list of what is not, roughly in the order it is worth doing. Dates are when the item was
written down.

## Run the macOS and Windows backends on real machines (2026-09-10)

`media::desktop::{macos, windows}` (ScreenCaptureKit, Desktop Duplication) and
`media::camera::{macos, windows}` (AVFoundation, Media Foundation) are ported from robot2's
substrate and type-check in CI (a real macOS runner, a mingw cross clippy for Windows), but
neither has run. Things to look at first: the direction of the quarter turns on a rotated
Windows monitor (follows Microsoft's duplication sample, unseen); the Screen Recording
relaunch on macOS; whether the trimmed `objc2-*` feature lists are enough at link time.
Reference code: `/run/media/rouan/linux_backup/robot2/crates/substrate/src/plugin/display_capture`
and `.../sceneobjects/utilities/webcam/plugin/v4l2`.

## A DMA-BUF straight to the local screen plane (2026-09-10)

The local preview of a shared screen sits four to six display frames behind, and on NVIDIA
the compositor's shared-memory readback is jittery before bevy_iroh sees a byte. Measured on
this machine (RTX 4090, 4K, Hyprland): our own stages are about 10 ms per frame (copy 1.8 ms,
box-filter scale 5 ms, swizzle 1 ms, encode 2 ms median), so the lag is structural, not
compute. Two steps:

1. Cheap first: pull and scale the capture on its own cadence so the preview refreshes at the
   capture rate instead of the encoder's 30 fps loop, and hand the preview image over as
   BGRA so the swizzle goes. `cargo run --example screen_stats` prints what the compositor
   delivers, which says whether the readback is the bottleneck.
2. Then negotiate DMA-BUF from PipeWire (libspa has the modifier property and buffer type),
   import each pool buffer once through wgpu's Vulkan hal (`as_hal`,
   `create_texture_from_hal`; wgpu-hal already enables the external-memory extension), copy it
   into one persistent texture and release the PipeWire buffer at once (NVIDIA does not
   honour implicit sync), and point the entity's `GpuImage` at that texture from a render-app
   system. Read mip level one back for the encoder, which is 1080p from a 4K monitor with no
   CPU scaler. Substrate has the Vulkan side, modifier query and render-world seam in
   `display_capture/linux/{vulkan.rs,mod.rs}` and `livetexture/mod.rs`; it allocated and
   exported, here we import, so the Vulkan module becomes the import variant.
   `media/v4l2/dmabuf.rs` already imports camera DMA-BUFs the same way. Keep shared memory as
   the fallback.

## Substrate onto bevy_iroh

robot2's `bevy-iroh` branch (`crates/substrate/MIGRATION.md`): substrate depends on this
crate; `substrate-net` and `substrate-ticket` go; containment inserts `Shared + InRoom`;
scene objects collapse into one replicated `Body { kind, bytes }` component; players and
voice become ordinary shared entities; media keeps its MoQ handler through
`IrohPlugin::with_protocol`; the registry client stays in substrate.

## Echo cancellation on desktops

Browsers have it from Web Audio; native peers need headphones. Options are a WebRTC AEC port
or speex's; either needs the playback signal fed back into the capture path in
`media/audio.rs`, which already knows the buffering rules.

## A hardware encoder path

openh264 is fine at 1080p on a workstation (2 ms median, 43 ms worst on keyframes) and not
on a laptop sharing 4K. VA-API on Linux and NVENC can take the DMA-BUF from the item above
directly; VideoToolbox on macOS and Media Foundation on Windows take the platform's own
frames. Behind the same `VideoSource` to encoder seam, chosen per platform.

## Not doing

Simulcast, the relay tree for a thousand viewers, and a server of any kind. Meetings are
flat and everyone is direct; `VideoFeed::rendition` exists so the wire does not change if
renditions land later.
