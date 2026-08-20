//! A small TCP control API for the librespot binary.
//!
//! # The connection
//!
//! A client opens a TCP connection and keeps it open for as long as it wants to be talked to.
//! Everything travels over it as newline-delimited text: one message per line, in both
//! directions. `serde_json` never writes a raw newline inside a value, so `\n` is a frame
//! boundary and nothing has to be escaped or counted; a trailing `\r` on a request is stripped,
//! so a client that sends CRLF works too.
//!
//! Requests are plain text, `<command>[ <json payload>]`, one per line. Commands that query state
//! answer on the same connection:
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
//! | `unsubscribe`       | -                   | -                               |
//!
//! # Push events
//!
//! `subscribe` puts the connection on the push list and answers with a `snapshot` of the full
//! state; `unsubscribe` takes it off again without closing anything. There is no lease and no
//! keepalive: the connection *is* the subscription, and it ends when the socket does. A client
//! that goes away — cleanly, by crashing, or by losing the network — is noticed by TCP itself.
//!
//! **`subscribe` and `unsubscribe` are the whole client side of this.** Everything a subscriber
//! needs is pushed, artwork included; nothing has to be asked for, polled, retried or correlated
//! with a request. Nothing is lost, duplicated or reordered on a stream either, so the `snapshot`
//! plus every event since it *is* the state, by construction.
//!
//! Events are JSON lines discriminated by their `event` field:
//!
//! ```json
//! {"event":"snapshot","is_playing":true,"position_ms":41200,"volume":32768,"track":{…}}
//! {"event":"track_changed","track":{…}}
//! {"event":"playback_changed","is_playing":false,"position_ms":41200}
//! {"event":"volume_changed","volume":32768}
//! {"event":"cover","mime":"image/jpeg","bytes":184320,"data":"…"}
//! ```
//!
//! A whole client is therefore: connect, send `subscribe`, read lines, switch on `event`. The
//! query commands exist for a different audience — something that connects, asks one question and
//! leaves — and a subscriber has no reason to send any of them.
//!
//! `position_ms` is always valid at the moment the line is written, so a client extrapolates it
//! against its own clock (`position_ms` plus the time since it arrived, while `is_playing`) and
//! never has to agree with this machine on what time it is.
//!
//! # Keeping up
//!
//! What a stream gives in reliability it takes back in coupling: a client that stops reading
//! stops the writer, and the one thing that must never happen here is a stalled PC in another
//! room holding up the task that also handles player events. So a connection never blocks the
//! loop. Each one gets its own writer task fed by a bounded queue ([`CONNECTION_QUEUE`]), the
//! loop only ever tries to enqueue, and a queue that is full means the client is not draining its
//! socket — at which point the connection is dropped rather than waited on.
//!
//! Dropping it is the honest answer: on a stream there is no such thing as skipping one event and
//! carrying on, since the client would silently hold a state that never becomes right again.
//! Closing the socket tells it so, and a client that reconnects and subscribes gets a fresh
//! snapshot.
//!
//! # What a client has to size for
//!
//! A line has no length field, so a client reads until `\n` — which needs a bound, or a peer
//! decides how much it buffers. That bound is published rather than left to guess: the longest
//! line this server can produce is a `cover` answer at [`MAX_COVER_BYTES`], which base64 grows by
//! a third before the JSON envelope goes round it, so **2.8 MB is enough for any line** and
//! `the_longest_line_is_bounded` fails if that stops being true. Every other line is a few hundred
//! bytes.
//!
//! That is the second job [`MAX_COVER_BYTES`] does, and the reason it still exists now that no
//! datagram has to hold a picture: without a cap on what is accepted there is no figure to hand a
//! client, and "read until newline" has no safe implementation.
//!
//! The other half is a read timeout, and what it may mean. This server sends nothing at all while
//! nothing happens, so silence on an idle connection is normal and must not be mistaken for a
//! stall — a timeout is only evidence of trouble *part-way through a line*, where the rest was
//! promised and never came. A client that also wants to notice a peer that vanished without
//! closing the socket (a Pi losing power, Wi-Fi dropping) sends something itself now and then and
//! expects the answer; `status` is there for exactly that and costs the server no state.
//! `contrib/api_test.py` implements all of this the long way round, to be read as the spec.
//!
//! # Cover art
//!
//! A cover is **pushed** to subscribers, as its own `cover` event carrying the whole picture, the
//! moment there is one — and again to a client that subscribes while something is already
//! playing. A subscriber never asks for artwork.
//!
//! It is a separate event rather than a field on `track_changed` because the picture is not there
//! yet when the track changes: it is fetched from Spotify, which takes a few hundred
//! milliseconds. Folding it in would mean holding the track event back for it, which is what an
//! earlier design did and is worth not repeating — what is playing should be reported the instant
//! it is known. So `track_changed` arrives first and means "forget the old artwork", and the
//! `cover` event follows if there is any. A track with no cover simply never produces one.
//!
//! There is no way to *ask* for a cover, and no event saying a track hasn't got one. Both existed
//! and both are gone, because a push makes them unnecessary and neither survived the question of
//! what a client would do with them. Asking meant a client had to hold request state and tell an
//! answer from an event; a negative answer meant distinguishing "no cover" from "not yet", which
//! needed a `pending` flag to be correct at all. Under a push, `track_changed` clears the artwork
//! and a `cover` either follows or doesn't — a track with no picture, one whose fetch failed, and
//! one still fetching are all simply quiet, and a client that draws nothing until a `cover`
//! arrives is right in every one of those cases without being told which it is.
//!
//! **Nothing re-encodes a picture.** Every cover is held and sent exactly as it arrived, up to
//! [`MAX_COVER_BYTES`], and Spotify's *largest* cover is the one fetched. There used to be a
//! shrink above 256 kB, down to 640 px of JPEG; it existed because UDP could not deliver iTunes
//! on Windows pushing 1.4 MB of PNG, and on a stream that is simply a file transfer. What it cost
//! was the one thing a client actually notices — the picture — so it went, and the `image`
//! dependency with it.
//!
//! Urls are not reported at all: the clients here cannot reach the internet, only the Pi can. A
//! cover is fetched once per track no matter how many clients are listening, encoded once when it
//! is pushed, and the last few are cached, so skipping back and forth doesn't refetch.

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
#[cfg(feature = "airplay")]
use tokio::net::UdpSocket;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{
        TcpListener, TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::mpsc,
    task::JoinHandle,
    time::timeout,
};

/// The address the control API listens on by default.
pub const DEFAULT_BIND_ADDR: &str = "0.0.0.0:50505";

/// How many messages may be waiting for one client before its connection is dropped.
///
/// The queue is what keeps a client from reaching the server task at all: the loop enqueues and
/// moves on, and a writer task of its own does the blocking. Sixteen is far more slack than a
/// client on a LAN needs — the kernel's own send buffer already absorbs a burst several times
/// this size — so a full queue is not a busy client but one that has stopped reading. See the
/// module docs on why that ends the connection instead of skipping an event.
const CONNECTION_QUEUE: usize = 16;

/// The longest request line accepted, newline included.
///
/// Commands are short and only `setvol` carries a payload, so this is roomy. It exists because a
/// stream has no natural message boundary: without a cap, a peer that never sends a newline would
/// have this process buffer whatever it feels like sending, and this process is what plays the
/// music.
const MAX_REQUEST_BYTES: u64 = 1024;

/// A cover past this is refused rather than held.
///
/// It stopped being about what a datagram can carry and now earns its place twice over. It is the
/// only bound on memory here: an AirPlay sender pushes artwork over DMAP and this process believes
/// what it is told, so without a cap a malfunctioning one decides how much a Pi holds — times
/// [`COVER_CACHE_SIZE`], plus the base64 built for each client that asks. And it is the figure a
/// client sizes its line buffer from, since "read until newline" is only safe with a limit and the
/// limit has to come from somewhere (see the module docs, and `the_longest_line_is_bounded`).
///
/// A couple of megabytes of artwork is a sender malfunctioning, not being generous. Everything
/// under it is held and sent exactly as it arrived, so what a client gets is what the source
/// published.
pub const MAX_COVER_BYTES: usize = 2 * 1024 * 1024;

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
    /// The url it was fetched from, which is what the cache keys on.
    url: String,
    /// `None` when it could not be produced; the track then simply has no picture to ask for.
    cover: Option<Cover>,
}

/// A cover as it is held: raw bytes, base64-encoded only when the `cover` command asks for it, so
/// a picture nobody asks about is never encoded at all.
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
    /// Binds the listening socket and spawns the server task.
    ///
    /// Binding happens before the task is spawned, so a failure to take the port is reported
    /// here instead of getting lost in the background.
    pub async fn spawn(
        config: ApiServerConfig,
        player_events: PlayerEventChannel,
    ) -> Result<ApiServer, ApiServerError> {
        let listener = TcpListener::bind(&config.bind_addr)
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
        let (conn_tx, conn_rx) = mpsc::unbounded_channel();

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
                listener,
                allow_list: config.allow_list,
                spirc: None,
                session: None,
                cmd_rx,
                player_events,
                connections: HashMap::new(),
                next_connection_id: 0,
                conn_tx,
                conn_rx,
                current_track: TrackResponse::default(),
                current_volume: VolumeResponse::default(),
                playback: PlaybackState::default(),
                covers: CoverCache::default(),
                current_cover: None,
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
    /// The cover of whatever is playing: the whole picture, base64, in one line.
    ///
    /// Pushed when a picture arrives, and to a client that has just subscribed while something is
    /// already playing. It only ever goes out with a picture in it — a track with no cover simply
    /// never produces one, and `track_changed` is what tells a client to drop the old artwork.
    Cover {
        mime: &'a str,
        bytes: usize,
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

/// Fetches a cover and hands it back to the server task.
///
/// Runs off the task's loop, which has to keep answering commands meanwhile. Always reports back,
/// including when it failed, since a track event is waiting on it.
async fn fetch_cover(session: Session, url: String, cover_tx: mpsc::UnboundedSender<CoverFetched>) {
    let cover = match timeout(COVER_FETCH_TIMEOUT, session.spclient().request_url(&url)).await {
        Ok(Ok(bytes)) if bytes.len() > MAX_COVER_BYTES => {
            warn!(
                "cover {url} is {} bytes, past the {MAX_COVER_BYTES}-byte limit",
                bytes.len()
            );
            None
        }
        Ok(Ok(bytes)) => {
            debug!("fetched {} bytes of cover from {url}", bytes.len());

            Some(Cover {
                mime: cover_mime(&bytes),
                bytes: bytes.to_vec(),
            })
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
    let _ = cover_tx.send(CoverFetched { url, cover });
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

/// One connected client.
///
/// The loop never touches the socket: it hands a line to `tx` and a writer task of its own does
/// the waiting. Dropping this closes the connection — see [`ApiServerTask::disconnect`].
struct Connection {
    peer: SocketAddr,
    tx: mpsc::Sender<String>,
    /// Whether it asked for pushes. A connection is not a subscription: a client may just as well
    /// connect, ask `status` and leave.
    subscribed: bool,
}

/// What a connection's reader task tells the server task about.
enum ConnectionEvent {
    Request {
        id: u64,
        line: String,
    },
    /// The peer closed the connection, or reading from it failed.
    Closed {
        id: u64,
    },
}

struct ApiServerTask {
    listener: TcpListener,
    allow_list: AllowList,
    spirc: Option<Spirc>,
    /// What covers are fetched with. `None` until the first connect.
    session: Option<Session>,
    cmd_rx: mpsc::UnboundedReceiver<ApiServerCommand>,
    player_events: PlayerEventChannel,
    /// Every client currently connected, by the id [`Self::admit`] gave it.
    ///
    /// Keyed by an id of its own rather than by peer address: a client that reconnects can land on
    /// the same ephemeral port it just used, and its old connection's `Closed` would then arrive
    /// after the new one registered and unregister the live connection.
    connections: HashMap<u64, Connection>,
    next_connection_id: u64,
    /// Both ends of the connection-event channel, so the loop ends on `cmd_rx` closing rather than
    /// on the last client leaving.
    conn_tx: mpsc::UnboundedSender<ConnectionEvent>,
    conn_rx: mpsc::UnboundedReceiver<ConnectionEvent>,
    current_track: TrackResponse,
    current_volume: VolumeResponse,
    playback: PlaybackState,
    covers: CoverCache,
    /// The cover the current track is waiting for, if any. Its `track_changed` goes out once that
    /// fetch reports back, so that the event carries the picture with it.
    /// The cover of whatever is playing, once there is one. Never part of a track event or any
    /// other response — a client asks for it with `cover` and gets it in chunks.
    current_cover: Option<Cover>,
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
    /// The socket commands are forwarded to the helper on. The API itself speaks TCP; this is the
    /// one thing here that still sends a datagram, since the helper is a small script that listens
    /// for one. Nothing ever reads it — a helper answers what it is sent, and that answer is meant
    /// to be dropped.
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

/// Reads one connection's requests and forwards them to the server task, a line at a time.
///
/// Ends on end of stream, on a read error, or on a line past [`MAX_REQUEST_BYTES`] — and always
/// reports [`ConnectionEvent::Closed`], so the server task forgets the connection however it went
/// away.
async fn read_connection(
    read: OwnedReadHalf,
    id: u64,
    events: mpsc::UnboundedSender<ConnectionEvent>,
) {
    let mut reader = BufReader::new(read);
    let mut line = Vec::new();

    loop {
        line.clear();

        // Bounded rather than plain `lines()`: the limit is what a stream doesn't give for free,
        // and without one a peer that sends no newline decides how much this process buffers. The
        // `take` is rebuilt each round, so the limit is per line rather than per connection.
        let read = (&mut reader)
            .take(MAX_REQUEST_BYTES)
            .read_until(b'\n', &mut line)
            .await;

        match read {
            Ok(0) => break,
            // no newline within the limit: either an overlong line or a peer that left mid-request
            Ok(_) if !line.ends_with(b"\n") => {
                debug!(
                    "connection {id} ended after {} bytes of request",
                    line.len()
                );
                break;
            }
            Ok(_) => {
                let request = String::from_utf8_lossy(&line).trim().to_string();

                // clients send a bare newline as a poor man's keepalive; nothing to do about it
                if request.is_empty() {
                    continue;
                }

                if events
                    .send(ConnectionEvent::Request { id, line: request })
                    .is_err()
                {
                    // the server task is gone, so there is nobody left to report to either
                    return;
                }
            }
            Err(e) => {
                debug!("could not read from connection {id}: {e}");
                break;
            }
        }
    }

    let _ = events.send(ConnectionEvent::Closed { id });
}

/// Writes one connection's queued lines, and does all the waiting a slow client causes.
///
/// Ends when the queue closes, which is what [`ApiServerTask::disconnect`] does. The read half
/// lives in its own task, so this has to shut the socket down explicitly — otherwise dropping a
/// [`Connection`] would only close half of it, and the client would sit there reading nothing.
async fn write_connection(mut write: OwnedWriteHalf, mut lines: mpsc::Receiver<String>) {
    while let Some(line) = lines.recv().await {
        if let Err(e) = write.write_all(line.as_bytes()).await {
            debug!("could not write to a client: {e}");
            break;
        }
    }

    let _ = write.shutdown().await;
}

impl ApiServerTask {
    /// Two full definitions rather than `#[cfg]`s inside one `select!`: per-branch `#[cfg]`,
    /// though documented as supported, fails with a macro-parse error for this branch shape on
    /// the pinned `tokio`.
    #[cfg(feature = "airplay")]
    async fn run(mut self) {
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
                Some(fetched) = self.cover_rx.recv() => self.handle_cover(fetched),
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
                accepted = self.listener.accept() => match accepted {
                    Ok((stream, peer)) => self.accept(stream, peer),
                    Err(e) => warn!("could not accept an API connection: {e}"),
                },
                // likewise held by this task, so it cannot close under it
                Some(event) = self.conn_rx.recv() => match event {
                    ConnectionEvent::Request { id, line } => self.handle_request(&line, id).await,
                    ConnectionEvent::Closed { id } => self.disconnect(id),
                },
            }
        }
    }

    /// See the `#[cfg(feature = "airplay")]` twin above for why this is a full second copy rather
    /// than one `#[cfg]`-laden body — identical except for the one AirPlay-events branch.
    #[cfg(not(feature = "airplay"))]
    async fn run(mut self) {
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
                Some(fetched) = self.cover_rx.recv() => self.handle_cover(fetched),
                event = self.player_events.recv(), if player_events_open => match event {
                    Some(event) => self.handle_player_event(event).await,
                    None => {
                        debug!("player event channel closed");
                        player_events_open = false;
                    }
                },
                accepted = self.listener.accept() => match accepted {
                    Ok((stream, peer)) => self.accept(stream, peer),
                    Err(e) => warn!("could not accept an API connection: {e}"),
                },
                Some(event) = self.conn_rx.recv() => match event {
                    ConnectionEvent::Request { id, line } => self.handle_request(&line, id).await,
                    ConnectionEvent::Closed { id } => self.disconnect(id),
                },
            }
        }
    }

    /// Takes a connection the allow list lets in and sets its two tasks going, or drops it on the
    /// floor — which closes it, and is the whole answer a rejected peer gets.
    fn accept(&mut self, stream: TcpStream, peer: SocketAddr) {
        let (id, lines) = match self.admit(peer) {
            Some(admitted) => admitted,
            None => return,
        };

        // every line here is small and worth sending the moment it exists; there is no stream of
        // bulk data for Nagle to coalesce, only latency for it to add
        if let Err(e) = stream.set_nodelay(true) {
            debug!("could not disable Nagle for {peer}: {e}");
        }

        let (read, write) = stream.into_split();
        tokio::spawn(write_connection(write, lines));
        tokio::spawn(read_connection(read, id, self.conn_tx.clone()));
    }

    /// Registers a connection and hands back its id and the queue its writer task drains, or
    /// `None` when the peer is not on the allow list.
    ///
    /// The allow list is checked once here rather than per request, which is the one thing a
    /// stream makes cheaper than datagrams did: a peer that isn't welcome is refused at the
    /// connection and logged once, instead of on every message it sends.
    fn admit(&mut self, peer: SocketAddr) -> Option<(u64, mpsc::Receiver<String>)> {
        if !self.allow_list.allows(peer.ip()) {
            warn!("rejecting a connection from {peer}: not on the allow list");
            return None;
        }

        let id = self.next_connection_id;
        self.next_connection_id += 1;

        let (tx, rx) = mpsc::channel(CONNECTION_QUEUE);
        self.connections.insert(
            id,
            Connection {
                peer,
                tx,
                subscribed: false,
            },
        );

        info!("{peer} connected to the API");

        Some((id, rx))
    }

    /// Forgets a connection, which closes it: dropping the [`Connection`] drops the queue's
    /// sender, the writer task's loop ends, and its `shutdown` takes the socket down with it.
    fn disconnect(&mut self, id: u64) {
        if let Some(connection) = self.connections.remove(&id) {
            info!("{} disconnected from the API", connection.peer);
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
                self.announce_track();
            }
            PlayerEvent::Playing { position_ms, .. } => {
                #[cfg(feature = "airplay")]
                {
                    self.active_source = Source::Spotify;
                }
                self.update_playback(Some(true), position_ms)
            }
            PlayerEvent::Paused { position_ms, .. } => {
                self.update_playback(Some(false), position_ms)
            }
            PlayerEvent::Stopped { .. } => self.update_playback(Some(false), 0),
            // a seek or a drift correction moves the position without touching play / pause
            PlayerEvent::Seeked { position_ms, .. }
            | PlayerEvent::PositionCorrection { position_ms, .. }
            | PlayerEvent::Loading { position_ms, .. } => self.update_playback(None, position_ms),
            PlayerEvent::VolumeChanged { volume } => {
                self.current_volume.volume = volume;
                self.broadcast(encode(&Event::VolumeChanged { volume }));
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
                self.clear_now_playing();
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
    fn clear_now_playing(&mut self) {
        self.current_track = TrackResponse::default();
        self.current_cover = None;
        self.announce_track();
        self.update_playback(Some(false), 0);
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
                self.update_playback(Some(is_playing), position_ms);
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
                self.announce_track();

                // The DACP path fetches the picture before reporting the track, so it arrives
                // here; the push path sends it as its own request moments later. Either way the
                // sender handed over the bytes, so there is nothing to wait for and nothing to
                // cache it under — it is simply held.
                if let Some((bytes, mime)) = cover {
                    self.announce_cover(Cover { bytes, mime });
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
                self.announce_cover(Cover { bytes, mime });
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
                self.update_playback(None, position_ms);
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
                self.broadcast(encode(&Event::VolumeChanged { volume }));
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
                    self.clear_now_playing();
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

    /// Starts fetching the cover of the track that just changed, or pushes it straight out if it
    /// is already held.
    ///
    /// The track event does **not** wait for it — see the module docs on why the two are separate
    /// events. Whichever way the picture turns up, subscribers are sent it without asking.
    ///
    /// Clearing `current_cover` first is what makes a track with no artwork, an unreachable one,
    /// or a fetch that never lands all look the same from outside: no `cover` event follows the
    /// `track_changed`, and the client's own artwork stays cleared. Nothing has to report a
    /// negative.
    fn load_cover(&mut self, url: Option<String>) {
        self.current_cover = None;

        let Some(url) = url else {
            return;
        };
        if let Some(cover) = self.covers.get(&url).cloned() {
            // Straight to `announce_cover`, not just into the field: a cache hit is a picture
            // arriving as much as a fetch is, and skipping back to a track whose cover is still
            // held must not leave subscribers with nothing.
            self.announce_cover(cover);
            return;
        }
        let Some(session) = self.session.clone() else {
            // before the first connect there is nothing to fetch with
            debug!("no session to fetch the cover with yet");
            return;
        };

        tokio::spawn(fetch_cover(session, url, self.cover_tx.clone()));
    }

    /// Takes a fetched cover and pushes it to subscribers.
    ///
    /// A fetch that failed or timed out reports back too, carrying no cover, and there is nothing
    /// to do about it: no event goes out, and the client keeps showing nothing, which is what it
    /// has shown since `track_changed`.
    fn handle_cover(&mut self, fetched: CoverFetched) {
        // worth keeping even if a newer track overtook this fetch: whatever overtook it was most
        // likely a skip, and a skip back wants this picture again
        let Some(cover) = fetched.cover else {
            return;
        };

        self.covers.insert(fetched.url, cover.clone());
        self.announce_cover(cover);
    }

    /// Holds a cover for the current track and pushes it to subscribers.
    ///
    /// The picture itself, not a note that one exists: a subscriber is subscribed so that it does
    /// not have to ask for anything. It is encoded once here however many clients are listening.
    fn announce_cover(&mut self, cover: Cover) {
        self.current_cover = Some(cover);

        // Nobody listening means nobody to encode for, and this is the one expensive thing the
        // task does — worth the check, since a track change reaches here whether or not anyone
        // subscribed.
        if !self.has_subscribers() {
            return;
        }

        let payload = self.encode_cover();
        self.broadcast(payload);
    }

    /// The held cover as the `cover` event, picture and all.
    ///
    /// `None` when there is no picture — and then nothing is sent, because there is no such thing
    /// as an empty `cover` event: a track without artwork simply never produces one.
    fn encode_cover(&self) -> Option<String> {
        let cover = self.current_cover.as_ref()?;
        debug!("sending {} bytes of cover", cover.bytes.len());

        encode(&Event::Cover {
            mime: cover.mime,
            bytes: cover.bytes.len(),
            data: BASE64.encode(&cover.bytes),
        })
    }

    fn has_subscribers(&self) -> bool {
        self.connections
            .values()
            .any(|connection| connection.subscribed)
    }

    fn announce_track(&mut self) {
        self.broadcast(encode(&Event::TrackChanged {
            track: &self.current_track,
        }));
    }

    fn update_playback(&mut self, is_playing: Option<bool>, position_ms: u32) {
        self.playback.update(is_playing, position_ms);

        self.broadcast(encode(&Event::PlaybackChanged {
            is_playing: self.playback.is_playing,
            position_ms,
        }));
    }

    /// Handles one request line from a connection the allow list already let in.
    async fn handle_request(&mut self, request: &str, id: u64) {
        let Some(peer) = self.connections.get(&id).map(|connection| connection.peer) else {
            // it closed between the read and here, and its `Closed` is right behind this
            return;
        };

        let mut parts = request.splitn(2, ' ');
        let command = parts.next().unwrap_or_default();
        let payload = parts.next().unwrap_or_default().trim();

        // one line per command, and — unlike the lease keepalives this replaced — none of them
        // arrive on a timer, so all of them are worth seeing without RUST_LOG
        info!("received '{command}' from {peer}");

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
            "getvol" => self.send(encode(&self.current_volume), id),
            "current_track" => self.send(encode(&self.current_track), id),
            "status" => self.send(encode(&self.snapshot()), id),
            "subscribe" => {
                let is_new = self.subscribe(id);
                self.send(encode(&self.snapshot()), id);

                // The picture of whatever is already playing, which went out to whoever was
                // subscribed when it arrived. Without this a client joining mid-track shows no
                // artwork until the next track produces a new one — and, since a subscriber sends
                // nothing but `subscribe`, it would have no way to fix that itself.
                //
                // Only for a genuinely new subscription: subscribing twice on one connection must
                // not resend a picture the client already has.
                if is_new {
                    let payload = self.encode_cover();
                    self.send(payload, id);
                }
            }
            "unsubscribe" => self.unsubscribe(id),
            other => warn!("unknown command '{other}' from {peer}"),
        }
    }

    /// Puts a connection on the push list.
    ///
    /// `true` when it wasn't on it already — the caller uses that to tell a first-time subscriber
    /// about a cover that is already here, without repeating it if it subscribes again.
    fn subscribe(&mut self, id: u64) -> bool {
        let Some(connection) = self.connections.get_mut(&id) else {
            return false;
        };

        if connection.subscribed {
            return false;
        }

        connection.subscribed = true;
        info!("{} subscribed to API events", connection.peer);

        true
    }

    /// Takes a connection off the push list, leaving it open to keep asking things.
    fn unsubscribe(&mut self, id: u64) {
        if let Some(connection) = self.connections.get_mut(&id) {
            if connection.subscribed {
                connection.subscribed = false;
                info!("{} unsubscribed from API events", connection.peer);
            }
        }
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

    /// Sends an event to every subscribed connection.
    ///
    /// Takes an already-encoded payload rather than something to serialize, because most events
    /// borrow from `self` (`&self.current_track`) while this needs `&mut self` to drop whatever
    /// connections turn out to be gone.
    fn broadcast(&mut self, payload: Option<String>) {
        let Some(mut line) = payload else {
            return;
        };
        line.push('\n');

        // collected so that the sends don't borrow the connection list
        let subscribed: Vec<u64> = self
            .connections
            .iter()
            .filter(|(_, connection)| connection.subscribed)
            .map(|(id, _)| *id)
            .collect();

        for id in subscribed {
            self.send_line(line.clone(), id);
        }
    }

    /// Answers one connection.
    ///
    /// The newline that frames the message is appended in place: the payload is owned already, so
    /// a cover's base64 is never copied a second time just to be framed.
    fn send(&mut self, payload: Option<String>, id: u64) {
        let Some(mut line) = payload else {
            return;
        };
        line.push('\n');

        self.send_line(line, id);
    }

    /// Queues a line for one connection, or drops the connection if it cannot take it.
    ///
    /// Never waits: the queue is what stands between a client that stopped reading and the loop
    /// that also serves the player. A full one means it isn't draining its socket, and on a stream
    /// there is no way to skip an event and leave the client's state consistent — so it is told
    /// the only way that works, by having the connection closed under it.
    fn send_line(&mut self, line: String, id: u64) {
        let Some(connection) = self.connections.get(&id) else {
            return;
        };

        let reason = match connection.tx.try_send(line) {
            Ok(()) => return,
            Err(mpsc::error::TrySendError::Full(_)) => "it is not keeping up",
            // the writer task ended, so the socket is already down and `Closed` is on its way
            Err(mpsc::error::TrySendError::Closed(_)) => "it has gone away",
        };

        warn!("dropping the connection to {}: {reason}", connection.peer);
        self.disconnect(id);
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
    /// A middling one out of what the fixture ships, so a test that names it is not the one
    /// [`cover_to_ship`] would pick anyway.
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

    /// `\n` is the frame boundary, so a payload that contained one would split a message in two
    /// and leave the client parsing halves. Nothing carries newlines today, but a track title is
    /// whatever a sender pushed and `serde_json` is what has to keep escaping them.
    #[test]
    fn a_payload_never_contains_a_raw_newline() {
        let track = TrackResponse {
            song_name: "Sun\nIs\r\nShining".to_string(),
            album: "Ka\nya".to_string(),
            ..TrackResponse::default()
        };

        let payload = encode(&Event::TrackChanged { track: &track }).expect("encodes");

        assert!(!payload.contains('\n'), "{payload}");
        assert!(!payload.contains('\r'), "{payload}");
        // and it still round-trips, so the escaping is what keeps it out rather than a mangling
        let parsed: serde_json::Value = serde_json::from_str(&payload).expect("valid json");
        assert_eq!(parsed["track"]["song_name"], "Sun\nIs\r\nShining");
    }

    /// The number a client sizes its line buffer from, pinned so it cannot drift out of the docs
    /// and out of `contrib/api_test.py`.
    ///
    /// A client reads until `\n` and needs a limit to do that safely; this is where the limit
    /// comes from. Built at its worst: a full [`MAX_COVER_BYTES`] of picture and the longest mime
    /// [`cover_mime`] can report.
    #[test]
    fn the_longest_line_is_bounded() {
        /// What the module docs and `api_test.py` tell clients to allow for.
        const PUBLISHED: usize = 2_800_000;

        let bytes = vec![0xab; MAX_COVER_BYTES];
        let payload = encode(&Event::Cover {
            mime: "application/octet-stream",
            bytes: bytes.len(),
            data: BASE64.encode(&bytes),
        })
        .expect("encodes");

        // plus the newline `send` appends, which the client has to read too
        let line = payload.len() + 1;

        assert!(
            line <= PUBLISHED,
            "the longest line is {line} bytes, past the {PUBLISHED} clients are told to allow"
        );
        // and not so far under it that the published figure has quietly become nonsense
        assert!(
            line > PUBLISHED / 2,
            "the longest line is only {line} bytes"
        );
    }

    /// The bytes go out as they came in, whatever they are and however many. There used to be a
    /// re-encode above 256 kB, which UDP needed and a stream does not; a client gets the artwork,
    /// not a version of it. Also the biggest line the push path actually produces.
    #[tokio::test]
    async fn a_cover_is_sent_exactly_as_it_arrived() {
        // The size iTunes on Windows pushes, which is what used to be re-encoded. Bytes that vary
        // rather than a run of one value, so a truncation or a re-encode could not pass unnoticed.
        let png: Vec<u8> = (0..1_400_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 16) as u8)
            .collect();

        let mut server = TestServer::start_with(AllowList::default(), |task| {
            task.current_cover = Some(Cover {
                bytes: png.clone(),
                mime: "image/png",
            });
        })
        .await;

        // the only thing a client ever sends, and the picture follows the snapshot
        server.request("subscribe").await;
        assert_eq!(server.receive().await["event"], "snapshot");

        let pushed = server.receive().await;
        assert_eq!(pushed["event"], "cover");
        assert_eq!(pushed["mime"], "image/png", "the format is not changed");
        assert_eq!(pushed["bytes"], png.len());

        let received = BASE64
            .decode(pushed["data"].as_str().expect("base64 text").as_bytes())
            .expect("valid base64");

        assert_eq!(received, png, "the picture is not the one that was put in");
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
    /// event sender, and a connection to talk to it over.
    struct TestServer {
        events: mpsc::UnboundedSender<PlayerEvent>,
        requests: OwnedWriteHalf,
        responses: BufReader<OwnedReadHalf>,
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
        let (conn_tx, conn_rx) = mpsc::unbounded_channel();

        let task = ApiServerTask {
            listener: TcpListener::bind("127.0.0.1:0").await.expect("binds"),
            allow_list,
            spirc: None,
            session: None,
            cmd_rx,
            player_events,
            connections: HashMap::new(),
            next_connection_id: 0,
            conn_tx,
            conn_rx,
            current_track: TrackResponse::default(),
            current_volume: VolumeResponse::default(),
            playback: PlaybackState::default(),
            covers: CoverCache::default(),
            current_cover: None,
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
            let addr = task.listener.local_addr().expect("has an address");

            tokio::spawn(task.run());

            let (read, requests) = TcpStream::connect(addr)
                .await
                .expect("connects")
                .into_split();

            Self {
                events,
                requests,
                responses: BufReader::new(read),
                _cmd_tx: cmd_tx,
            }
        }

        async fn request(&mut self, command: &str) {
            self.requests
                .write_all(format!("{command}\n").as_bytes())
                .await
                .expect("sends");
        }

        async fn receive(&mut self) -> serde_json::Value {
            let mut line = String::new();

            let read =
                tokio::time::timeout(Duration::from_secs(2), self.responses.read_line(&mut line))
                    .await
                    .expect("a line arrives in time")
                    .expect("receives");

            assert_ne!(read, 0, "the server closed the connection");

            serde_json::from_str(&line).expect("valid json")
        }

        /// Whether nothing arrives in the time a loopback round trip would need many times over.
        async fn receives_nothing(&mut self) -> bool {
            let mut line = String::new();

            tokio::time::timeout(
                Duration::from_millis(250),
                self.responses.read_line(&mut line),
            )
            .await
            .is_err()
        }
    }

    /// No response carries a picture: a track event is small, and the cover is asked for
    /// separately. A real sender's artwork runs to ~180 kB, which base64 turns into 240 kB, and
    /// most clients want it at most once per track — nothing that belongs in an event fired at
    /// every seek.
    #[tokio::test]
    async fn no_response_carries_the_cover() {
        let mut server = TestServer::start_with(AllowList::default(), |task| {
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

    /// A track without artwork, and a fetch that fails, are both simply quiet. There is no empty
    /// `cover` event and nothing for a client to interpret: it drew nothing from `track_changed`
    /// onwards and stays right.
    #[tokio::test]
    async fn a_cover_that_never_arrives_is_silence() {
        let mut cover_tx = None;

        let mut server = TestServer::start_with(AllowList::default(), |task| {
            cover_tx = Some(task.cover_tx.clone());
        })
        .await;

        // subscribing with nothing playing must not produce a cover event of its own
        server.request("subscribe").await;
        assert_eq!(server.receive().await["event"], "snapshot");

        // and neither must a fetch coming back empty, which is what a failure or timeout looks
        // like from the task's side
        cover_tx
            .expect("the task handed one over")
            .send(CoverFetched {
                url: COVER_URL.to_string(),
                cover: None,
            })
            .expect("the task is running");

        assert!(
            server.receives_nothing().await,
            "a cover that isn't there should produce no event at all"
        );
    }

    /// The point of subscribing: everything arrives without being asked for, artwork included.
    /// A subscriber that had to send `cover` would need request/response state, retries and a way
    /// to tell an answer from an event — all of which is what a push removes.
    #[tokio::test]
    async fn a_cover_is_pushed_to_subscribers_without_being_asked_for() {
        let mut cover_tx = None;

        let mut server = TestServer::start_with(AllowList::default(), |task| {
            cover_tx = Some(task.cover_tx.clone());
        })
        .await;

        server.request("subscribe").await;
        assert_eq!(server.receive().await["event"], "snapshot");

        // what a fetch landing looks like from the task's side
        cover_tx
            .expect("the task handed one over")
            .send(CoverFetched {
                url: COVER_URL.to_string(),
                cover: Some(Cover {
                    bytes: vec![0xab; 100],
                    mime: "image/jpeg",
                }),
            })
            .expect("the task is running");

        let pushed = server.receive().await;
        assert_eq!(pushed["event"], "cover");
        assert_eq!(pushed["mime"], "image/jpeg");
        assert_eq!(pushed["bytes"], 100);
        assert_eq!(
            BASE64
                .decode(pushed["data"].as_str().expect("base64 text").as_bytes())
                .expect("valid base64"),
            vec![0xab; 100],
            "the push has to carry the picture, not a note that one exists"
        );
    }

    /// A client that subscribes while something is already playing gets its picture too. The push
    /// went out when the cover arrived, which was before this client existed — and since a
    /// subscriber never asks for anything, without this it would show no artwork until the next
    /// track.
    #[tokio::test]
    async fn a_late_subscriber_is_sent_the_cover_already_playing() {
        // The state a client walks in on: something is already playing and its picture is here.
        let mut server = TestServer::start_with(AllowList::default(), |task| {
            task.current_cover = Some(Cover {
                bytes: vec![0xab; 100],
                mime: "image/jpeg",
            });
        })
        .await;

        server.request("subscribe").await;

        let snapshot = server.receive().await;
        assert_eq!(snapshot["event"], "snapshot");

        let cover = server.receive().await;
        assert_eq!(cover["event"], "cover");
        assert_eq!(cover["mime"], "image/jpeg");
        assert_eq!(cover["bytes"], 100);
    }

    /// A cover in the cache is a picture arriving as much as a fetched one is. Skipping back to a
    /// track whose cover is still held used to set it and tell nobody, which under a push model
    /// leaves the client showing nothing.
    #[tokio::test]
    async fn a_cached_cover_is_pushed_like_any_other() {
        let (mut task, ..) = idle_task(AllowList::default()).await;

        let (id, mut queue) = task
            .admit("127.0.0.1:1234".parse().expect("parses"))
            .expect("loopback is allowed");
        task.subscribe(id);
        task.covers.insert(
            COVER_URL.to_string(),
            Cover {
                bytes: vec![0xcd; 100],
                mime: "image/jpeg",
            },
        );

        task.load_cover(Some(COVER_URL.to_string()));

        let line = queue.try_recv().expect("a cover was pushed");
        let pushed: serde_json::Value = serde_json::from_str(line.trim()).expect("valid json");
        assert_eq!(pushed["event"], "cover");
        assert_eq!(pushed["bytes"], 100);
    }

    /// A client that subscribes again on a connection that already is must not be sent the same
    /// picture a second time — it is the one big thing on this wire.
    #[tokio::test]
    async fn subscribing_twice_does_not_resend_the_cover() {
        let mut server = TestServer::start_with(AllowList::default(), |task| {
            task.current_cover = Some(Cover {
                bytes: vec![0xab; 100],
                mime: "image/jpeg",
            });
        })
        .await;

        server.request("subscribe").await;
        assert_eq!(server.receive().await["event"], "snapshot");
        assert_eq!(server.receive().await["event"], "cover");

        server.request("subscribe").await;
        assert_eq!(server.receive().await["event"], "snapshot");

        assert!(
            server.receives_nothing().await,
            "subscribing again sent the cover a second time"
        );
    }

    /// There is no way to ask for a cover any more, and the command that did is gone rather than
    /// left lying around: a subscriber is sent one, and nothing else needs one.
    #[tokio::test]
    async fn there_is_no_cover_command() {
        let mut server = TestServer::start_with(AllowList::default(), |task| {
            task.current_cover = Some(Cover {
                bytes: vec![0xab; 100],
                mime: "image/jpeg",
            });
        })
        .await;

        server.request("cover").await;

        assert!(
            server.receives_nothing().await,
            "`cover` is not a command and must not answer like one"
        );
    }

    #[tokio::test]
    async fn subscribers_are_pushed_to_until_they_unsubscribe() {
        let mut server = TestServer::start(AllowList::default()).await;

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

    /// The allow list is now checked once, when the connection is accepted, so this is where a
    /// peer that isn't welcome is turned away — before it can send anything at all.
    ///
    /// Driven through `admit` rather than over the socket, because a test client is always on
    /// loopback and loopback is allowed unconditionally.
    #[tokio::test]
    async fn the_allow_list_keeps_others_from_connecting() {
        let (mut task, ..) = idle_task("192.168.2.0/24".parse().expect("parses")).await;

        assert!(
            task.admit("8.8.8.8:1234".parse().expect("parses"))
                .is_none(),
            "a client outside the allow list should be refused the connection"
        );
        assert!(task.connections.is_empty());

        assert!(
            task.admit("192.168.2.5:1234".parse().expect("parses"))
                .is_some(),
            "a listed client should be served"
        );
        assert_eq!(task.connections.len(), 1);
    }

    #[tokio::test]
    async fn a_local_client_is_served_without_being_listed() {
        let mut server = TestServer::start("192.168.2.0/24".parse().expect("parses")).await;

        server.request("subscribe").await;

        assert_eq!(server.receive().await["event"], "snapshot");
    }

    /// The connection is the subscription: there is no lease to run out, and nothing to sweep, so
    /// a client that goes away has to be noticed by the socket closing or it is pushed to forever.
    #[tokio::test]
    async fn a_client_that_disconnects_is_forgotten() {
        let mut server = TestServer::start(AllowList::default()).await;

        server.request("subscribe").await;
        assert_eq!(server.receive().await["event"], "snapshot");

        drop(server.requests);
        drop(server.responses);

        // the events keep flowing; the point is that nothing tries to push them to a socket that
        // is gone, and that the task neither panics nor holds the connection open
        for volume in [1_234, 4_321] {
            server
                .events
                .send(PlayerEvent::VolumeChanged { volume })
                .expect("the task is running");
        }

        tokio::time::sleep(Duration::from_millis(100)).await;

        // it is still serving, which is the only thing observable from out here
        let mut second = TestServer::start(AllowList::default()).await;
        second.request("status").await;
        assert_eq!(second.receive().await["event"], "snapshot");
    }

    /// A client that stops reading must not hold up the task that also serves the player, so its
    /// queue fills and the connection goes rather than the loop waiting on it.
    #[tokio::test]
    async fn a_client_that_stops_reading_is_dropped() {
        let (mut task, ..) = idle_task(AllowList::default()).await;

        let (id, queue) = task
            .admit("127.0.0.1:1234".parse().expect("parses"))
            .expect("loopback is allowed");
        task.subscribe(id);

        // nothing drains it, which is exactly what a client that stopped reading looks like once
        // its socket buffer is full
        for _ in 0..CONNECTION_QUEUE + 1 {
            task.broadcast(encode(&Event::VolumeChanged { volume: 1_234 }));
        }

        assert!(
            task.connections.is_empty(),
            "a client that never drains its queue should have been dropped"
        );
        drop(queue);
    }
}
