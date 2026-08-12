//! AirPlay receiver.
//!
//! Classic AirPlay (AirPlay 1 / RAOP), which is what a modern sender falls back to when a
//! receiver advertises itself as one — and the only generation that lets a receiver control the
//! sender at all: it puts `Active-Remote`/`DACP-ID` on its requests, and those are what `dacp`
//! turns into pause, resume, next and volume. AirPlay 2 offers no such channel to a receiver,
//! which is an unsolved problem in every open implementation, shairport-sync included.
//!
//! What runs: `discovery` advertises `_raop._tcp` over mDNS; `rtsp` answers a sender's
//! `OPTIONS`/`ANNOUNCE`/`SETUP`/`RECORD`/`SET_PARAMETER` on a real TCP accept loop, including
//! the `Apple-Challenge` a sender demands proof of before streaming; `legacy` decrypts the
//! AES-CBC audio, `codec` decodes the ALAC, and `sink` hands the PCM to a real
//! `librespot_playback` backend — reusing the `--backend`/`--device`/`--format` picked for
//! Spotify Connect, as an independent `Sink` instance rather than a shared or arbitrated one.
//! `dmap` reads the metadata, cover art and position a sender pushes.
//!
//! Everything it plays and everything a client can do to it is reported through
//! [`AirplayEvent`], which the fork's `ApiServer` (`src/server.rs`) turns into the same UDP API
//! Spotify Connect already uses there.
//!
//! Audio only. No mirroring or video stream type is or will be handled.

mod codec;
#[cfg(feature = "remote-control")]
pub mod dacp;
mod device_id;
mod discovery;
mod dmap;
mod legacy;
mod rtsp;
mod sink;

use std::{net::IpAddr, sync::Arc};

use librespot_core::Error;
use librespot_playback::{audio_backend::SinkBuilder, config::AudioFormat};
use log::{info, warn};

/// Pushed from a running AirPlay session up to whatever holds this crate's [`AirplayServer`] —
/// currently the fork's `ApiServer` (`src/server.rs`), so a companion app gets AirPlay's
/// now-playing info and remote control through the same UDP channel Spotify Connect uses.
///
/// Every variant carries the number of the RTSP connection it came from, because a receiver can
/// have two at once: a sender that reconnects opens the new connection before the old one gets
/// to its `TEARDOWN`, and without the number a consumer can't tell "the session you follow has
/// ended" from "some abandoned session has ended". shairport-sync guards the same handover the
/// same way (`dacp_server.players_connection_thread_index`).
#[derive(Debug, Clone)]
pub enum AirplayEvent {
    /// `Active-Remote`/`DACP-ID` resolved to a reachable DACP endpoint — everything a caller
    /// needs to call [`dacp::send_command`](crate::dacp::send_command) itself.
    DacpAvailable {
        connection: u64,
        host: IpAddr,
        port: u16,
        active_remote: String,
        /// This receiver's own speaker id — see `dacp::DacpTarget::machine_number`.
        machine_number: u64,
        /// The interface a link-local IPv6 `host` is reachable on — see
        /// `dacp::DacpTarget::scope_id`. `0` for anything else.
        scope_id: u32,
        /// See `dacp::DacpTarget::local_bind` — carried so a caller rebuilding a target from
        /// this event gets it too.
        local_bind: Option<IpAddr>,
    },
    /// From a poll's `caps` tag, only when it changes — plus `is_playing: true` on `RECORD`,
    /// where audio really starts flowing. `position_ms` is `0` when the sender doesn't say.
    PlaybackChanged {
        connection: u64,
        is_playing: bool,
        position_ms: u32,
    },
    /// A different track is playing. Two sources produce it, indistinguishably: the sender's
    /// `SET_PARAMETER` push (what a real iPhone uses), or a `playstatusupdate` poll. Fields the
    /// sender didn't report come through empty, not omitted.
    ///
    /// `cover` is `None` when there is no artwork *yet*: the push path sends the picture in a
    /// separate request reported as [`Self::CoverChanged`], while the DACP path fetches it first
    /// and carries it here.
    TrackChanged {
        connection: u64,
        title: String,
        artist: String,
        album: String,
        album_artist: String,
        duration_ms: u32,
        cover: Option<(Vec<u8>, &'static str)>,
    },
    /// Cover art for the track [`Self::TrackChanged`] last reported — the sender pushes the
    /// picture right after the text, so a consumer attaches it to the track it already shows.
    CoverChanged {
        connection: u64,
        bytes: Vec<u8>,
        mime: &'static str,
    },
    /// Where the sender says it is, from its own `progress:` push — at the start of a track,
    /// after a seek, and periodically. The only position a classic session states outright.
    ProgressChanged {
        connection: u64,
        position_ms: u32,
        duration_ms: u32,
    },
    /// From a `dacp::get_volume` poll, 0..=100, only when it changes — and only from senders
    /// that answer that request at all. Left as the sender's percentage: the consumer knows what
    /// scale it publishes.
    VolumeChanged { connection: u64, percent: u8 },
    /// This connection is done with playback, so what it reported no longer describes anything
    /// playing here. Note its DACP endpoint stays usable — that is how a stopped sender can be
    /// told to play again — so a consumer routes commands rather than discarding it.
    ///
    /// Sent on `TEARDOWN` and again when the connection ends, since a sender that vanishes never
    /// sends `TEARDOWN`; both carry the same number, so the second is a no-op.
    ///
    /// Deliberately distinct from `PlaybackChanged { is_playing: false }`, which also means a
    /// live session that is merely paused.
    SessionEnded { connection: u64 },
}
use tokio::net::TcpListener;

/// Configuration shared with the rest of librespot — in particular the device name, which
/// must be the same value used for Spotify Connect (`-n`/`--name`) rather than a separate flag.
#[derive(Clone, Debug)]
pub struct AirplayConfig {
    /// Shared device name (from `-n`/`--name`) — the same one Spotify Connect uses. Advertised
    /// as the `_raop._tcp` instance name, and the seed for this receiver's [`device_id`].
    pub device_name: String,
    /// Addresses to advertise on; empty means "all interfaces", mirroring
    /// `librespot_discovery`'s `zeroconf_ip` handling.
    pub bind_ip: Vec<IpAddr>,
    /// RTSP control-port override. `None` picks an ephemeral port.
    pub port: Option<u16>,
    /// The same backend/device/format Spotify Connect's `Player` uses (`Setup.backend`/`.device`/
    /// `.format` in `src/main.rs`) — reused rather than adding separate AirPlay flags. See
    /// `sink.rs`'s module docs for what this does and doesn't get you yet (no shared output, no
    /// arbitration with Spotify Connect, no resampling).
    pub backend: SinkBuilder,
    pub device: Option<String>,
    pub format: AudioFormat,
}

/// Handle to a running AirPlay receiver task, mirroring the fork's `ApiServer` pattern in
/// `src/server.rs`: bind synchronously in `spawn` so port conflicts surface immediately, then
/// run the protocol state machine on its own task until `shutdown` is called.
pub struct AirplayServer {
    cmd_tx: tokio::sync::mpsc::UnboundedSender<AirplayServerCommand>,
    task: tokio::task::JoinHandle<()>,
    mdns: discovery::MdnsHandle,
}

enum AirplayServerCommand {
    Shutdown,
}

/// A cheap, clonable handle for controlling a running [`AirplayServer`] from outside — currently
/// the fork's `ApiServer` (`src/server.rs`), so a client on the UDP API can set the volume.
///
/// Volume is the receiver's own output gain rather than anything sent back to the sender: an
/// AirPlay 2 sender exposes no way to be told to change its volume that this crate can reach (its
/// `Active-Remote`/`DACP-ID` headers, which the classic path uses for exactly that, are simply
/// absent), so what a client changes here is how loud *this device* plays.
#[derive(Clone)]
pub struct AirplayControl {
    volume: Arc<tokio::sync::watch::Sender<f64>>,
}

impl AirplayControl {
    /// Sets the output gain from a percentage, 0..=100, where `0` is silence. Converted through
    /// the same dB curve a sender's own `SET_PARAMETER volume:` goes through, so both ways of
    /// changing the volume land on one scale (see `legacy::volume_to_gain`).
    pub fn set_volume(&self, percent: u8) {
        let _ = self
            .volume
            .send(crate::legacy::volume_to_gain(db_from_percent(percent)));
    }
}

/// The inverse of [`db_from_percent`], for reporting a sender's own volume change onward as the
/// percentage the fork's UDP API publishes.
pub(crate) fn percent_from_db(db: f64) -> u8 {
    if db <= -144.0 {
        return 0;
    }
    (((db.clamp(-30.0, 0.0) + 30.0) / 0.3).round() as i64).clamp(0, 100) as u8
}

/// AirPlay's volume is in dB: `-30.0` (quietest) to `0.0`, with `-144.0` meaning muted — the
/// domain shairport-sync's own `dasl_tapered_vol2attn` documents and range-checks.
fn db_from_percent(percent: u8) -> f64 {
    match percent.min(100) {
        0 => -144.0,
        percent => -30.0 + f64::from(percent) * 0.3,
    }
}

impl AirplayServer {
    /// Binds the RTSP control socket, starts advertising it over mDNS, and starts the receiver
    /// task, mirroring `ApiServer::spawn` (`src/server.rs`): the bind happens here,
    /// synchronously, so a port conflict surfaces as an `Err` instead of failing silently inside
    /// the spawned task.
    ///
    /// `config.bind_ip` only restricts which addresses `discovery::advertise` reports over mDNS
    /// — this listener itself still always binds `0.0.0.0` (accepting-on and advertised-as are
    /// separate concerns; a real sender only ever learns the listener's port from the mDNS `SRV`
    /// record, not this bind address, so restricting the bind address wouldn't change what's
    /// reachable). The receiver's identity needs no store: it is derived from the device name
    /// (`device_id`), so it survives restarts on its own.
    ///
    /// Also returns the receiving half of this server's [`AirplayEvent`] stream, which every
    /// accepted connection shares.
    pub async fn spawn(
        config: AirplayConfig,
    ) -> Result<
        (
            Self,
            tokio::sync::mpsc::UnboundedReceiver<AirplayEvent>,
            AirplayControl,
        ),
        Error,
    > {
        let bind_addr = format!("0.0.0.0:{}", config.port.unwrap_or(0));
        let listener = TcpListener::bind(&bind_addr)
            .await
            .map_err(Error::unavailable)?;
        let local_addr = listener.local_addr().map_err(Error::unavailable)?;
        info!("AirPlay RTSP control server listening on {local_addr}");

        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        // One gain for the whole receiver rather than one per connection: there is a single audio
        // output, and both ways of changing the volume — the sender's `SET_PARAMETER volume:` and
        // a client on the UDP API — mean the same thing by it.
        let (volume_tx, _) = tokio::sync::watch::channel(1.0);
        let volume = Arc::new(volume_tx);
        let connection_volume = volume.clone();
        // Derived from the name, so it is stable across restarts with nothing persisted — see
        // `device_id`'s module docs.
        let device_id = Arc::new(device_id::DeviceId::from_name(&config.device_name));
        let device_name: Arc<str> = Arc::from(config.device_name.as_str());
        let sink_config = sink::SinkConfig {
            backend: config.backend,
            device: config.device,
            format: config.format,
        };
        // Also handed to each connection, for `dacp::resolve_port`: the same interface
        // restriction advertising needs turned out to be necessary for resolution too.
        let bind_ip = Arc::new(config.bind_ip.clone());

        let mdns = discovery::advertise(
            device_id.clone(),
            device_name.clone(),
            local_addr.port(),
            config.bind_ip,
        );

        let task = tokio::spawn(async move {
            // Numbered here because this is the one place that sees every connection exactly
            // once. Starts at 1, so 0 never names a real connection.
            let mut connections = 0u64;

            loop {
                tokio::select! {
                    cmd = cmd_rx.recv() => match cmd {
                        Some(AirplayServerCommand::Shutdown) | None => break,
                    },
                    accepted = listener.accept() => {
                        let (stream, peer) = match accepted {
                            Ok(accepted) => accepted,
                            Err(err) => {
                                warn!("airplay: failed to accept a connection: {err}");
                                continue;
                            }
                        };
                        connections += 1;
                        let connection = connections;
                        info!("airplay: accepted connection {connection} from {peer}");
                        let context = rtsp::ConnectionContext {
                            device_id: device_id.clone(),
                            device_name: device_name.clone(),
                            sink_config: sink_config.clone(),
                            events: event_tx.clone(),
                            bind_ip: bind_ip.clone(),
                            volume: connection_volume.clone(),
                        };
                        tokio::spawn(async move {
                            if let Err(err) = rtsp::serve_connection(stream, connection, context).await {
                                warn!("airplay: connection {connection} from {peer} ended: {err}");
                            }
                        });
                    }
                }
            }
        });

        Ok((
            Self { cmd_tx, task, mdns },
            event_rx,
            AirplayControl { volume },
        ))
    }

    pub async fn shutdown(self) {
        let _ = self.cmd_tx.send(AirplayServerCommand::Shutdown);
        let _ = self.task.await;
        self.mdns.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both ways of changing the volume — a sender's own `SET_PARAMETER volume:` and a client on
    /// the UDP API — have to land on one scale, so the two conversions must be inverses.
    #[test]
    fn the_volume_conversions_are_inverses() {
        for percent in [1u8, 25, 50, 99, 100] {
            assert_eq!(percent_from_db(db_from_percent(percent)), percent);
        }
    }

    #[test]
    fn zero_percent_is_airplays_mute_sentinel() {
        assert_eq!(db_from_percent(0), -144.0);
        assert_eq!(percent_from_db(-144.0), 0);
        assert_eq!(crate::legacy::volume_to_gain(db_from_percent(0)), 0.0);
    }

    #[test]
    fn full_volume_is_unity_gain() {
        assert_eq!(db_from_percent(100), 0.0);
        assert_eq!(crate::legacy::volume_to_gain(db_from_percent(100)), 1.0);
    }
}
