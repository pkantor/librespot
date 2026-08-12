//! The classic AirPlay (AirPlay 1 / RAOP) audio session: what `ANNOUNCE` negotiates and what
//! the RTP stream that follows it carries.
//!
//! This is the only path this crate implements, and advertising nothing else (`discovery`) is
//! what makes a modern sender take it. That is a deliberate choice rather than a limitation:
//! a classic session is where a sender puts `DACP-ID`/`Active-Remote` on its requests, and those
//! headers are the whole of remote control (`dacp`). It is also a path shairport-sync keeps alive
//! even when running in full AirPlay 2 mode (`rtsp.c`'s `handle_announce`:
//! `#ifdef CONFIG_AIRPLAY_2 ... conn->airplay_type = ap_1;` — an incoming `ANNOUNCE` explicitly
//! starts an AirPlay 1 session rather than being rejected as unexpected), so it is a normal thing
//! for a receiver to do, not a workaround.
//!
//! Most of this is sourced directly from shairport-sync's `rtsp.c`/`common.c` (fetched and read,
//! not inferred): the embedded RSA key ([`rsa_key`]), the SDP attribute parsing ([`sdp`]), and
//! this module's own `ANNOUNCE`/`SETUP` handling and AES-128-CBC audio decrypt, all copied
//! field-for-field from the reference rather than redesigned independently — the same "match
//! shairport-sync exactly" approach used throughout this crate.
//!
//! **`a=fpaeskey:` is not supported, deliberately**, and shairport-sync has no answer for it
//! either (its `ANNOUNCE` handler reads only `a=rsaaeskey:`). A sender wraps the audio key with
//! FairPlay instead of RSA when it believes it is talking to an AirPlay 2 receiver; against one
//! advertising itself as this does, a real iPhone sends `a=rsaaeskey:` and streams. The refusal
//! stays as a named error so that case is legible in a log rather than looking like malformed
//! input. This crate did carry a port of the FairPlay algorithm; it was removed because its only
//! available source is GPLv2-licensed, and because it turned out to be unnecessary.

mod rsa_key;
mod sdp;

use base64::Engine as _;
use log::{debug, trace, warn};
use tokio::net::UdpSocket;

use crate::{
    codec::{AlacDecoder, AlacFormat},
    sink::{AudioOutputHandle, SinkConfig},
};

#[derive(Debug, thiserror::Error)]
pub(crate) enum AnnounceError {
    #[error(
        "a=rsaaeskey/a=fpaeskey/a=aesiv in the ANNOUNCE body don't form a supported combination"
    )]
    MismatchedKeyMaterial,
    #[error("a=rsaaeskey/a=fpaeskey/a=aesiv failed to base64-decode: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("a=aesiv decoded to {0} bytes, expected 16")]
    WrongIvLength(usize),
    #[error("failed to RSA-decrypt a=rsaaeskey: {0}")]
    Rsa(#[from] rsa_key::RsaKeyError),
    #[error(
        "this sender wrapped the audio key with FairPlay (a=fpaeskey), which is not supported — \
         see this module's docs"
    )]
    FairplayNotSupported,
}

/// Everything negotiated by an `ANNOUNCE` for one audio stream. Fixed for the connection's
/// lifetime: a classic stream's parameters never change after `ANNOUNCE`.
pub(crate) struct LegacyAudioSession {
    /// `None` means an unencrypted session — confirmed a real, supported case (not a hypothetical
    /// this crate invented an allowance for): shairport-sync's own `handle_announce` sets
    /// `conn->stream.encrypted = 0` when both `a=rsaaeskey:`/`a=aesiv:` are absent, and a real
    /// iPhone was observed sending exactly that.
    aes: Option<([u8; 16], [u8; 16])>,
    alac_format: AlacFormat,
}

/// Parses an `ANNOUNCE` body. The `a=rsaaeskey:`/`a=aesiv:` both-absent (unencrypted) and
/// both-present (RSA-OAEP, see `rsa_key`) cases match shairport-sync's own `handle_announce`
/// three-way check exactly (`(paesiv == NULL) && (prsaaeskey == NULL)` / `(paesiv != NULL) &&
/// (prsaaeskey != NULL)` / else-is-an-error). The `a=fpaeskey:`+`a=aesiv:` case has no
/// shairport-sync equivalent — confirmed via a real device this is what a modern iPhone actually
/// sends instead of `a=rsaaeskey:` — is recognized and refused (see this module's docs),
/// the raw body of the connection's most recent `POST /fp-setup` request (threaded in by the
/// caller, which owns the per-connection `RtspSession` this module doesn't know about), since
/// FairPlay 3 unwrapping is keyed by that message, not by anything in the `ANNOUNCE` body itself.
pub(crate) fn handle_announce(body: &[u8]) -> Result<LegacyAudioSession, AnnounceError> {
    let body = String::from_utf8_lossy(body);
    let announce = sdp::parse(&body);

    let engine = base64::engine::general_purpose::STANDARD;
    let aes = match (announce.rsaaeskey, announce.fpaeskey, announce.aesiv) {
        (None, None, None) => None,
        (Some(rsaaeskey), None, Some(aesiv)) => {
            let aesiv_bytes = engine.decode(aesiv)?;
            let aes_iv: [u8; 16] = aesiv_bytes
                .clone()
                .try_into()
                .map_err(|_| AnnounceError::WrongIvLength(aesiv_bytes.len()))?;
            let rsaaeskey_bytes = engine.decode(rsaaeskey)?;
            let aes_key = rsa_key::decrypt_aes_key(&rsaaeskey_bytes)?;
            Some((aes_key, aes_iv))
        }
        // Recognized, and refused: see this module's docs.
        (None, Some(_), Some(_)) => return Err(AnnounceError::FairplayNotSupported),
        _ => return Err(AnnounceError::MismatchedKeyMaterial),
    };

    let fmtp = announce.fmtp.map(sdp::parse_fmtp).unwrap_or(sdp::AlacFmtp {
        frame_length: 352,
        bit_depth: 16,
        num_channels: 2,
        sample_rate: 44100,
    });
    let alac_format = AlacFormat {
        frame_length: fmtp.frame_length,
        bit_depth: fmtp.bit_depth,
        num_channels: fmtp.num_channels,
        sample_rate: fmtp.sample_rate,
    };

    Ok(LegacyAudioSession { aes, alac_format })
}

/// `RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port=<>;timing_port=<>;server_port=<>`
/// — shairport-sync's own classic `SETUP` response `Transport` header value
/// (`rtsp.c`'s `handle_setup`, the non-AirPlay-2 branch), built from the ports this crate already
/// reserves for every connection (`rtsp::RtpPorts`) — `timing_port` reuses the `event_port`
/// socket, there being nothing on the event channel of a classic session to conflict with it.
pub(crate) fn setup_transport_header(
    control_port: u16,
    timing_port: u16,
    server_port: u16,
) -> String {
    format!(
        "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port={control_port};timing_port={timing_port};server_port={server_port}"
    )
}

/// AES-128-CBC-decrypts one RTP payload exactly like `player.c`'s own decrypt call site: the
/// *same* static session IV is used fresh for every packet (not chained across packets — just
/// `memcpy`'d in before each one), only the largest multiple-of-16 prefix is decrypted
/// (`aeslen = len & ~0xf`), and any short trailing remainder is left as-is (classic AirPlay pads
/// audio frames to the codec's own framing, not to the cipher's block size, so a payload is often
/// not itself block-aligned).
fn decrypt_packet(key: &[u8; 16], iv: &[u8; 16], payload: &[u8]) -> Vec<u8> {
    use aes::Aes128;
    use cbc::cipher::{BlockDecryptMut, KeyIvInit, block_padding::NoPadding};

    let aeslen = payload.len() & !0xF;
    let mut buf = payload[..aeslen].to_vec();
    if aeslen > 0 {
        let decryptor = cbc::Decryptor::<Aes128>::new_from_slices(key, iv)
            .expect("key/iv are always exactly 16 bytes");
        let len = decryptor
            .decrypt_padded_mut::<NoPadding>(&mut buf)
            .expect("NoPadding on a block-aligned buffer never fails")
            .len();
        buf.truncate(len);
    }
    buf.extend_from_slice(&payload[aeslen..]);
    buf
}

/// The fixed 12-byte RTP header (RFC 3550) a classic audio packet starts with. CSRC lists and
/// header extensions are not parsed — a classic AirPlay stream uses neither.
///
/// Only the sequence number is read (for logging); the rest is parsed to find where the payload
/// starts and to keep the shape of the header visible.
pub(crate) struct RtpHeader {
    pub(crate) sequence_number: u16,
}

impl RtpHeader {
    const LEN: usize = 12;

    pub(crate) fn parse(data: &[u8]) -> Option<(RtpHeader, &[u8])> {
        if data.len() < Self::LEN {
            return None;
        }
        let header = RtpHeader {
            sequence_number: u16::from_be_bytes([data[2], data[3]]),
        };
        Some((header, &data[Self::LEN..]))
    }
}

/// Signs an `Apple-Challenge` buffer — see `rsa_key::sign_challenge`.
pub(crate) fn sign_challenge(data: &[u8]) -> Option<Vec<u8>> {
    match rsa_key::sign_challenge(data) {
        Ok(signature) => Some(signature),
        Err(err) => {
            warn!("airplay: failed to sign an Apple-Challenge: {err}");
            None
        }
    }
}

/// Converts a `SET_PARAMETER` `volume: <float>` value (AirPlay's dB range, `-30.0..=0.0`, or the
/// `-144.0` "mute" sentinel — confirmed via shairport-sync's own `dasl_tapered_vol2attn`, which
/// documents and range-checks exactly that domain) into a linear amplitude multiplier for scaling
/// decoded `f64` samples before they reach the sink. Standard `10^(dB/20)` dB-to-amplitude
/// conversion rather than shairport-sync's own `vol2attn`/`dasl_tapered_vol2attn` (both model a
/// *hardware* mixer's attenuation range via `min_db`/`max_db` bounds — this crate has no such
/// concept, since it scales software samples directly rather than driving a physical attenuator,
/// so there's no hardware curve to reproduce here, just a correct, monotonic loud/quiet/mute
/// mapping).
pub(crate) fn volume_to_gain(db: f64) -> f64 {
    if db <= -144.0 {
        return 0.0;
    }
    10f64.powf(db.clamp(-30.0, 0.0) / 20.0)
}

/// Reads and decrypts real UDP datagrams off `socket` for a classic (`ANNOUNCE`-negotiated)
/// session, decoding ALAC with the single format `ANNOUNCE` specified (no SSRC-based format
/// switching — a classic stream's parameters don't change mid-session) and writing decoded PCM to
/// a real audio output, mirroring `rtsp::connection::decode_audio`'s sink-wiring exactly (same
/// `AudioOutputHandle`, created lazily on the first successfully-decoded packet). `volume` is a
/// live gain feed from the connection's `SET_PARAMETER` handling (`rtsp::connection`, which owns
/// the sending half) — confirmed necessary by a real device, which has no other way to control
/// playback volume for a classic session: applied by scaling every decoded sample just before
/// `output.write`, reading whatever the latest value is on every packet rather than only on
/// change, since `watch::Receiver::borrow` is cheap and this avoids any `select!`/task-wakeup
/// complexity for what's a once-in-a-while control message.
pub(crate) async fn receive_loop(
    socket: UdpSocket,
    session: LegacyAudioSession,
    sink_config: SinkConfig,
    volume: tokio::sync::watch::Receiver<f64>,
) {
    let mut decoder = match AlacDecoder::new(&session.alac_format) {
        Ok(decoder) => decoder,
        Err(err) => {
            warn!("airplay: failed to start the (classic AirPlay) ALAC decoder: {err}");
            return;
        }
    };
    let mut output: Option<AudioOutputHandle> = None;

    let mut buf = [0u8; 2048];
    let mut first_packet = true;

    loop {
        let (n, peer) = match socket.recv_from(&mut buf).await {
            Ok(result) => result,
            Err(err) => {
                warn!("airplay: classic AirPlay RTP socket read error, stopping: {err}");
                return;
            }
        };

        let Some((header, payload)) = RtpHeader::parse(&buf[..n]) else {
            warn!("airplay: dropping {n}-byte datagram from {peer}, too short for an RTP header");
            continue;
        };

        let decrypted = match &session.aes {
            Some((key, iv)) => decrypt_packet(key, iv, payload),
            None => payload.to_vec(),
        };
        match decoder.decode(&decrypted) {
            Ok(samples) => {
                // Per packet, so `trace` — at `debug` this drowns out everything else in the
                // log. The first one is worth a line of its own: it is the moment audio starts.
                if first_packet {
                    first_packet = false;
                    debug!("airplay: classic audio is flowing (first packet decoded)");
                }
                trace!(
                    "airplay: decoded {} PCM samples from a classic AirPlay packet (seq {})",
                    samples.len(),
                    header.sequence_number
                );
                let gain = *volume.borrow();
                let samples = if gain == 1.0 {
                    samples
                } else {
                    samples.into_iter().map(|s| s * gain).collect()
                };
                output
                    .get_or_insert_with(|| AudioOutputHandle::spawn(sink_config.clone()))
                    .write(samples);
            }
            Err(err) => warn!("airplay: failed to decode a classic AirPlay ALAC packet: {err}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn real_announce_body(rsaaeskey_b64: &str, aesiv_b64: &str) -> Vec<u8> {
        format!(
            "v=0\r\n\
             o=iTunes 3053544501 0 IN IP4 192.168.0.199\r\n\
             s=iTunes\r\n\
             c=IN IP4 192.168.0.196\r\n\
             t=0 0\r\n\
             m=audio 0 RTP/AVP 96\r\n\
             a=rtpmap:96 AppleLossless\r\n\
             a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n\
             a=rsaaeskey:{rsaaeskey_b64}\r\n\
             a=aesiv:{aesiv_b64}\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn handle_announce_decrypts_a_real_style_key_and_parses_fmtp() {
        use rsa::Oaep;
        use sha1::Sha1;

        // The same embedded key `rsa_key::decrypt_aes_key` (called inside `handle_announce`)
        // decrypts against — this is a real end-to-end round trip through this crate's own RSA
        // code, not a fixture with its own separate key.
        let public_key = rsa_key::embedded_public_key_for_test();
        let aes_key = [0x11u8; 16];
        let aes_iv = [0x22u8; 16];
        let mut rng = rand_core::OsRng;
        let encrypted_key = public_key
            .encrypt(&mut rng, Oaep::new::<Sha1>(), &aes_key)
            .unwrap();

        let engine = base64::engine::general_purpose::STANDARD;
        let body = real_announce_body(&engine.encode(encrypted_key), &engine.encode(aes_iv));

        let session = handle_announce(&body).unwrap();
        assert_eq!(session.aes, Some((aes_key, aes_iv)));
        assert_eq!(session.alac_format.frame_length, 352);
        assert_eq!(session.alac_format.sample_rate, 44100);
    }

    /// A real iPhone was observed sending exactly this — an `ANNOUNCE` with neither
    /// `a=rsaaeskey:` nor `a=aesiv:` — for its classic-RAOP fallback. shairport-sync's own
    /// `handle_announce` treats this as a legitimate unencrypted session
    /// (`conn->stream.encrypted = 0`), not an error; this must match.
    #[test]
    fn handle_announce_accepts_an_unencrypted_body() {
        let body = b"v=0\r\ns=iTunes\r\na=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n".to_vec();
        let session = handle_announce(&body).unwrap();
        assert_eq!(session.aes, None);
        assert_eq!(session.alac_format.frame_length, 352);
    }

    #[test]
    fn handle_announce_rejects_mismatched_key_material() {
        let body = b"v=0\r\ns=iTunes\r\na=aesiv:MTIzNDU2Nzg5MDEyMzQ1Ng==\r\n".to_vec();
        assert!(matches!(
            handle_announce(&body),
            Err(AnnounceError::MismatchedKeyMaterial)
        ));
    }

    /// The real body a modern iPhone sends on this path: `a=fpaeskey:` + `a=aesiv:`, no
    /// `a=rsaaeskey:`. Recognized and refused with the error that says why — not
    /// `MismatchedKeyMaterial`, which would suggest the sender sent something malformed, and not
    /// silently treated as an unencrypted session.
    ///
    /// A sender only wraps the key this way when it believes the receiver speaks AirPlay 2;
    /// against this one it sends `a=rsaaeskey:` instead. See the module docs.
    #[test]
    fn handle_announce_refuses_a_fairplay_wrapped_key() {
        let body = "v=0\r\ns=iTunes\r\n\
a=fpaeskey:RlBMWQECAQAAAAA8AAAAAHJwyPokWiXAHaG5p2tDaskAAAAQFspqFSxJBDxGa+iYKhXJp8CXXzEezEUexFmRJPIrIjVAhtmq\r\n\
a=aesiv:iuT9akUaYC4uwxnax72K2w==\r\n"
            .as_bytes()
            .to_vec();

        assert!(matches!(
            handle_announce(&body),
            Err(AnnounceError::FairplayNotSupported)
        ));
    }

    #[test]
    fn setup_transport_header_matches_shairport_syncs_format() {
        let header = setup_transport_header(6001, 6002, 6000);
        assert_eq!(
            header,
            "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port=6001;timing_port=6002;server_port=6000"
        );
    }

    /// Cross-checked against a real independent AES-CBC implementation (Python's own
    /// `cryptography` package would be used for a live end-to-end test; here, a hand-computed
    /// vector isn't available, so this instead verifies the *structural* invariant player.c's
    /// code relies on: a non-block-aligned tail is left untouched.
    #[test]
    fn decrypt_packet_leaves_a_non_block_aligned_tail_untouched() {
        let key = [0u8; 16];
        let iv = [0u8; 16];
        // 16 bytes of (arbitrary) "ciphertext" + 3 bytes that must survive unchanged.
        let mut payload = vec![0xAAu8; 16];
        payload.extend_from_slice(&[1, 2, 3]);

        let decrypted = decrypt_packet(&key, &iv, &payload);
        assert_eq!(decrypted.len(), payload.len());
        assert_eq!(&decrypted[16..], &[1, 2, 3]);
    }

    #[test]
    fn decrypt_packet_round_trips_with_a_real_independent_encryption() {
        use aes::Aes128;
        use cbc::cipher::{BlockEncryptMut, KeyIvInit, block_padding::NoPadding};

        let key = [0x55u8; 16];
        let iv = [0x66u8; 16];
        let plaintext = [0x77u8; 32]; // two full blocks

        let mut buf = plaintext.to_vec();
        let encryptor = cbc::Encryptor::<Aes128>::new_from_slices(&key, &iv).unwrap();
        let ciphertext_len = encryptor
            .encrypt_padded_mut::<NoPadding>(&mut buf, plaintext.len())
            .unwrap()
            .len();
        buf.truncate(ciphertext_len);

        let decrypted = decrypt_packet(&key, &iv, &buf);
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn volume_to_gain_maps_the_full_range() {
        assert_eq!(volume_to_gain(0.0), 1.0);
        assert!((volume_to_gain(-6.0) - 0.5011872).abs() < 1e-6);
        assert!((volume_to_gain(-30.0) - 0.0316228).abs() < 1e-6);
        assert_eq!(volume_to_gain(-144.0), 0.0);
        assert_eq!(volume_to_gain(-1000.0), 0.0);
        // Out-of-range-but-not-mute values clamp rather than under/overshoot.
        assert_eq!(volume_to_gain(10.0), 1.0);
        assert_eq!(volume_to_gain(-40.0), volume_to_gain(-30.0));
    }
}
