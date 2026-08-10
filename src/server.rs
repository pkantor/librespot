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
//! | `subscribe_refresh` | -                   | `snapshot` without cover bytes  |
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
//! `subscribe_refresh` is the keepalive to renew with. It answers with the same `snapshot`, minus
//! the cover bytes the client already has, which is what keeps a keepalive down to one small
//! datagram instead of a fragmented one — see the cover section below. It registers a sender that
//! isn't subscribed yet just as `subscribe` would, so a client can lose its lease and keep going.
//!
//! A client therefore subscribes once, renews with `subscribe_refresh`, and takes the cover from
//! the `track_changed` it gets pushed. The one case that leaves it without a picture is a lost
//! `track_changed` — the only event carrying a cover, and so the only one that travels
//! fragmented — after which a keepalive reports a `song_uri` the client has no cover for. The rule
//! that repairs it, and startup, and a client that dropped its cache, is: **ask `current_track`
//! whenever you don't have the cover for the `song_uri` last reported**. The server keeps no
//! record of who has what.
//!
//! Events are JSON datagrams discriminated by their `event` field:
//!
//! ```json
//! {"event":"snapshot","is_playing":true,"position_ms":41200,"volume":32768,"track":{…}}
//! {"event":"track_changed","track":{…}}
//! {"event":"playback_changed","is_playing":false,"position_ms":41200}
//! {"event":"volume_changed","volume":32768}
//! ```
//!
//! `position_ms` is always valid at the moment the datagram is sent, so a client extrapolates it
//! against its own clock (`position_ms` plus the time since the datagram arrived, while
//! `is_playing`) and never has to agree with this machine on what time it is.
//!
//! # Cover art
//!
//! A track carries its cover as bytes (`cover_data`, base64) and by no other means: the clients
//! here cannot reach the internet, so the urls Spotify offers are not reported at all — only the
//! Pi ever follows one. The picture is fetched once per track no matter how many clients are
//! listening, and the last few are cached, so skipping back and forth doesn't refetch.
//!
//! `cover_data` is empty whenever there is no picture to be had — nothing on offer big enough to
//! display, a failed or slow fetch, no session yet. That is a case clients have to handle anyway,
//! so no effort is spent on second-best substitutes.
//!
//! That makes any datagram carrying a track a few tens of kB, well past the MTU, so it travels as
//! a fragmented datagram rather than a single packet. Two things follow for clients:
//!
//! - the receive buffer has to be at least [`MAX_COVER_BYTES`] plus change; 128 kB is a safe size.
//!   Note that a short buffer is not merely truncating on Windows: `recvfrom` fails with
//!   `WSAEMSGSIZE` and drops the datagram whole,
//! - the kernel reassembles the fragments, so a client still reads one whole event in one `recv`,
//!   or nothing at all — but losing any single fragment loses the whole event, which the keepalive
//!   repairs at the next lease at the latest.
//!
//! Only the datagrams that actually carry a cover are that big, which is why the keepalive is
//! `subscribe_refresh` rather than `subscribe`: a client re-syncs every few seconds, but it only
//! takes the fragmented path when the track changed under it.
//!
//! On Linux — what this runs on — a datagram may be up to 65507 bytes, comfortably more than a
//! cover needs. macOS caps outgoing datagrams at `net.inet.udp.maxdgram`, 9216 by default, so a
//! server run there fails to send covers until that sysctl is raised.

use std::{
    collections::{HashMap, VecDeque},
    io,
    net::{IpAddr, SocketAddr},
    str::FromStr,
    time::{Duration, Instant},
};

use data_encoding::BASE64;
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

/// The cover that gets shipped is the smallest one Spotify offers that is still at least this
/// wide. Clients scale down for display, which keeps the Pi out of the business of decoding and
/// resizing pictures, and keeps the datagram down to what anyone actually looks at.
const COVER_MIN_WIDTH: i32 = 200;

/// Covers bigger than this are dropped, and the track goes out without one. A picture this size is
/// far past what [`COVER_MIN_WIDTH`] asks for, so hitting this means something is off — and it is
/// more fragments to lose than a cover is worth.
pub const MAX_COVER_BYTES: usize = 64 * 1024;

/// How long a cover fetch may take before the track event goes out without it.
const COVER_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// How many covers to keep, so that skipping back and forth doesn't refetch them.
const COVER_CACHE_SIZE: usize = 4;

const SPOTIFY_ITEM_TYPE_TRACK: &str = "track";

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
}

impl Default for ApiServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: DEFAULT_BIND_ADDR.to_string(),
            allow_list: AllowList::default(),
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
    SetSession { spirc: Spirc, session: Session },
}

/// A cover fetched off the loop, on its way back to the task that asked for it.
struct CoverFetched {
    /// The url it was fetched from, which is what the cache and the pending track key on.
    url: String,
    /// `None` when it could not be fetched; the track then goes out without a cover.
    cover: Option<Cover>,
}

/// Cover bytes, ready to go into a datagram.
#[derive(Debug, Clone)]
struct Cover {
    /// Base64, since the wire format is JSON.
    data: String,
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
                pending_cover: None,
                cover_tx,
                cover_rx,
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
    /// Empty when there is no cover to be had — see [`cover_to_ship`].
    cover_data: String,
    /// The media type of `cover_data`, empty along with it.
    cover_mime: String,
    /// The dimensions of `cover_data`, zero along with it. See [`COVER_MIN_WIDTH`] for which of
    /// the covers on offer gets shipped.
    cover_width: i32,
    cover_height: i32,
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
            cover_data: String::new(),
            cover_mime: String::new(),
            cover_width: 0,
            cover_height: 0,
        }
    }
}

/// The cover whose bytes get shipped, out of what a track offers, largest first.
///
/// The smallest one that is still big enough for what clients display, so that neither the
/// datagram nor the network carries more pixels than anyone looks at. Nothing at all when
/// everything on offer is smaller than that: a picture too small to display is not worth the
/// bytes, and no cover is a case clients have to handle anyway.
fn cover_to_ship(covers: &[CoverImage]) -> Option<&CoverImage> {
    covers
        .iter()
        .rev()
        .find(|cover| cover.width >= COVER_MIN_WIDTH)
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
                "cover {url} is {} bytes, too big to put in a datagram",
                bytes.len()
            );
            None
        }
        Ok(Ok(bytes)) => {
            debug!("fetched {} bytes of cover from {url}", bytes.len());

            Some(Cover {
                mime: cover_mime(&bytes),
                data: BASE64.encode(&bytes),
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

/// The cover a track is to go out with: where to get it, and how big it is.
///
/// Picked off the [`AudioItem`] before it is turned into a [`TrackResponse`], which keeps no urls.
#[derive(Debug, Clone)]
struct WantedCover {
    url: String,
    width: i32,
    height: i32,
}

impl From<&CoverImage> for WantedCover {
    fn from(cover: &CoverImage) -> Self {
        Self {
            url: cover.url.clone(),
            width: cover.width,
            height: cover.height,
        }
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
    pending_cover: Option<WantedCover>,
    cover_tx: mpsc::UnboundedSender<CoverFetched>,
    cover_rx: mpsc::UnboundedReceiver<CoverFetched>,
}

impl ApiServerTask {
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
                let wanted = cover_to_ship(&audio_item.covers).map(WantedCover::from);

                self.current_track = TrackResponse::from(*audio_item);
                self.load_cover(wanted).await;
            }
            PlayerEvent::Playing { position_ms, .. } => {
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
            _ => (),
        }
    }

    /// Puts the cover of the current track on it, and announces the track.
    ///
    /// When the bytes have to be fetched, the announcement waits for them, so that clients get the
    /// track and its picture in one event. The fetch itself runs off the loop, which has to stay
    /// responsive meanwhile, and always reports back — so the event goes out either way.
    async fn load_cover(&mut self, wanted: Option<WantedCover>) {
        self.pending_cover = None;

        if let Some(wanted) = wanted {
            if let Some(cover) = self.covers.get(&wanted.url).cloned() {
                self.attach_cover(&cover, &wanted);
            } else if let Some(session) = self.session.clone() {
                tokio::spawn(fetch_cover(
                    session,
                    wanted.url.clone(),
                    self.cover_tx.clone(),
                ));
                self.pending_cover = Some(wanted);

                return;
            } else {
                // before the first connect there is nothing to fetch with
                debug!("no session to fetch the cover with yet");
            }
        }

        self.announce_track().await;
    }

    /// Takes a fetched cover, and announces the track that was waiting for it.
    async fn handle_cover(&mut self, fetched: CoverFetched) {
        // worth keeping even if nothing is waiting for it any more: whatever overtook this fetch
        // was most likely a skip, and a skip back wants this picture again
        if let Some(cover) = fetched.cover.clone() {
            self.covers.insert(fetched.url.clone(), cover);
        }

        let Some(wanted) = self
            .pending_cover
            .take_if(|wanted| wanted.url == fetched.url)
        else {
            // a newer track overtook this fetch, and brings its own event along
            return;
        };

        if let Some(cover) = fetched.cover {
            self.attach_cover(&cover, &wanted);
        }

        self.announce_track().await;
    }

    fn attach_cover(&mut self, cover: &Cover, wanted: &WantedCover) {
        self.current_track.cover_data = cover.data.clone();
        self.current_track.cover_mime = cover.mime.to_string();
        self.current_track.cover_width = wanted.width;
        self.current_track.cover_height = wanted.height;
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
        if matches!(command, "subscribe" | "subscribe_refresh" | "unsubscribe") {
            debug!("received '{command}' from {peer}");
        } else {
            info!("received '{command}' from {peer}");
        }

        match command {
            "next" => self.command(command, Spirc::next),
            "pause" => self.command(command, Spirc::pause),
            "resume" => self.command(command, |spirc| {
                // the device may be idle, so take over playback before resuming
                spirc.activate()?;
                spirc.play()
            }),
            "volup" => self.command(command, Spirc::volume_up),
            "voldown" => self.command(command, Spirc::volume_down),
            "setvol" => match serde_json::from_str::<SetVolumeRequest>(payload) {
                Ok(request) => {
                    let percentage = (request.volume as f64 / u16::MAX as f64) * 100.0;
                    debug!("setting volume to {percentage:.2}%");
                    self.command(command, |spirc| spirc.set_volume(request.volume));
                }
                Err(e) => warn!("invalid `setvol` payload '{payload}' from {peer}: {e}"),
            },
            "getvol" => self.reply(&self.current_volume, peer).await,
            "current_track" => self.reply(&self.current_track, peer).await,
            "status" => self.reply(&self.snapshot(), peer).await,
            "subscribe" => {
                self.renew_lease(peer);
                // the full state, cover included: this is what a client asks for when it needs
                // the picture, be it on startup or because the track changed under it
                self.reply(&self.snapshot(), peer).await;
            }
            "subscribe_refresh" => {
                self.renew_lease(peer);

                // the same re-sync, minus the cover the client already has — the bytes are what
                // makes a datagram fragment, and a keepalive should not pay for them every few
                // seconds. A client that finds `song_uri` changed asks for the whole thing.
                let cover = std::mem::take(&mut self.current_track.cover_data);
                let payload = encode(&self.snapshot());
                self.current_track.cover_data = cover;

                if let Some(payload) = payload {
                    self.send(&payload, peer).await;
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
    fn renew_lease(&mut self, peer: SocketAddr) {
        if self
            .subscribers
            .insert(peer, Instant::now() + SUBSCRIPTION_LEASE)
            .is_none()
        {
            info!("{peer} subscribed to API events");
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

        // no urls: what a client cannot reach is not worth reporting
        assert!(json.get("cover_url").is_none());
        assert!(json.get("covers").is_none());
    }

    #[test]
    fn the_shipped_cover_is_the_smallest_one_big_enough() {
        let covers = |edges: &[i32]| -> Vec<CoverImage> {
            edges
                .iter()
                .map(|edge| cover("https://i.scdn.co/image/x", ImageSize::DEFAULT, *edge))
                .collect()
        };

        // covers arrive largest first, and 300 is the smallest that still covers 200px of display
        let big_enough = covers(&[640, 300, 64]);
        assert_eq!(cover_to_ship(&big_enough).expect("picks one").width, 300);

        // exactly the wanted width is big enough
        let exact = covers(&[640, 200]);
        assert_eq!(cover_to_ship(&exact).expect("picks one").width, 200);

        // nothing worth shipping when everything on offer is too small to display
        let all_small = covers(&[64, 32]);
        assert!(cover_to_ship(&all_small).is_none());

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
                    data: i.to_string(),
                    mime: "image/jpeg",
                },
            );
        }

        assert!(cache.get("url-0").is_none(), "the oldest should be gone");
        assert_eq!(
            cache
                .get(&format!("url-{COVER_CACHE_SIZE}"))
                .expect("kept")
                .data,
            COVER_CACHE_SIZE.to_string()
        );
        assert_eq!(cache.covers.len(), COVER_CACHE_SIZE);
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
            "cover_data",
            "cover_mime",
            "cover_width",
            "cover_height",
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
        let allow_list: AllowList = "172.30.2.0/24, 10.1.2.3".parse().expect("parses");

        assert!(allow_list.allows("172.30.2.1".parse().unwrap()));
        assert!(allow_list.allows("172.30.2.255".parse().unwrap()));
        assert!(!allow_list.allows("172.30.3.1".parse().unwrap()));

        // a bare address only ever matches itself
        assert!(allow_list.allows("10.1.2.3".parse().unwrap()));
        assert!(!allow_list.allows("10.1.2.4".parse().unwrap()));

        // v4 rules don't cover v6 peers, but do cover v4 mapped ones
        assert!(!allow_list.allows("fd00::1".parse().unwrap()));
        assert!(allow_list.allows("::ffff:172.30.2.1".parse().unwrap()));
    }

    #[test]
    fn loopback_is_allowed_without_being_listed() {
        let allow_list: AllowList = "172.30.2.0/24".parse().expect("parses");

        assert!(allow_list.allows("127.0.0.1".parse().unwrap()));
        // the whole 127.0.0.0/8 block, however it arrives at a dual stack socket
        assert!(allow_list.allows("127.0.0.53".parse().unwrap()));
        assert!(allow_list.allows("::1".parse().unwrap()));
        assert!(allow_list.allows("::ffff:127.0.0.1".parse().unwrap()));

        // and it does not weaken anything else
        assert!(!allow_list.allows("172.30.3.1".parse().unwrap()));
    }

    #[test]
    fn allow_list_handles_odd_prefixes() {
        let allow_list: AllowList = "172.30.0.0/20".parse().expect("parses");

        assert!(allow_list.allows("172.30.15.255".parse().unwrap()));
        assert!(!allow_list.allows("172.30.16.0".parse().unwrap()));

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
        assert!("172.30.2.0/33".parse::<AllowList>().is_err());
        assert!("172.30.2.0/nope".parse::<AllowList>().is_err());
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
            pending_cover: None,
            cover_tx,
            cover_rx,
        };

        (task, events, cmd_tx)
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
        // there is no session here to fetch a cover with, so the event goes out without one
        assert_eq!(event["track"]["cover_data"], "");

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

    #[tokio::test]
    async fn the_track_event_carries_the_cover_bytes() {
        const JPEG: &[u8] = &[0xff, 0xd8, 0xff, 0xe0, 0x01, 0x02, 0x03];

        let server = TestServer::start_with(AllowList::default(), |task| {
            task.covers.insert(
                COVER_URL.to_string(),
                Cover {
                    data: BASE64.encode(JPEG),
                    mime: "image/jpeg",
                },
            );
        })
        .await;

        server.request("subscribe").await;
        assert_eq!(server.receive().await["event"], "snapshot");

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

        let track = &event["track"];
        assert_eq!(track["cover_data"], BASE64.encode(JPEG));
        assert_eq!(track["cover_mime"], "image/jpeg");
        // the 300px one, not the 640px the fixture also offers
        assert_eq!(track["cover_width"], 300);
        assert_eq!(track["cover_height"], 300);

        // and a client that polls instead of subscribing gets the same picture
        server.request("current_track").await;
        assert_eq!(server.receive().await["cover_data"], BASE64.encode(JPEG));
    }

    #[tokio::test]
    async fn a_keepalive_re_syncs_without_resending_the_cover() {
        const JPEG: &[u8] = &[0xff, 0xd8, 0xff, 0xe0, 0x01, 0x02, 0x03];

        let server = TestServer::start_with(AllowList::default(), |task| {
            task.covers.insert(
                COVER_URL.to_string(),
                Cover {
                    data: BASE64.encode(JPEG),
                    mime: "image/jpeg",
                },
            );
        })
        .await;

        // a keepalive subscribes a client that isn't on the list yet, so a lost lease is not fatal
        server.request("subscribe_refresh").await;
        assert_eq!(server.receive().await["event"], "snapshot");

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

        // proving it was subscribed: the track event arrives, with the cover
        let event = server.receive().await;
        assert_eq!(event["event"], "track_changed");
        assert_eq!(event["track"]["cover_data"], BASE64.encode(JPEG));

        server.request("subscribe_refresh").await;
        let snapshot = server.receive().await;

        // the state is re-synced in full, so a client can tell whether its picture is still current
        assert_eq!(snapshot["event"], "snapshot");
        assert_eq!(snapshot["track"]["song_name"], "Sun Is Shining");
        assert_eq!(snapshot["track"]["song_uri"], TRACK_URI);
        // but the bytes are left out, which is the whole point of the keepalive
        assert_eq!(snapshot["track"]["cover_data"], "");

        // and holding them back does not lose them: `subscribe` still hands over the picture
        server.request("subscribe").await;
        assert_eq!(
            server.receive().await["track"]["cover_data"],
            BASE64.encode(JPEG)
        );
    }

    #[tokio::test]
    async fn the_allow_list_keeps_others_from_subscribing() {
        // driven directly rather than over the socket, because a test client is always on
        // loopback and loopback is allowed unconditionally
        let (mut task, ..) = idle_task("172.30.2.0/24".parse().expect("parses")).await;

        task.handle_request(b"subscribe", "8.8.8.8:1234".parse().expect("parses"))
            .await;
        assert!(
            task.subscribers.is_empty(),
            "a client outside the allow list should be dropped before its command is read"
        );

        task.handle_request(b"subscribe", "172.30.2.5:1234".parse().expect("parses"))
            .await;
        assert_eq!(
            task.subscribers.len(),
            1,
            "a listed client should be served"
        );
    }

    #[tokio::test]
    async fn a_local_client_is_served_without_being_listed() {
        let server = TestServer::start("172.30.2.0/24".parse().expect("parses")).await;

        server.request("subscribe").await;

        assert_eq!(server.receive().await["event"], "snapshot");
    }
}
