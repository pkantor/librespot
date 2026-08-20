//! A small UDP control API for the librespot binary.
//!
//! # Requests
//!
//! The server accepts plain text datagrams, one command per datagram, in the form
//! `<command>[ <json payload>]`. Commands that query state answer with a JSON datagram sent back
//! to the requesting socket:
//!
//! | command             | payload             | response                        |
//! |---------------------|---------------------|---------------------------------|
//! | `next`              | -                   | -                               |
//! | `pause`             | -                   | -                               |
//! | `resume`            | -                   | -                               |
//! | `volup`             | -                   | -                               |
//! | `voldown`           | -                   | -                               |
//! | `setvol`            | `{"volume":<u16>}`  | -                               |
//! | `getvol`            | -                   | [`VolumeResponse`] as JSON      |
//! | `current_track`     | -                   | [`TrackResponse`] as JSON       |
//! | `status`            | -                   | `snapshot` event as JSON        |
//! | `subscribe`         | -                   | `snapshot` event as JSON        |
//! | `cover`             | -                   | `cover_chunk` events as JSON    |
//! | `unsubscribe`       | -                   | -                               |
//!
//! # Push events
//!
//! `subscribe` registers the sender for push events for [`SUBSCRIPTION_LEASE`] and answers with a
//! `snapshot` of the full state. Clients are expected to renew well within the lease as a
//! keepalive; that also re-syncs them, so a dropped datagram leaves a client stale for at most one
//! keepalive interval instead of indefinitely. A client that goes away silently is dropped when
//! its lease runs out; `unsubscribe` deregisters it right away.
//!
//! Renewing is just `subscribe` again: it registers a sender that isn't subscribed yet and
//! refreshes one that is, so a client that lost its lease keeps going without noticing, and every
//! keepalive doubles as a re-sync. There is nothing cheaper to send instead, since no response
//! carries a picture any more.
//!
//! Events are JSON datagrams discriminated by their `event` field:
//!
//! ```json
//! {"event":"snapshot","is_playing":true,"position_ms":41200,"volume":32768,"track":{…}}
//! {"event":"track_changed","track":{…}}
//! {"event":"playback_changed","is_playing":false,"position_ms":41200}
//! {"event":"volume_changed","volume":32768}
//! {"event":"cover_available","mime":"image/jpeg","bytes":184320}
//! {"event":"cover_chunk","mime":"image/jpeg","index":0,"count":60,"pending":false,"data":"…"}
//! ```
//!
//! `position_ms` is always valid at the moment the datagram is sent, so a client extrapolates it
//! against its own clock (`position_ms` plus the time since the datagram arrived, while
//! `is_playing`) and never has to agree with this machine on what time it is.
//!
//! # Cover art
//!
//! Covers travel **only** in answer to the `cover` command, never in a track event or any other
//! response. A picture is a hundred times the size of everything else here — a real AirPlay
//! sender pushes ~180 kB, which base64 turns into 240 kB — and carrying one in a track event
//! meant the event exceeded what a datagram can hold: the send fails outright (`Message too
//! long`) and the client sees nothing at all. So every response is small now, and a client that
//! wants a picture asks for one.
//!
//! `cover` answers with as many `cover_chunk` datagrams as it takes, each carrying
//! [`COVER_CHUNK_BYTES`] of the picture, its own `index` and the `count` — enough to reassemble
//! them and to notice a lost one. A track with no cover is answered with a single `count: 0`,
//! which is an answer rather than silence. Nothing is retransmitted: a client that misses a chunk
//! asks again.
//!
//! `count: 0` alone would mean two different things, and a client would have no way to tell them
//! apart. The picture is fetched from Spotify when the track changes, which takes a few hundred
//! milliseconds, and a client watching `track_changed` asks inside that window — so "no cover"
//! and "no cover *yet*" both had to come out as `count: 0`. Believing it means showing nothing
//! for the whole track; disbelieving it means asking forever at tracks that genuinely have none.
//!
//! So the answer says which it is. `pending: true` means a fetch is still running and this track
//! may well have a picture: wait for `cover_available` and ask again. `pending: false` with
//! `count: 0` is final — this track has no cover, and there is nothing to wait for. Chunks
//! carrying picture always report `pending: false`; the field only ever decides anything on the
//! `count: 0` answer, but is on every chunk so the event has one shape.
//!
//! Because nothing is retransmitted, the size of a picture decides whether it arrives at all, and
//! two things keep that in hand. The chunks go out in paced bursts ([`COVER_BURST_PAUSE`]) instead
//! of one tight loop, which is what a client's receive buffer — tens of datagrams, not hundreds —
//! can actually take in; and a picture over [`COVER_SHRINK_ABOVE_BYTES`] is re-encoded smaller
//! ([`shrink_cover`]) before it is ever held. Both were learned from iTunes on Windows, which
//! pushes its artwork at full size: 1.4 MB of PNG became ~460 datagrams fired back to back, the
//! client's buffer overran every time, and the cover never appeared no matter how often it asked.
//!
//! A chunk is also sized so the datagram carrying it crosses a link in one packet
//! ([`MTU_UDP_PAYLOAD`]) rather than as IP fragments. That is not what decides whether a picture
//! arrives — the packet count is much the same either way — but fragments bring failure modes of
//! their own, and none of them are worth carrying. See [`COVER_CHUNK_BYTES`].
//!
//! Within that, size is not otherwise a design constraint. Spotify's *largest* cover is fetched
//! rather than the smallest one big enough to display, and anything already small enough is kept
//! byte for byte as it arrived. `cover_available` tells subscribers a picture can be asked for, so
//! nobody has to poll.
//!
//! Urls are still not reported at all: the clients here cannot reach the internet, only the Pi
//! can. A cover is fetched once per track no matter how many clients are listening, and the last
//! few are cached, so skipping back and forth doesn't refetch.
//!
//! No datagram this server sends exceeds one MTU, so nothing here depends on IP fragmentation and
//! a modest receive buffer is enough — 64 kB per read is plenty. The platform difference that used
//! to matter (macOS caps outgoing datagrams at `net.inet.udp.maxdgram`, 9216 by default, while
//! Linux allows 65507) is far above anything sent now and no longer affects this server.

use std::{
    collections::{HashMap, VecDeque},
    io,
    net::{IpAddr, SocketAddr},
    str::FromStr,
    time::{Duration, Instant},
};

use data_encoding::BASE64;
#[cfg(feature = "airplay-remote-control")]
use librespot::airplay::dacp::DacpTarget;
#[cfg(feature = "airplay")]
use librespot::airplay::{AirplayControl, AirplayEvent};
use librespot::{
    connect::Spirc,
    core::{Error, Session},
    metadata::audio::{AudioItem, UniqueFields, item::CoverImage},
    playback::player::{PlayerEvent, PlayerEventChannel},
};
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use thiserror::Error as ThisError;
use tokio::{net::UdpSocket, sync::mpsc, task::JoinHandle, time::timeout};

/// The address the control API listens on by default.
pub const DEFAULT_BIND_ADDR: &str = "0.0.0.0:50505";

/// How long a `subscribe` keeps a client on the push list. Clients should refresh it about every
/// third of that, so that a lost keepalive doesn't cost them their subscription.
pub const SUBSCRIPTION_LEASE: Duration = Duration::from_secs(30);

/// Incoming datagrams larger than this are truncated. Commands are short, only `setvol` carries a
/// payload. Answers are not bound by this — one carrying a cover runs to tens of kB.
const MAX_DATAGRAM_SIZE: usize = 1024;

/// Raw bytes per `cover` chunk: as much picture as fits in one Ethernet frame once base64 has
/// grown it by a third and [`COVER_CHUNK_ENVELOPE_BYTES`] of JSON is wrapped around it.
///
/// Derived rather than picked, so the two cannot drift apart; `chunk_datagrams_fit_one_frame`
/// checks the envelope estimate the derivation rests on.
///
/// This does not make a picture likelier to arrive on its own: base64 and the envelope cost the
/// same fraction whatever the split, so a given cover takes about the same number of *packets*
/// either way, and a client re-asks for the whole picture on any loss regardless. What it removes
/// is IP fragmentation, which at the previous 3 kB turned every chunk into three fragments and
/// brought its own ways to fail — reassembly-queue pressure on the receiver with hundreds of
/// datagrams in flight, consumer routers and tunnels that drop fragments outright, and a receive
/// buffer that holds far fewer datagrams because each costs three buffers' worth of accounting.
///
/// (Not to be confused with the 9216-byte cap macOS puts on a datagram handed to `sendto`,
/// `net.inet.udp.maxdgram`. That bounds what the local stack accepts; it says nothing about what
/// crosses the network in one piece.)
const COVER_CHUNK_BYTES: usize = ((MTU_UDP_PAYLOAD - COVER_CHUNK_ENVELOPE_BYTES) / 4) * 3;

/// What [`Event::CoverChunk`]'s JSON costs around the base64 at its worst: the longest mime
/// [`cover_mime`] reports, four digits each of `index` and `count` (a cover of
/// [`MAX_COVER_BYTES`] does not reach five), and `"pending":false`, the longer of the two values
/// that field takes. Only used to size [`COVER_CHUNK_BYTES`], and only right as long as the
/// event's shape is — which is what `chunk_datagrams_fit_one_frame` checks.
const COVER_CHUNK_ENVELOPE_BYTES: usize = 109;

/// What a UDP payload has to stay inside to cross an Ethernet or Wi-Fi link as one packet: 1500
/// bytes of MTU less 20 of IP header and 8 of UDP header. Past this the kernel splits the datagram
/// into IP fragments, and losing any one of them loses the whole datagram — see
/// [`COVER_CHUNK_BYTES`].
const MTU_UDP_PAYLOAD: usize = 1472;

/// How many chunks go out back to back before the send pauses to let the client catch up.
///
/// Nothing here retransmits, so a chunk the client's socket buffer had no room for is a chunk the
/// picture never gets. See [`COVER_BURST_PAUSE`].
const COVER_CHUNK_BURST: usize = 8;

/// How long the `cover` answer pauses between bursts of [`COVER_CHUNK_BURST`] chunks.
///
/// A receive buffer holds tens of datagrams, not hundreds: Linux clamps whatever a client asks for
/// to `net.core.rmem_max`, ~208 kB by default. Firing a whole picture's worth of chunks in one
/// tight loop overruns that before the client can drain it, and since nothing is retransmitted the
/// client re-asks and loses the same race again — a cover that never arrives, no matter how often
/// it is requested. Eight chunks per millisecond is far below what a client drains and still puts
/// even the largest picture on the wire in well under a second.
const COVER_BURST_PAUSE: Duration = Duration::from_millis(1);

/// A cover past this is refused rather than sent. This is the bound on what is *accepted*, before
/// [`shrink_cover`] cuts it down — a sender that pushes more than a couple of megabytes of artwork
/// is malfunctioning, not being generous.
pub const MAX_COVER_BYTES: usize = 2 * 1024 * 1024;

/// Above this, a cover is re-encoded smaller before it is held. Below it, the bytes are kept
/// exactly as they arrived.
///
/// Spotify's largest cover sits far under this, so that path never re-encodes. AirPlay senders are
/// the reason this exists: iTunes on Windows pushes the artwork at full size — 1.4 MB of PNG
/// observed — which is twenty times more picture than a client on this LAN displays.
const COVER_SHRINK_ABOVE_BYTES: usize = 256 * 1024;

/// The longest edge a re-encoded cover is scaled down to. Not scaled *up*: a picture already
/// smaller than this only gets re-encoded, never stretched.
const COVER_MAX_EDGE: u32 = 640;

/// JPEG quality a re-encoded cover is written at.
const COVER_JPEG_QUALITY: u8 = 85;

/// How long a cover fetch may take before the track event goes out without it.
const COVER_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// How many covers to keep, so that skipping back and forth doesn't refetch them.
const COVER_CACHE_SIZE: usize = 4;

const SPOTIFY_ITEM_TYPE_TRACK: &str = "track";

/// Default port of the per-sender control helper (`contrib/airplay-control-windows.ps1`), one
/// above the API's own so the pair reads as a pair.
///
/// The fallback exists because a sender can send `DACP-ID`/`Active-Remote` and still never
/// resolve: Apple Music on Windows does exactly that, leaving `dacp::resolve_port` to time out
/// and `next`/`pause`/`resume` with nowhere to go. A helper on the sender's own machine can
/// always control it locally, whatever mDNS does.
#[cfg(feature = "airplay")]
pub const DEFAULT_AIRPLAY_HELPER_PORT: u16 = 50506;

#[derive(Debug, ThisError)]
pub enum ApiServerError {
    #[error("could not bind the API server to {addr}: {source}")]
    Bind {
        addr: String,
        #[source]
        source: io::Error,
    },
}

/// What the API server is reachable on, and by whom.
#[derive(Debug, Clone)]
pub struct ApiServerConfig {
    pub bind_addr: String,
    pub allow_list: AllowList,
    /// Port of the control helper on an AirPlay sender's own machine, used only when that sender
    /// offers no DACP endpoint — see [`ApiServerTask::control`]. `None` disables the fallback, so
    /// such a sender simply stays uncontrollable.
    #[cfg(feature = "airplay")]
    pub airplay_helper_port: Option<u16>,
}

impl Default for ApiServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: DEFAULT_BIND_ADDR.to_string(),
            allow_list: AllowList::default(),
            #[cfg(feature = "airplay")]
            airplay_helper_port: Some(DEFAULT_AIRPLAY_HELPER_PORT),
        }
    }
}

#[derive(Debug, ThisError)]
pub enum AllowListError {
    #[error("'{0}' is not an IP address or CIDR network")]
    Address(String),
    #[error("'{0}' has an out of range prefix length")]
    PrefixLength(String),
}

/// The IP networks allowed to talk to the API.
///
/// The API has no authentication of its own, so this is the only thing between the control socket
/// and the rest of the network. An empty list allows everyone; loopback is always allowed, since
/// anything on this host can talk to the process directly anyway.
#[derive(Debug, Default, Clone)]
pub struct AllowList(Vec<IpNetwork>);

impl AllowList {
    pub fn is_unrestricted(&self) -> bool {
        self.0.is_empty()
    }

    pub fn allows(&self, addr: IpAddr) -> bool {
        if self.is_unrestricted() {
            return true;
        }

        // a v4 peer reaching a dual stack socket shows up as ::ffff:a.b.c.d, while rules for it
        // are written in v4
        let addr = match addr {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(addr, IpAddr::V4),
            v4 => v4,
        };

        // implicit, so a local client never has to be spelled out on the command line
        if addr.is_loopback() {
            return true;
        }

        self.0.iter().any(|network| network.contains(addr))
    }
}

impl FromStr for AllowList {
    type Err = AllowListError;

    /// Parses a comma-separated list of IP addresses and CIDR networks.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(IpNetwork::from_str)
            .collect::<Result<Vec<_>, _>>()
            .map(AllowList)
    }
}

#[derive(Debug, Clone, Copy)]
struct IpNetwork {
    addr: IpAddr,
    prefix_len: u8,
}

impl IpNetwork {
    fn contains(&self, addr: IpAddr) -> bool {
        match (self.addr, addr) {
            (IpAddr::V4(network), IpAddr::V4(addr)) => {
                prefix_matches(&network.octets(), &addr.octets(), self.prefix_len)
            }
            (IpAddr::V6(network), IpAddr::V6(addr)) => {
                prefix_matches(&network.octets(), &addr.octets(), self.prefix_len)
            }
            // a v4 rule never covers a v6 peer, nor the other way around
            _ => false,
        }
    }
}

impl FromStr for IpNetwork {
    type Err = AllowListError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // a bare address is a single host
        let (addr, prefix) = match s.split_once('/') {
            Some((addr, prefix)) => (addr, Some(prefix)),
            None => (s, None),
        };

        let addr = addr
            .parse::<IpAddr>()
            .map_err(|_| AllowListError::Address(s.to_string()))?;

        let max_prefix_len = if addr.is_ipv4() { 32 } else { 128 };

        let prefix_len = match prefix {
            Some(prefix) => prefix
                .parse::<u8>()
                .ok()
                .filter(|prefix_len| *prefix_len <= max_prefix_len)
                .ok_or_else(|| AllowListError::PrefixLength(s.to_string()))?,
            None => max_prefix_len,
        };

        Ok(Self { addr, prefix_len })
    }
}

/// Whether two addresses agree on their first `prefix_len` bits.
fn prefix_matches(network: &[u8], addr: &[u8], prefix_len: u8) -> bool {
    let full_bytes = usize::from(prefix_len / 8);
    let remaining_bits = prefix_len % 8;

    if network[..full_bytes] != addr[..full_bytes] {
        return false;
    }

    if remaining_bits == 0 {
        return true;
    }

    let mask = 0xffu8 << (8 - remaining_bits);
    network[full_bytes] & mask == addr[full_bytes] & mask
}

enum ApiServerCommand {
    SetSession {
        spirc: Spirc,
        session: Session,
    },
    /// Hands over a running `AirplayServer`'s event stream — see `ApiServer::set_airplay_events`.
    /// A second call just replaces the first receiver.
    #[cfg(feature = "airplay")]
    SetAirplay {
        events: mpsc::UnboundedReceiver<AirplayEvent>,
        control: AirplayControl,
    },
}

/// A cover prepared off the loop, on its way back to the task that asked for it.
struct CoverFetched {
    /// The url it was fetched from, which is what the cache keys on. `None` for a cover that came
    /// from an AirPlay sender: it was pushed rather than fetched, so there is nothing to key on and
    /// nothing to re-fetch later.
    url: Option<String>,
    /// `None` when it could not be produced; the track then simply has no picture to ask for.
    cover: Option<Cover>,
}

/// A cover as it is held and sent: raw bytes, base64-encoded one chunk at a time by the `cover`
/// command rather than all at once.
#[derive(Debug, Clone)]
struct Cover {
    bytes: Vec<u8>,
    mime: &'static str,
}

/// Handle to the running API server task.
///
/// Dropping or [`ApiServer::shutdown`]ing the handle stops the task.
pub struct ApiServer {
    cmd_tx: mpsc::UnboundedSender<ApiServerCommand>,
    task: JoinHandle<()>,
}

impl ApiServer {
    /// Binds the control socket and spawns the server task.
    ///
    /// Binding happens before the task is spawned, so a failure to take the port is reported
    /// here instead of getting lost in the background.
    pub async fn spawn(
        config: ApiServerConfig,
        player_events: PlayerEventChannel,
    ) -> Result<ApiServer, ApiServerError> {
        let socket =
            UdpSocket::bind(&config.bind_addr)
                .await
                .map_err(|source| ApiServerError::Bind {
                    addr: config.bind_addr.clone(),
                    source,
                })?;

        info!("API server listening on {}", config.bind_addr);

        if config.allow_list.is_unrestricted() {
            warn!("The API server accepts commands from anyone who can reach it.");
        }

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (cover_tx, cover_rx) = mpsc::unbounded_channel();

        // An ephemeral port is all this needs, and failing to get one only costs the fallback —
        // not a reason to refuse to start the API.
        #[cfg(feature = "airplay")]
        let helper_socket = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(socket) => Some(socket),
            Err(e) => {
                warn!("could not bind a socket for the AirPlay control helper: {e}");
                None
            }
        };

        let task = tokio::spawn(
            ApiServerTask {
                socket,
                allow_list: config.allow_list,
                spirc: None,
                session: None,
                cmd_rx,
                player_events,
                subscribers: HashMap::new(),
                current_track: TrackResponse::default(),
                current_volume: VolumeResponse::default(),
                playback: PlaybackState::default(),
                covers: CoverCache::default(),
                current_cover: None,
                cover_pending: false,
                cover_tx,
                cover_rx,
                #[cfg(feature = "airplay")]
                airplay_events: None,
                #[cfg(feature = "airplay")]
                active_source: Source::default(),
                #[cfg(feature = "airplay-remote-control")]
                airplay_dacp: None,
                #[cfg(feature = "airplay")]
                airplay_connection: None,
                #[cfg(feature = "airplay")]
                airplay_peer: None,
                #[cfg(feature = "airplay")]
                airplay_helper_port: config.airplay_helper_port,
                #[cfg(feature = "airplay")]
                helper_socket,
                #[cfg(feature = "airplay")]
                airplay_control: None,
            }
            .run(),
        );

        Ok(ApiServer { cmd_tx, task })
    }

    /// Hands the current [`Spirc`] and [`Session`] to the server task.
    ///
    /// Called on every (re)connect, as each connection gets its own of both. They travel together
    /// so that the two can never end up belonging to different connections.
    pub fn set_session(&self, spirc: Spirc, session: Session) {
        if self
            .cmd_tx
            .send(ApiServerCommand::SetSession { spirc, session })
            .is_err()
        {
            warn!("could not hand the session to the API server: task is not running");
        }
    }

    /// Hands over an `AirplayServer`'s event receiver, so AirPlay's now-playing info and control
    /// fold into the same state and events Spotify Connect already reports here. Called from
    /// `main.rs` once both servers exist; not called at all when AirPlay isn't running, which is
    /// why it isn't a `spawn` argument.
    #[cfg(feature = "airplay")]
    pub fn set_airplay_events(
        &self,
        events: mpsc::UnboundedReceiver<AirplayEvent>,
        control: AirplayControl,
    ) {
        if self
            .cmd_tx
            .send(ApiServerCommand::SetAirplay { events, control })
            .is_err()
        {
            warn!("could not hand AirPlay events to the API server: task is not running");
        }
    }

    /// Stops the server task and waits for it to finish.
    pub async fn shutdown(self) {
        // dropping the sender closes the command channel, which ends the task's loop
        drop(self.cmd_tx);

        if let Err(e) = self.task.await {
            error!("API server task failed: {e}");
        }

        debug!("API server stopped");
    }
}

/// A push event, and the answer to the state queries.
#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum Event<'a> {
    Snapshot {
        is_playing: bool,
        position_ms: u32,
        volume: u16,
        track: &'a TrackResponse,
    },
    TrackChanged {
        track: &'a TrackResponse,
    },
    PlaybackChanged {
        is_playing: bool,
        position_ms: u32,
    },
    VolumeChanged {
        volume: u16,
    },
    /// A cover for the current track is now held and can be asked for with `cover`. Carries the
    /// size so a client can decide whether it wants it, not the picture itself.
    CoverAvailable {
        mime: &'static str,
        bytes: usize,
    },
    /// One chunk of an answer to `cover`, `index` of `count`.
    ///
    /// `count: 0` means there is no picture to send, and `pending` says why: `true` while the
    /// cover for the current track is still being fetched — ask again once `cover_available`
    /// arrives — and `false` when this track simply has no cover, which is final. Without it a
    /// client cannot tell "none" from "not yet", and the two need opposite responses.
    ///
    /// Chunks that carry picture always report `pending: false`: the answer is right here.
    CoverChunk {
        mime: &'a str,
        index: usize,
        count: usize,
        pending: bool,
        data: String,
    },
}

/// Answer to `current_track`.
///
/// Empty / zeroed as long as nothing has been played yet. Fields that don't apply to the current
/// item type are left empty rather than omitted, so the shape of the response never changes.
#[derive(Debug, Default, Serialize)]
pub struct TrackResponse {
    song_name: String,
    song_id: String,
    song_artists: Vec<String>,
    song_uri: String,
    /// `track`, `episode` or `local`
    item_type: String,
    /// The show name for episodes.
    album: String,
    album_artists: Vec<String>,
    duration_ms: u32,
    is_explicit: bool,
    /// The cover, base64. The urls Spotify offers are deliberately not reported: clients here
    /// cannot reach the internet, so a url would be something they can only fail to follow.
    /// `"spotify"` or `"airplay"` — empty in [`TrackResponse::default`] (nothing has played yet).
    /// Lets a client tell the two sources apart in the *same* `current_track`/`subscribe` shape,
    /// since both write into these same fields (see `ApiServerTask::handle_airplay_event`).
    source: String,
}

impl From<AudioItem> for TrackResponse {
    fn from(audio_item: AudioItem) -> Self {
        // podcasts and local files have no artists of their own, so use what stands in for them:
        // the show, respectively whatever the file's tags carried
        let (song_artists, album, album_artists) = match audio_item.unique_fields {
            UniqueFields::Track {
                artists,
                album,
                album_artists,
                ..
            } => (
                artists.iter().map(|artist| artist.name.clone()).collect(),
                album,
                album_artists,
            ),
            UniqueFields::Episode { show_name, .. } => {
                (vec![show_name.clone()], show_name, Vec::new())
            }
            UniqueFields::Local {
                artists,
                album,
                album_artists,
                ..
            } => (
                artists.into_iter().collect(),
                album.unwrap_or_default(),
                album_artists.into_iter().collect(),
            ),
        };

        // only tracks are reported with an id, other item types report an empty one
        let song_id = if audio_item.track_id.item_type() == SPOTIFY_ITEM_TYPE_TRACK {
            audio_item.track_id.to_id()
        } else {
            String::new()
        };

        Self {
            song_name: audio_item.name,
            song_id,
            song_artists,
            song_uri: audio_item.uri,
            item_type: audio_item.track_id.item_type().to_string(),
            album,
            album_artists,
            duration_ms: audio_item.duration_ms,
            is_explicit: audio_item.is_explicit,
            // filled in once the bytes are there, which is what the track event waits for
            source: "spotify".to_string(),
        }
    }
}

/// The cover to fetch, out of what a track offers: the largest.
///
/// Size stopped being a reason to compromise once covers left the track event and got their own
/// chunked command — a client that asks for a picture wants the good one, and it costs a few
/// datagrams more only when it asks.
fn cover_to_ship(covers: &[CoverImage]) -> Option<&CoverImage> {
    covers.iter().max_by_key(|cover| cover.width)
}

/// What the bytes actually are, since the url doesn't say and clients hand them to an image
/// decoder. Spotify serves JPEG, but saying so on the strength of the magic number is cheap.
fn cover_mime(bytes: &[u8]) -> &'static str {
    match bytes {
        [0xff, 0xd8, 0xff, ..] => "image/jpeg",
        [0x89, b'P', b'N', b'G', ..] => "image/png",
        [b'G', b'I', b'F', b'8', ..] => "image/gif",
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => "image/webp",
        _ => "application/octet-stream",
    }
}

/// Cuts an oversized cover down to something a client can actually receive, leaving anything
/// already small enough exactly as it arrived.
///
/// The `cover` command splits a picture into datagrams and retransmits none of them, so the size
/// of the picture decides whether it arrives at all — see [`COVER_BURST_PAUSE`]. Pacing keeps a
/// reasonable picture intact; a 1.4 MB PNG is not a reasonable picture, and shrinking it is the
/// only thing that makes it deliverable rather than merely slower to lose.
///
/// Anything that goes wrong — a format neither decoder handles, a corrupt body, a failed encode —
/// keeps the original bytes. A big cover the client may fail to reassemble beats no cover.
///
/// CPU-bound, and slow enough on a Pi to matter: call it off the server task's loop.
fn shrink_cover(cover: Cover) -> Cover {
    if cover.bytes.len() <= COVER_SHRINK_ABOVE_BYTES {
        return cover;
    }

    let image = match image::load_from_memory(&cover.bytes) {
        Ok(image) => image,
        Err(e) => {
            warn!(
                "could not decode the {}-byte {} cover to shrink it: {e}",
                cover.bytes.len(),
                cover.mime
            );
            return cover;
        }
    };

    // `resize` fits inside the bounds and would happily scale a smaller picture up; a cover this
    // big because of how it was encoded rather than how large it is only needs the re-encode.
    let image = if image.width() > COVER_MAX_EDGE || image.height() > COVER_MAX_EDGE {
        image.resize(
            COVER_MAX_EDGE,
            COVER_MAX_EDGE,
            image::imageops::FilterType::Triangle,
        )
    } else {
        image
    };

    let mut bytes = Vec::new();
    let encoded =
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, COVER_JPEG_QUALITY)
            .encode_image(&image.to_rgb8());

    if let Err(e) = encoded {
        warn!("could not re-encode the cover: {e}");
        return cover;
    }

    debug!(
        "shrank a {}-byte {} cover to {} bytes of JPEG, {}x{}",
        cover.bytes.len(),
        cover.mime,
        bytes.len(),
        image.width(),
        image.height()
    );

    Cover {
        bytes,
        mime: "image/jpeg",
    }
}

/// [`shrink_cover`] on a blocking thread, since decoding and re-encoding a megabyte of PNG takes
/// long enough on a Pi to be worth keeping off an async worker.
///
/// `None` only when the blocking thread itself failed, which takes the cover with it — the picture
/// is then simply absent, the same as any other cover that could not be produced.
async fn shrink_cover_off_thread(cover: Cover) -> Option<Cover> {
    match tokio::task::spawn_blocking(move || shrink_cover(cover)).await {
        Ok(cover) => Some(cover),
        Err(e) => {
            warn!("shrinking the cover failed: {e}");
            None
        }
    }
}

/// Fetches a cover and hands it back to the server task.
///
/// Runs off the task's loop, which has to keep answering commands meanwhile. Always reports back,
/// including when it failed, since a track event is waiting on it.
async fn fetch_cover(session: Session, url: String, cover_tx: mpsc::UnboundedSender<CoverFetched>) {
    let cover = match timeout(COVER_FETCH_TIMEOUT, session.spclient().request_url(&url)).await {
        Ok(Ok(bytes)) if bytes.len() > MAX_COVER_BYTES => {
            warn!(
                "cover {url} is {} bytes, too big to put in a datagram",
                bytes.len()
            );
            None
        }
        Ok(Ok(bytes)) => {
            debug!("fetched {} bytes of cover from {url}", bytes.len());

            shrink_cover_off_thread(Cover {
                mime: cover_mime(&bytes),
                bytes: bytes.to_vec(),
            })
            .await
        }
        Ok(Err(e)) => {
            warn!("could not fetch the cover {url}: {e}");
            None
        }
        Err(_) => {
            warn!("fetching the cover {url} timed out");
            None
        }
    };

    // the receiver lives as long as the server task, which is what spawned this
    let _ = cover_tx.send(CoverFetched {
        url: Some(url),
        cover,
    });
}

/// The last few covers, so that skipping back and forth doesn't refetch them.
///
/// Bounded by [`COVER_CACHE_SIZE`] entries of at most [`MAX_COVER_BYTES`] each, evicted oldest
/// first — enough for going back and forth over a couple of tracks, small enough to not matter on
/// a Pi.
#[derive(Debug, Default)]
struct CoverCache {
    covers: HashMap<String, Cover>,
    /// Insertion order, for eviction.
    order: VecDeque<String>,
}

impl CoverCache {
    fn get(&self, url: &str) -> Option<&Cover> {
        self.covers.get(url)
    }

    fn insert(&mut self, url: String, cover: Cover) {
        if self.covers.contains_key(&url) {
            return;
        }

        if self.order.len() >= COVER_CACHE_SIZE {
            if let Some(oldest) = self.order.pop_front() {
                self.covers.remove(&oldest);
            }
        }

        self.order.push_back(url.clone());
        self.covers.insert(url, cover);
    }
}

/// Answer to `getvol`.
#[derive(Debug, Default, Serialize)]
pub struct VolumeResponse {
    volume: u16,
}

#[derive(Debug, Deserialize)]
struct SetVolumeRequest {
    volume: u16,
}

/// DACP's 0..=100 to the `u16` this server reports and Spotify uses, so a client sees one scale
/// whichever source is playing. Rounded rather than truncated, so 100% is exactly `u16::MAX`
/// instead of one short of it.
#[cfg(feature = "airplay")]
fn volume_from_percent(percent: u8) -> u16 {
    (f64::from(percent.min(100)) / 100.0 * f64::from(u16::MAX)).round() as u16
}

/// The other direction, for a `setvol` that has to travel as an AirPlay percentage.
#[cfg(feature = "airplay")]
fn percent_from_volume(volume: u16) -> u8 {
    (f64::from(volume) / f64::from(u16::MAX) * 100.0).round() as u8
}

/// Where playback stands, as of the last event the player sent.
#[derive(Debug)]
struct PlaybackState {
    is_playing: bool,
    position_ms: u32,
    measured_at: Instant,
}

impl Default for PlaybackState {
    fn default() -> Self {
        Self {
            is_playing: false,
            position_ms: 0,
            measured_at: Instant::now(),
        }
    }
}

impl PlaybackState {
    fn update(&mut self, is_playing: Option<bool>, position_ms: u32) {
        if let Some(is_playing) = is_playing {
            self.is_playing = is_playing;
        }

        self.position_ms = position_ms;
        self.measured_at = Instant::now();
    }

    /// The position as of right now, so that clients can extrapolate from when they received it.
    ///
    /// The player only reports on state changes, so while playing this counts up from the last
    /// report. `duration_ms` caps it, which keeps a progress bar from running past the end of the
    /// track should a `Playing` / `Paused` event never arrive.
    fn position_ms(&self, duration_ms: u32) -> u32 {
        if !self.is_playing {
            return self.position_ms;
        }

        let elapsed = u32::try_from(self.measured_at.elapsed().as_millis()).unwrap_or(u32::MAX);
        let position_ms = self.position_ms.saturating_add(elapsed);

        if duration_ms > 0 {
            position_ms.min(duration_ms)
        } else {
            position_ms
        }
    }
}

struct ApiServerTask {
    socket: UdpSocket,
    allow_list: AllowList,
    spirc: Option<Spirc>,
    /// What covers are fetched with. `None` until the first connect.
    session: Option<Session>,
    cmd_rx: mpsc::UnboundedReceiver<ApiServerCommand>,
    player_events: PlayerEventChannel,
    /// Subscribed clients and when their lease runs out.
    subscribers: HashMap<SocketAddr, Instant>,
    current_track: TrackResponse,
    current_volume: VolumeResponse,
    playback: PlaybackState,
    covers: CoverCache,
    /// The cover the current track is waiting for, if any. Its `track_changed` goes out once that
    /// fetch reports back, so that the event carries the picture with it.
    /// The cover of whatever is playing, once there is one. Never part of a track event or any
    /// other response — a client asks for it with `cover` and gets it in chunks.
    current_cover: Option<Cover>,
    /// Whether a fetch (or an off-loop shrink) for the current track's cover is still running.
    ///
    /// Reported as `pending` on the `count: 0` answer, which is the whole reason it is tracked:
    /// it is what lets a client tell "this track has no cover" from "the picture isn't here
    /// yet". Set for exactly as long as something is on its way — every path that ends a wait
    /// clears it, so a client is never told to keep waiting for something that isn't coming.
    cover_pending: bool,
    cover_tx: mpsc::UnboundedSender<CoverFetched>,
    cover_rx: mpsc::UnboundedReceiver<CoverFetched>,
    /// `None` until `set_airplay_events` hands one over (or forever, if AirPlay isn't running at
    /// all) — see `Self::run`'s `select!` for how a `None` here just never fires that branch.
    #[cfg(feature = "airplay")]
    airplay_events: Option<mpsc::UnboundedReceiver<AirplayEvent>>,
    /// Which source `next`/`pause`/`resume` currently reach — see [`Source`].
    #[cfg(feature = "airplay")]
    active_source: Source,
    /// Where to send a DACP command once AirPlay is the active source, and which AirPlay
    /// connection it came from — `None` until a session's `Active-Remote`/`DACP-ID` headers have
    /// actually resolved to a reachable DACP endpoint, and then **kept**, including after that
    /// session ends: a sender's DACP server outlives its AirPlay session, and reaching it is what
    /// makes `resume` able to start the music again (see the `SessionEnded` handling, and
    /// shairport-sync's `relinquish_dacp_server_information`, which likewise never clears it).
    /// Replaced wholesale when another connection resolves one of its own.
    #[cfg(feature = "airplay-remote-control")]
    airplay_dacp: Option<(u64, DacpTarget)>,
    /// The AirPlay connection that last reported itself playing, i.e. the one [`Source::Airplay`]
    /// currently means. Tracked separately from `airplay_dacp` above because a session can be the
    /// active source without ever offering remote control.
    #[cfg(feature = "airplay")]
    airplay_connection: Option<u64>,
    /// The address the active AirPlay session's sender connected from, and which connection it
    /// belongs to. Known from the RTSP connection itself, so — unlike `airplay_dacp` — it is there
    /// for every sender, including the ones whose DACP port never resolves. Kept after the session
    /// ends for the same reason `airplay_dacp` is: a stopped sender is still what `resume` has to
    /// reach.
    #[cfg(feature = "airplay")]
    airplay_peer: Option<(u64, IpAddr)>,
    /// Where the helper listens on that address; `None` disables the fallback entirely.
    #[cfg(feature = "airplay")]
    airplay_helper_port: Option<u16>,
    /// A socket of its own for talking to the helper, rather than the API's.
    ///
    /// Not tidiness: a helper answers what it is sent, and anything arriving on `socket` is parsed
    /// as a command — so forwarding from there fed the helper's `ok` straight back into
    /// `handle_request`, which logged it and warned about an unknown command. Sending from a
    /// separate socket makes that structurally impossible instead of relying on the far end
    /// staying silent. Nothing ever reads this socket; the replies are meant to be dropped.
    #[cfg(feature = "airplay")]
    helper_socket: Option<UdpSocket>,
    /// Set alongside `airplay_events`. What `setvol` acts on while AirPlay is the source: an
    /// AirPlay 2 sender offers no way to be told to change *its* volume that this crate can reach
    /// (no `Active-Remote`/`DACP-ID` headers on that path), so the volume a client sets is this
    /// receiver's own output gain.
    #[cfg(feature = "airplay")]
    airplay_control: Option<AirplayControl>,
}

/// Which source `next`/`pause`/`resume` control: **whichever one last played**. Moved only by a
/// *positive* "now playing" signal (`PlayerEvent::Playing` /
/// `AirplayEvent::PlaybackChanged{is_playing: true}`) — nothing else, so neither pausing a source
/// nor its session ending reassigns control to the other one. Control routing and now-playing
/// display only: it does not pause one source's audio when the other starts.
///
/// That a stopped source keeps control is the point rather than an oversight: a paused phone is
/// still what the user was listening to, and its DACP server outlives the AirPlay session, so
/// `resume` can reach it. Routing to an idle Spotify session instead would send `resume` to a
/// spirc with nothing loaded — silence, and no way back to the music that was playing.
#[cfg(feature = "airplay")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Source {
    #[default]
    Spotify,
    Airplay,
}

/// A free function, not a method: `select!` needs each branch to borrow only the field it uses,
/// and a `&mut self` method would conflict with every other branch. `None` never resolves, so
/// the branch simply never fires.
#[cfg(feature = "airplay")]
async fn recv_airplay_event(
    rx: &mut Option<mpsc::UnboundedReceiver<AirplayEvent>>,
) -> Option<AirplayEvent> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

impl ApiServerTask {
    /// Two full definitions rather than `#[cfg]`s inside one `select!`: per-branch `#[cfg]`,
    /// though documented as supported, fails with a macro-parse error for this branch shape on
    /// the pinned `tokio`.
    #[cfg(feature = "airplay")]
    async fn run(mut self) {
        let mut buf = [0u8; MAX_DATAGRAM_SIZE];
        // the player outlives this task, but don't spin on a closed channel if it doesn't
        let mut player_events_open = true;

        loop {
            tokio::select! {
                cmd = self.cmd_rx.recv() => match cmd {
                    Some(ApiServerCommand::SetSession { spirc, session }) => {
                        debug!("API server got a new session");
                        self.spirc = Some(spirc);
                        self.session = Some(session);
                    }
                    Some(ApiServerCommand::SetAirplay { events, control }) => {
                        debug!("API server got the AirPlay event stream");
                        self.airplay_events = Some(events);
                        self.airplay_control = Some(control);
                    }
                    // the handle was dropped or shut down
                    None => break,
                },
                // this task holds the sender, so the channel cannot close under it
                Some(fetched) = self.cover_rx.recv() => self.handle_cover(fetched).await,
                event = self.player_events.recv(), if player_events_open => match event {
                    Some(event) => self.handle_player_event(event).await,
                    None => {
                        debug!("player event channel closed");
                        player_events_open = false;
                    }
                },
                event = recv_airplay_event(&mut self.airplay_events) => match event {
                    Some(event) => self.handle_airplay_event(event).await,
                    None => {
                        debug!("AirPlay event channel closed");
                        self.airplay_events = None;
                    }
                },
                received = self.socket.recv_from(&mut buf) => match received {
                    Ok((len, peer)) => self.handle_request(&buf[..len], peer).await,
                    Err(e) => warn!("could not receive on the API socket: {e}"),
                },
            }
        }
    }

    /// See the `#[cfg(feature = "airplay")]` twin above for why this is a full second copy rather
    /// than one `#[cfg]`-laden body — identical except for the one AirPlay-events branch.
    #[cfg(not(feature = "airplay"))]
    async fn run(mut self) {
        let mut buf = [0u8; MAX_DATAGRAM_SIZE];
        let mut player_events_open = true;

        loop {
            tokio::select! {
                cmd = self.cmd_rx.recv() => match cmd {
                    Some(ApiServerCommand::SetSession { spirc, session }) => {
                        debug!("API server got a new session");
                        self.spirc = Some(spirc);
                        self.session = Some(session);
                    }
                    None => break,
                },
                Some(fetched) = self.cover_rx.recv() => self.handle_cover(fetched).await,
                event = self.player_events.recv(), if player_events_open => match event {
                    Some(event) => self.handle_player_event(event).await,
                    None => {
                        debug!("player event channel closed");
                        player_events_open = false;
                    }
                },
                received = self.socket.recv_from(&mut buf) => match received {
                    Ok((len, peer)) => self.handle_request(&buf[..len], peer).await,
                    Err(e) => warn!("could not receive on the API socket: {e}"),
                },
            }
        }
    }

    /// Keeps the state that the queries report, and pushes it to the subscribers.
    ///
    /// Everything else the player emits is not exposed by the API.
    async fn handle_player_event(&mut self, event: PlayerEvent) {
        match event {
            PlayerEvent::TrackChanged { audio_item } => {
                debug!("changing currently played song to {}", audio_item.name);

                // the response keeps no urls, so pick the cover before converting
                let cover_url = cover_to_ship(&audio_item.covers).map(|cover| cover.url.clone());

                self.current_track = TrackResponse::from(*audio_item);
                self.load_cover(cover_url);
                self.announce_track().await;
            }
            PlayerEvent::Playing { position_ms, .. } => {
                #[cfg(feature = "airplay")]
                {
                    self.active_source = Source::Spotify;
                }
                self.update_playback(Some(true), position_ms).await
            }
            PlayerEvent::Paused { position_ms, .. } => {
                self.update_playback(Some(false), position_ms).await
            }
            PlayerEvent::Stopped { .. } => self.update_playback(Some(false), 0).await,
            // a seek or a drift correction moves the position without touching play / pause
            PlayerEvent::Seeked { position_ms, .. }
            | PlayerEvent::PositionCorrection { position_ms, .. }
            | PlayerEvent::Loading { position_ms, .. } => {
                self.update_playback(None, position_ms).await
            }
            PlayerEvent::VolumeChanged { volume } => {
                self.current_volume.volume = volume;
                self.broadcast(encode(&Event::VolumeChanged { volume }))
                    .await;
            }
            // Nobody is connected to this device on Spotify any more. What it was reporting
            // describes a session that is over, so it goes — see [`Self::clear_now_playing`].
            PlayerEvent::SessionDisconnected { .. } => {
                #[cfg(feature = "airplay")]
                if self.active_source != Source::Spotify {
                    debug!("Spotify disconnected, but AirPlay is the source: keeping its track");
                    return;
                }
                debug!("Spotify disconnected: clearing what it was playing");
                self.clear_now_playing().await;
            }
            _ => (),
        }
    }

    /// Forgets the track, its cover and the playback position, and tells subscribers.
    ///
    /// A source that has gone away leaves its last track behind otherwise, and a client has no
    /// way to tell that from something still playing — it would keep showing a track and a
    /// counting position for a session that ended. An empty track is the same shape as any other,
    /// so nothing special is needed to display it.
    ///
    /// The volume is left alone: it belongs to this device's output, not to whoever was playing.
    async fn clear_now_playing(&mut self) {
        self.current_track = TrackResponse::default();
        self.current_cover = None;
        // nothing is playing, so nothing is on its way either
        self.cover_pending = false;
        self.announce_track().await;
        self.update_playback(Some(false), 0).await;
    }

    /// Is this the AirPlay session whose state this server publishes? AirPlay writes through the
    /// same fields Spotify's events use, so anything from a session that isn't on the air is
    /// dropped rather than written.
    ///
    /// Both halves earn their place: `active_source` stops a `TEARDOWN` publishing "paused,
    /// position 0" over live Spotify state, and the connection number stops an abandoned session
    /// speaking for a live one.
    ///
    /// Known limitation of having one published state: an AirPlay session that keeps playing
    /// while Spotify holds the source loses its track updates, and shows the previous track until
    /// the next change once it takes the source back. Per-source state would fix that; this
    /// receiver has no arbiter letting two sources play at once anyway (see [`Source`]).
    #[cfg(feature = "airplay")]
    fn airplay_is_on_the_air(&self, connection: u64) -> bool {
        self.active_source == Source::Airplay && self.airplay_connection == Some(connection)
    }

    /// Keeps the session on the air in the same fields Spotify's own events update above — see
    /// [`Self::airplay_is_on_the_air`] for what gets dropped, and [`Source`] for the routing rule
    /// `PlaybackChanged{is_playing: true}` triggers.
    #[cfg(feature = "airplay")]
    async fn handle_airplay_event(&mut self, event: AirplayEvent) {
        match event {
            AirplayEvent::SessionStarted { connection, peer } => {
                // Stored for every connection, not just the one on the air: the address has to be
                // known *before* anything plays, since the first command may well be the one that
                // starts it. Whoever connected last wins, matching `airplay_dacp`.
                debug!("AirPlay connection {connection} is from {peer}");
                self.airplay_peer = Some((connection, peer));
            }
            #[cfg(feature = "airplay-remote-control")]
            AirplayEvent::DacpAvailable {
                connection,
                host,
                port,
                active_remote,
                machine_number,
                local_bind,
                scope_id,
            } => {
                debug!("AirPlay DACP control available at {host}:{port} (connection {connection})");
                self.airplay_dacp = Some((
                    connection,
                    DacpTarget {
                        host,
                        port,
                        active_remote,
                        local_bind,
                        machine_number,
                        scope_id,
                    },
                ));
            }
            // Without the feature nothing can resolve a target to store; this arm only keeps the
            // match exhaustive.
            #[cfg(not(feature = "airplay-remote-control"))]
            AirplayEvent::DacpAvailable { .. } => {}
            AirplayEvent::PlaybackChanged {
                connection,
                is_playing,
                position_ms,
            } => {
                // Starting to play is the one event that doesn't need to already be on the air —
                // it's what *puts* a session on the air.
                if is_playing {
                    self.active_source = Source::Airplay;
                    self.airplay_connection = Some(connection);
                } else if !self.airplay_is_on_the_air(connection) {
                    debug!(
                        "ignoring a stop from AirPlay connection {connection}: it is not the \
                         session on the air"
                    );
                    return;
                }
                // `0` when the sender didn't say where it is (`RECORD`, `TEARDOWN`, or a
                // `playstatusupdate` without `cant`/`cast`) — the same "don't know" sentinel
                // every other unavailable field here uses. A sender that pushes `progress:`
                // corrects it within moments anyway.
                self.update_playback(Some(is_playing), position_ms).await;
            }
            AirplayEvent::TrackChanged {
                connection,
                title,
                artist,
                album,
                album_artist,
                duration_ms,
                cover,
            } => {
                if !self.airplay_is_on_the_air(connection) {
                    debug!(
                        "ignoring a track from AirPlay connection {connection}: it is not the \
                         session on the air"
                    );
                    return;
                }
                debug!("AirPlay now playing (connection {connection}): {title}");
                self.current_cover = None;
                self.current_track = TrackResponse {
                    song_name: title,
                    song_id: String::new(),
                    song_artists: vec![artist],
                    song_uri: String::new(),
                    item_type: "airplay".to_string(),
                    album,
                    // Only the push path reports one (`assl`); empty, not the artist repeated.
                    album_artists: if album_artist.is_empty() {
                        Vec::new()
                    } else {
                        vec![album_artist]
                    },
                    duration_ms,
                    is_explicit: false,
                    source: "airplay".to_string(),
                };
                self.announce_track().await;

                // The DACP path fetches the picture before reporting the track, so it arrives
                // here; the push path sends it as its own request moments later.
                if let Some((bytes, mime)) = cover {
                    self.load_airplay_cover(Cover { bytes, mime }).await;
                }
            }
            AirplayEvent::CoverChanged {
                connection,
                bytes,
                mime,
            } => {
                if !self.airplay_is_on_the_air(connection) {
                    debug!(
                        "ignoring cover art from AirPlay connection {connection}: it is not the \
                         session on the air"
                    );
                    return;
                }
                debug!(
                    "AirPlay cover art (connection {connection}): {} bytes",
                    bytes.len()
                );
                if bytes.len() > MAX_COVER_BYTES {
                    warn!(
                        "AirPlay cover art is {} bytes, over the {MAX_COVER_BYTES}-byte limit — \
                         dropped",
                        bytes.len()
                    );
                    return;
                }
                self.load_airplay_cover(Cover { bytes, mime }).await;
            }
            AirplayEvent::ProgressChanged {
                connection,
                position_ms,
                duration_ms,
            } => {
                if !self.airplay_is_on_the_air(connection) {
                    debug!(
                        "ignoring progress from AirPlay connection {connection}: it is not the \
                         session on the air"
                    );
                    return;
                }
                // Better than no duration, but never overrides the metadata's: `astm` is the
                // track's length, while `progress:` describes the stretch being streamed.
                if self.current_track.duration_ms == 0 && duration_ms != 0 {
                    self.current_track.duration_ms = duration_ms;
                }
                // Position only — `None` leaves play/pause alone, since a sender pushes progress
                // while paused too.
                self.update_playback(None, position_ms).await;
            }
            AirplayEvent::VolumeChanged {
                connection,
                percent,
            } => {
                if !self.airplay_is_on_the_air(connection) {
                    debug!(
                        "ignoring the volume from AirPlay connection {connection}: it is not the \
                         session on the air"
                    );
                    return;
                }
                debug!("AirPlay volume (connection {connection}): {percent}%");
                let volume = volume_from_percent(percent);
                self.current_volume.volume = volume;
                self.broadcast(encode(&Event::VolumeChanged { volume }))
                    .await;
            }
            AirplayEvent::SessionEnded { connection } => {
                // Named connection only: another session may already have taken over.
                if self.airplay_connection != Some(connection) {
                    return;
                }
                let was_on_the_air = self.airplay_is_on_the_air(connection);
                self.airplay_connection = None;

                // Whatever it was playing is over, so it stops being reported — the same rule
                // Spotify's own disconnect follows (see `clear_now_playing`). Only when this
                // session was the one on the air: a sender that stops while Spotify plays must
                // not wipe Spotify's track.
                if was_on_the_air {
                    debug!("AirPlay connection {connection} ended: clearing what it was playing");
                    self.clear_now_playing().await;
                }

                // The DACP endpoint deliberately **survives** its session, exactly as in
                // shairport-sync: `relinquish_dacp_server_information` zeroes only the "which
                // connection owns playback" index and leaves address, port and `Active-Remote`
                // alone. A stopped sender still runs its DACP server, and that endpoint is the
                // only thing that can tell it to play again — so control stays with it too, even
                // when there is a Spotify session sitting there idle. Handing it back on
                // `TEARDOWN` is what made `resume` unable to restart a phone that had just been
                // paused: the command went to a spirc with nothing loaded, and the sender, the
                // one thing that *was* playing, never heard it. `PlayerEvent::Playing` takes the
                // source back the moment Spotify actually plays something.
                debug!(
                    "AirPlay connection {connection} ended: transport commands keep going to the \
                     sender, which is what last played"
                );
            }
        }
    }

    /// Starts fetching the cover of the track that just changed, if it isn't already held.
    ///
    /// The track event does **not** wait for it: covers travel only in answer to a `cover`
    /// request now, so there is nothing to hold the announcement for. A client learns the picture
    /// arrived from [`Event::CoverAvailable`] and asks for it when it wants it.
    ///
    /// Every exit sets `cover_pending`, and only the spawning one sets it true: a cover served
    /// from the cache, a track with no artwork and a missing session are all cases where nothing
    /// further is coming, and a client asking now deserves a final answer rather than being told
    /// to wait for a fetch that will never report back.
    fn load_cover(&mut self, url: Option<String>) {
        self.current_cover = None;
        self.cover_pending = false;

        let Some(url) = url else {
            return;
        };
        if let Some(cover) = self.covers.get(&url).cloned() {
            self.current_cover = Some(cover);
            return;
        }
        let Some(session) = self.session.clone() else {
            // before the first connect there is nothing to fetch with
            debug!("no session to fetch the cover with yet");
            return;
        };

        self.cover_pending = true;
        tokio::spawn(fetch_cover(session, url, self.cover_tx.clone()));
    }

    /// Takes a fetched cover and tells subscribers it is there to be asked for.
    ///
    /// Reports back on every outcome, failure and timeout included, which is what makes this the
    /// one place `cover_pending` has to be cleared: past here nothing is on its way any more, and
    /// a `count: 0` answer stops meaning "wait".
    async fn handle_cover(&mut self, fetched: CoverFetched) {
        self.cover_pending = false;

        // worth keeping even if a newer track overtook this fetch: whatever overtook it was most
        // likely a skip, and a skip back wants this picture again
        let Some(cover) = fetched.cover else {
            return;
        };
        // an AirPlay sender's own artwork has no url to key on, and it pushes it again anyway
        if let Some(url) = fetched.url {
            self.covers.insert(url, cover.clone());
        }

        self.announce_cover(cover).await;
    }

    /// Takes a cover an AirPlay sender pushed: held right away when it is small enough, shrunk
    /// off the loop first when it isn't.
    ///
    /// Nothing was fetched here — the sender handed over the bytes — so this is where the size
    /// decision happens for that path. A shrink reports back through the same channel
    /// [`fetch_cover`] uses, carrying no url, since there is nothing to cache it under.
    #[cfg(feature = "airplay")]
    async fn load_airplay_cover(&mut self, cover: Cover) {
        if cover.bytes.len() <= COVER_SHRINK_ABOVE_BYTES {
            self.announce_cover(cover).await;
            return;
        }

        let cover_tx = self.cover_tx.clone();

        // a shrink is the same kind of wait as a fetch: the picture is not here yet, but it is
        // coming, and a client asking meanwhile should be told to wait rather than that there is
        // nothing — it reports back through the same channel, so `handle_cover` clears this
        self.cover_pending = true;

        tokio::spawn(async move {
            let cover = shrink_cover_off_thread(cover).await;

            // the receiver lives as long as the server task, which is what spawned this
            let _ = cover_tx.send(CoverFetched { url: None, cover });
        });
    }

    /// Holds a cover for the current track and tells subscribers it can be asked for.
    async fn announce_cover(&mut self, cover: Cover) {
        let payload = encode(&Event::CoverAvailable {
            mime: cover.mime,
            bytes: cover.bytes.len(),
        });
        self.current_cover = Some(cover);
        self.cover_pending = false;
        self.broadcast(payload).await;
    }

    /// Answers `cover` with the current picture, split across as many datagrams as it takes.
    ///
    /// One response per chunk, in order, each carrying its index and the total — so a client can
    /// reassemble them and tell a lost one from the end of the picture.
    ///
    /// With no picture to send the answer is a single `count: 0`, which is a real answer rather
    /// than silence — and it carries `pending`, so the client knows whether that is final or
    /// whether the fetch simply hasn't landed yet. See [`Event::CoverChunk`].
    ///
    /// Sent in bursts of [`COVER_CHUNK_BURST`] with a [`COVER_BURST_PAUSE`] between them, so the
    /// client's receive buffer is never the reason a chunk goes missing.
    async fn send_cover(&self, peer: SocketAddr) {
        let Some(cover) = &self.current_cover else {
            self.reply(
                &Event::CoverChunk {
                    mime: "",
                    index: 0,
                    count: 0,
                    pending: self.cover_pending,
                    data: String::new(),
                },
                peer,
            )
            .await;
            return;
        };

        let chunks: Vec<&[u8]> = cover.bytes.chunks(COVER_CHUNK_BYTES).collect();
        debug!(
            "sending {} bytes of cover to {peer} in {} chunks",
            cover.bytes.len(),
            chunks.len()
        );
        for (index, chunk) in chunks.iter().enumerate() {
            if index > 0 && index % COVER_CHUNK_BURST == 0 {
                tokio::time::sleep(COVER_BURST_PAUSE).await;
            }

            self.reply(
                &Event::CoverChunk {
                    mime: cover.mime,
                    index,
                    count: chunks.len(),
                    // the picture is right here, so there is nothing left to wait for
                    pending: false,
                    data: BASE64.encode(chunk),
                },
                peer,
            )
            .await;
        }
    }

    async fn announce_track(&mut self) {
        self.broadcast(encode(&Event::TrackChanged {
            track: &self.current_track,
        }))
        .await;
    }

    async fn update_playback(&mut self, is_playing: Option<bool>, position_ms: u32) {
        self.playback.update(is_playing, position_ms);

        self.broadcast(encode(&Event::PlaybackChanged {
            is_playing: self.playback.is_playing,
            position_ms,
        }))
        .await;
    }

    async fn handle_request(&mut self, datagram: &[u8], peer: SocketAddr) {
        if !self.allow_list.allows(peer.ip()) {
            warn!("rejecting a datagram from {peer}: not on the allow list");
            return;
        }

        let message = String::from_utf8_lossy(datagram);
        let mut parts = message.trim().splitn(2, ' ');
        let command = parts.next().unwrap_or_default();
        let payload = parts.next().unwrap_or_default().trim();

        // subscription keepalives arrive every lease period, so they stay at `debug`;
        // everything else is a user-triggered command and is worth seeing without RUST_LOG
        if matches!(command, "subscribe" | "unsubscribe") {
            debug!("received '{command}' from {peer}");
        } else {
            info!("received '{command}' from {peer}");
        }

        match command {
            "next" => self.control(command, Spirc::next, "nextitem").await,
            "pause" => self.control(command, Spirc::pause, "pause").await,
            "resume" => {
                self.control(
                    command,
                    |spirc| {
                        // the device may be idle, so take over playback before resuming
                        spirc.activate()?;
                        spirc.play()
                    },
                    "play",
                )
                .await
            }
            "volup" => self.command(command, Spirc::volume_up),
            "voldown" => self.command(command, Spirc::volume_down),
            "setvol" => match serde_json::from_str::<SetVolumeRequest>(payload) {
                Ok(request) => {
                    let percentage = (request.volume as f64 / u16::MAX as f64) * 100.0;
                    debug!("setting volume to {percentage:.2}%");
                    self.set_volume(command, request.volume);
                }
                Err(e) => warn!("invalid `setvol` payload '{payload}' from {peer}: {e}"),
            },
            "getvol" => self.reply(&self.current_volume, peer).await,
            "current_track" => self.reply(&self.current_track, peer).await,
            "cover" => self.send_cover(peer).await,
            "status" => self.reply(&self.snapshot(), peer).await,
            // Also the keepalive: renewing is subscribing again, which costs one small datagram
            // now that no response carries a picture.
            "subscribe" => {
                let is_new = self.renew_lease(peer);
                self.reply(&self.snapshot(), peer).await;

                // A picture that arrived before this client did was announced to whoever was
                // subscribed at the time, so without this a client joining mid-track has no reason
                // to send `cover` and shows no artwork until the next track announces a new one.
                // Only for a genuinely new subscription: a renewal arrives every lease period, and
                // re-announcing there would have every client refetch the same cover forever.
                if is_new {
                    if let Some(cover) = &self.current_cover {
                        let announcement = Event::CoverAvailable {
                            mime: cover.mime,
                            bytes: cover.bytes.len(),
                        };
                        self.reply(&announcement, peer).await;
                    }
                }
            }
            "unsubscribe" => {
                if self.subscribers.remove(&peer).is_some() {
                    info!("{peer} unsubscribed from API events");
                }
            }
            other => warn!("unknown command '{other}' from {peer}"),
        }
    }

    /// Puts a client on the push list, or keeps it there for another lease.
    ///
    /// `true` when this was a new subscription rather than a renewal — the caller uses that to
    /// tell a first-time subscriber about a cover that is already here, without repeating it on
    /// every keepalive.
    fn renew_lease(&mut self, peer: SocketAddr) -> bool {
        let is_new = self
            .subscribers
            .insert(peer, Instant::now() + SUBSCRIPTION_LEASE)
            .is_none();

        if is_new {
            info!("{peer} subscribed to API events");
        }

        is_new
    }

    fn snapshot(&self) -> Event<'_> {
        Event::Snapshot {
            is_playing: self.playback.is_playing,
            position_ms: self.playback.position_ms(self.current_track.duration_ms),
            volume: self.current_volume.volume,
            track: &self.current_track,
        }
    }

    /// Runs a command against the current session, if there is one.
    fn command(&self, name: &str, f: impl FnOnce(&Spirc) -> Result<(), Error>) {
        match &self.spirc {
            Some(spirc) => {
                if let Err(e) = f(spirc) {
                    warn!("`{name}` failed: {e}");
                }
            }
            None => warn!("cannot handle `{name}`: not connected to Spotify yet"),
        }
    }

    /// Routes a transport command to whichever source is active: Spotify via `spirc`, AirPlay via
    /// DACP, and — for a sender that offers no DACP endpoint — via the helper on that sender's own
    /// machine (see [`Self::forward_to_helper`]).
    ///
    /// DACP first, always: it is the sender's own protocol and needs nothing installed at the far
    /// end. The helper is the fallback for senders that never resolve one, not an alternative to
    /// be preferred. A spirc call is local; both AirPlay routes are network sends, so both go out
    /// fire-and-forget.
    #[cfg_attr(not(feature = "airplay-remote-control"), allow(unused_variables))]
    async fn control(
        &self,
        name: &str,
        spotify: impl FnOnce(&Spirc) -> Result<(), Error>,
        airplay_command: &'static str,
    ) {
        #[cfg(feature = "airplay")]
        if self.active_source == Source::Airplay {
            #[cfg(feature = "airplay-remote-control")]
            if let Some(target) = self.airplay_dacp.clone().map(|(_, target)| target) {
                tokio::spawn(async move {
                    librespot::airplay::dacp::send_command(&target, airplay_command).await;
                });
                return;
            }

            // The command name is forwarded verbatim: the helper speaks this same vocabulary, so
            // there is nothing to translate — unlike DACP, whose `nextitem`/`play` differ.
            if self.forward_to_helper(name).await {
                return;
            }

            #[cfg(feature = "airplay-remote-control")]
            warn!(
                "cannot handle `{name}`: AirPlay is the active source but no DACP endpoint resolved \
                 and no helper is configured or its sender's address is unknown"
            );
            #[cfg(not(feature = "airplay-remote-control"))]
            warn!(
                "cannot handle `{name}`: AirPlay is the active source but remote control isn't compiled in"
            );
            return;
        }

        self.command(name, spotify);
    }

    /// Sends a command to the control helper on the AirPlay sender's own machine, which can reach
    /// the player locally whatever mDNS does.
    ///
    /// `false` when there is nothing to send to — no helper port configured, no sender address
    /// seen yet, or no socket to send from — which leaves the caller to report the command as
    /// unhandled. A `true` only means the datagram left: the helper's reply lands on
    /// [`Self::helper_socket`], which nothing reads, and nothing here waits for one.
    #[cfg(feature = "airplay")]
    async fn forward_to_helper(&self, command: &str) -> bool {
        let (Some(port), Some((_, peer)), Some(socket)) = (
            self.airplay_helper_port,
            self.airplay_peer,
            self.helper_socket.as_ref(),
        ) else {
            return false;
        };

        let target = SocketAddr::new(peer, port);
        match socket.send_to(command.as_bytes(), target).await {
            Ok(_) => {
                info!("forwarded `{command}` to the AirPlay control helper at {target}");
                true
            }
            Err(e) => {
                warn!("could not forward `{command}` to the helper at {target}: {e}");
                // The route existed and was tried; reporting it as absent would only produce a
                // second, misleading warning about there being nowhere to send.
                true
            }
        }
    }

    /// `setvol`, routed like [`Self::control`] routes transport commands — a second body rather
    /// than a parameter on that one, since the AirPlay side needs a *value*, not a fixed command
    /// string. The conversion to the sender's 0..=100 happens here, on the side that knows what
    /// scale it publishes.
    #[cfg_attr(not(feature = "airplay"), allow(unused_variables))]
    fn set_volume(&self, name: &str, volume: u16) {
        #[cfg(feature = "airplay")]
        if self.active_source == Source::Airplay {
            let percent = percent_from_volume(volume);

            // A DACP endpoint, when there is one, moves the *sender's* own volume — which is what
            // a classic-path sender expects. An AirPlay 2 sender never offers one, so the volume
            // is applied to this receiver's output instead (`AirplayControl`). Both are "make it
            // quieter", and a client can't act on the difference.
            #[cfg(feature = "airplay-remote-control")]
            if let Some((_, target)) = self.airplay_dacp.clone() {
                tokio::spawn(async move {
                    librespot::airplay::dacp::set_volume(&target, percent).await;
                });
                return;
            }

            match &self.airplay_control {
                Some(control) => control.set_volume(percent),
                None => warn!("cannot handle `{name}`: AirPlay is not running"),
            }
            return;
        }

        self.command(name, |spirc| spirc.set_volume(volume));
    }

    /// Sends an event to every subscriber whose lease is still good.
    async fn broadcast(&mut self, payload: Option<String>) {
        let now = Instant::now();

        self.subscribers.retain(|peer, lease| {
            let alive = *lease > now;

            if !alive {
                info!("subscription of {peer} expired");
            }

            alive
        });

        let Some(payload) = payload else {
            return;
        };

        // collected so that the sends don't borrow the subscriber list
        let peers: Vec<SocketAddr> = self.subscribers.keys().copied().collect();

        for peer in peers {
            self.send(&payload, peer).await;
        }
    }

    async fn reply<T: Serialize>(&self, response: &T, peer: SocketAddr) {
        if let Some(payload) = encode(response) {
            self.send(&payload, peer).await;
        }
    }

    async fn send(&self, payload: &str, peer: SocketAddr) {
        if let Err(e) = self.socket.send_to(payload.as_bytes(), peer).await {
            warn!("could not send to {peer}: {e}");
        }
    }
}

fn encode<T: Serialize>(payload: &T) -> Option<String> {
    match serde_json::to_string(payload) {
        Ok(payload) => Some(payload),
        Err(e) => {
            error!("could not serialize an API payload: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use librespot::{
        core::SpotifyUri,
        metadata::{
            artist::{ArtistRole, ArtistWithRole, ArtistsWithRole},
            audio::AudioFiles,
            image::ImageSize,
        },
    };

    const TRACK_URI: &str = "spotify:track:2WUy2Uywcj5cP0IXQagO3z";
    /// The one the fixture ships: the smallest at least [`COVER_MIN_WIDTH`] wide.
    const COVER_URL: &str = "https://i.scdn.co/image/default";

    fn artist(name: &str) -> ArtistWithRole {
        ArtistWithRole {
            id: SpotifyUri::from_uri(TRACK_URI).expect("valid uri"),
            name: name.to_string(),
            role: ArtistRole::ARTIST_ROLE_MAIN_ARTIST,
        }
    }

    fn cover(url: &str, size: ImageSize, edge: i32) -> CoverImage {
        CoverImage {
            url: url.to_string(),
            size,
            width: edge,
            height: edge,
        }
    }

    fn audio_item(unique_fields: UniqueFields) -> AudioItem {
        let track_id = SpotifyUri::from_uri(TRACK_URI).expect("valid uri");

        AudioItem {
            uri: track_id.to_uri(),
            track_id,
            files: AudioFiles::default(),
            name: "Sun Is Shining".to_string(),
            // as Spotify serves them: largest first
            covers: vec![
                cover("https://i.scdn.co/image/big", ImageSize::LARGE, 640),
                cover(COVER_URL, ImageSize::DEFAULT, 300),
                cover("https://i.scdn.co/image/small", ImageSize::SMALL, 64),
            ],
            language: vec!["en".to_string()],
            duration_ms: 213_000,
            is_explicit: false,
            availability: Ok(()),
            alternatives: None,
            unique_fields,
        }
    }

    #[test]
    fn track_response_reports_artists_and_album() {
        let response = TrackResponse::from(audio_item(UniqueFields::Track {
            artists: ArtistsWithRole(vec![artist("Bob Marley"), artist("The Wailers")]),
            album: "Kaya".to_string(),
            album_artists: vec!["Bob Marley".to_string()],
            popularity: 42,
            number: 3,
            disc_number: 1,
        }));

        let json = serde_json::to_value(&response).expect("serializes");

        assert_eq!(json["song_name"], "Sun Is Shining");
        assert_eq!(json["song_id"], "2WUy2Uywcj5cP0IXQagO3z");
        assert_eq!(json["song_uri"], TRACK_URI);
        assert_eq!(json["item_type"], "track");
        assert_eq!(json["song_artists"][0], "Bob Marley");
        assert_eq!(json["song_artists"][1], "The Wailers");
        assert_eq!(json["album"], "Kaya");
        assert_eq!(json["album_artists"][0], "Bob Marley");
        assert_eq!(json["duration_ms"], 213_000);
        assert_eq!(json["is_explicit"], false);
        assert_eq!(json["source"], "spotify");

        // no urls: what a client cannot reach is not worth reporting
        assert!(json.get("cover_url").is_none());
        assert!(json.get("covers").is_none());
    }

    /// The best one on offer: size stopped being a reason to compromise once covers got their
    /// own chunked command instead of riding along in every track event.
    #[test]
    fn the_cover_fetched_is_the_largest_on_offer() {
        let covers = |edges: &[i32]| -> Vec<CoverImage> {
            edges
                .iter()
                .map(|edge| cover("https://i.scdn.co/image/x", ImageSize::DEFAULT, *edge))
                .collect()
        };

        assert_eq!(
            cover_to_ship(&covers(&[640, 300, 64]))
                .expect("picks one")
                .width,
            640
        );
        assert_eq!(
            cover_to_ship(&covers(&[64, 32])).expect("picks one").width,
            64
        );
        assert!(cover_to_ship(&[]).is_none());
    }

    #[test]
    fn cover_mime_follows_the_bytes() {
        assert_eq!(cover_mime(&[0xff, 0xd8, 0xff, 0xe0, 0x00]), "image/jpeg");
        assert_eq!(cover_mime(b"\x89PNG\r\n\x1a\n"), "image/png");
        assert_eq!(cover_mime(b"GIF89a"), "image/gif");
        assert_eq!(cover_mime(b"RIFF\0\0\0\0WEBPVP8 "), "image/webp");
        // an image decoder still gets the bytes, it just isn't told what they are
        assert_eq!(cover_mime(b"nonsense"), "application/octet-stream");
        assert_eq!(cover_mime(&[]), "application/octet-stream");
    }

    #[test]
    fn the_cover_cache_evicts_the_oldest() {
        let mut cache = CoverCache::default();

        for i in 0..COVER_CACHE_SIZE + 1 {
            cache.insert(
                format!("url-{i}"),
                Cover {
                    bytes: i.to_string().into_bytes(),
                    mime: "image/jpeg",
                },
            );
        }

        assert!(cache.get("url-0").is_none(), "the oldest should be gone");
        assert_eq!(
            cache
                .get(&format!("url-{COVER_CACHE_SIZE}"))
                .expect("kept")
                .bytes,
            COVER_CACHE_SIZE.to_string().into_bytes()
        );
        assert_eq!(cache.covers.len(), COVER_CACHE_SIZE);
    }

    /// The chunk size is only right in relation to the envelope around it, and the envelope is
    /// free to grow whenever [`Event::CoverChunk`] changes. Built at its worst: the longest mime
    /// [`cover_mime`] can report, and the largest indices a cover of [`MAX_COVER_BYTES`] reaches.
    #[test]
    fn chunk_datagrams_fit_one_frame() {
        let last = MAX_COVER_BYTES.div_ceil(COVER_CHUNK_BYTES);
        let datagram = encode(&Event::CoverChunk {
            mime: "application/octet-stream",
            index: last,
            count: last,
            // the longer of the two, so the envelope is measured at its worst
            pending: false,
            data: BASE64.encode(&vec![0xab; COVER_CHUNK_BYTES]),
        })
        .expect("encodes");

        assert!(
            datagram.len() <= MTU_UDP_PAYLOAD,
            "a chunk datagram is {} bytes and would be fragmented",
            datagram.len()
        );
    }

    /// A PNG of noise, which barely compresses — the point is to get past
    /// [`COVER_SHRINK_ABOVE_BYTES`] the way a real sender's full-size artwork does.
    fn noisy_png(edge: u32) -> Vec<u8> {
        let pixels = image::RgbImage::from_fn(edge, edge, |x, y| {
            let seed = x.wrapping_mul(2654435761) ^ y.wrapping_mul(2246822519);
            image::Rgb([(seed >> 16) as u8, (seed >> 8) as u8, seed as u8])
        });
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(pixels)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .expect("encodes");

        bytes
    }

    /// The picture iTunes on Windows pushes is megabytes of PNG, which no amount of pacing gets
    /// across in one piece. It has to come down in size before it is ever held.
    #[test]
    fn an_oversized_cover_is_re_encoded_smaller() {
        let bytes = noisy_png(1024);
        assert!(
            bytes.len() > COVER_SHRINK_ABOVE_BYTES,
            "the fixture has to be oversized to test anything"
        );

        let before = bytes.len();
        let shrunk = shrink_cover(Cover {
            bytes,
            mime: "image/png",
        });

        assert_eq!(shrunk.mime, "image/jpeg");
        // What is guaranteed is the cap on the picture, not an absolute byte count: how far the
        // bytes fall is up to the content, and noise is the worst case JPEG ever meets. Real
        // artwork of this size comes out around 50 kB.
        assert!(
            shrunk.bytes.len() < before / 2,
            "{before} bytes became {}",
            shrunk.bytes.len()
        );

        let decoded = image::load_from_memory(&shrunk.bytes).expect("a readable JPEG");
        assert_eq!(decoded.width(), COVER_MAX_EDGE);
        assert_eq!(decoded.height(), COVER_MAX_EDGE);
    }

    /// Everything else is kept byte for byte: Spotify's covers arrive small already, and
    /// re-encoding one would only cost quality.
    #[test]
    fn a_cover_small_enough_to_send_is_left_alone() {
        let bytes = noisy_png(64);
        assert!(bytes.len() <= COVER_SHRINK_ABOVE_BYTES);

        let kept = shrink_cover(Cover {
            bytes: bytes.clone(),
            mime: "image/png",
        });

        assert_eq!(kept.mime, "image/png");
        assert_eq!(kept.bytes, bytes);
    }

    /// A body no decoder makes sense of is passed through rather than dropped — a picture the
    /// client may struggle with beats no picture.
    #[test]
    fn a_cover_that_cannot_be_decoded_survives_the_shrink() {
        let bytes = vec![0x5a; COVER_SHRINK_ABOVE_BYTES + 1];

        let kept = shrink_cover(Cover {
            bytes: bytes.clone(),
            mime: "application/octet-stream",
        });

        assert_eq!(kept.bytes, bytes);
    }

    #[test]
    fn episode_response_falls_back_to_the_show_name() {
        let response = TrackResponse::from(audio_item(UniqueFields::Episode {
            description: "an episode".to_string(),
            publish_time: librespot::core::date::Date::now_utc(),
            show_name: "Darknet Diaries".to_string(),
        }));

        assert_eq!(response.song_artists, vec!["Darknet Diaries".to_string()]);
        assert_eq!(response.album, "Darknet Diaries");
        assert!(response.album_artists.is_empty());
    }

    #[test]
    fn default_response_is_empty_but_complete() {
        let json = serde_json::to_value(TrackResponse::default()).expect("serializes");

        for field in [
            "song_name",
            "song_id",
            "song_artists",
            "song_uri",
            "item_type",
            "album",
            "album_artists",
            "duration_ms",
            "is_explicit",
            "source",
        ] {
            assert!(json.get(field).is_some(), "missing field {field}");
        }
    }

    #[test]
    fn events_carry_their_name() {
        let track = TrackResponse::default();

        let json = serde_json::to_value(Event::Snapshot {
            is_playing: true,
            position_ms: 41_200,
            volume: 32_768,
            track: &track,
        })
        .expect("serializes");

        assert_eq!(json["event"], "snapshot");
        assert_eq!(json["is_playing"], true);
        assert_eq!(json["position_ms"], 41_200);
        assert_eq!(json["volume"], 32_768);
        assert!(json["track"].is_object());

        let json = serde_json::to_value(Event::PlaybackChanged {
            is_playing: false,
            position_ms: 0,
        })
        .expect("serializes");

        assert_eq!(json["event"], "playback_changed");
    }

    #[test]
    fn position_counts_up_while_playing_only() {
        let mut playback = PlaybackState::default();

        playback.update(Some(true), 1_000);
        playback.measured_at -= Duration::from_millis(500);
        assert_eq!(playback.position_ms(213_000), 1_500);

        // and stays put once paused
        playback.update(Some(false), 1_500);
        playback.measured_at -= Duration::from_millis(500);
        assert_eq!(playback.position_ms(213_000), 1_500);
    }

    #[test]
    fn position_never_runs_past_the_track() {
        let mut playback = PlaybackState::default();

        playback.update(Some(true), 200_000);
        playback.measured_at -= Duration::from_secs(60);

        assert_eq!(playback.position_ms(213_000), 213_000);
        // an unknown duration can't cap anything
        assert_eq!(playback.position_ms(0), 260_000);
    }

    #[test]
    fn allow_list_matches_networks() {
        let allow_list: AllowList = "192.168.2.0/24, 10.1.2.3".parse().expect("parses");

        assert!(allow_list.allows("192.168.2.1".parse().unwrap()));
        assert!(allow_list.allows("192.168.2.255".parse().unwrap()));
        assert!(!allow_list.allows("192.168.3.1".parse().unwrap()));

        // a bare address only ever matches itself
        assert!(allow_list.allows("10.1.2.3".parse().unwrap()));
        assert!(!allow_list.allows("10.1.2.4".parse().unwrap()));

        // v4 rules don't cover v6 peers, but do cover v4 mapped ones
        assert!(!allow_list.allows("fd00::1".parse().unwrap()));
        assert!(allow_list.allows("::ffff:192.168.2.1".parse().unwrap()));
    }

    #[test]
    fn loopback_is_allowed_without_being_listed() {
        let allow_list: AllowList = "192.168.2.0/24".parse().expect("parses");

        assert!(allow_list.allows("127.0.0.1".parse().unwrap()));
        // the whole 127.0.0.0/8 block, however it arrives at a dual stack socket
        assert!(allow_list.allows("127.0.0.53".parse().unwrap()));
        assert!(allow_list.allows("::1".parse().unwrap()));
        assert!(allow_list.allows("::ffff:127.0.0.1".parse().unwrap()));

        // and it does not weaken anything else
        assert!(!allow_list.allows("192.168.3.1".parse().unwrap()));
    }

    #[test]
    fn allow_list_handles_odd_prefixes() {
        let allow_list: AllowList = "192.168.0.0/20".parse().expect("parses");

        assert!(allow_list.allows("192.168.15.255".parse().unwrap()));
        assert!(!allow_list.allows("192.168.16.0".parse().unwrap()));

        let allow_list: AllowList = "0.0.0.0/0".parse().expect("parses");
        assert!(allow_list.allows("8.8.8.8".parse().unwrap()));

        let allow_list: AllowList = "fd00::/8".parse().expect("parses");
        assert!(allow_list.allows("fd00::1".parse().unwrap()));
        assert!(!allow_list.allows("fe00::1".parse().unwrap()));
    }

    #[test]
    fn an_empty_allow_list_allows_everyone() {
        let allow_list = AllowList::default();

        assert!(allow_list.is_unrestricted());
        assert!(allow_list.allows("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn allow_list_rejects_nonsense() {
        assert!("192.168.2.0/33".parse::<AllowList>().is_err());
        assert!("192.168.2.0/nope".parse::<AllowList>().is_err());
        assert!("not-an-address".parse::<AllowList>().is_err());
        assert!("::1/129".parse::<AllowList>().is_err());
    }

    /// A server task on a loopback port, with the handles a test needs to drive it: the player
    /// event sender, and a client socket to talk to it with.
    struct TestServer {
        addr: SocketAddr,
        events: mpsc::UnboundedSender<PlayerEvent>,
        client: UdpSocket,
        /// The task stops when the command channel closes, so hold on to the sender.
        _cmd_tx: mpsc::UnboundedSender<ApiServerCommand>,
    }

    /// A task on a loopback port that is not running its loop yet, plus the handles to drive it.
    /// Tests that only need `handle_request` can use it directly and skip the socket.
    async fn idle_task(
        allow_list: AllowList,
    ) -> (
        ApiServerTask,
        mpsc::UnboundedSender<PlayerEvent>,
        mpsc::UnboundedSender<ApiServerCommand>,
    ) {
        let (events, player_events) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (cover_tx, cover_rx) = mpsc::unbounded_channel();

        let task = ApiServerTask {
            socket: UdpSocket::bind("127.0.0.1:0").await.expect("binds"),
            allow_list,
            spirc: None,
            session: None,
            cmd_rx,
            player_events,
            subscribers: HashMap::new(),
            current_track: TrackResponse::default(),
            current_volume: VolumeResponse::default(),
            playback: PlaybackState::default(),
            covers: CoverCache::default(),
            current_cover: None,
            cover_pending: false,
            cover_tx,
            cover_rx,
            #[cfg(feature = "airplay")]
            airplay_events: None,
            #[cfg(feature = "airplay")]
            active_source: Source::default(),
            #[cfg(feature = "airplay-remote-control")]
            airplay_dacp: None,
            #[cfg(feature = "airplay")]
            airplay_connection: None,
            #[cfg(feature = "airplay")]
            airplay_peer: None,
            #[cfg(feature = "airplay")]
            airplay_helper_port: None,
            #[cfg(feature = "airplay")]
            helper_socket: Some(UdpSocket::bind("127.0.0.1:0").await.expect("binds")),
            #[cfg(feature = "airplay")]
            airplay_control: None,
        };

        (task, events, cmd_tx)
    }

    /// An AirPlay session reporting that it started or stopped playing. A helper because every
    /// test that wants a session on the air has to send one of these first, and none of them care
    /// about the position.
    #[cfg(feature = "airplay")]
    fn airplay_playing(connection: u64, is_playing: bool) -> AirplayEvent {
        AirplayEvent::PlaybackChanged {
            connection,
            is_playing,
            position_ms: 0,
        }
    }

    impl TestServer {
        async fn start(allow_list: AllowList) -> Self {
            Self::start_with(allow_list, |_| ()).await
        }

        /// Starts a server whose state a test got to set up first — there is no session to fetch
        /// anything with here, so a seeded cover cache stands in for the network.
        async fn start_with(
            allow_list: AllowList,
            prepare: impl FnOnce(&mut ApiServerTask),
        ) -> Self {
            let (mut task, events, cmd_tx) = idle_task(allow_list).await;
            prepare(&mut task);
            let addr = task.socket.local_addr().expect("has an address");

            tokio::spawn(task.run());

            Self {
                addr,
                events,
                client: UdpSocket::bind("127.0.0.1:0").await.expect("binds"),
                _cmd_tx: cmd_tx,
            }
        }

        async fn request(&self, command: &str) {
            self.client
                .send_to(command.as_bytes(), self.addr)
                .await
                .expect("sends");
        }

        async fn receive(&self) -> serde_json::Value {
            // what a real client needs: enough for a track event carrying a cover
            let mut buf = vec![0u8; MAX_COVER_BYTES * 2];

            let (len, _) =
                tokio::time::timeout(Duration::from_secs(2), self.client.recv_from(&mut buf))
                    .await
                    .expect("a datagram arrives in time")
                    .expect("receives");

            serde_json::from_slice(&buf[..len]).expect("valid json")
        }

        /// Whether nothing arrives in the time a loopback datagram would need many times over.
        async fn receives_nothing(&self) -> bool {
            let mut buf = [0u8; MAX_DATAGRAM_SIZE];

            tokio::time::timeout(Duration::from_millis(250), self.client.recv_from(&mut buf))
                .await
                .is_err()
        }
    }

    /// No response carries a picture any more — a track event is small, and the cover is asked
    /// for separately. This is what keeps a datagram from exceeding what the OS will send: a real
    /// sender's artwork runs to ~180 kB, which base64 turns into 240 kB, well past the 65507-byte
    /// ceiling.
    #[tokio::test]
    async fn no_response_carries_the_cover() {
        let server = TestServer::start_with(AllowList::default(), |task| {
            task.current_cover = Some(Cover {
                bytes: vec![0xff; 8 * 1024],
                mime: "image/jpeg",
            });
        })
        .await;

        server.request("current_track").await;
        let track = server.receive().await;
        for absent in ["cover_data", "cover_mime", "cover_width", "cover_height"] {
            assert!(track.get(absent).is_none(), "{absent} should be gone");
        }

        server.request("status").await;
        let snapshot = server.receive().await;
        assert!(snapshot["track"].get("cover_data").is_none());
    }

    /// The window that made `count: 0` mean two things: the fetch from Spotify starts when the
    /// track changes, and a client watching `track_changed` asks inside it. The answer has to say
    /// so, or the client either shows nothing all track or never believes a `count: 0` at all.
    #[tokio::test]
    async fn a_cover_still_being_fetched_is_answered_as_pending() {
        let server = TestServer::start_with(AllowList::default(), |task| {
            // what the state looks like between `track_changed` and the cover landing
            task.cover_pending = true;
        })
        .await;

        server.request("cover").await;

        let chunk = server.receive().await;
        assert_eq!(chunk["event"], "cover_chunk");
        assert_eq!(chunk["count"], 0);
        // told `count: 0` here, a client shows no artwork for the whole track
        assert_eq!(chunk["pending"], true);
    }

    /// The other half of the same guarantee: with nothing being fetched, `count: 0` is final and
    /// a client can act on it. A client that keeps asking at every track without artwork is no
    /// better off than one that was told to give up too early.
    #[tokio::test]
    async fn a_track_with_no_cover_is_answered_as_final() {
        let server = TestServer::start(AllowList::default()).await;

        server.request("cover").await;

        let chunk = server.receive().await;
        assert_eq!(chunk["event"], "cover_chunk");
        assert_eq!(chunk["count"], 0);
        assert_eq!(chunk["pending"], false);
    }

    /// Whatever the fetch reports — a picture, a failure, a timeout — it comes back through one
    /// channel, and past it nothing is on its way. `pending` has to stop saying otherwise, or a
    /// client waits out a cover that is never coming.
    #[tokio::test]
    async fn a_cover_fetch_that_fails_stops_being_pending() {
        let mut cover_tx = None;

        let server = TestServer::start_with(AllowList::default(), |task| {
            task.cover_pending = true;
            cover_tx = Some(task.cover_tx.clone());
        })
        .await;

        let cover_tx = cover_tx.expect("the task handed one over");
        let fetched = CoverFetched {
            url: None,
            cover: None,
        };

        cover_tx.send(fetched).expect("the task is still running");

        // the send above only queues it; asking gives the loop a turn to take it first
        tokio::time::sleep(Duration::from_millis(50)).await;
        server.request("cover").await;

        let chunk = server.receive().await;
        assert_eq!(chunk["count"], 0);
        assert_eq!(chunk["pending"], false);
    }

    /// A chunk carrying picture is its own answer, so it never asks anyone to wait.
    #[tokio::test]
    async fn a_chunk_carrying_picture_is_never_pending() {
        let server = TestServer::start_with(AllowList::default(), |task| {
            task.current_cover = Some(Cover {
                bytes: vec![0xcd; 100],
                mime: "image/jpeg",
            });
        })
        .await;

        server.request("cover").await;

        let chunk = server.receive().await;
        assert_eq!(chunk["event"], "cover_chunk");
        assert_eq!(chunk["count"], 1);
        assert_eq!(chunk["pending"], false);
    }

    /// A client that subscribes while something is already playing has to learn there is a picture
    /// to ask for, the same as one that was there when it arrived.
    #[tokio::test]
    async fn a_late_subscriber_is_told_about_the_cover_already_playing() {
        // The state a client walks in on: something is already playing and its picture is here.
        let server = TestServer::start_with(AllowList::default(), |task| {
            task.current_cover = Some(Cover {
                bytes: vec![0xab; 100],
                mime: "image/jpeg",
            });
        })
        .await;

        server.request("subscribe").await;

        let snapshot = server.receive().await;
        assert_eq!(snapshot["event"], "snapshot");

        // `cover_available` is broadcast when a picture *arrives*, so a client that subscribed
        // afterwards never heard about this one and had no reason to send `cover` — it showed no
        // artwork until the next track produced a fresh announcement.
        let available = server.receive().await;
        assert_eq!(available["event"], "cover_available");
        assert_eq!(available["mime"], "image/jpeg");
        assert_eq!(available["bytes"], 100);
    }

    /// Renewing a lease is `subscribe` again, every 10s in `api_test.py`. That must not re-announce
    /// the same picture each time, or a subscriber refetches a cover it already has all day.
    #[tokio::test]
    async fn renewing_a_lease_does_not_re_announce_the_cover() {
        let server = TestServer::start_with(AllowList::default(), |task| {
            task.current_cover = Some(Cover {
                bytes: vec![0xab; 100],
                mime: "image/jpeg",
            });
        })
        .await;

        server.request("subscribe").await;
        assert_eq!(server.receive().await["event"], "snapshot");
        assert_eq!(server.receive().await["event"], "cover_available");

        server.request("subscribe").await;
        assert_eq!(server.receive().await["event"], "snapshot");

        assert!(
            server.receives_nothing().await,
            "a renewal announced the cover a second time"
        );
    }

    /// The cover arrives in as many datagrams as it takes, each small enough to send anywhere,
    /// each saying which of how many it is.
    #[tokio::test]
    async fn the_cover_command_answers_in_chunks() {
        // spanning several bursts, so the pacing in `send_cover` is part of what this exercises
        const CHUNKS: usize = COVER_CHUNK_BURST * 2 + 1;
        const SIZE: usize = COVER_CHUNK_BYTES * (CHUNKS - 1) + 100;

        let server = TestServer::start_with(AllowList::default(), |task| {
            task.current_cover = Some(Cover {
                bytes: vec![0xab; SIZE],
                mime: "image/jpeg",
            });
        })
        .await;

        server.request("cover").await;

        let mut reassembled = Vec::new();
        for expected_index in 0..CHUNKS {
            let chunk = server.receive().await;
            assert_eq!(chunk["event"], "cover_chunk");
            assert_eq!(chunk["index"], expected_index);
            assert_eq!(chunk["count"], CHUNKS);
            assert_eq!(chunk["mime"], "image/jpeg");
            reassembled.extend(
                BASE64
                    .decode(chunk["data"].as_str().expect("base64 text").as_bytes())
                    .expect("valid base64"),
            );
        }

        assert_eq!(reassembled, vec![0xab; SIZE]);
    }

    /// A track with no cover is answered, not met with silence — a client waiting for a reply
    /// would otherwise wait forever.
    #[tokio::test]
    async fn asking_for_a_cover_that_is_not_there_is_still_answered() {
        let server = TestServer::start(AllowList::default()).await;

        server.request("cover").await;

        let answer = server.receive().await;
        assert_eq!(answer["event"], "cover_chunk");
        assert_eq!(answer["count"], 0);
    }

    #[tokio::test]
    async fn subscribers_are_pushed_to_until_they_unsubscribe() {
        let server = TestServer::start(AllowList::default()).await;

        server.request("subscribe").await;
        assert_eq!(server.receive().await["event"], "snapshot");

        server
            .events
            .send(PlayerEvent::VolumeChanged { volume: 1_234 })
            .expect("the task is running");

        let event = server.receive().await;
        assert_eq!(event["event"], "volume_changed");
        assert_eq!(event["volume"], 1_234);

        server
            .events
            .send(PlayerEvent::TrackChanged {
                audio_item: Box::new(audio_item(UniqueFields::Track {
                    artists: ArtistsWithRole(vec![artist("Bob Marley")]),
                    album: "Kaya".to_string(),
                    album_artists: Vec::new(),
                    popularity: 42,
                    number: 3,
                    disc_number: 1,
                })),
            })
            .expect("the task is running");

        let event = server.receive().await;
        assert_eq!(event["event"], "track_changed");
        assert_eq!(event["track"]["song_name"], "Sun Is Shining");

        server.request("unsubscribe").await;
        // the answer to this proves the unsubscribe was handled before the event below
        server.request("status").await;
        assert_eq!(server.receive().await["event"], "snapshot");

        server
            .events
            .send(PlayerEvent::VolumeChanged { volume: 4_321 })
            .expect("the task is running");

        assert!(
            server.receives_nothing().await,
            "an unsubscribed client should not be pushed to"
        );
    }

    /// Clients see one volume scale whichever source is playing, so the ends of DACP's 0..=100
    /// have to land exactly on the ends of this server's own range.
    #[cfg(feature = "airplay")]
    #[test]
    fn the_volume_scales_line_up_at_both_ends() {
        assert_eq!(volume_from_percent(0), 0);
        assert_eq!(volume_from_percent(100), u16::MAX);
        assert_eq!(volume_from_percent(50), 32768);
        // A sender reporting nonsense is clamped, not wrapped around.
        assert_eq!(volume_from_percent(200), u16::MAX);
    }

    #[cfg(feature = "airplay")]
    #[test]
    fn a_volume_survives_the_round_trip_to_a_percentage() {
        assert_eq!(percent_from_volume(0), 0);
        assert_eq!(percent_from_volume(u16::MAX), 100);
        for percent in [0, 1, 37, 50, 99, 100] {
            assert_eq!(percent_from_volume(volume_from_percent(percent)), percent);
        }
    }

    /// AirPlay's volume has to reach the same field `getvol` answers from, or a client would ask
    /// for the volume and get Spotify's while AirPlay is what it can hear.
    #[cfg(feature = "airplay")]
    #[tokio::test]
    async fn an_airplay_volume_becomes_the_reported_volume() {
        let (mut task, ..) = idle_task(AllowList::default()).await;

        task.handle_airplay_event(airplay_playing(1, true)).await;
        task.handle_airplay_event(AirplayEvent::VolumeChanged {
            connection: 1,
            percent: 50,
        })
        .await;

        assert_eq!(task.current_volume.volume, 32768);
    }

    /// The published state describes whatever is actually on the air, so an AirPlay session
    /// ending while Spotify plays must not report "stopped" over live Spotify playback — the
    /// `TEARDOWN` case this guard was written for.
    #[cfg(feature = "airplay")]
    #[tokio::test]
    async fn an_airplay_teardown_does_not_report_over_spotify() {
        let (mut task, ..) = idle_task(AllowList::default()).await;

        // AirPlay played, then Spotify took the source back (what `PlayerEvent::Playing` does).
        task.handle_airplay_event(airplay_playing(1, true)).await;
        task.active_source = Source::Spotify;
        task.playback.update(Some(true), 42_000);

        // The AirPlay session is torn down: `PlaybackChanged`, then `SessionEnded`.
        task.handle_airplay_event(airplay_playing(1, false)).await;
        task.handle_airplay_event(AirplayEvent::SessionEnded { connection: 1 })
            .await;

        assert!(
            task.playback.is_playing,
            "Spotify was playing and nothing about it changed"
        );
        assert_eq!(task.playback.position_ms, 42_000);
    }

    /// A cover far past anything legitimate is refused rather than held.
    #[cfg(feature = "airplay")]
    #[tokio::test]
    async fn an_oversized_airplay_cover_is_dropped() {
        let (mut task, ..) = idle_task(AllowList::default()).await;
        task.handle_airplay_event(airplay_playing(1, true)).await;

        task.handle_airplay_event(AirplayEvent::CoverChanged {
            connection: 1,
            bytes: vec![0xff; MAX_COVER_BYTES + 1],
            mime: "image/jpeg",
        })
        .await;

        assert!(task.current_cover.is_none());
    }

    /// A sender's own artwork is held whole — no size compromise, since it leaves in chunks.
    #[cfg(feature = "airplay")]
    #[tokio::test]
    async fn an_airplay_cover_is_held_for_the_cover_command() {
        let (mut task, ..) = idle_task(AllowList::default()).await;
        task.handle_airplay_event(airplay_playing(1, true)).await;

        // The size a real iPhone pushes, which no single datagram could ever carry.
        let jpeg: Vec<u8> = vec![0xff; 180 * 1024];
        task.handle_airplay_event(AirplayEvent::CoverChanged {
            connection: 1,
            bytes: jpeg.clone(),
            mime: "image/jpeg",
        })
        .await;

        let held = task.current_cover.expect("held for the `cover` command");
        assert_eq!(held.bytes, jpeg);
        assert_eq!(held.mime, "image/jpeg");
    }

    /// The same filter, for an abandoned session that is still polling: whatever it reports is
    /// about a session nobody is listening to.
    #[cfg(feature = "airplay")]
    #[tokio::test]
    async fn a_stale_sessions_track_is_not_published() {
        let (mut task, ..) = idle_task(AllowList::default()).await;
        let track_changed = |connection, title: &str| AirplayEvent::TrackChanged {
            connection,
            title: title.to_string(),
            artist: "Bob Marley".to_string(),
            album: "Kaya".to_string(),
            album_artist: String::new(),
            duration_ms: 213_000,
            cover: None,
        };

        for connection in [1, 2] {
            task.handle_airplay_event(airplay_playing(connection, true))
                .await;
        }
        task.handle_airplay_event(track_changed(2, "Sun Is Shining"))
            .await;

        task.handle_airplay_event(track_changed(1, "Something Else"))
            .await;

        assert_eq!(task.current_track.song_name, "Sun Is Shining");
    }

    /// A source that goes away must not leave its last track on display: a client cannot tell
    /// that from something still playing.
    #[tokio::test]
    async fn a_spotify_disconnect_clears_what_it_was_playing() {
        let (mut task, ..) = idle_task(AllowList::default()).await;
        task.current_track = TrackResponse {
            song_name: "Sun Is Shining".to_string(),
            source: "spotify".to_string(),
            ..TrackResponse::default()
        };
        task.current_cover = Some(Cover {
            bytes: vec![0xff; 16],
            mime: "image/jpeg",
        });
        task.playback.update(Some(true), 42_000);

        task.handle_player_event(PlayerEvent::SessionDisconnected {
            connection_id: String::new(),
            user_name: String::new(),
        })
        .await;

        assert!(task.current_track.song_name.is_empty());
        assert!(task.current_cover.is_none());
        assert!(!task.playback.is_playing);
        assert_eq!(task.playback.position_ms, 0);
    }

    /// The same, for the other source.
    #[cfg(feature = "airplay")]
    #[tokio::test]
    async fn an_airplay_session_ending_clears_what_it_was_playing() {
        let (mut task, ..) = idle_task(AllowList::default()).await;
        task.handle_airplay_event(airplay_playing(1, true)).await;
        task.handle_airplay_event(AirplayEvent::TrackChanged {
            connection: 1,
            title: "Enemy".to_string(),
            artist: "Father Of Peace".to_string(),
            album: "The Year Of Madness".to_string(),
            album_artist: String::new(),
            duration_ms: 205_615,
            cover: None,
        })
        .await;
        assert_eq!(task.current_track.song_name, "Enemy");

        task.handle_airplay_event(AirplayEvent::SessionEnded { connection: 1 })
            .await;

        assert!(task.current_track.song_name.is_empty());
        assert!(task.current_cover.is_none());
        assert!(!task.playback.is_playing);
    }

    /// Neither source may wipe the other's track: only the one that was on the air clears.
    #[cfg(feature = "airplay")]
    #[tokio::test]
    async fn a_disconnect_leaves_the_other_sources_track_alone() {
        let (mut task, ..) = idle_task(AllowList::default()).await;

        // AirPlay is playing; Spotify disconnects.
        task.handle_airplay_event(airplay_playing(1, true)).await;
        task.handle_airplay_event(AirplayEvent::TrackChanged {
            connection: 1,
            title: "Enemy".to_string(),
            artist: "Father Of Peace".to_string(),
            album: "The Year Of Madness".to_string(),
            album_artist: String::new(),
            duration_ms: 205_615,
            cover: None,
        })
        .await;

        task.handle_player_event(PlayerEvent::SessionDisconnected {
            connection_id: String::new(),
            user_name: String::new(),
        })
        .await;
        assert_eq!(task.current_track.song_name, "Enemy");

        // Spotify is playing; an abandoned AirPlay session ends.
        task.active_source = Source::Spotify;
        task.current_track = TrackResponse {
            song_name: "Sun Is Shining".to_string(),
            source: "spotify".to_string(),
            ..TrackResponse::default()
        };

        task.handle_airplay_event(AirplayEvent::SessionEnded { connection: 1 })
            .await;
        assert_eq!(task.current_track.song_name, "Sun Is Shining");
    }

    /// An ended AirPlay session keeps control, because it is still the thing that last played:
    /// only Spotify actually starting to play takes the source back. Handing it over on
    /// `TEARDOWN` — as this did while a Spotify session existed — sent `resume` to an idle spirc
    /// while the paused phone, the only thing that could start the music again, heard nothing.
    #[cfg(feature = "airplay")]
    #[tokio::test]
    async fn a_session_ending_keeps_control_on_airplay() {
        let (mut task, ..) = idle_task(AllowList::default()).await;

        task.handle_airplay_event(airplay_playing(1, true)).await;
        assert_eq!(task.active_source, Source::Airplay);

        // A pause is not the end of a session: a paused sender still owns the remote.
        task.handle_airplay_event(airplay_playing(1, false)).await;
        assert_eq!(task.active_source, Source::Airplay);

        task.handle_airplay_event(AirplayEvent::SessionEnded { connection: 1 })
            .await;
        assert_eq!(task.active_source, Source::Airplay);
        // The session is over even though its remote lives on, so nothing it might still report
        // gets published (`airplay_is_on_the_air`).
        assert_eq!(task.airplay_connection, None);
    }

    /// Spotify actually playing is the one thing that takes the source back — and, since an ended
    /// AirPlay session no longer hands it over, the only one.
    #[cfg(feature = "airplay")]
    #[tokio::test]
    async fn spotify_playing_takes_the_source_back() {
        let (mut task, ..) = idle_task(AllowList::default()).await;

        task.handle_airplay_event(airplay_playing(1, true)).await;
        task.handle_airplay_event(AirplayEvent::SessionEnded { connection: 1 })
            .await;
        assert_eq!(task.active_source, Source::Airplay);

        task.handle_player_event(PlayerEvent::Playing {
            play_request_id: 0,
            track_id: SpotifyUri::from_uri(TRACK_URI).expect("valid uri"),
            position_ms: 0,
        })
        .await;

        assert_eq!(task.active_source, Source::Spotify);
    }

    /// A sender's DACP server outlives the AirPlay session that revealed it — that is what makes
    /// `resume` able to tell a stopped sender to play. shairport-sync's own
    /// `relinquish_dacp_server_information` likewise clears only which connection owns playback,
    /// never the endpoint.
    #[cfg(feature = "airplay-remote-control")]
    #[tokio::test]
    async fn a_session_ending_keeps_its_dacp_endpoint() {
        let (mut task, ..) = idle_task(AllowList::default()).await;

        task.handle_airplay_event(AirplayEvent::DacpAvailable {
            connection: 1,
            host: "192.168.2.5".parse().expect("parses"),
            port: 3689,
            active_remote: "1234567890".to_string(),
            machine_number: 0xAABB_CCDD_EEFF,
            scope_id: 0,
            local_bind: None,
        })
        .await;
        task.handle_airplay_event(airplay_playing(1, true)).await;

        task.handle_airplay_event(AirplayEvent::SessionEnded { connection: 1 })
            .await;

        assert!(
            task.airplay_dacp.is_some(),
            "the sender is still reachable, and telling it to play is the only way back"
        );
    }

    /// The whole point of the helper fallback: a sender that sends `DACP-ID`/`Active-Remote` but
    /// whose `_dacp._tcp` never resolves (Apple Music on Windows) produces no `DacpAvailable`, so
    /// before this the command had nowhere to go. Its address is known regardless, from the RTSP
    /// connection itself.
    #[cfg(feature = "airplay")]
    #[tokio::test]
    async fn a_sender_without_dacp_is_controlled_through_the_helper() {
        let helper = UdpSocket::bind("127.0.0.1:0").await.expect("binds");
        let helper_addr = helper.local_addr().expect("has an address");

        let (mut task, ..) = idle_task(AllowList::default()).await;
        task.airplay_helper_port = Some(helper_addr.port());

        task.handle_airplay_event(AirplayEvent::SessionStarted {
            connection: 1,
            peer: helper_addr.ip(),
        })
        .await;
        task.handle_airplay_event(airplay_playing(1, true)).await;

        // No `DacpAvailable` ever arrives, which is exactly the case under test.
        task.control("pause", Spirc::pause, "pause").await;

        let mut buf = [0u8; 64];
        let (len, _) = timeout(Duration::from_secs(1), helper.recv_from(&mut buf))
            .await
            .expect("the helper was sent something")
            .expect("received it");

        // The API's own command name, not DACP's — the helper speaks this vocabulary.
        assert_eq!(&buf[..len], b"pause");
    }

    /// Regression test for a real bug: forwarding used to go out on the API's own socket, so the
    /// helper's `ok` came back to the port `handle_request` reads and was parsed as a command —
    /// "received 'ok'" followed by "unknown command 'ok'" in the log, once per keypress. The
    /// forwarding socket must be a different one.
    #[cfg(feature = "airplay")]
    #[tokio::test]
    async fn a_helpers_reply_does_not_come_back_as_a_command() {
        let helper = UdpSocket::bind("127.0.0.1:0").await.expect("binds");
        let helper_addr = helper.local_addr().expect("has an address");

        let (mut task, ..) = idle_task(AllowList::default()).await;
        task.airplay_helper_port = Some(helper_addr.port());
        let api_addr = task.socket.local_addr().expect("has an address");

        task.handle_airplay_event(AirplayEvent::SessionStarted {
            connection: 1,
            peer: helper_addr.ip(),
        })
        .await;
        task.handle_airplay_event(airplay_playing(1, true)).await;

        task.control("pause", Spirc::pause, "pause").await;

        // The helper answers whoever asked, as any reasonable one does.
        let mut buf = [0u8; 64];
        let (_, from) = timeout(Duration::from_secs(1), helper.recv_from(&mut buf))
            .await
            .expect("the helper was sent something")
            .expect("received it");
        helper.send_to(b"ok", from).await.expect("answers");

        assert_ne!(
            from, api_addr,
            "the forward must not come from the port commands are read on"
        );

        // Nothing may be waiting on the API socket: an answer arriving there would be parsed as a
        // command by the next `handle_request`.
        let mut unexpected = [0u8; 64];
        assert!(
            timeout(
                Duration::from_millis(200),
                task.socket.recv_from(&mut unexpected)
            )
            .await
            .is_err(),
            "the helper's reply reached the command socket: {:?}",
            String::from_utf8_lossy(&unexpected)
        );
    }

    /// DACP is the sender's own protocol and needs nothing installed at the far end, so it stays
    /// the first choice; the helper must not steal commands from a sender that resolved one.
    #[cfg(feature = "airplay-remote-control")]
    #[tokio::test]
    async fn a_resolved_dacp_endpoint_wins_over_the_helper() {
        let helper = UdpSocket::bind("127.0.0.1:0").await.expect("binds");
        let helper_addr = helper.local_addr().expect("has an address");

        let (mut task, ..) = idle_task(AllowList::default()).await;
        task.airplay_helper_port = Some(helper_addr.port());

        task.handle_airplay_event(AirplayEvent::SessionStarted {
            connection: 1,
            peer: helper_addr.ip(),
        })
        .await;
        task.handle_airplay_event(AirplayEvent::DacpAvailable {
            connection: 1,
            host: "127.0.0.1".parse().expect("parses"),
            port: 3689,
            active_remote: "1234567890".to_string(),
            machine_number: 0,
            scope_id: 0,
            local_bind: None,
        })
        .await;
        task.handle_airplay_event(airplay_playing(1, true)).await;

        task.control("pause", Spirc::pause, "pause").await;

        let mut buf = [0u8; 64];
        assert!(
            timeout(Duration::from_millis(200), helper.recv_from(&mut buf))
                .await
                .is_err(),
            "the command belonged to DACP, so nothing should have reached the helper"
        );
    }

    /// Spotify keeps its own route: the fallback is for AirPlay only, and a sender's address
    /// lingering from an earlier session must not divert a Spotify command.
    #[cfg(feature = "airplay")]
    #[tokio::test]
    async fn the_helper_is_not_used_while_spotify_is_the_source() {
        let helper = UdpSocket::bind("127.0.0.1:0").await.expect("binds");
        let helper_addr = helper.local_addr().expect("has an address");

        let (mut task, ..) = idle_task(AllowList::default()).await;
        task.airplay_helper_port = Some(helper_addr.port());

        task.handle_airplay_event(AirplayEvent::SessionStarted {
            connection: 1,
            peer: helper_addr.ip(),
        })
        .await;
        assert_eq!(
            task.active_source,
            Source::Spotify,
            "nothing has played yet"
        );

        task.control("pause", Spirc::pause, "pause").await;

        let mut buf = [0u8; 64];
        assert!(
            timeout(Duration::from_millis(200), helper.recv_from(&mut buf))
                .await
                .is_err(),
            "Spotify is the source, so the helper is not involved"
        );
    }

    /// `--airplay-helper-port 0` disables the fallback, which then has to stay disabled even for
    /// the sender it would otherwise have handled.
    #[cfg(feature = "airplay")]
    #[tokio::test]
    async fn no_helper_port_means_no_forwarding() {
        let (mut task, ..) = idle_task(AllowList::default()).await;
        assert_eq!(task.airplay_helper_port, None);

        task.handle_airplay_event(AirplayEvent::SessionStarted {
            connection: 1,
            peer: "127.0.0.1".parse().expect("parses"),
        })
        .await;
        task.handle_airplay_event(airplay_playing(1, true)).await;

        assert!(
            !task.forward_to_helper("pause").await,
            "with no port configured there is nowhere to forward to"
        );
    }

    /// A sender that reconnects (network drop, moving between rooms) opens the new connection
    /// before the old one tears down, so the two sessions overlap and the events arrive
    /// interleaved — the abandoned one must not take control away from the live one.
    #[cfg(feature = "airplay")]
    #[tokio::test]
    async fn a_stale_session_ending_leaves_a_live_one_in_control() {
        let (mut task, ..) = idle_task(AllowList::default()).await;

        for connection in [1, 2] {
            task.handle_airplay_event(airplay_playing(connection, true))
                .await;
        }

        task.handle_airplay_event(AirplayEvent::SessionEnded { connection: 1 })
            .await;

        assert_eq!(task.active_source, Source::Airplay);
        assert_eq!(task.airplay_connection, Some(2));
    }

    /// Which connection's endpoint is the current one, as sessions come and go.
    #[cfg(feature = "airplay-remote-control")]
    #[tokio::test]
    async fn a_newer_session_replaces_an_older_sessions_endpoint() {
        let (mut task, ..) = idle_task(AllowList::default()).await;
        let dacp_available = |connection| AirplayEvent::DacpAvailable {
            connection,
            host: "192.168.2.5".parse().expect("parses"),
            port: 3689,
            active_remote: "1234567890".to_string(),
            local_bind: None,
            machine_number: 0xAABB_CCDD_EEFF,
            scope_id: 0,
        };

        task.handle_airplay_event(dacp_available(2)).await;

        task.handle_airplay_event(AirplayEvent::SessionEnded { connection: 2 })
            .await;
        assert_eq!(
            task.airplay_dacp.as_ref().map(|(owner, _)| *owner),
            Some(2),
            "an ended session's endpoint stays; only another connection's replaces it"
        );

        task.handle_airplay_event(dacp_available(3)).await;
        assert_eq!(task.airplay_dacp.as_ref().map(|(owner, _)| *owner), Some(3));
    }

    #[tokio::test]
    async fn the_allow_list_keeps_others_from_subscribing() {
        // driven directly rather than over the socket, because a test client is always on
        // loopback and loopback is allowed unconditionally
        let (mut task, ..) = idle_task("192.168.2.0/24".parse().expect("parses")).await;

        task.handle_request(b"subscribe", "8.8.8.8:1234".parse().expect("parses"))
            .await;
        assert!(
            task.subscribers.is_empty(),
            "a client outside the allow list should be dropped before its command is read"
        );

        task.handle_request(b"subscribe", "192.168.2.5:1234".parse().expect("parses"))
            .await;
        assert_eq!(
            task.subscribers.len(),
            1,
            "a listed client should be served"
        );
    }

    #[tokio::test]
    async fn a_local_client_is_served_without_being_listed() {
        let server = TestServer::start("192.168.2.0/24".parse().expect("parses")).await;

        server.request("subscribe").await;

        assert_eq!(server.receive().await["event"], "snapshot");
    }
}
