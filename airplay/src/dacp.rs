//! DACP: controlling the *sender* — telling the phone to skip, pause or change volume.
//!
//! Ported from shairport-sync's `dacp.c`. A classic sender's `SETUP` (and `GET /info`) carries
//! `Active-Remote`/`DACP-ID` headers; its DACP server's address is the RTSP connection's peer
//! address (`dacp_server.ip_string = conn->client_ip_string`), so only the **port** needs
//! resolving, from the sender's own `iTunes_Ctrl_<DACP-ID>._dacp._tcp` advertisement.
//!
//! The endpoints, and how well each is confirmed:
//! - [`send_command`] — `dacp_send_command`, live code.
//! - [`poll_status`] — `playstatusupdate`, live code.
//! - [`get_volume`]/[`set_volume`] — `dacp_get_client_volume` /
//!   `dacp_set_include_speaker_volume`, live code, each simplified to one speaker (see the
//!   functions).
//! - [`fetch_artwork`] — **the weakest**: shairport-sync never calls it, and it appears in
//!   `dacp.c` only in a commented-out debug block. Confirmed instead by the other side, owntone's
//!   `dacp_reply_nowplayingartwork` (`httpd_dacp.c`).
//!
//! Now-playing info also arrives the other way, and that one is primary: the sender's own
//! `SET_PARAMETER` push (`crate::dmap`), which needs no DACP server, mDNS query or polling.
//! Polling stays for senders that don't push, and costs nothing extra since the DACP connection
//! has to exist for remote control anyway.

use std::{net::IpAddr, time::Duration};

use log::{debug, warn};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpSocket, TcpStream},
    time::timeout,
};

const DACP_SERVICE_TYPE: &str = "_dacp._tcp.local.";
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How to reach a sender's DACP server. Resolved once, right after the headers arrive, and not
/// re-resolved if the sender's port changes mid-session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DacpTarget {
    pub host: IpAddr,
    pub port: u16,
    pub active_remote: String,
    /// Which local address to originate the connection from, on a machine with several on the
    /// same interface. Without it the OS picks, and a sender's DACP server may reject a request
    /// whose source IP isn't the one it handed the `Active-Remote` token to. `None` leaves the
    /// choice to the OS.
    pub local_bind: Option<IpAddr>,
    /// **This receiver's** own speaker id — not a property of the target, but carried with it
    /// because `setproperty?include-speaker-id=` needs it (see [`set_volume`]) and a caller
    /// rebuilding a target from an `AirplayEvent` has no other way to know it.
    pub machine_number: u64,
    /// Which interface a link-local IPv6 `host` is reachable on; `0` otherwise. An `IpAddr`
    /// alone cannot reach `fe80::/10` — the same address can exist on several interfaces.
    /// shairport-sync carries it for the same reason (`dacp.c`: `"%s%%%u"` with
    /// `dacp_server.scope_id`). It belongs to the socket address only: the `Host:` header keeps
    /// the bare address, as shairport-sync's does.
    pub scope_id: u32,
}

impl DacpTarget {
    /// Where to actually connect: the address plus, for a link-local IPv6 host, the scope that
    /// makes it routable (see [`scope_id`](Self::scope_id)).
    fn socket_addr(&self) -> std::net::SocketAddr {
        match self.host {
            IpAddr::V6(host) => {
                std::net::SocketAddrV6::new(host, self.port, 0, self.scope_id).into()
            }
            IpAddr::V4(host) => std::net::SocketAddrV4::new(host, self.port).into(),
        }
    }
}

/// The speaker id a DACP server knows this receiver by: the six bytes of its `deviceid` as one
/// big-endian integer — shairport-sync's own derivation from `config.ap1_prefix` (`dacp.c`,
/// `dacp_set_volume`). Malformed input yields `0`, which no real device advertises.
pub fn machine_number(device_id: &str) -> u64 {
    let mut number = 0u64;
    let mut bytes = 0;
    for part in device_id.split(':') {
        let Ok(byte) = u8::from_str_radix(part, 16) else {
            return 0;
        };
        number = (number << 8) | u64::from(byte);
        bytes += 1;
    }
    if bytes == 6 { number } else { 0 }
}

/// Resolves `iTunes_Ctrl_<dacp_id>._dacp._tcp`'s port by mDNS browse. `None` on timeout or if the
/// daemon can't start (e.g. no multicast-capable interface).
///
/// `bind_ip` restricts which interfaces are queried, the same way `--airplay-bind-ip` already
/// restricts this crate's own advertising — necessary on a machine with an unreachable second
/// address on the AirPlay interface, where resolution otherwise times out. Empty leaves every
/// interface enabled.
pub async fn resolve_port(dacp_id: &str, bind_ip: &[IpAddr]) -> Option<u16> {
    let daemon = match mdns_sd::ServiceDaemon::new() {
        Ok(daemon) => Mdns(daemon),
        Err(err) => {
            warn!("airplay: failed to start the mDNS daemon for DACP resolution: {err}");
            return None;
        }
    };

    if !bind_ip.is_empty() {
        if let Err(err) = daemon.0.disable_interface(mdns_sd::IfKind::All) {
            warn!("airplay: failed to restrict the DACP mDNS daemon's interfaces: {err}");
        }
        if let Err(err) = daemon.0.enable_interface(bind_ip.to_vec()) {
            warn!("airplay: failed to enable {bind_ip:?} on the DACP mDNS daemon: {err}");
        }
    }

    let receiver = match daemon.0.browse(DACP_SERVICE_TYPE) {
        Ok(receiver) => receiver,
        Err(err) => {
            warn!("airplay: failed to browse {DACP_SERVICE_TYPE}: {err}");
            return None;
        }
    };

    let port = timeout(RESOLVE_TIMEOUT, async {
        while let Ok(event) = receiver.recv_async().await {
            if let mdns_sd::ServiceEvent::ServiceResolved(resolved) = event {
                if matches_dacp_id(&resolved.fullname, dacp_id) {
                    return Some(resolved.port);
                }
            }
        }
        None
    })
    .await
    .ok()
    .flatten();

    if port.is_none() {
        warn!("airplay: timed out resolving a DACP port for {dacp_id}");
    }
    port
}

/// Owns an `mdns_sd::ServiceDaemon` and shuts it down when dropped.
///
/// Required, not tidiness: the daemon is a background thread owning the multicast sockets, and it
/// exits on exactly one condition — the `Exit` command `shutdown()` sends. `mdns-sd` gives it no
/// `Drop` of its own, so dropping the last handle leaves the thread running forever, once per
/// AirPlay session. (`stop_browse`, which used to stand here, stops the browse but not the
/// thread.)
///
/// `Drop` rather than a call at the end of [`resolve_port`] because the poll task is aborted on
/// `TEARDOWN` and on an ungraceful disconnect, dropping this future at whatever `await` it sits
/// on — the disconnect-heavy case is exactly where the leak would accumulate fastest.
struct Mdns(mdns_sd::ServiceDaemon);

impl Drop for Mdns {
    fn drop(&mut self) {
        if let Err(err) = self.0.shutdown() {
            warn!("airplay: failed to shut the DACP mDNS daemon down: {err}");
        }
    }
}

/// Does an advertised `_dacp._tcp` service (`iTunes_Ctrl_<id>._dacp._tcp.local.`) belong to the
/// sender that sent `dacp_id` in its `DACP-ID` header?
///
/// Not a plain comparison: shairport-sync strips **leading zeroes** off the advertised id first
/// (`mdns_avahi.c`, `resolve_callback`: `while (*dacpid == '0') dacpid++;`), so a sender
/// advertising `iTunes_Ctrl_0BCE…` while sending `DACP-ID: BCE…` still matches. Without that, its
/// port never resolves and the failure looks exactly like having no DACP server at all.
///
/// One step past shairport-sync: the stripping is applied to **both** sides, which matches
/// everywhere the one-sided form does plus the case where the header itself carries the zero.
/// Case is left alone (shairport-sync uses `strcmp`).
fn matches_dacp_id(fullname: &str, dacp_id: &str) -> bool {
    let Some(rest) = fullname.strip_prefix("iTunes_Ctrl_") else {
        return false;
    };
    // `fullname` carries the service type and domain too; the id is everything up to the first
    // dot. `split` always yields at least one item, so this can't be `None`.
    let Some(advertised) = rest.split('.').next() else {
        return false;
    };
    let advertised = advertised.trim_start_matches('0');
    let wanted = dacp_id.trim_start_matches('0');

    // An all-zero id on one side only would otherwise match anything on the other.
    !advertised.is_empty() && advertised == wanted
}

/// One DACP HTTP GET: `(status, headers, body)`. Every call in this module goes through here,
/// with a fresh TCP connection per request, as `dacp_send_command` does.
async fn get(target: &DacpTarget, path: &str) -> Option<(u16, String, Vec<u8>)> {
    let connect = async {
        let remote = target.socket_addr();
        match target.local_bind {
            None => TcpStream::connect(remote).await,
            Some(local_ip) => {
                let socket = if remote.is_ipv4() {
                    TcpSocket::new_v4()
                } else {
                    TcpSocket::new_v6()
                }?;
                socket.bind(std::net::SocketAddr::new(local_ip, 0))?;
                socket.connect(remote).await
            }
        }
    };
    let mut stream = match timeout(REQUEST_TIMEOUT, connect).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(err)) => {
            warn!(
                "airplay: DACP connect to {}:{} (from {:?}) failed: {err}",
                target.host, target.port, target.local_bind
            );
            return None;
        }
        Err(_) => {
            warn!(
                "airplay: DACP connect to {}:{} timed out",
                target.host, target.port
            );
            return None;
        }
    };

    // Byte-for-byte shairport-sync's request: these three lines and nothing else. Adding a
    // `User-Agent` (which real DACP clients send) was tried against a real iPhone and changed
    // nothing.
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {}:{}\r\nActive-Remote: {}\r\n\r\n",
        target.host, target.port, target.active_remote
    );
    debug!("airplay: DACP request: {request:?}");
    if let Err(err) = stream.write_all(request.as_bytes()).await {
        warn!("airplay: DACP request to {path} failed to send: {err}");
        return None;
    }

    // Without this, every parse failure looks the same from the log: "connection refused",
    // "server sent nothing" and "server sent something unparseable" are worth telling apart.
    let fail = |reason: &str, raw: &[u8]| {
        warn!(
            "airplay: DACP response to {path} {reason} ({} bytes): {:?}",
            raw.len(),
            String::from_utf8_lossy(&raw[..raw.len().min(200)])
        );
    };

    // A real DACP server is a normal HTTP/1.1 server: it keeps the connection open after
    // answering, so reading to EOF never completes. shairport-sync runs an incremental HTTP
    // parser (`http_data`) that stops as soon as the message is complete; the framing rules below
    // are the same idea spelled out.
    let deadline = tokio::time::Instant::now() + REQUEST_TIMEOUT;
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];

    let header_end = loop {
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()) else {
            fail("timed out before its headers finished", &raw);
            return None;
        };
        match timeout(remaining, stream.read(&mut buf)).await {
            Ok(Ok(0)) => {
                fail("closed the connection before its headers finished", &raw);
                return None;
            }
            Ok(Ok(n)) => raw.extend_from_slice(&buf[..n]),
            Ok(Err(err)) => {
                warn!("airplay: DACP response to {path} read error: {err}");
                return None;
            }
            Err(_) => {
                fail("timed out before its headers finished", &raw);
                return None;
            }
        }
    };

    let headers = String::from_utf8_lossy(&raw[..header_end]);
    let Some(status_line) = headers.lines().next() else {
        fail("has an empty status line", &raw);
        return None;
    };
    let Some(status) = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
    else {
        fail("has an unparseable status line", &raw);
        return None;
    };

    let header_value = |wanted: &str| -> Option<&str> {
        headers.lines().skip(1).find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case(wanted).then(|| value.trim())
        })
    };
    let content_length: Option<usize> =
        header_value("content-length").and_then(|value| value.parse().ok());
    let chunked = header_value("transfer-encoding").is_some_and(|value| {
        value
            .split(',')
            .any(|v| v.trim().eq_ignore_ascii_case("chunked"))
    });
    // Owned from here on: the loops below extend `raw`, which `headers` (from `from_utf8_lossy`)
    // still borrows otherwise.
    let headers = headers.into_owned();

    // How this response says where its body ends, in the order RFC 9110 §6.3 resolves it.
    enum Framing {
        /// The status forbids a body outright — see [`status_has_no_body`].
        Empty,
        Chunked,
        Length(usize),
        /// No framing at all: the body ends when the connection does. Correct, but it costs the
        /// full request timeout against a server that keeps the connection open, so it is the
        /// last resort rather than the default.
        UntilClose,
    }

    let body_start = header_end + 4;
    let framing = if status_has_no_body(status) {
        Framing::Empty
    } else if chunked {
        Framing::Chunked
    } else if let Some(len) = content_length {
        Framing::Length(len)
    } else {
        Framing::UntilClose
    };

    loop {
        let complete = match framing {
            Framing::Empty => true,
            Framing::Chunked => raw.get(body_start..).and_then(decode_chunked).is_some(),
            Framing::Length(len) => raw.len() >= body_start + len,
            Framing::UntilClose => false,
        };
        if complete {
            break;
        }
        let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()) else {
            break;
        };
        match timeout(remaining, stream.read(&mut buf)).await {
            // Closed or timed out: whatever arrived is all there will be. For `UntilClose` that
            // is the normal ending, not a failure.
            Ok(Ok(0)) | Err(_) => break,
            Ok(Ok(n)) => raw.extend_from_slice(&buf[..n]),
            Ok(Err(err)) => {
                warn!("airplay: DACP response to {path} read error: {err}");
                break;
            }
        }
    }

    let body = match framing {
        Framing::Empty => Vec::new(),
        Framing::Chunked => match raw.get(body_start..).and_then(decode_chunked) {
            Some(body) => body,
            None => {
                fail("did not finish its chunked body", &raw);
                Vec::new()
            }
        },
        Framing::Length(len) => raw[body_start..(body_start + len).min(raw.len())].to_vec(),
        Framing::UntilClose => raw[body_start..].to_vec(),
    };

    Some((status, headers, body))
}

/// Statuses that carry no body whatever the headers claim (RFC 9110 §6.4.1). `204` is the one
/// that matters here: it is what a DACP server answers `setproperty` with.
fn status_has_no_body(status: u16) -> bool {
    matches!(status, 100..=199 | 204 | 304)
}

/// Reassembles a `Transfer-Encoding: chunked` body: `<hex length>\r\n<data>\r\n`, repeated,
/// ending with a zero-length chunk. `None` means "keep reading", not "malformed".
fn decode_chunked(mut bytes: &[u8]) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line_end = bytes.windows(2).position(|w| w == b"\r\n")?;
        // A chunk size may carry `;`-separated extensions after it, which are not part of it.
        let header = std::str::from_utf8(&bytes[..line_end]).ok()?;
        let size = usize::from_str_radix(header.split(';').next()?.trim(), 16).ok()?;
        let start = line_end + 2;

        if size == 0 {
            // A trailer section may follow; nothing here reads trailers.
            return Some(body);
        }

        let end = start + size;
        body.extend_from_slice(bytes.get(start..end)?);
        // Each chunk's data is followed by its own CRLF.
        bytes = bytes.get(end + 2..)?;
    }
}

/// Sends a transport-control command (`nextitem`, `previtem`, `pause`, `playpause`, `play` — the
/// strings shairport-sync's `dbus-service.c`/`mpris-service.c` send). Logs and drops failures:
/// there is nothing useful to hand back.
pub async fn send_command(target: &DacpTarget, command: &str) {
    match get(target, &format!("/ctrl-int/1/{command}")).await {
        Some((status, _, _)) if (200..300).contains(&status) => {
            debug!("airplay: DACP '{command}' -> {status}");
        }
        Some((status, headers, _)) => {
            warn!(
                "airplay: DACP '{command}' got an unexpected status {status}, headers: {headers:?}"
            );
        }
        None => warn!("airplay: DACP '{command}' failed"),
    }
}

/// What a `playstatusupdate` poll reported. A field is `None` when the response didn't carry
/// that tag, which means "nothing to report", not an error.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct PlayStatus {
    /// `caps`: `4` = playing, mapped to `true`; anything else (`2` stopped, `3` paused) to
    /// `false`.
    pub playing: Option<bool>,
    /// `cann`.
    pub title: Option<String>,
    /// `cana`.
    pub artist: Option<String>,
    /// `canl`.
    pub album: Option<String>,
    /// `astm`, or `cast` when the sender sends that instead.
    pub duration_ms: Option<u32>,
    /// **Derived**, not sent: a DACP server reports `cast` (length) and `cant` (time left), so
    /// the position is the difference — `None` unless both arrived. owntone's server writes
    /// exactly that pair (`httpd_dacp.c`); shairport-sync reads neither, having no use for a
    /// position, so they appear only commented out in its `dacp.c`.
    pub position_ms: Option<u32>,
}

fn parse_play_status(body: &[u8]) -> PlayStatus {
    let mut status = PlayStatus::default();

    let Some((tag, value, _)) = crate::dmap::read_item(body) else {
        return status;
    };
    if &tag != b"cmst" {
        return status;
    }

    let mut total_ms = None;
    let mut remaining_ms = None;

    let mut rest = value;
    while let Some((tag, item, next)) = crate::dmap::read_item(rest) {
        match &tag {
            b"caps" => status.playing = item.first().map(|&b| b == 4),
            b"cann" => status.title = Some(String::from_utf8_lossy(item).into_owned()),
            b"cana" => status.artist = Some(String::from_utf8_lossy(item).into_owned()),
            b"canl" => status.album = Some(String::from_utf8_lossy(item).into_owned()),
            b"astm" => status.duration_ms = crate::dmap::read_u32(item),
            b"cast" => total_ms = crate::dmap::read_u32(item),
            b"cant" => remaining_ms = crate::dmap::read_u32(item),
            _ => {}
        }
        rest = next;
    }

    if let (Some(total), Some(remaining)) = (total_ms, remaining_ms) {
        // A sender that has just finished a track can report more remaining than total for an
        // instant; saturating keeps that from wrapping into a position near u32::MAX.
        status.position_ms = Some(total.saturating_sub(remaining));
    }
    // `astm` and `cast` are the same quantity, and a sender may send either — prefer the one that
    // was actually there.
    status.duration_ms = status.duration_ms.or(total_ms);

    status
}

/// Polls `playstatusupdate` once. The revision number is **always** `1`, never the `cmsr` the
/// previous response reported — shairport-sync's own `always_use_revision_number_1` (`dacp.c`).
///
/// That is load-bearing: `revision-number` *is* the long-polling handle. A real server answers
/// at once only while the number differs from its current revision, and otherwise leaves the
/// request hanging until something changes (owntone's `dacp_reply_playstatusupdate`). Feeding
/// `cmsr` back makes every poll after the first hang until [`REQUEST_TIMEOUT`], and nothing here
/// implements long polling.
///
/// **`400 Bad Request` is not a failure**: the server is there but offers no *advanced*
/// (metadata) features, which is what a real iPhone answers, and control commands still work.
/// shairport-sync reads `400` the same way — though in its loop the `400` being judged comes
/// from its *volume* probe, which it uses to gate `playstatusupdate` entirely; this crate polls
/// the two independently, so the reading is applied here directly.
///
/// `Some(PlayStatus::default())` for that case; `None` only for a transport failure or an
/// unexpected status.
pub async fn poll_status(target: &DacpTarget) -> Option<PlayStatus> {
    let path = "/ctrl-int/1/playstatusupdate?revision-number=1";
    let (status, headers, body) = get(target, path).await?;
    match status {
        200 => Some(parse_play_status(&body)),
        400 => {
            debug!(
                "airplay: DACP playstatusupdate -> 400: server is present but has no advanced \
                 (metadata) support; control commands are unaffected"
            );
            Some(PlayStatus::default())
        }
        _ => {
            warn!(
                "airplay: DACP playstatusupdate got status {status}, headers: {headers:?}, body: {:?}",
                String::from_utf8_lossy(&body[..body.len().min(200)])
            );
            None
        }
    }
}

/// Pulls `cmvo` out of a `getproperty?properties=dmcp.volume` response — a `cmgt` container of
/// the usual TLV items. Ported from `dacp_get_client_volume` (`dacp.c`), including treating a
/// missing tag as unknown (it reports `-1`, this `None`).
fn parse_volume(body: &[u8]) -> Option<i32> {
    let (tag, value, _) = crate::dmap::read_item(body)?;
    if &tag != b"cmgt" {
        return None;
    }

    let mut rest = value;
    while let Some((tag, item, next)) = crate::dmap::read_item(rest) {
        if &tag == b"cmvo" {
            return crate::dmap::read_u32(item).map(|volume| volume as i32);
        }
        rest = next;
    }
    None
}

/// The sender's current volume, 0..=100, or `None` when it doesn't report one.
///
/// **Single-speaker simplification**: this reports the *overall* (master) volume, while
/// shairport-sync's `dacp_get_volume` computes `overall * relative / 100`, taking `relative` from
/// a second `getspeakers` request. With one receiver those agree exactly; with several active
/// speakers this reports the group's volume rather than this device's. `getspeakers` is not
/// built out blind — it can't be validated without a multi-speaker setup.
pub async fn get_volume(target: &DacpTarget) -> Option<u8> {
    let (status, headers, body) =
        get(target, "/ctrl-int/1/getproperty?properties=dmcp.volume").await?;
    if status != 200 {
        // `400` here means the same thing it means for `poll_status` — a DACP server without the
        // advanced features — and is just as much a normal configuration, so it isn't a warning.
        debug!("airplay: DACP getproperty(dmcp.volume) -> {status}, headers: {headers:?}");
        return None;
    }
    match parse_volume(&body) {
        Some(volume @ 0..=100) => Some(volume as u8),
        Some(volume) => {
            debug!("airplay: DACP reported a volume of {volume}, which is not a percentage");
            None
        }
        None => {
            debug!("airplay: DACP getproperty(dmcp.volume) carried no cmvo tag");
            None
        }
    }
}

/// Sets the volume, 0..=100, via `setproperty?include-speaker-id=<machine_number>&dmcp.volume=` —
/// shairport-sync's `dacp_set_include_speaker_volume`, which is what its `dacp_set_volume` sends
/// when one speaker is active. Naming the speaker is what moves *this* device's volume rather
/// than the sender's master level.
///
/// Same single-speaker simplification as [`get_volume`]: with several speakers active,
/// shairport-sync reads the speaker list first to decide whether to split the target into a new
/// master level plus a relative volume. This always takes the direct route.
pub async fn set_volume(target: &DacpTarget, percent: u8) {
    let percent = percent.min(100);
    send_command(
        target,
        &format!(
            "setproperty?include-speaker-id={}&dmcp.volume={percent}",
            target.machine_number
        ),
    )
    .await;
}

/// The current track's cover art: raw image bytes in the response body, no TLV wrapping and no
/// fetch-by-url step. `None` on any failure, non-200 status, or an empty body.
///
/// Both `mw` and `mh` are always sent because owntone's server fails the request without them —
/// see this module's top docs for why that server, not shairport-sync, confirms this endpoint.
pub async fn fetch_artwork(target: &DacpTarget, width: u32, height: u32) -> Option<Vec<u8>> {
    let path = format!("/ctrl-int/1/nowplayingartwork?mw={width}&mh={height}");
    let (status, headers, body) = get(target, &path).await?;
    if status != 200 || body.is_empty() {
        warn!(
            "airplay: DACP nowplayingartwork got status {status}, headers: {headers:?}, {} bytes",
            body.len()
        );
        return None;
    }
    Some(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a real-shaped `cmst` TLV blob the same way a DACP server would, so
    /// `parse_play_status` is tested against the actual wire format rather than a synthetic
    /// struct.
    fn tlv_item(tag: &[u8; 4], value: &[u8]) -> Vec<u8> {
        let mut out = tag.to_vec();
        out.extend_from_slice(&(value.len() as u32).to_be_bytes());
        out.extend_from_slice(value);
        out
    }

    fn cmst_blob(items: &[u8]) -> Vec<u8> {
        tlv_item(b"cmst", items)
    }

    #[test]
    fn parses_a_realistic_playstatusupdate_body() {
        let mut items = Vec::new();
        items.extend(tlv_item(b"caps", &[4]));
        items.extend(tlv_item(b"cann", b"Sun Is Shining"));
        items.extend(tlv_item(b"cana", b"Bob Marley"));
        items.extend(tlv_item(b"canl", b"Kaya"));
        items.extend(tlv_item(b"astm", &213_000u32.to_be_bytes()));

        let status = parse_play_status(&cmst_blob(&items));

        assert_eq!(status.playing, Some(true));
        assert_eq!(status.title.as_deref(), Some("Sun Is Shining"));
        assert_eq!(status.artist.as_deref(), Some("Bob Marley"));
        assert_eq!(status.album.as_deref(), Some("Kaya"));
        assert_eq!(status.duration_ms, Some(213_000));
    }

    /// A DACP server never sends a position: it sends the track's length and how much of it is
    /// left, and the position is the difference.
    #[test]
    fn the_position_comes_from_the_length_and_the_time_left() {
        let mut items = tlv_item(b"cast", &213_000u32.to_be_bytes());
        items.extend(tlv_item(b"cant", &200_000u32.to_be_bytes()));

        let status = parse_play_status(&cmst_blob(&items));

        assert_eq!(status.position_ms, Some(13_000));
        // `cast` is the same quantity as `astm`, so it stands in when `astm` is absent.
        assert_eq!(status.duration_ms, Some(213_000));
    }

    #[test]
    fn a_status_without_both_time_tags_reports_no_position() {
        let only_total =
            parse_play_status(&cmst_blob(&tlv_item(b"cast", &213_000u32.to_be_bytes())));
        assert_eq!(only_total.position_ms, None);

        let only_remaining =
            parse_play_status(&cmst_blob(&tlv_item(b"cant", &200_000u32.to_be_bytes())));
        assert_eq!(only_remaining.position_ms, None);
    }

    /// A track that has just ended can report more left than there is, for an instant.
    #[test]
    fn more_time_left_than_the_track_has_is_not_a_position_near_the_end_of_time() {
        let mut items = tlv_item(b"cast", &1_000u32.to_be_bytes());
        items.extend(tlv_item(b"cant", &2_000u32.to_be_bytes()));

        assert_eq!(parse_play_status(&cmst_blob(&items)).position_ms, Some(0));
    }

    #[test]
    fn paused_and_stopped_map_to_not_playing() {
        assert_eq!(
            parse_play_status(&cmst_blob(&tlv_item(b"caps", &[3]))).playing,
            Some(false)
        );
        assert_eq!(
            parse_play_status(&cmst_blob(&tlv_item(b"caps", &[2]))).playing,
            Some(false)
        );
    }

    #[test]
    fn unknown_tags_are_skipped_without_breaking_the_walk() {
        let mut items = Vec::new();
        items.extend(tlv_item(b"cang", b"Reggae")); // genre, not one this crate reads
        items.extend(tlv_item(b"cann", b"Sun Is Shining"));

        let status = parse_play_status(&cmst_blob(&items));
        assert_eq!(status.title.as_deref(), Some("Sun Is Shining"));
    }

    #[test]
    fn a_body_not_wrapped_in_cmst_reports_nothing() {
        let not_cmst = tlv_item(b"nope", b"whatever");
        assert_eq!(parse_play_status(&not_cmst), PlayStatus::default());
    }

    #[test]
    fn malformed_input_does_not_panic() {
        assert_eq!(parse_play_status(&[]), PlayStatus::default());
        assert_eq!(parse_play_status(b"cmst"), PlayStatus::default());
        // claims a length far longer than what's actually present
        let mut truncated = b"cmst".to_vec();
        truncated.extend_from_slice(&1000u32.to_be_bytes());
        truncated.extend_from_slice(b"short");
        assert_eq!(parse_play_status(&truncated), PlayStatus::default());
    }

    #[test]
    fn parses_a_volume_response() {
        let body = tlv_item(b"cmgt", &{
            let mut items = tlv_item(b"mstt", &200u32.to_be_bytes());
            items.extend(tlv_item(b"cmvo", &43i32.to_be_bytes()));
            items
        });

        assert_eq!(parse_volume(&body), Some(43));
    }

    #[test]
    fn a_volume_response_without_the_tag_is_unknown() {
        // The right container, but no `cmvo` in it.
        let body = tlv_item(b"cmgt", &tlv_item(b"mstt", &200u32.to_be_bytes()));
        assert_eq!(parse_volume(&body), None);

        // A `cmvo` that isn't inside `cmgt` at all.
        assert_eq!(parse_volume(&tlv_item(b"cmvo", &43i32.to_be_bytes())), None);

        assert_eq!(parse_volume(&[]), None);
    }

    /// The six bytes of `deviceid`, big-endian — the id a DACP server knows this speaker by.
    #[test]
    fn the_machine_number_is_the_device_id_as_an_integer() {
        assert_eq!(machine_number("00:00:00:00:00:01"), 1);
        assert_eq!(machine_number("aa:bb:cc:dd:ee:ff"), 0xAABB_CCDD_EEFF);
        // Uppercase is the same id.
        assert_eq!(machine_number("AA:BB:CC:DD:EE:FF"), 0xAABB_CCDD_EEFF);
    }

    #[test]
    fn a_malformed_device_id_has_no_machine_number() {
        assert_eq!(machine_number(""), 0);
        assert_eq!(machine_number("aa:bb:cc"), 0);
        assert_eq!(machine_number("aa:bb:cc:dd:ee:ff:00"), 0);
        assert_eq!(machine_number("not:a:device:id:at:all"), 0);
    }

    /// The real advertised name from this session's own `tcpdump` capture, matched against the
    /// `DACP-ID` header that came with it.
    #[test]
    fn matches_a_real_advertised_dacp_service() {
        assert!(matches_dacp_id(
            "iTunes_Ctrl_1122334455667788._dacp._tcp.local.",
            "1122334455667788"
        ));
    }

    /// The case shairport-sync's own zero-stripping exists for.
    #[test]
    fn leading_zeroes_on_either_side_still_match() {
        assert!(matches_dacp_id(
            "iTunes_Ctrl_01122334455667788._dacp._tcp.local.",
            "1122334455667788"
        ));
        assert!(matches_dacp_id(
            "iTunes_Ctrl_1122334455667788._dacp._tcp.local.",
            "01122334455667788"
        ));
    }

    #[test]
    fn a_different_sender_does_not_match() {
        // Another sender's service, browsed on the same network at the same time.
        assert!(!matches_dacp_id(
            "iTunes_Ctrl_99AABBCCDDEEFF00._dacp._tcp.local.",
            "1122334455667788"
        ));
        // A prefix of the wanted id is not the wanted id.
        assert!(!matches_dacp_id(
            "iTunes_Ctrl_11223344._dacp._tcp.local.",
            "1122334455667788"
        ));
        // Some other service type that happens to be on the network.
        assert!(!matches_dacp_id(
            "Living Room._raop._tcp.local.",
            "1122334455667788"
        ));
        // An all-zero id must not become an empty string that matches everything.
        assert!(!matches_dacp_id(
            "iTunes_Ctrl_0000._dacp._tcp.local.",
            "1122334455667788"
        ));
    }

    /// The `Mdns` guard is the only thing that stops a per-session mDNS daemon thread from
    /// outliving its session (see that type's docs), so assert the thread really is gone
    /// afterwards rather than trusting that `shutdown()` was called. Skips itself where a daemon
    /// can't start at all (a sandbox with no multicast-capable interface) — the alternative would
    /// be a test that fails for reasons unrelated to this crate.
    #[test]
    fn dropping_the_mdns_guard_shuts_the_daemon_thread_down() {
        let Ok(daemon) = mdns_sd::ServiceDaemon::new() else {
            return;
        };
        // Keeps a handle on the command channel after the guard is gone; the daemon's own end of
        // it is dropped when its thread returns, which is what `status()` reports as `Shutdown`.
        let handle = daemon.clone();

        drop(Mdns(daemon));

        for _ in 0..100 {
            if let Ok(status) = handle.status() {
                if matches!(
                    status.recv_timeout(Duration::from_millis(20)),
                    Ok(mdns_sd::DaemonStatus::Shutdown)
                ) {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the mDNS daemon thread was still running after its guard was dropped");
    }

    #[test]
    fn a_chunked_body_is_reassembled() {
        assert_eq!(
            decode_chunked(b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n").as_deref(),
            Some(&b"hello world"[..])
        );
        // Chunk sizes are hex, and may carry extensions this doesn't care about.
        assert_eq!(
            decode_chunked(b"a;first=1\r\n0123456789\r\n0\r\n\r\n").as_deref(),
            Some(&b"0123456789"[..])
        );
        // A body of nothing is still a complete body.
        assert_eq!(decode_chunked(b"0\r\n\r\n").as_deref(), Some(&b""[..]));
    }

    /// `None` means "keep reading", which is what makes the read loop wait for the rest instead
    /// of handing back half a body.
    #[test]
    fn an_unfinished_chunked_body_is_not_a_body_yet() {
        assert_eq!(decode_chunked(b""), None);
        assert_eq!(decode_chunked(b"5\r\nhel"), None);
        // Every chunk arrived, but the terminating zero-length one hasn't.
        assert_eq!(decode_chunked(b"5\r\nhello\r\n"), None);
    }

    /// `204 No Content` is what a DACP server answers `setproperty` with — every volume change
    /// this crate makes. Without this rule the response has no framing at all, and reading it
    /// would sit for the full request timeout on a connection the server is keeping open.
    #[test]
    fn statuses_that_carry_no_body_are_known() {
        assert!(status_has_no_body(204));
        assert!(status_has_no_body(304));
        assert!(status_has_no_body(100));
        assert!(!status_has_no_body(200));
        assert!(!status_has_no_body(400));
    }

    /// A link-local IPv6 sender is unreachable without the interface its address belongs to.
    #[test]
    fn a_link_local_ipv6_target_keeps_its_scope() {
        let mut target = DacpTarget {
            host: "fe80::1".parse().expect("parses"),
            port: 3689,
            active_remote: "test".to_string(),
            local_bind: None,
            machine_number: 0,
            scope_id: 7,
        };

        match target.socket_addr() {
            std::net::SocketAddr::V6(addr) => assert_eq!(addr.scope_id(), 7),
            other => panic!("expected an IPv6 address, got {other:?}"),
        }

        // An IPv4 target has nowhere to put a scope, and needs none.
        target.host = "192.168.2.5".parse().expect("parses");
        assert!(target.socket_addr().is_ipv4());
    }

    /// Regression test for the real bug found this session: a loopback server that answers with
    /// a complete, well-formed `Content-Length`-framed response but — like a real DACP server —
    /// does **not** close the connection afterward (HTTP/1.1 keep-alive). `get()` (exercised here
    /// through `poll_status`, its only outward-visible caller with parseable output) used to wait
    /// for EOF and would time out on this every time; it must now return promptly instead.
    #[tokio::test]
    async fn poll_status_does_not_wait_for_the_connection_to_close() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binds");
        let addr = listener.local_addr().expect("has an address");

        let items = tlv_item(b"caps", &[4]);
        let body = cmst_blob(&items);

        let (request_tx, request_rx) = tokio::sync::oneshot::channel();

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accepts");
            let mut request = [0u8; 4096];
            // The fixed response below doesn't depend on the request, but the request line is
            // itself under test (see the `revision-number` assertion at the end).
            let read = socket.read(&mut request).await.unwrap_or(0);
            let _ = request_tx.send(String::from_utf8_lossy(&request[..read]).into_owned());

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/x-dmap-tagged\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("writes headers");
            socket.write_all(&body).await.expect("writes body");
            // Deliberately keep `socket` alive instead of dropping/shutting it down here — this
            // is the real-world condition (`Connection` header absent, so HTTP/1.1's keep-alive
            // default applies) that made the pre-fix implementation hang until its own timeout.
            tokio::time::sleep(Duration::from_secs(10)).await;
        });

        let target = DacpTarget {
            host: addr.ip(),
            port: addr.port(),
            active_remote: "test".to_string(),
            local_bind: None,
            machine_number: machine_number("aa:bb:cc:dd:ee:ff"),
            scope_id: 0,
        };

        let status = tokio::time::timeout(Duration::from_secs(1), poll_status(&target))
            .await
            .expect("returned well within the 5s request timeout, let alone the server's 10s hold")
            .expect("parsed a valid response");

        assert_eq!(status.playing, Some(true));

        // The revision number asked for is always 1, never the `cmsr` a response reports — a real
        // DACP server long-polls (hangs) on anything else, see `poll_status`'s docs.
        let request = request_rx.await.expect("the server reported the request");
        assert!(
            request.starts_with("GET /ctrl-int/1/playstatusupdate?revision-number=1 HTTP/1.1\r\n"),
            "unexpected request line: {request:?}"
        );

        server.abort();
    }
}
