//! Linux: the ScreenCast portal chooses, PipeWire delivers.
//!
//! Two threads. The first owns a small tokio runtime for the D-Bus conversation with
//! `xdg-desktop-portal`: create a session, select sources (this is where the compositor's
//! picker appears and the call waits for the user), start, and open the PipeWire remote. It
//! keeps the session alive until the capture is dropped, then closes it. The second runs
//! PipeWire's main loop on the file descriptor the portal handed over, with one input stream
//! on the node the portal named, and copies each buffer out into a tightly packed frame.
//!
//! Only shared-memory buffers are negotiated: the format offer carries no modifier, so the
//! compositor does not try DMA-BUF, and `MAP_BUFFERS` maps what it sends. The picker itself
//! is the portal's, so which monitor or window, and whether both may be offered, is the
//! backend's policy and not ours.

use std::{
    sync::{Arc, Once},
    time::Duration,
};

use ashpd::desktop::{
    CreateSessionOptions, PersistMode, Session,
    screencast::{
        CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
        StartCastOptions,
    },
};
use futures_util::StreamExt;
use pipewire as pw;
use pw::spa::{
    self,
    param::video::{VideoFormat, VideoInfoRaw},
    pod::Pod,
};

use super::{DesktopConfig, Format, Shared, State, unpad_rows};

static INIT: Once = Once::new();

pub(crate) fn spawn(config: DesktopConfig, shared: Arc<Shared>) -> Result<(), String> {
    std::thread::Builder::new()
        .name("desktop-portal".into())
        .spawn(move || portal_thread(config, shared))
        .map_err(|e| format!("could not start the portal thread: {e}"))?;
    Ok(())
}

fn portal_thread(config: DesktopConfig, shared: Arc<Shared>) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            shared.finish(State::Failed(format!("no runtime for the portal: {e}")));
            return;
        }
    };
    runtime.block_on(async {
        let (proxy, session, node, fd) = match handshake(&config, &shared).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("bevy_iroh: screen: {e}");
                shared.finish(State::Failed(e));
                return;
            }
        };
        let pw_shared = shared.clone();
        let capture = std::thread::Builder::new()
            .name("desktop-pipewire".into())
            .spawn(move || {
                if let Err(e) = pipewire_thread(fd, node, pw_shared.clone()) {
                    tracing::warn!("bevy_iroh: screen: pipewire: {e}");
                    pw_shared.finish(State::Failed(e));
                }
            });
        if let Err(e) = capture {
            shared.finish(State::Failed(format!("could not start the capture thread: {e}")));
            return;
        }
        // The session stays open while the capture lives, and closes when it is dropped or
        // the compositor ends the share.
        let mut closed = match session.receive_closed().await {
            Ok(stream) => Some(stream),
            Err(e) => {
                tracing::debug!("bevy_iroh: screen: no Closed signal: {e}");
                None
            }
        };
        loop {
            let tick = tokio::time::sleep(Duration::from_millis(100));
            let ended = async {
                match closed.as_mut() {
                    Some(stream) => stream.next().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = tick => {
                    if shared.stopped() || matches!(shared.state(), State::Ended | State::Failed(_)) {
                        break;
                    }
                }
                _ = ended => {
                    tracing::info!("bevy_iroh: screen: the share was ended from the desktop side");
                    shared.finish(State::Ended);
                    break;
                }
            }
        }
        drop(proxy);
        if let Err(e) = session.close().await {
            tracing::debug!("bevy_iroh: screen: closing the portal session: {e}");
        }
    });
}

type Handshake = (Screencast, Session<Screencast>, u32, std::os::fd::OwnedFd);

async fn handshake(config: &DesktopConfig, shared: &Shared) -> Result<Handshake, String> {
    let proxy = Screencast::new()
        .await
        .map_err(|e| format!("no ScreenCast portal: {e}"))?;
    let session = proxy
        .create_session(CreateSessionOptions::default())
        .await
        .map_err(|e| format!("portal session: {e}"))?;
    let cursor = if config.cursor {
        CursorMode::Embedded
    } else {
        CursorMode::Hidden
    };
    let cursor = match proxy.available_cursor_modes().await {
        Ok(modes) if !modes.contains(cursor) => CursorMode::Hidden,
        _ => cursor,
    };
    let sources = match proxy.available_source_types().await {
        Ok(types) => types & (SourceType::Monitor | SourceType::Window),
        Err(_) => SourceType::Monitor.into(),
    };
    let options = SelectSourcesOptions::default()
        .set_cursor_mode(cursor)
        .set_sources(sources)
        .set_multiple(false)
        .set_persist_mode(PersistMode::ExplicitlyRevoked)
        .set_restore_token(config.restore_token.as_deref());
    proxy
        .select_sources(&session, options)
        .await
        .map_err(|e| format!("select sources: {e}"))?
        .response()
        .map_err(|e| format!("select sources: {e}"))?;
    if shared.stopped() {
        return Err("stopped while the picker was up".into());
    }
    let streams = proxy
        .start(&session, None, StartCastOptions::default())
        .await
        .map_err(|e| format!("start: {e}"))?
        .response()
        .map_err(|e| format!("start: {e}"))?;
    shared.set_restore_token(streams.restore_token().map(str::to_owned));
    let stream = streams
        .streams()
        .first()
        .ok_or("the portal started no stream")?;
    let node = stream.pipe_wire_node_id();
    tracing::info!(
        "bevy_iroh: screen: sharing {:?} {:?} on PipeWire node {node}",
        stream.source_type(),
        stream.size()
    );
    let fd = proxy
        .open_pipe_wire_remote(&session, OpenPipeWireRemoteOptions::default())
        .await
        .map_err(|e| format!("PipeWire remote: {e}"))?;
    Ok((proxy, session, node, fd))
}

struct Negotiated {
    info: VideoInfoRaw,
    format: Option<Format>,
    shared: Arc<Shared>,
    /// Buffer reused across frames when nobody has taken the last one yet.
    scratch: Vec<u8>,
}

fn pipewire_thread(fd: std::os::fd::OwnedFd, node: u32, shared: Arc<Shared>) -> Result<(), String> {
    INIT.call_once(pw::init);
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| format!("main loop: {e}"))?;
    let context =
        pw::context::ContextRc::new(&mainloop, None).map_err(|e| format!("context: {e}"))?;
    let core = context
        .connect_fd_rc(fd, None)
        .map_err(|e| format!("connect: {e}"))?;
    let stream = pw::stream::StreamBox::new(
        &core,
        "bevy_iroh",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| format!("stream: {e}"))?;

    let data = Negotiated {
        info: VideoInfoRaw::default(),
        format: None,
        shared: shared.clone(),
        scratch: Vec::new(),
    };
    let state_shared = shared.clone();
    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(move |_, _, old, new| {
            tracing::debug!("bevy_iroh: screen: stream {old:?} -> {new:?}");
            match new {
                pw::stream::StreamState::Error(e) => {
                    state_shared.finish(State::Failed(format!("stream: {e}")));
                }
                pw::stream::StreamState::Unconnected
                    if matches!(
                        old,
                        pw::stream::StreamState::Streaming | pw::stream::StreamState::Paused
                    ) =>
                {
                    state_shared.finish(State::Ended);
                }
                _ => {}
            }
        })
        .param_changed(|_, data, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = spa::param::format_utils::parse_format(param)
            else {
                return;
            };
            if media_type != spa::param::format::MediaType::Video
                || media_subtype != spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            if data.info.parse(param).is_err() {
                return;
            }
            let format = data.info.format();
            data.format = if format == VideoFormat::BGRx || format == VideoFormat::BGRA {
                Some(Format::Bgra)
            } else if format == VideoFormat::RGBx || format == VideoFormat::RGBA {
                Some(Format::Rgba)
            } else {
                None
            };
            let size = data.info.size();
            tracing::info!(
                "bevy_iroh: screen: {}x{} {:?} at {}/{} fps",
                size.width,
                size.height,
                format,
                data.info.framerate().num,
                data.info.framerate().denom
            );
        })
        .process(|stream, data| {
            // Take everything queued and keep the newest: a frame that waited in the queue
            // while we were busy is latency, not information.
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            while let Some(newer) = stream.dequeue_buffer() {
                buffer = newer;
            }
            let Some(format) = data.format else { return };
            let size = data.info.size();
            let datas = buffer.datas_mut();
            let Some(first) = datas.first_mut() else {
                return;
            };
            let (len, stride, offset, corrupted) = {
                let chunk = first.chunk();
                (
                    chunk.size() as usize,
                    chunk.stride().max(0) as usize,
                    chunk.offset() as usize,
                    chunk.flags().contains(spa::buffer::ChunkFlags::CORRUPTED),
                )
            };
            if len == 0 || corrupted {
                return;
            }
            let Some(bytes) = first.data() else { return };
            let bytes = &bytes[offset.min(bytes.len())..];
            let stride = if stride == 0 {
                size.width as usize * 4
            } else {
                stride
            };
            let mut out = std::mem::take(&mut data.scratch);
            let out_len = size.width as usize * size.height as usize * 4;
            if out.capacity() < out_len {
                out = data.shared.buffer(out_len);
            }
            unpad_rows(
                &bytes[..len.min(bytes.len())],
                size.width,
                size.height,
                stride,
                &mut out,
            );
            data.shared.publish(size.width, size.height, format, out);
            data.scratch = data.shared.buffer(out_len);
        })
        .register()
        .map_err(|e| format!("listener: {e}"))?;

    let obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle {
                width: 1280,
                height: 720
            },
            spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            spa::utils::Rectangle {
                width: 16384,
                height: 16384
            }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 30, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );
    let values: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map_err(|e| format!("format pod: {e:?}"))?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).ok_or("format pod")?];
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| format!("connect stream: {e}"))?;

    // The loop runs until the capture is dropped or the share ends.
    let quit_loop = mainloop.downgrade();
    let quit_shared = shared.clone();
    let timer = mainloop.loop_().add_timer(move |_| {
        let done =
            quit_shared.stopped() || matches!(quit_shared.state(), State::Ended | State::Failed(_));
        if done && let Some(l) = quit_loop.upgrade() {
            l.quit();
        }
    });
    timer
        .update_timer(
            Some(Duration::from_millis(100)),
            Some(Duration::from_millis(100)),
        )
        .into_result()
        .map_err(|e| format!("timer: {e}"))?;
    mainloop.run();
    let _ = stream.disconnect();
    Ok(())
}
