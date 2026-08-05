//! A small UDP control API for the librespot binary.
//!
//! # Requests
//!
//! The server accepts plain text datagrams, one command per datagram, in the form
//! `<command>[ <json payload>]`. Commands that query state answer with a JSON datagram sent back
//! to the requesting socket:
//!
//! | command         | payload             | response                   |
//! |-----------------|---------------------|----------------------------|
//! | `next`          | -                   | -                          |
//! | `pause`         | -                   | -                          |
//! | `resume`        | -                   | -                          |
//! | `volup`         | -                   | -                          |
//! | `voldown`       | -                   | -                          |
//! | `setvol`        | `{"volume":<u16>}`  | -                          |
//! | `getvol`        | -                   | [`VolumeResponse`] as JSON |
//! | `current_track` | -                   | [`TrackResponse`] as JSON  |
//! | `status`        | -                   | `snapshot` event as JSON   |
//! | `subscribe`     | -                   | `snapshot` event as JSON   |
//! | `unsubscribe`   | -                   | -                          |
//!
//! # Push events
//!
//! `subscribe` registers the sender for push events for [`SUBSCRIPTION_LEASE`] and answers with a
//! `snapshot` of the full state. Clients are expected to re-send `subscribe` well within the lease
//! as a keepalive; that also re-syncs them, so a dropped datagram leaves a client stale for at
//! most one keepalive interval instead of indefinitely. A client that goes away silently is
//! dropped when its lease runs out; `unsubscribe` deregisters it right away.
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

use std::{
    collections::HashMap,
    io,
    net::{IpAddr, SocketAddr},
    str::FromStr,
    time::{Duration, Instant},
};

use librespot::{
    connect::Spirc,
    core::Error,
    metadata::{
        audio::{AudioItem, UniqueFields, item::CoverImage},
        image::ImageSize,
    },
    playback::player::{PlayerEvent, PlayerEventChannel},
};
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use thiserror::Error as ThisError;
use tokio::{net::UdpSocket, sync::mpsc, task::JoinHandle};

/// The address the control API listens on by default.
pub const DEFAULT_BIND_ADDR: &str = "0.0.0.0:50505";

/// How long a `subscribe` keeps a client on the push list. Clients should refresh it about every
/// third of that, so that a lost keepalive doesn't cost them their subscription.
pub const SUBSCRIPTION_LEASE: Duration = Duration::from_secs(30);

/// Datagrams larger than this are truncated. Commands are short, only `setvol` carries a payload.
const MAX_DATAGRAM_SIZE: usize = 1024;

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
/// and the rest of the network. An empty list allows everyone.
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
    SetSpirc(Spirc),
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

        let task = tokio::spawn(
            ApiServerTask {
                socket,
                allow_list: config.allow_list,
                spirc: None,
                cmd_rx,
                player_events,
                subscribers: HashMap::new(),
                current_track: TrackResponse::default(),
                current_volume: VolumeResponse::default(),
                playback: PlaybackState::default(),
            }
            .run(),
        );

        Ok(ApiServer { cmd_tx, task })
    }

    /// Hands the current [`Spirc`] to the server task.
    ///
    /// Called on every (re)connect, as each session gets its own `Spirc`.
    pub fn set_spirc(&self, spirc: Spirc) {
        if self.cmd_tx.send(ApiServerCommand::SetSpirc(spirc)).is_err() {
            warn!("could not hand the spirc to the API server: task is not running");
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
    /// The largest available cover, for clients that just want one image.
    cover_url: String,
    /// All covers of the album / single (episode cover for podcasts), largest first.
    covers: Vec<CoverResponse>,
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

        // covers arrive sorted largest first
        let covers: Vec<CoverResponse> =
            audio_item.covers.iter().map(CoverResponse::from).collect();
        let cover_url = covers
            .first()
            .map(|cover| cover.url.clone())
            .unwrap_or_default();

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
            cover_url,
            covers,
        }
    }
}

#[derive(Debug, Serialize)]
struct CoverResponse {
    url: String,
    /// `default`, `small`, `large` or `xlarge`
    size: &'static str,
    width: i32,
    height: i32,
}

impl From<&CoverImage> for CoverResponse {
    fn from(cover: &CoverImage) -> Self {
        Self {
            url: cover.url.clone(),
            size: match cover.size {
                ImageSize::DEFAULT => "default",
                ImageSize::SMALL => "small",
                ImageSize::LARGE => "large",
                ImageSize::XLARGE => "xlarge",
            },
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
    cmd_rx: mpsc::UnboundedReceiver<ApiServerCommand>,
    player_events: PlayerEventChannel,
    /// Subscribed clients and when their lease runs out.
    subscribers: HashMap<SocketAddr, Instant>,
    current_track: TrackResponse,
    current_volume: VolumeResponse,
    playback: PlaybackState,
}

impl ApiServerTask {
    async fn run(mut self) {
        let mut buf = [0u8; MAX_DATAGRAM_SIZE];
        // the player outlives this task, but don't spin on a closed channel if it doesn't
        let mut player_events_open = true;

        loop {
            tokio::select! {
                cmd = self.cmd_rx.recv() => match cmd {
                    Some(ApiServerCommand::SetSpirc(spirc)) => {
                        debug!("API server got a new spirc handle");
                        self.spirc = Some(spirc);
                    }
                    // the handle was dropped or shut down
                    None => break,
                },
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
                self.current_track = TrackResponse::from(*audio_item);

                self.broadcast(encode(&Event::TrackChanged {
                    track: &self.current_track,
                }))
                .await;
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
            debug!("ignoring a datagram from {peer}: not on the allow list");
            return;
        }

        let message = String::from_utf8_lossy(datagram);
        let mut parts = message.trim().splitn(2, ' ');
        let command = parts.next().unwrap_or_default();
        let payload = parts.next().unwrap_or_default().trim();

        debug!("received '{command}' from {peer}");

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
                Err(e) => warn!("invalid `setvol` payload '{payload}': {e}"),
            },
            "getvol" => self.reply(&self.current_volume, peer).await,
            "current_track" => self.reply(&self.current_track, peer).await,
            "status" => self.reply(&self.snapshot(), peer).await,
            "subscribe" => {
                if self
                    .subscribers
                    .insert(peer, Instant::now() + SUBSCRIPTION_LEASE)
                    .is_none()
                {
                    debug!("{peer} subscribed to API events");
                }

                // answering with the full state makes every keepalive a re-sync
                self.reply(&self.snapshot(), peer).await;
            }
            "unsubscribe" => {
                if self.subscribers.remove(&peer).is_some() {
                    debug!("{peer} unsubscribed from API events");
                }
            }
            other => warn!("unknown command: {other}"),
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
                debug!("subscription of {peer} expired");
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
        },
    };

    const TRACK_URI: &str = "spotify:track:2WUy2Uywcj5cP0IXQagO3z";

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
            covers: vec![
                cover("https://i.scdn.co/image/big", ImageSize::XLARGE, 640),
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
    fn track_response_reports_artists_album_and_covers() {
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

        // the largest cover is the one clients get as `cover_url`
        assert_eq!(json["cover_url"], "https://i.scdn.co/image/big");
        assert_eq!(json["covers"][0]["size"], "xlarge");
        assert_eq!(json["covers"][0]["width"], 640);
        assert_eq!(json["covers"][1]["size"], "small");
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
            "cover_url",
            "covers",
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
        assert!(!allow_list.allows("::1".parse().unwrap()));
        assert!(allow_list.allows("::ffff:172.30.2.1".parse().unwrap()));
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

    impl TestServer {
        async fn start(allow_list: AllowList) -> Self {
            let (events, player_events) = mpsc::unbounded_channel();
            let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

            let socket = UdpSocket::bind("127.0.0.1:0").await.expect("binds");
            let addr = socket.local_addr().expect("has an address");

            tokio::spawn(
                ApiServerTask {
                    socket,
                    allow_list,
                    spirc: None,
                    cmd_rx,
                    player_events,
                    subscribers: HashMap::new(),
                    current_track: TrackResponse::default(),
                    current_volume: VolumeResponse::default(),
                    playback: PlaybackState::default(),
                }
                .run(),
            );

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
            let mut buf = [0u8; MAX_DATAGRAM_SIZE * 8];

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
    async fn the_allow_list_keeps_others_from_subscribing() {
        // the client talks from 127.0.0.1, which this does not cover
        let server = TestServer::start("172.30.2.0/24".parse().expect("parses")).await;

        server.request("subscribe").await;

        assert!(
            server.receives_nothing().await,
            "a client outside the allow list should be ignored"
        );
    }
}
