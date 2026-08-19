//! Drives one TCP connection's worth of RTSP request/response exchanges against an
//! [`RtspSession`], reading and writing a real socket.
//!
//! Plaintext throughout: classic AirPlay encrypts the *audio*, not the control channel. What a
//! connection owns beyond the exchanges themselves is the UDP ports its `SETUP` hands out, the
//! task decoding audio once `RECORD` starts, and — when the sender said how to reach it back —
//! the `dacp` poll that keeps its now-playing state fresh.

use std::sync::Arc;

use log::{debug, info};
use tokio::{
    io::{self, AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
};

use super::{Request, RtpPorts, RtspAction, RtspSession, handle_request, message::ParseError};
use crate::{AirplayEvent, device_id::DeviceId, sink::SinkConfig};

const READ_CHUNK: usize = 4096;

/// Aborts a spawned task when dropped, so a device disconnecting mid-poll (rather than via
/// `TEARDOWN`, which aborts it explicitly) doesn't leave the DACP poll loop running forever. Not
/// extended to `recording_task`, a pre-existing gap. A few lines rather than a `tokio-util`
/// dependency for its `AbortOnDropHandle`.
struct AbortOnDrop(Option<tokio::task::JoinHandle<()>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

/// Sends `AirplayEvent::SessionEnded` for a connection when dropped — see that variant's docs for
/// what a consumer is expected to do with it, and `serve_connection` for why this is a guard.
struct SessionEndGuard {
    connection: u64,
    events: tokio::sync::mpsc::UnboundedSender<AirplayEvent>,
}

impl Drop for SessionEndGuard {
    fn drop(&mut self) {
        let _ = self.events.send(AirplayEvent::SessionEnded {
            connection: self.connection,
        });
    }
}

/// Reads and responds to RTSP requests on `stream` until the peer closes the connection or a
/// framing error occurs. Binds this connection's own event/control/data UDP ports up front (see
/// [`reserve_rtp_ports`]) so `SETUP` has real port numbers to hand back; the `data` socket is
/// moved into an `rtp::receive_loop` task on `RECORD` (see [`RtspAction`]) and never comes back
/// — a second `RECORD` on the same connection needs a fresh `SETUP` first, per `rtsp`'s own
/// module docs.
/// What every connection shares with the server that accepted it. Cheap to clone — each field is
/// already a handle.
#[derive(Clone)]
pub(crate) struct ConnectionContext {
    pub(crate) device_id: Arc<DeviceId>,
    pub(crate) device_name: Arc<str>,
    pub(crate) sink_config: SinkConfig,
    pub(crate) events: tokio::sync::mpsc::UnboundedSender<AirplayEvent>,
    /// The list `--airplay-bind-ip` restricts this crate's mDNS advertising to, reused to
    /// restrict the DACP resolver's interfaces — see `dacp::resolve_port`.
    pub(crate) bind_ip: Arc<Vec<std::net::IpAddr>>,
    /// The receiver's own output gain, shared by every connection and by the UDP API — see
    /// `AirplayControl`. A `Sender` rather than a `Receiver` because a sender's own
    /// `SET_PARAMETER volume:` sets it too.
    pub(crate) volume: Arc<tokio::sync::watch::Sender<f64>>,
}

pub(crate) async fn serve_connection(
    mut stream: TcpStream,
    // This connection's number, from `AirplayServer::spawn`'s accept loop — stamped on every
    // `AirplayEvent` this connection sends, see that enum's docs.
    connection: u64,
    context: ConnectionContext,
) -> io::Result<()> {
    let ConnectionContext {
        device_id,
        device_name,
        sink_config,
        events: airplay_events,
        bind_ip,
        volume,
    } = context;
    #[cfg(not(feature = "remote-control"))]
    let _ = &bind_ip;
    let mut session = RtspSession::new();
    let mut plain_buf = Vec::new();
    let mut chunk = [0u8; READ_CHUNK];
    // A sender's DACP server is at its RTSP peer address; only the port needs resolving. Kept
    // whole rather than as an `IpAddr` because an IPv6 peer's scope id is part of reaching it
    // back (`dacp::DacpTarget::scope_id`).
    let peer_addr = stream.peer_addr().ok();
    // Announced before anything is parsed: this is the only address a sender that never resolves
    // a DACP port ever offers, and the fork's UDP API routes on it (`AirplayEvent::SessionStarted`).
    if let Some(peer) = peer_addr {
        let _ = airplay_events.send(AirplayEvent::SessionStarted {
            connection,
            peer: peer.ip(),
        });
    }

    let local_ip = stream
        .local_addr()
        .map(|addr| addr.ip())
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    let (ports, (_event_socket, _control_socket, data_socket)) =
        reserve_rtp_ports(local_ip).await?;
    let mut data_socket = Some(data_socket);
    let mut recording_task: Option<tokio::task::JoinHandle<()>> = None;
    // The DACP resolve+poll task, once a request carries `Active-Remote`/`DACP-ID`.
    #[cfg_attr(not(feature = "remote-control"), allow(unused_mut))]
    let mut dacp_task = AbortOnDrop(None);
    // The pair the running `dacp_task` was started for, so a repeat of the same headers doesn't
    // restart it — see the `DacpInfoCaptured` arm below.
    #[cfg(feature = "remote-control")]
    let mut last_dacp_info: Option<(String, String)> = None;
    // Announces the end of this session however this function ends — clean `return`, framing
    // error, or the peer vanishing. A guard because the interesting case is the one that doesn't
    // reach the bottom: a sender that loses power never sends `TEARDOWN`.
    let _session_end = SessionEndGuard {
        connection,
        events: airplay_events.clone(),
    };

    loop {
        let Some(request) = read_request(&mut stream, &mut plain_buf, &mut chunk).await? else {
            return Ok(()); // peer closed the connection cleanly
        };

        // `OPTIONS` is a keepalive a sender repeats every couple of seconds for as long as a
        // session lasts; logging it at `info` would drown out everything else in `journalctl`.
        if request.method == "OPTIONS" {
            debug!("airplay: {} {}", request.method, request.uri);
        } else {
            info!("airplay: {} {}", request.method, request.uri);
        }
        debug!(
            "airplay: headers: {:?}",
            request
                .headers
                .iter()
                .map(|(name, value)| format!("{name}: {value}"))
                .collect::<Vec<_>>()
        );
        let (response, action) =
            handle_request(&mut session, &device_id, &device_name, &ports, &request);
        stream.write_all(&response.encode()).await?;

        match action {
            Some(RtspAction::StartRecording) => {
                let mut started_playing = false;
                debug!(
                    "airplay: starting the audio receive loop on UDP port {}",
                    ports.data_port
                );
                if let Some(socket) = data_socket.take() {
                    if let Some(audio) = session.take_audio_session() {
                        // One task: decrypt, decode and write to the sink all happen inline in
                        // `legacy::receive_loop`, since a classic stream's format is fixed by
                        // `ANNOUNCE` and never changes mid-stream.
                        recording_task = Some(tokio::spawn(crate::legacy::receive_loop(
                            socket,
                            audio,
                            sink_config.clone(),
                            volume.subscribe(),
                        )));
                        started_playing = true;
                    }
                }

                // Audio is really flowing — the one "AirPlay is the active source" signal that
                // can't fail, and what shairport-sync itself uses
                // (`metadata_store.player_thread_active`).
                if started_playing {
                    let _ = airplay_events.send(AirplayEvent::PlaybackChanged {
                        connection,
                        is_playing: true,
                        // Audio starting says nothing about where in the track it is; the
                        // sender's own `progress:` push follows and says that.
                        position_ms: 0,
                    });
                }
            }
            Some(RtspAction::StopRecording) => {
                if let Some(task) = recording_task.take() {
                    task.abort();
                }
                if let Some(task) = dacp_task.0.take() {
                    task.abort();
                }
                let _ = airplay_events.send(AirplayEvent::PlaybackChanged {
                    connection,
                    is_playing: false,
                    position_ms: 0,
                });
                // The connection itself usually lives on for a moment after `TEARDOWN` (and the
                // guard would report this anyway), but control has to go back the instant
                // playback does, not whenever the socket happens to close.
                let _ = airplay_events.send(AirplayEvent::SessionEnded { connection });
            }
            Some(RtspAction::SetVolume { gain, percent }) => {
                // The sender's own slider. Applied to this receiver's output — an AirPlay sender
                // expects the receiver to attenuate, it does not send pre-scaled audio — and
                // reported onward so a client of the UDP API sees the same number the phone
                // shows.
                let _ = volume.send(gain);
                let _ = airplay_events.send(AirplayEvent::VolumeChanged {
                    connection,
                    percent,
                });
            }
            #[cfg(feature = "remote-control")]
            Some(RtspAction::DacpInfoCaptured {
                active_remote,
                dacp_id,
            }) => {
                // A sender repeats these headers on every `GET /info` and again on `SETUP`, so
                // most captures say nothing new. Acting on each would start a second resolve+poll
                // and leave the first running, since dropping a `JoinHandle` doesn't stop its
                // task. shairport-sync likewise keeps the port it already discovered when
                // `active_remote_id` is unchanged. A *finished* task means its resolve failed, so
                // a repeat capture is a free retry.
                let captured = (active_remote, dacp_id);
                let running = dacp_task.0.as_ref().is_some_and(|task| !task.is_finished());
                if last_dacp_info.as_ref() == Some(&captured) && running {
                    debug!("airplay: DACP info unchanged, keeping the running poll");
                } else if let Some(peer) = peer_addr {
                    if let Some(previous) = dacp_task.0.take() {
                        previous.abort();
                    }
                    let events = airplay_events.clone();
                    let bind_ip = bind_ip.clone();
                    dacp_task.0 = Some(tokio::spawn(resolve_and_poll_dacp(
                        DacpSender {
                            connection,
                            host: peer.ip(),
                            scope_id: match peer {
                                std::net::SocketAddr::V6(peer) => peer.scope_id(),
                                std::net::SocketAddr::V4(_) => 0,
                            },
                            active_remote: captured.0.clone(),
                            dacp_id: captured.1.clone(),
                            machine_number: crate::dacp::machine_number(
                                &device_id.colon_separated(),
                            ),
                        },
                        events,
                        bind_ip,
                    )));
                    last_dacp_info = Some(captured);
                }
            }
            #[cfg(not(feature = "remote-control"))]
            Some(RtspAction::DacpInfoCaptured { .. }) => {}
            // The push path: each request becomes its event, with nothing to resolve or poll.
            Some(RtspAction::MetadataReported(metadata)) => {
                let _ = airplay_events.send(AirplayEvent::TrackChanged {
                    connection,
                    title: metadata.title.unwrap_or_default(),
                    artist: metadata.artist.unwrap_or_default(),
                    album: metadata.album.unwrap_or_default(),
                    album_artist: metadata.album_artist.unwrap_or_default(),
                    duration_ms: metadata.duration_ms.unwrap_or(0),
                    // The picture is a separate request, still on its way — see
                    // `AirplayEvent::CoverChanged`.
                    cover: None,
                });
            }
            Some(RtspAction::CoverReported { bytes, mime }) => {
                let _ = airplay_events.send(AirplayEvent::CoverChanged {
                    connection,
                    bytes,
                    mime,
                });
            }
            Some(RtspAction::ProgressReported(progress)) => {
                let _ = airplay_events.send(AirplayEvent::ProgressChanged {
                    connection,
                    position_ms: progress.position_ms,
                    duration_ms: progress.duration_ms,
                });
            }
            None => {}
        }
    }
}

/// Who to poll, minus the one thing that still has to be looked up: the DACP port. Everything
/// here is known the moment a request carries `Active-Remote`/`DACP-ID` (`rtsp::dacp_info`).
#[cfg(feature = "remote-control")]
struct DacpSender {
    connection: u64,
    host: std::net::IpAddr,
    /// The peer's *port* is its RTSP source port and means nothing to DACP — only the scope
    /// travels with the address, and only for link-local IPv6 (`dacp::DacpTarget::scope_id`).
    scope_id: u32,
    active_remote: String,
    dacp_id: String,
    /// Ours, not the sender's — see `dacp::DacpTarget::machine_number`.
    machine_number: u64,
}

/// How often to poll `playstatusupdate` and the volume once a DACP endpoint is known.
///
/// One second, which is shairport-sync's own default for both of its intervals
/// (`config.scan_interval_when_active` and `..._when_inactive`, both `1` in `shairport.c`) — it
/// does not slow down when nothing is playing, and neither does this. Since neither implements
/// long polling (`dacp::poll_status`'s docs), the interval *is* the responsiveness of every event
/// this loop produces, not a keepalive: at the previous three seconds, a pause on the phone could
/// take that long to reach a client.
#[cfg(feature = "remote-control")]
const DACP_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Resolves `dacp_id`'s port, announces it, then polls until aborted, pushing events only when a
/// polled value actually changes.
#[cfg(feature = "remote-control")]
async fn resolve_and_poll_dacp(
    sender: DacpSender,
    events: tokio::sync::mpsc::UnboundedSender<AirplayEvent>,
    bind_ip: std::sync::Arc<Vec<std::net::IpAddr>>,
) {
    let DacpSender {
        connection,
        host,
        scope_id,
        active_remote,
        dacp_id,
        machine_number,
    } = sender;

    let Some(port) = crate::dacp::resolve_port(&dacp_id, &bind_ip).await else {
        return;
    };
    // Same address family as `host`: binding an IPv4 socket to an IPv6 local address is an
    // error, and `bind_ip` may list both.
    let local_bind = bind_ip
        .iter()
        .find(|ip| ip.is_ipv4() == host.is_ipv4())
        .copied();
    let target = crate::dacp::DacpTarget {
        host,
        port,
        active_remote: active_remote.clone(),
        local_bind,
        machine_number,
        scope_id,
    };
    let _ = events.send(AirplayEvent::DacpAvailable {
        connection,
        host,
        port,
        active_remote,
        machine_number,
        local_bind,
        scope_id,
    });

    let mut last_playing = None;
    let mut last_track: Option<(String, String, String)> = None;
    let mut last_volume = None;

    loop {
        // Note there is no revision number to carry between iterations: every poll asks for
        // revision 1, on purpose — see `dacp::poll_status`'s docs.
        let Some(status) = crate::dacp::poll_status(&target).await else {
            debug!("airplay: DACP playstatusupdate poll failed");
            tokio::time::sleep(DACP_POLL_INTERVAL).await;
            continue;
        };
        debug!("airplay: DACP playstatusupdate poll -> {status:?}");

        if let Some(playing) = status.playing {
            if Some(playing) != last_playing {
                last_playing = Some(playing);
                let _ = events.send(AirplayEvent::PlaybackChanged {
                    connection,
                    is_playing: playing,
                    // On the change itself rather than every tick: a subscriber extrapolates
                    // position from the last value it got, so what matters is the moments where
                    // that extrapolation would go wrong.
                    position_ms: status.position_ms.unwrap_or(0),
                });
            }
        }

        let track = (
            status.title.clone().unwrap_or_default(),
            status.artist.clone().unwrap_or_default(),
            status.album.clone().unwrap_or_default(),
        );
        let track_known =
            status.title.is_some() || status.artist.is_some() || status.album.is_some();
        if track_known && Some(&track) != last_track.as_ref() {
            last_track = Some(track.clone());
            let cover = crate::dacp::fetch_artwork(&target, 320, 320)
                .await
                .map(|bytes| {
                    let mime = crate::dmap::sniff_image_mime(&bytes);
                    (bytes, mime)
                });
            let _ = events.send(AirplayEvent::TrackChanged {
                connection,
                title: track.0,
                artist: track.1,
                album: track.2,
                // `playstatusupdate` has no album-artist tag; `assl` only comes from the push
                // path. Empty rather than the artist repeated.
                album_artist: String::new(),
                duration_ms: status.duration_ms.unwrap_or(0),
                cover,
            });
        }

        // Same tick as the status above, as shairport-sync's monitor polls both every scan —
        // but not gated on it. (shairport-sync gates the other way round, asking for
        // `playstatusupdate` only after the volume probe returns 200.) A sender that answers one
        // and not the other is real, and neither should blind the other.
        if let Some(percent) = crate::dacp::get_volume(&target).await {
            if Some(percent) != last_volume {
                last_volume = Some(percent);
                let _ = events.send(AirplayEvent::VolumeChanged {
                    connection,
                    percent,
                });
            }
        }

        tokio::time::sleep(DACP_POLL_INTERVAL).await;
    }
}
/// Reads off `stream` into `buf` until one full request can be parsed out of it, returning that
/// request and leaving any bytes belonging to a subsequent (pipelined) request in `buf` for the
/// next call. Returns `Ok(None)` on a clean EOF with no partial request pending.
async fn read_request(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    chunk: &mut [u8],
) -> io::Result<Option<Request>> {
    loop {
        match Request::parse(buf) {
            Ok((request, consumed)) => {
                buf.drain(..consumed);
                return Ok(Some(request));
            }
            Err(ParseError::Empty | ParseError::BodyTooShort) => {
                let n = stream.read(chunk).await?;
                if n == 0 {
                    return if buf.is_empty() {
                        Ok(None)
                    } else {
                        Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "connection closed mid-request",
                        ))
                    };
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(err) => return Err(io::Error::new(io::ErrorKind::InvalidData, err.to_string())),
        }
    }
}
/// Binds three ephemeral UDP sockets (event/control/data) and reports the ports the OS picked.
/// The sockets themselves must be returned alongside the ports and kept alive for the
/// connection's lifetime — dropping them would let the OS hand the port to something else while
/// `SETUP` responses are still advertising it as reserved.
async fn reserve_rtp_ports(
    local_ip: std::net::IpAddr,
) -> io::Result<(RtpPorts, (UdpSocket, UdpSocket, UdpSocket))> {
    let event = UdpSocket::bind("0.0.0.0:0").await?;
    let control = UdpSocket::bind("0.0.0.0:0").await?;
    let data = UdpSocket::bind("0.0.0.0:0").await?;
    let ports = RtpPorts {
        event_port: event.local_addr()?.port(),
        control_port: control.local_addr()?.port(),
        data_port: data.local_addr()?.port(),
        local_ip,
    };
    Ok((ports, (event, control, data)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    fn test_sink_config() -> SinkConfig {
        SinkConfig {
            backend: librespot_playback::audio_backend::find(Some("pipe".to_string()))
                .expect("the pipe backend is always compiled in"),
            device: Some("/dev/null".to_string()),
            format: librespot_playback::config::AudioFormat::default(),
        }
    }

    /// A real connection on a loopback port, driven the way a sender drives one.
    async fn start_server() -> (
        std::net::SocketAddr,
        tokio::task::JoinHandle<io::Result<()>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_connection(
                stream,
                1, // the connection number a real accept loop would have handed out
                ConnectionContext {
                    device_id: Arc::new(crate::device_id::DeviceId::from_name("test-device")),
                    device_name: Arc::from("test-device"),
                    sink_config: test_sink_config(),
                    events: event_tx,
                    bind_ip: Arc::new(Vec::new()),
                    volume: Arc::new(tokio::sync::watch::channel(1.0).0),
                },
            )
            .await
        });

        (addr, server)
    }

    /// The first thing a real sender does, and the response it needs before it will go on.
    #[tokio::test]
    async fn serves_options_over_a_real_socket() {
        let (addr, server) = start_server().await;
        let mut client = TcpStream::connect(addr).await.unwrap();

        client
            .write_all(b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n")
            .await
            .unwrap();

        let mut buf = [0u8; 1024];
        let read = client.read(&mut buf).await.unwrap();
        let response = String::from_utf8_lossy(&buf[..read]).into_owned();
        assert!(response.starts_with("RTSP/1.0 200 OK"), "got {response:?}");
        assert!(response.contains("CSeq: 1"));

        drop(client);
        let _ = server.await;
    }
}
