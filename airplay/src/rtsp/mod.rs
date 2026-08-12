//! Apple's RTSP/1.0 control server: request parsing and verb dispatch.
//!
//! `fp-setup`/`pair-setup`/`pair-verify`/`pair-add`/`pair-remove`/`pair-list` (`crate::pairing`),
//! `GET /info`, `SETUP`, `RECORD`, `TEARDOWN`/`FLUSH`, and `SET_PARAMETER`/`GET_PARAMETER` are
//! wired up; everything else is still a placeholder `501 Not Implemented`.
//! `fp-setup` is dispatched *first* in the match below, ahead of `pair-setup` — confirmed via a
//! real device that this is the actual order a real sender calls them in.
//!
//! `SET_PARAMETER`/`GET_PARAMETER` always answer `200`, matching shairport-sync's own
//! `handle_set_parameter`/`handle_get_parameter` exactly (both set `resp->respcode = 200`
//! unconditionally, regardless of `Content-Type` or body content) — confirmed necessary by a real
//! device: it sends `SET_PARAMETER` (a volume announcement, `Content-Type: text/parameters`,
//! body `volume: <float>\r\n`) immediately after `RECORD`, and this crate previously fell through
//! to the same `501` as every other unhandled verb, which was enough for the sender to abort the
//! whole session (`FLUSH`/`TEARDOWN` right after, no audio ever sent). Once audio was actually
//! playing, a real device's `volume: <float>` line turned out to matter for real too — the
//! iPhone's own volume slider has no other way to reach this receiver — so `handle_set_parameter`
//! now parses it and surfaces `RtspAction::SetLegacyVolume` for `connection::serve_connection` to
//! forward to the running classic session (see `legacy::volume_to_gain`'s docs for the
//! conversion). `GET_PARAMETER`'s volume-report body (shairport-sync replies
//! `\r\nvolume: <current>\r\n` when asked specifically for `volume`) is still skipped — this
//! crate has no per-session volume *state* to report honestly (only a one-way gain feed), and
//! nothing observed so far depends on that body being present, only on the response code not
//! being an error.
//! `handle_request` returns an [`RtspAction`]
//! alongside the `Response` for the verbs that need `connection::serve_connection` to actually
//! start/stop reading the RTP data socket — this module has no socket access itself, only
//! `RtpPorts`' numbers. `connection::serve_connection` drives a real `TcpStream` through
//! `RtspSession`/`handle_request`, switching to `encrypted_stream`-framed traffic once
//! `pair-verify` completes — see `connection`'s and `encrypted_stream`'s module docs for which
//! parts of that switch-over are confirmed correct vs. still need validating against a live
//! capture.
//!
//! Only `TEARDOWN` ends the data flow for a connection (consumes the recording task and its RTP
//! data socket via `RtspAction::StopRecording`) — a real sender expecting to `RECORD` again after
//! `TEARDOWN` without a fresh `SETUP` won't work yet. Real RTSP allows that; this doesn't yet.
//! `FLUSH` does **not** stop anything (a real device sends it routinely right after `RECORD`,
//! while continuing to stream RTP afterward — confirmed via shairport-sync's own classic
//! `handle_flush`, which only discards buffered audio up to an RTP timestamp and never touches the
//! player thread/socket; conflating it with `TEARDOWN`, as an earlier version of this dispatcher
//! did, silently discarded every classic session's audio the moment the first `FLUSH` arrived).

mod connection;
mod message;

pub(crate) use connection::{ConnectionContext, serve_connection};
pub(crate) use message::{Request, Response};

use base64::Engine as _;
use log::{debug, warn};

use crate::{device_id::DeviceId, legacy::LegacyAudioSession};

/// What this connection's `SETUP` response tells the sender about itself: the UDP ports bound for
/// the event, control and data channels, and the address the sender reached this receiver at.
pub(crate) struct RtpPorts {
    pub(crate) event_port: u16,
    pub(crate) control_port: u16,
    pub(crate) data_port: u16,
    /// Signed into the `Apple-Response` a sender demands — see [`apple_challenge`].
    pub(crate) local_ip: std::net::IpAddr,
}

/// Per-connection state: the two pairing state machines (a fresh connection may go through
/// either or both, depending on whether the sender already completed `pair-setup` in an earlier
/// session), where paired controllers get looked up/saved, and — once negotiated — the audio
/// stream's decrypt key and codec parameters.
/// Per-connection state: what `ANNOUNCE` negotiated for the audio stream, once it has.
pub(crate) struct RtspSession {
    audio: Option<LegacyAudioSession>,
}

impl Default for RtspSession {
    fn default() -> Self {
        Self::new()
    }
}

impl RtspSession {
    pub(crate) fn new() -> Self {
        Self { audio: None }
    }

    /// Moves the negotiated session out, for `connection::serve_connection` to start the audio
    /// loop with on `RECORD`. Leaves `None` behind, so a `RECORD` without an intervening
    /// `ANNOUNCE` gets `handle_request`'s 455 rather than trying to play with nothing to decrypt
    /// with.
    pub(crate) fn take_audio_session(&mut self) -> Option<LegacyAudioSession> {
        self.audio.take()
    }
}

/// What `connection::serve_connection` should do about the RTP data socket after sending the
/// paired `Response` — `handle_request` itself has no socket access, only `RtpPorts`' numbers.
#[derive(Debug)]
pub(crate) enum RtspAction {
    StartRecording,
    StopRecording,
    /// A `SET_PARAMETER` `volume: <float>` line was parsed (see `handle_set_parameter`), already
    /// converted to a linear gain (`legacy::volume_to_gain`) — `connection::serve_connection`
    /// forwards it to the running session's `legacy::receive_loop`, if any. A no-op when nothing
    /// is recording yet (a sender sets the volume before `RECORD`) — `handle_set_parameter` has
    /// no way to know that, only the connection driver does.
    SetVolume {
        /// Linear amplitude multiplier for the decoded samples.
        gain: f64,
        /// The same value as a percentage, for reporting onward — a client of the fork's UDP API
        /// publishes percentages, not decibels.
        percent: u8,
    },
    /// `Active-Remote`/`DACP-ID` headers captured from a classic `SETUP` — see
    /// `handle_legacy_setup`'s docs for why this travels as an action rather than living on
    /// `RtspSession`. A no-op without the `remote-control` feature, hence the fields being
    /// genuinely unread (not just unused) in that configuration.
    #[cfg_attr(not(feature = "remote-control"), allow(dead_code))]
    DacpInfoCaptured {
        active_remote: String,
        dacp_id: String,
    },
    /// Now-playing text the sender pushed in a `SET_PARAMETER`. Not gated on any feature: it
    /// arrives on the RTSP connection that already exists.
    MetadataReported(crate::dmap::TrackMetadata),
    /// Cover art the sender pushed, raw bytes plus the type sniffed from them.
    CoverReported {
        bytes: Vec<u8>,
        mime: &'static str,
    },
    /// Where the sender says it is in the track, from a `progress:` line.
    ProgressReported(crate::dmap::Progress),
}

/// `device_name` is the shared `-n`/`--name` value (plan §3) — threaded in here rather than
/// stored on `RtspSession`/`DeviceId` since it's display-only and can change without
/// needing a new pairing identity.
pub(crate) fn handle_request(
    session: &mut RtspSession,
    device_id: &DeviceId,
    _device_name: &str,
    ports: &RtpPorts,
    request: &Request,
) -> (Response, Option<RtspAction>) {
    let (mut response, action) = match (request.method.as_str(), request.uri.as_str()) {
        ("ANNOUNCE", _) => (handle_announce(session, request), None),
        ("SETUP", _) => handle_legacy_setup(ports, request),
        ("RECORD", _) => handle_record(session, request),
        ("TEARDOWN", _) => (Response::ok(request.cseq), Some(RtspAction::StopRecording)),
        // Real `FLUSH` doesn't end the stream — confirmed via shairport-sync's own classic
        // `handle_flush`, which only calls `player_flush(rtptime, conn)` (discard buffered audio
        // up to an RTP timestamp) and never touches the player thread or its socket; only
        // `handle_teardown` does that. A real device sends `FLUSH` routinely right after `RECORD`
        // (priming playback) while continuing to stream RTP afterward — this crate previously
        // treated `FLUSH` the same as `TEARDOWN` (`RtspAction::StopRecording`, which aborts the
        // RTP receive task and drops its socket), so every classic session's audio was silently
        // discarded the moment the very first `FLUSH` arrived. This crate has no playback buffer
        // to actually discard up to `rtptime` (`legacy::receive_loop` decodes and plays
        // immediately), so a bare `200` is the whole of a correct response here.
        ("FLUSH", _) => (Response::ok(request.cseq), None),
        ("OPTIONS", _) => (Response::ok(request.cseq), None),
        ("SET_PARAMETER", _) => handle_set_parameter(request),
        ("GET_PARAMETER", _) => (handle_get_parameter(request), None),
        _ => (Response::not_implemented(request.cseq), None),
    };

    // Answered on whatever the response turns out to be, exactly as shairport-sync's
    // `apple_challenge` is called for every request and returns immediately when the header is
    // absent.
    apple_challenge(request, &mut response, ports.local_ip, device_id);
    (response, action)
}

/// Answers a classic sender's `Apple-Challenge` with the `Apple-Response` it will not stream
/// without — the AirPlay 1 authentication step, in which the receiver proves it holds the RAOP
/// private key (`legacy::rsa_key`).
///
/// The signed buffer is shairport-sync's, byte for byte (`apple_challenge`, `rtsp.c`): the
/// base64-decoded challenge, then the address the sender reached this receiver at (4 bytes for
/// IPv4, 16 for IPv6), then the six bytes of the device id, zero-padded to at least 32 bytes.
/// The signature is base64 with the padding stripped.
///
/// A sender that gets no answer to this hangs up — which is exactly what a real iPhone did
/// against this receiver before it was implemented.
fn apple_challenge(
    request: &Request,
    response: &mut Response,
    local_ip: std::net::IpAddr,
    device_id: &DeviceId,
) {
    let Some(challenge) = request
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("Apple-Challenge"))
        .map(|(_, value)| value.as_str())
    else {
        return;
    };

    let engine = base64::engine::general_purpose::STANDARD;
    let Ok(challenge) = engine.decode(challenge.trim()) else {
        warn!("airplay: Apple-Challenge is not base64");
        return;
    };
    if challenge.len() > 16 {
        warn!(
            "airplay: oversized Apple-Challenge ({} bytes)",
            challenge.len()
        );
        return;
    }

    let mut buffer = challenge;
    match local_ip {
        std::net::IpAddr::V4(address) => buffer.extend_from_slice(&address.octets()),
        std::net::IpAddr::V6(address) => buffer.extend_from_slice(&address.octets()),
    }
    buffer.extend_from_slice(&device_id.bytes());
    buffer.resize(buffer.len().max(0x20), 0);

    let Some(signature) = crate::legacy::sign_challenge(&buffer) else {
        return;
    };
    response.headers.push((
        "Apple-Response".to_string(),
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(&signature),
    ));
    debug!("airplay: answered an Apple-Challenge");
}

/// `RECORD` follows `ANNOUNCE` and `SETUP`, and starts the audio flowing. A `RECORD` on a
/// connection that negotiated nothing gets `455` ("Method Not Valid In This State"), the standard
/// RTSP status for exactly that.
fn handle_record(session: &RtspSession, request: &Request) -> (Response, Option<RtspAction>) {
    if session.audio.is_some() {
        return (Response::ok(request.cseq), Some(RtspAction::StartRecording));
    }

    (
        Response::new(455, "Method Not Valid In This State", request.cseq),
        None,
    )
}

/// `ANNOUNCE` — the session-negotiation step; see `crate::legacy`'s module docs for what it
/// negotiates, including the unencrypted case (confirmed by a real device, not merely supported
/// because the reference is).
/// `456` ("Header Field Not Valid for Resource") on failure mirrors shairport-sync's own
/// `handle_announce` exactly — every `resp->respcode` assignment in that function for a malformed
/// or mismatched `a=rsaaeskey:`/`a=aesiv:` pair uses 456, never 451.
fn handle_announce(session: &mut RtspSession, request: &Request) -> Response {
    match crate::legacy::handle_announce(&request.body) {
        Ok(legacy_audio) => {
            session.audio = Some(legacy_audio);
            Response::ok(request.cseq)
        }
        Err(err) => {
            warn!(
                "airplay: failed to process ANNOUNCE: {err}; body: {:?}",
                String::from_utf8_lossy(&request.body)
            );
            Response::new(456, "Header Field Not Valid for Resource", request.cseq)
        }
    }
}

/// `SET_PARAMETER` — see this module's own top-level docs for why this always answers `200`
/// regardless of content. `text/parameters` bodies (one `key: value` pair per line, shairport-
/// sync's own `handle_set_parameter_parameter`) are logged at `debug` for visibility; `volume:
/// <float>` lines are additionally parsed and surfaced as `RtspAction::SetLegacyVolume` — a real
/// device has no other way to control playback volume for a classic session, confirmed necessary
/// once audio was actually flowing (see the `legacy::volume_to_gain` docs for the conversion).
/// `progress: <start>/<now>/<end>` becomes [`RtspAction::ProgressReported`];
/// `application/x-dmap-tagged` and `image/*` become [`RtspAction::MetadataReported`] and
/// [`RtspAction::CoverReported`]. Any other line is logged only.
///
/// None of those three arrive unless the receiver asked for them with `md=0,1,2` in its
/// `_raop._tcp` TXT record — see `discovery::raop_txt_record`. The parsing follows shairport-sync's
/// own code; `crate::dmap` cites it.
fn handle_set_parameter(request: &Request) -> (Response, Option<RtspAction>) {
    let content_type = request
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("Content-Type"))
        .map(|(_, value)| value.as_str());
    let mut action = None;
    match content_type {
        Some(ct) if ct.starts_with("text/parameters") => {
            let body = String::from_utf8_lossy(&request.body);
            debug!("airplay: SET_PARAMETER text/parameters: {body:?}");
            // One action per request, and volume wins: it is the one with an audible effect,
            // and a real sender sends one parameter per request anyway.
            if let Some(db) = body
                .lines()
                .find_map(|line| line.strip_prefix("volume: "))
                .and_then(|v| v.trim().parse::<f64>().ok())
            {
                action = Some(RtspAction::SetVolume {
                    gain: crate::legacy::volume_to_gain(db),
                    percent: crate::percent_from_db(db),
                });
            } else if let Some(progress) = body
                .lines()
                .find_map(|line| line.strip_prefix("progress: "))
                .and_then(crate::dmap::parse_progress)
            {
                action = Some(RtspAction::ProgressReported(progress));
            }
        }
        Some(ct) if ct.starts_with("application/x-dmap-tagged") => {
            let metadata = crate::dmap::parse_metadata(&request.body);
            debug!(
                "airplay: SET_PARAMETER metadata, {} bytes: {metadata:?}",
                request.body.len()
            );
            // A body carrying only tags this crate has nowhere to put is not a track change.
            if !metadata.is_empty() {
                action = Some(RtspAction::MetadataReported(metadata));
            }
        }
        Some(ct) if ct.starts_with("image/") => {
            let mime = crate::dmap::sniff_image_mime(&request.body);
            debug!(
                "airplay: SET_PARAMETER cover art, {} bytes, sniffed as {mime}",
                request.body.len()
            );
            // A sender with no artwork for this track sends an empty body; shipping that on
            // would blank a cover the client already has.
            if !request.body.is_empty() {
                action = Some(RtspAction::CoverReported {
                    bytes: request.body.clone(),
                    mime,
                });
            }
        }
        Some(ct) => debug!("airplay: SET_PARAMETER with unhandled Content-Type {ct:?}"),
        None => debug!("airplay: SET_PARAMETER with no Content-Type header"),
    }
    (Response::ok(request.cseq), action)
}

/// `SETUP` — reads the client's `Transport` header (`control_port=`/`timing_port=`, both required
/// for a 200, as shairport-sync's own `handle_setup` requires them) and replies with this crate's
/// reserved ports in the same shape. The body is empty: everything travels in the header.
fn handle_legacy_setup(ports: &RtpPorts, request: &Request) -> (Response, Option<RtspAction>) {
    let transport = request
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("Transport"))
        .map(|(_, value)| value.as_str());
    let Some(transport) = transport else {
        return (Response::new(451, "Invalid Arguments", request.cseq), None);
    };
    if !transport.contains("control_port=") || !transport.contains("timing_port=") {
        return (Response::new(451, "Invalid Arguments", request.cseq), None);
    }

    let mut response = Response::ok(request.cseq);
    response.headers.push((
        "Transport".to_string(),
        crate::legacy::setup_transport_header(
            ports.control_port,
            ports.event_port,
            ports.data_port,
        ),
    ));
    response
        .headers
        .push(("Session".to_string(), "1".to_string()));

    (response, dacp_info(request))
}

/// The volume this receiver reports when asked, in AirPlay's dB scale: `-30.0` (quietest) to
/// `0.0` (loudest), with `-144.0` meaning muted.
///
/// Fixed, because this crate keeps no per-session volume state — a sender's `SET_PARAMETER`
/// `volume:` line is forwarded straight to the running session's gain (`legacy::volume_to_gain`)
/// and not stored. shairport-sync answers from `suggested_volume`, which is its
/// `config.default_airplay_volume` (`-24.0`) until a session sets its own; this reports that same
/// default and never changes it.
const DEFAULT_AIRPLAY_VOLUME: f64 = -24.0;

/// `GET_PARAMETER` — answers a `volume` query with the volume, and everything else with a bare
/// `200`.
///
/// A real sender asks for this right after `SETUP` and will not go on without an answer, which is
/// how the empty `200` this used to return turned out to matter. shairport-sync's
/// `handle_get_parameter` replies with exactly this body shape — `"\r\nvolume: %.6f\r\n"`, and
/// no `Content-Type` header — when the request body is the string `volume`.
fn handle_get_parameter(request: &Request) -> Response {
    let mut response = Response::ok(request.cseq);
    if request.body.starts_with(b"volume") {
        response.body = format!("\r\nvolume: {DEFAULT_AIRPLAY_VOLUME:.6}\r\n").into_bytes();
        debug!("airplay: GET_PARAMETER volume -> {DEFAULT_AIRPLAY_VOLUME}");
    } else {
        debug!(
            "airplay: GET_PARAMETER for something other than volume: {:?}",
            String::from_utf8_lossy(&request.body)
        );
    }
    response
}

/// `Active-Remote` + `DACP-ID`, the pair that says how to reach a sender back for remote control
/// (`crate::dacp`). Both or neither: one without the other names no server.
///
/// A classic sender puts them on every request, `OPTIONS` included — which is the whole reason
/// this crate advertises itself as one: on AirPlay 2 they never appear at all, and with them goes
/// any way to pause or skip.
///
/// Absent headers produce no action rather than clearing what was already captured — unlike
/// shairport-sync, which nulls its stored pair whenever a request omits them. Forgetting a
/// working endpoint because one request didn't repeat it would only lose control of a live
/// session.
fn dacp_info(request: &Request) -> Option<RtspAction> {
    let header = |name: &str| {
        request
            .headers
            .iter()
            .find(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    };

    match (header("Active-Remote"), header("DACP-ID")) {
        (Some(active_remote), Some(dacp_id)) => {
            debug!(
                "airplay: {} carried Active-Remote={active_remote:?} DACP-ID={dacp_id:?}",
                request.method
            );
            Some(RtspAction::DacpInfoCaptured {
                active_remote,
                dacp_id,
            })
        }
        _ => {
            debug!(
                "airplay: {} has no Active-Remote/DACP-ID — headers present: {:?}",
                request.method,
                request
                    .headers
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, uri: &str, body: Vec<u8>) -> Request {
        Request {
            method: method.to_string(),
            uri: uri.to_string(),
            cseq: Some(1),
            headers: Vec::new(),
            body,
        }
    }

    fn ports() -> RtpPorts {
        RtpPorts {
            event_port: 7000,
            control_port: 7001,
            data_port: 7002,
            local_ip: "192.168.0.196".parse().expect("parses"),
        }
    }

    #[test]
    fn options_is_ok() {
        let mut session = RtspSession::new();
        let device_id = DeviceId::from_name("test-device");
        let (response, _action) = handle_request(
            &mut session,
            &device_id,
            "test-device",
            &ports(),
            &request("OPTIONS", "*", Vec::new()),
        );
        assert_eq!(response.status, 200);
    }

    #[test]
    fn unhandled_verb_is_not_implemented() {
        let mut session = RtspSession::new();
        let device_id = DeviceId::from_name("test-device");
        // SETPEERS: a real classic-AirPlay verb shairport-sync itself still leaves as
        // `handle_unimplemented_ap1` — genuinely not wired up here either, unlike
        // SET_PARAMETER/GET_PARAMETER (see their own tests below).
        let (response, _action) = handle_request(
            &mut session,
            &device_id,
            "test-device",
            &ports(),
            &request("SETPEERS", "/", Vec::new()),
        );
        assert_eq!(response.status, 501);
    }

    /// A classic sender hangs up if this goes unanswered, so the header has to appear on the
    /// response to whatever request carried the challenge.
    #[test]
    fn an_apple_challenge_is_answered() {
        let mut session = RtspSession::new();
        let device_id = DeviceId::from_name("test-device");
        let mut req = request("OPTIONS", "*", Vec::new());
        req.headers.push((
            "Apple-Challenge".to_string(),
            // 16 bytes, the size a real sender sends.
            "ZjhFk3uE8EuJTz8mVVszVg==".to_string(),
        ));

        let (response, _action) =
            handle_request(&mut session, &device_id, "test-device", &ports(), &req);

        let answer = response
            .headers
            .iter()
            .find(|(name, _)| name == "Apple-Response")
            .map(|(_, value)| value.clone())
            .expect("the challenge is answered");
        // A 2048-bit RSA signature, base64 with the padding stripped, as shairport-sync sends it.
        assert_eq!(
            base64::engine::general_purpose::STANDARD_NO_PAD
                .decode(&answer)
                .expect("valid base64")
                .len(),
            256
        );
        assert!(!answer.contains('='), "the padding is stripped");
    }

    #[test]
    fn a_request_without_a_challenge_is_answered_without_one() {
        let mut session = RtspSession::new();
        let device_id = DeviceId::from_name("test-device");

        let (response, _action) = handle_request(
            &mut session,
            &device_id,
            "test-device",
            &ports(),
            &request("OPTIONS", "*", Vec::new()),
        );

        assert!(
            !response
                .headers
                .iter()
                .any(|(name, _)| name == "Apple-Response")
        );
    }

    /// Adds the pair a sender uses to say how to reach it back.
    fn with_dacp_headers(mut request: Request) -> Request {
        request
            .headers
            .push(("Active-Remote".to_string(), "1234567890".to_string()));
        request
            .headers
            .push(("DACP-ID".to_string(), "1122334455667788".to_string()));
        request
    }

    fn assert_captured_dacp_info(action: Option<RtspAction>) {
        match action {
            Some(RtspAction::DacpInfoCaptured {
                active_remote,
                dacp_id,
            }) => {
                assert_eq!(active_remote, "1234567890");
                assert_eq!(dacp_id, "1122334455667788");
            }
            other => panic!("expected Some(DacpInfoCaptured {{ .. }}), got {other:?}"),
        }
    }

    #[test]
    fn a_request_without_the_headers_captures_nothing() {
        let mut session = RtspSession::new();
        let device_id = DeviceId::from_name("test-device");

        let (_response, action) = handle_request(
            &mut session,
            &device_id,
            "test-device",
            &ports(),
            &request("GET", "/info", Vec::new()),
        );

        assert!(action.is_none());
    }

    /// Half the pair names no server: one without the other is nothing to act on.
    #[test]
    fn only_one_of_the_two_headers_captures_nothing() {
        let mut session = RtspSession::new();
        let device_id = DeviceId::from_name("test-device");
        let mut req = request("GET", "/info", Vec::new());
        req.headers
            .push(("Active-Remote".to_string(), "1234567890".to_string()));

        let (_response, action) =
            handle_request(&mut session, &device_id, "test-device", &ports(), &req);

        assert!(action.is_none());
    }

    /// `SETUP` is where a sender says how to reach it back for remote control, and every request
    /// on this path repeats it.
    #[test]
    fn a_classic_setup_captures_dacp_info() {
        let mut session = RtspSession::new();
        let device_id = DeviceId::from_name("test-device");
        let announce = request(
            "ANNOUNCE",
            "/",
            b"v=0\r\ns=iTunes\r\na=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n".to_vec(),
        );
        handle_request(&mut session, &device_id, "test-device", &ports(), &announce);

        let mut req = with_dacp_headers(request("SETUP", "/", Vec::new()));
        req.headers.push((
            "Transport".to_string(),
            "RTP/AVP/UDP;unicast;control_port=6001;timing_port=6002".to_string(),
        ));

        let (response, action) =
            handle_request(&mut session, &device_id, "test-device", &ports(), &req);

        assert_eq!(response.status, 200);
        assert_captured_dacp_info(action);
    }

    #[test]
    fn set_parameter_always_answers_ok() {
        let mut session = RtspSession::new();
        let device_id = DeviceId::from_name("test-device");
        let mut req = request("SET_PARAMETER", "/", b"something: else\r\n".to_vec());
        req.headers
            .push(("Content-Type".to_string(), "text/parameters".to_string()));

        let (response, action) =
            handle_request(&mut session, &device_id, "test-device", &ports(), &req);

        assert_eq!(response.status, 200);
        assert!(action.is_none());
    }

    /// Builds the `SET_PARAMETER` a sender pushes, with the `Content-Type` that decides how the
    /// body is read.
    fn set_parameter(content_type: &str, body: Vec<u8>) -> Request {
        let mut req = request("SET_PARAMETER", "/", body);
        req.headers
            .push(("Content-Type".to_string(), content_type.to_string()));
        req
    }

    fn dmap_item(tag: &[u8; 4], value: &[u8]) -> Vec<u8> {
        let mut out = tag.to_vec();
        out.extend_from_slice(&(value.len() as u32).to_be_bytes());
        out.extend_from_slice(value);
        out
    }

    /// The push a sender makes once `md=0,1,2` has asked for it: now-playing text as DMAP.
    #[test]
    fn set_parameter_metadata_surfaces_the_track() {
        let mut session = RtspSession::new();
        let device_id = DeviceId::from_name("test-device");
        let mut items = dmap_item(b"minm", b"Sun Is Shining");
        items.extend(dmap_item(b"asar", b"Bob Marley"));
        let req = set_parameter("application/x-dmap-tagged", dmap_item(b"mlit", &items));

        let (response, action) =
            handle_request(&mut session, &device_id, "test-device", &ports(), &req);

        assert_eq!(response.status, 200);
        match action {
            Some(RtspAction::MetadataReported(metadata)) => {
                assert_eq!(metadata.title.as_deref(), Some("Sun Is Shining"));
                assert_eq!(metadata.artist.as_deref(), Some("Bob Marley"));
            }
            other => panic!("expected Some(MetadataReported(_)), got {other:?}"),
        }
    }

    /// The picture arrives as its own request, right after the text.
    #[test]
    fn set_parameter_cover_art_surfaces_its_bytes_and_type() {
        let mut session = RtspSession::new();
        let device_id = DeviceId::from_name("test-device");
        let jpeg = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10];
        let req = set_parameter("image/jpeg", jpeg.clone());

        let (response, action) =
            handle_request(&mut session, &device_id, "test-device", &ports(), &req);

        assert_eq!(response.status, 200);
        match action {
            Some(RtspAction::CoverReported { bytes, mime }) => {
                assert_eq!(bytes, jpeg);
                assert_eq!(mime, "image/jpeg");
            }
            other => panic!("expected Some(CoverReported {{ .. }}), got {other:?}"),
        }
    }

    /// A sender with artwork enabled but none for this track sends an empty body — blanking a
    /// cover the client already has would be worse than leaving it alone.
    #[test]
    fn set_parameter_empty_cover_art_reports_nothing() {
        let mut session = RtspSession::new();
        let device_id = DeviceId::from_name("test-device");
        let req = set_parameter("image/none", Vec::new());

        let (response, action) =
            handle_request(&mut session, &device_id, "test-device", &ports(), &req);

        assert_eq!(response.status, 200);
        assert!(action.is_none());
    }

    /// `progress:` is the only place a classic session states its position outright — three RTP
    /// timestamps at 44100 Hz, so 45100 frames past a start of 1000 is one second in.
    #[test]
    fn set_parameter_progress_line_surfaces_a_position() {
        let mut session = RtspSession::new();
        let device_id = DeviceId::from_name("test-device");
        let req = set_parameter(
            "text/parameters",
            b"progress: 1000/45100/9394000\r\n".to_vec(),
        );

        let (response, action) =
            handle_request(&mut session, &device_id, "test-device", &ports(), &req);

        assert_eq!(response.status, 200);
        match action {
            Some(RtspAction::ProgressReported(progress)) => {
                assert_eq!(progress.position_ms, 1_000);
                assert_eq!(progress.duration_ms, 212_993);
            }
            other => panic!("expected Some(ProgressReported(_)), got {other:?}"),
        }
    }

    /// The real shape captured this session — a device's routine post-`RECORD` volume
    /// announcement, the only way it can control this receiver's playback volume for a classic
    /// session.
    #[test]
    fn set_parameter_volume_line_surfaces_a_legacy_volume_action() {
        let mut session = RtspSession::new();
        let device_id = DeviceId::from_name("test-device");
        let mut req = request("SET_PARAMETER", "/", b"volume: -15.0\r\n".to_vec());
        req.headers
            .push(("Content-Type".to_string(), "text/parameters".to_string()));

        let (response, action) =
            handle_request(&mut session, &device_id, "test-device", &ports(), &req);

        assert_eq!(response.status, 200);
        match action {
            Some(RtspAction::SetVolume { gain, percent }) => {
                assert_eq!(gain, crate::legacy::volume_to_gain(-15.0));
                assert_eq!(percent, 50, "halfway up AirPlay's -30..0 dB scale");
            }
            other => panic!("expected Some(SetVolume {{ .. }}), got {other:?}"),
        }
    }

    #[test]
    /// A real sender asks for the volume right after `SETUP` and does not continue without an
    /// answer, so the body matters, not just the status.
    fn get_parameter_reports_the_volume() {
        let mut session = RtspSession::new();
        let device_id = DeviceId::from_name("test-device");
        let (response, _action) = handle_request(
            &mut session,
            &device_id,
            "test-device",
            &ports(),
            &request("GET_PARAMETER", "/", b"volume\r\n".to_vec()),
        );

        assert_eq!(response.status, 200);
        assert_eq!(
            String::from_utf8_lossy(&response.body),
            "\r\nvolume: -24.000000\r\n",
            "shairport-sync's exact body shape"
        );
    }

    /// Anything else is still answered, just with nothing in it.
    #[test]
    fn get_parameter_for_anything_else_is_an_empty_ok() {
        let mut session = RtspSession::new();
        let device_id = DeviceId::from_name("test-device");
        let (response, _action) = handle_request(
            &mut session,
            &device_id,
            "test-device",
            &ports(),
            &request("GET_PARAMETER", "/", b"something\r\n".to_vec()),
        );

        assert_eq!(response.status, 200);
        assert!(response.body.is_empty());
    }

    #[test]
    fn record_without_setup_is_rejected() {
        let mut session = RtspSession::new();
        let device_id = DeviceId::from_name("test-device");
        let (response, action) = handle_request(
            &mut session,
            &device_id,
            "test-device",
            &ports(),
            &request("RECORD", "/", Vec::new()),
        );
        assert_eq!(response.status, 455);
        assert!(action.is_none());
    }

    #[test]
    fn record_after_setup_starts_recording() {
        let mut session = RtspSession::new();
        // What `ANNOUNCE` leaves behind: an unencrypted session, which is a real, supported case.
        let body = b"v=0\r\ns=iTunes\r\na=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n".to_vec();
        handle_request(
            &mut session,
            &DeviceId::from_name("test-device"),
            "test-device",
            &ports(),
            &request("ANNOUNCE", "/", body),
        );
        let device_id = DeviceId::from_name("test-device");
        let (response, action) = handle_request(
            &mut session,
            &device_id,
            "test-device",
            &ports(),
            &request("RECORD", "/", Vec::new()),
        );
        assert_eq!(response.status, 200);
        assert!(matches!(action, Some(RtspAction::StartRecording)));
    }

    #[test]
    fn teardown_stops_recording() {
        let mut session = RtspSession::new();
        let device_id = DeviceId::from_name("test-device");
        let (response, action) = handle_request(
            &mut session,
            &device_id,
            "test-device",
            &ports(),
            &request("TEARDOWN", "/", Vec::new()),
        );
        assert_eq!(response.status, 200);
        assert!(matches!(action, Some(RtspAction::StopRecording)));
    }

    /// Regression test for the real bug this session found: `FLUSH` conflated with `TEARDOWN`
    /// silently killed a classic session's RTP receive task the moment a real device sent its
    /// routine post-`RECORD` `FLUSH`, discarding all audio with no error anywhere.
    #[test]
    fn flush_does_not_stop_recording() {
        let mut session = RtspSession::new();
        let device_id = DeviceId::from_name("test-device");
        let (response, action) = handle_request(
            &mut session,
            &device_id,
            "test-device",
            &ports(),
            &request("FLUSH", "/", Vec::new()),
        );
        assert_eq!(response.status, 200);
        assert!(action.is_none());
    }
}
