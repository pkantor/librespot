//! Minimal SDP (RFC 4566) attribute extraction for classic `ANNOUNCE` bodies — just enough to
//! read the handful of `a=` lines this crate's classic-RAOP path needs: `a=rsaaeskey:`,
//! `a=fpaeskey:`, `a=aesiv:`, `a=fmtp:`. Not a general SDP parser — shairport-sync's own isn't one
//! either (`rtsp.c`'s `handle_announce`, a byte-for-byte `strncmp` prefix scan over lines, matched
//! here the same way rather than pulling in a real SDP crate for four attribute lines).
//! `a=fpaeskey:` has no shairport-sync equivalent to match: it is read only to tell a
//! FairPlay-wrapped key apart from a missing one, so the refusal can say which (see
//! `legacy::AnnounceError::FairplayNotSupported`). The field name is taken from real `ANNOUNCE`
//! bodies captured from an iPhone that believed it was talking to an AirPlay 2 receiver.

/// Finds the value after `prefix` on whichever line starts with it, matching shairport-sync's own
/// `strncmp(cp, "a=fmtp:", strlen("a=fmtp:"))`-style scan. Lines are split on `\r\n` or `\n`,
/// matching real SDP line endings the same way shairport-sync's own `nextline` does (splitting
/// generically on `\r`/`\n`).
fn find_attribute<'a>(body: &'a str, prefix: &str) -> Option<&'a str> {
    body.lines()
        .find_map(|line| line.strip_prefix(prefix))
        .map(str::trim)
}

pub(crate) struct Announce<'a> {
    /// `a=fmtp:` — space-separated integers: payload type, then the 11
    /// `ALACSpecificConfig` fields in order (frame_length, compatible_version, bit_depth, pb, mb,
    /// kb, num_channels, max_run, max_frame_bytes, avg_bit_rate, sample_rate). `None` if the
    /// stream isn't ALAC (this crate doesn't decode anything else regardless).
    pub(crate) fmtp: Option<&'a str>,
    /// `a=rsaaeskey:` — base64, RSA-OAEP-encrypted 16-byte AES key. `None` together with `aesiv`
    /// means an unencrypted session (shairport-sync supports this; a real AirPlay sender never
    /// actually sends one, so this crate doesn't bother handling that combination specially
    /// beyond not populating a decryptor).
    pub(crate) rsaaeskey: Option<&'a str>,
    /// `a=fpaeskey:` — base64, a 72-byte FairPlay-wrapped AES key (starts `FPLY...` once
    /// decoded). What a sender sends instead of `a=rsaaeskey:` when it believes the receiver
    /// speaks AirPlay 2; recognized only so it can be refused by name. See `legacy`'s module docs.
    pub(crate) fpaeskey: Option<&'a str>,
    /// `a=aesiv:` — base64, the 16-byte AES-CBC IV reused unchanged for every packet in the
    /// session (confirmed via `player.c`'s own decrypt call site: `memcpy(iv, conn->stream.aesiv,
    /// ...)` runs fresh before *every* packet, not chained across packets).
    pub(crate) aesiv: Option<&'a str>,
}

pub(crate) fn parse(body: &str) -> Announce<'_> {
    Announce {
        fmtp: find_attribute(body, "a=fmtp:"),
        rsaaeskey: find_attribute(body, "a=rsaaeskey:"),
        fpaeskey: find_attribute(body, "a=fpaeskey:"),
        aesiv: find_attribute(body, "a=aesiv:"),
    }
}

/// Parses `fmtp`'s space/tab-separated integers into the 4 `ALACSpecificConfig` fields this
/// crate's own `codec::AlacFormat` needs — mirrors shairport-sync's own
/// `strsep(&pfmtp, " \t")` loop into `conn->stream.fmtp[]` at the same field indices, but doesn't
/// keep the other 7 fields shairport-sync's own array does (`pb`/`mb`/`kb`/`max_run`/
/// `max_frame_bytes`/`avg_bit_rate`, plus the leading payload-type token): this crate's
/// `codec::magic_cookie` already hardcodes `pb`/`mb`/`kb`/`max_run` as Apple's standard encoder
/// defaults (confirmed to match a real encoder's output independently, see `codec`'s own tests),
/// and `max_frame_bytes`/`avg_bit_rate` are unused unknown/lossless markers `codec::magic_cookie`
/// already hardcodes to `0` too — so capturing them here would just be dead weight, not
/// completeness. Missing/unparseable fields silently default to `0`, matching `atoi`'s own
/// behavior on a bad token in C.
pub(crate) struct AlacFmtp {
    pub(crate) frame_length: u32,
    pub(crate) bit_depth: u8,
    pub(crate) num_channels: u8,
    pub(crate) sample_rate: u32,
}

pub(crate) fn parse_fmtp(fmtp: &str) -> AlacFmtp {
    let field = |i: usize| -> u64 {
        fmtp.split_whitespace()
            .nth(i)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    };
    // field(0) is the payload type (always "96" in practice) — not part of ALACSpecificConfig,
    // matching shairport-sync's own array where fmtp[0] holds it separately from fmtp[1..11].
    AlacFmtp {
        frame_length: field(1) as u32,
        bit_depth: field(3) as u8,
        num_channels: field(7) as u8,
        sample_rate: field(11) as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL_ANNOUNCE_BODY: &str = "v=0\r\n\
o=iTunes 3053544501 0 IN IP4 192.168.0.199\r\n\
s=iTunes\r\n\
c=IN IP4 192.168.0.196\r\n\
t=0 0\r\n\
m=audio 0 RTP/AVP 96\r\n\
a=rtpmap:96 AppleLossless\r\n\
a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n\
a=rsaaeskey:AbCdEf==\r\n\
a=aesiv:MTIzNDU2Nzg5MDEyMzQ1Ng==\r\n";

    #[test]
    fn parses_a_realistic_announce_body() {
        let announce = parse(REAL_ANNOUNCE_BODY);
        assert_eq!(announce.fmtp, Some("96 352 0 16 40 10 14 2 255 0 0 44100"));
        assert_eq!(announce.rsaaeskey, Some("AbCdEf=="));
        assert_eq!(announce.fpaeskey, None);
        assert_eq!(announce.aesiv, Some("MTIzNDU2Nzg5MDEyMzQ1Ng=="));
    }

    /// The real shape a modern iPhone actually sends (captured this session): `a=fpaeskey:`
    /// instead of `a=rsaaeskey:`, no `a=min-latency:`/`a=max-latency:` handling needed here since
    /// this module only reads the four attributes it declares.
    #[test]
    fn parses_a_real_fpaeskey_announce_body() {
        let body = "v=0\r\n\
o=AirTunes 4441089655859134464 0 IN IP4 192.168.0.199\r\n\
s=AirTunes\r\n\
i=iPhone\r\n\
c=IN IP4 192.168.0.199\r\n\
t=0 0\r\n\
m=audio 0 RTP/AVP 96\r\n\
a=rtpmap:96 AppleLossless\r\n\
a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n\
a=fpaeskey:RlBMWQECAQAAAAA8AAAAAHJwyPokWiXAHaG5p2tDaskAAAAQFspqFSxJBDxGa+iYKhXJp8CXXzEezEUexFmRJPIrIjVAhtmq\r\n\
a=aesiv:iuT9akUaYC4uwxnax72K2w==\r\n\
a=min-latency:11025\r\n\
a=max-latency:88200\r\n";
        let announce = parse(body);
        assert_eq!(announce.rsaaeskey, None);
        assert_eq!(
            announce.fpaeskey,
            Some(
                "RlBMWQECAQAAAAA8AAAAAHJwyPokWiXAHaG5p2tDaskAAAAQFspqFSxJBDxGa+iYKhXJp8CXXzEezEUexFmRJPIrIjVAhtmq"
            )
        );
        assert_eq!(announce.aesiv, Some("iuT9akUaYC4uwxnax72K2w=="));
    }

    #[test]
    fn missing_attributes_are_none() {
        let announce = parse("v=0\r\ns=iTunes\r\n");
        assert!(announce.fmtp.is_none());
        assert!(announce.rsaaeskey.is_none());
        assert!(announce.fpaeskey.is_none());
        assert!(announce.aesiv.is_none());
    }

    #[test]
    fn parse_fmtp_matches_the_real_alac_defaults() {
        let fmtp = parse_fmtp("96 352 0 16 40 10 14 2 255 0 0 44100");
        assert_eq!(fmtp.frame_length, 352);
        assert_eq!(fmtp.bit_depth, 16);
        assert_eq!(fmtp.num_channels, 2);
        assert_eq!(fmtp.sample_rate, 44100);
    }

    #[test]
    fn parse_fmtp_defaults_missing_fields_to_zero() {
        let fmtp = parse_fmtp("96 352");
        assert_eq!(fmtp.frame_length, 352);
        assert_eq!(fmtp.sample_rate, 0);
    }
}
