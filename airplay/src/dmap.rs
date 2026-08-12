//! DMAP — the tag/length/value encoding Apple uses for now-playing metadata, in both a sender's
//! `SET_PARAMETER` pushes and a DACP server's replies.
//!
//! One implementation for both because it is one format: a 4-byte big-endian tag, a 4-byte
//! big-endian length, then the value. shairport-sync's `dacp_tlv_crawl` (`dacp.c`) and
//! `handle_set_parameter_metadata` (`rtsp.c`) walk it identically.
//!
//! Not behind the `remote-control` feature, unlike `dacp`: a push needs no DACP server, mDNS
//! query or polling — only the RTSP connection that already exists.

use log::debug;

/// One DMAP item: tag, value, and what follows it. `None` for a truncated or malformed body, so
/// the walk ends instead of panicking — shairport-sync's walker stops on the same condition
/// (`if (vl > cl - off) break;`).
pub(crate) fn read_item(bytes: &[u8]) -> Option<([u8; 4], &[u8], &[u8])> {
    if bytes.len() < 8 {
        return None;
    }
    let tag: [u8; 4] = bytes[..4].try_into().ok()?;
    let len = u32::from_be_bytes(bytes[4..8].try_into().ok()?) as usize;
    let value = bytes.get(8..8 + len)?;
    let rest = &bytes[8 + len..];
    Some((tag, value, rest))
}

/// Reads a 4-byte big-endian value, or `None` if the item isn't exactly four bytes long.
pub(crate) fn read_u32(item: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(item.try_into().ok()?))
}

/// What a sender says about the track it is streaming. `None` where the sender omitted the tag,
/// which is routine and not an error — this layer must not turn that into an empty string.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct TrackMetadata {
    /// `minm`.
    pub title: Option<String>,
    /// `asar`.
    pub artist: Option<String>,
    /// `asal`.
    pub album: Option<String>,
    /// `assl`.
    pub album_artist: Option<String>,
    /// `astm`, milliseconds.
    pub duration_ms: Option<u32>,
}

impl TrackMetadata {
    /// Did the sender actually say anything about a track? A `SET_PARAMETER` carrying only tags
    /// this crate doesn't read is not a track change.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Parses a `SET_PARAMETER` body of type `application/x-dmap-tagged`.
///
/// Tag names from shairport-sync's `metadata_hub_process_metadata` (`metadata/hub.c`): `minm`
/// track, `asar` artist, `asal` album, `assl` album artist, `astm` length in ms. Tags this crate
/// has nowhere to put (`asgn`, `ascp`, `mper`, …) fall through with the unknown ones.
///
/// The **leading 8 bytes are skipped, not parsed**: the body is one outer container whose header
/// precedes the flat list of items, and shairport-sync skips it the same way without checking its
/// tag (`unsigned int off = 8;`).
pub(crate) fn parse_metadata(body: &[u8]) -> TrackMetadata {
    let mut metadata = TrackMetadata::default();
    if body.len() < 8 {
        return metadata;
    }

    let mut rest = &body[8..];
    while let Some((tag, item, next)) = read_item(rest) {
        let text = || String::from_utf8_lossy(item).into_owned();
        match &tag {
            b"minm" => metadata.title = Some(text()),
            b"asar" => metadata.artist = Some(text()),
            b"asal" => metadata.album = Some(text()),
            b"assl" => metadata.album_artist = Some(text()),
            b"astm" => metadata.duration_ms = read_u32(item),
            _ => debug!(
                "airplay: unhandled DMAP tag {:?}",
                String::from_utf8_lossy(&tag)
            ),
        }
        rest = next;
    }

    metadata
}

/// How far into the track the sender is, from a `text/parameters` `progress:` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Progress {
    pub position_ms: u32,
    pub duration_ms: u32,
}

/// The clock `progress:` timestamps are counted in: 44100 Hz, the rate classic AirPlay always
/// streams at, and what shairport-sync's comment on the same line says
/// ("rtpstampstart/rtpstampnow/rtpstampend 44100").
const RTP_CLOCK_HZ: u64 = 44_100;

/// Parses `progress: <start>/<now>/<end>` — three RTP timestamps, not times: the frame the track
/// starts at, the one playing now, and the one it ends at. shairport-sync forwards the line
/// verbatim (as `prgr`); this converts it, since the fork's API publishes milliseconds.
///
/// The sender's own unprompted report of where it is, and the reason `md=…,2` is advertised.
/// `None` for a line that isn't three parsable numbers, or whose timestamps run backwards.
pub(crate) fn parse_progress(line: &str) -> Option<Progress> {
    let mut stamps = line.trim().split('/');
    let start: u64 = stamps.next()?.trim().parse().ok()?;
    let now: u64 = stamps.next()?.trim().parse().ok()?;
    let end: u64 = stamps.next()?.trim().parse().ok()?;
    if stamps.next().is_some() || now < start || end < start {
        return None;
    }

    let to_ms = |frames: u64| u32::try_from(frames * 1000 / RTP_CLOCK_HZ).unwrap_or(u32::MAX);
    Some(Progress {
        position_ms: to_ms(now - start),
        duration_ms: to_ms(end - start),
    })
}

/// What an image body actually is, by its first bytes rather than its `Content-Type` —
/// shairport-sync does the same and says why: "the image/type tag isn't reliable, so it's not
/// being sent -- best look at the first few bytes of the image".
pub(crate) fn sniff_image_mime(bytes: &[u8]) -> &'static str {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn item(tag: &[u8; 4], value: &[u8]) -> Vec<u8> {
        let mut out = tag.to_vec();
        out.extend_from_slice(&(value.len() as u32).to_be_bytes());
        out.extend_from_slice(value);
        out
    }

    /// A body shaped the way a real sender sends one: an outer `mlit` container header followed
    /// by a flat list of items.
    fn body(items: &[u8]) -> Vec<u8> {
        item(b"mlit", items)
    }

    #[test]
    fn parses_a_real_shaped_metadata_body() {
        let mut items = Vec::new();
        items.extend(item(b"mper", &1_234_567_890u64.to_be_bytes())); // item id, not read here
        items.extend(item(b"minm", "Sun Is Shining".as_bytes()));
        items.extend(item(b"asar", "Bob Marley".as_bytes()));
        items.extend(item(b"asal", "Kaya".as_bytes()));
        items.extend(item(b"assl", "Bob Marley & The Wailers".as_bytes()));
        items.extend(item(b"asgn", "Reggae".as_bytes())); // genre, nowhere to put it
        items.extend(item(b"astm", &213_000u32.to_be_bytes()));

        let metadata = parse_metadata(&body(&items));

        assert_eq!(metadata.title.as_deref(), Some("Sun Is Shining"));
        assert_eq!(metadata.artist.as_deref(), Some("Bob Marley"));
        assert_eq!(metadata.album.as_deref(), Some("Kaya"));
        assert_eq!(
            metadata.album_artist.as_deref(),
            Some("Bob Marley & The Wailers")
        );
        assert_eq!(metadata.duration_ms, Some(213_000));
    }

    /// Real senders omit tags routinely — a missing album is not an empty album.
    #[test]
    fn absent_tags_stay_absent() {
        let metadata = parse_metadata(&body(&item(b"minm", "Sun Is Shining".as_bytes())));

        assert_eq!(metadata.title.as_deref(), Some("Sun Is Shining"));
        assert_eq!(metadata.album, None);
        assert_eq!(metadata.duration_ms, None);
        assert!(!metadata.is_empty());
    }

    #[test]
    fn a_body_with_nothing_this_crate_reads_is_empty() {
        assert!(parse_metadata(&body(&item(b"asgn", b"Reggae"))).is_empty());
        assert!(parse_metadata(&[]).is_empty());
        assert!(parse_metadata(b"mlit").is_empty());
    }

    /// A truncated body ends the walk at the bad item, keeping what was already read.
    #[test]
    fn a_truncated_body_keeps_what_came_before_it() {
        let mut items = item(b"minm", "Sun Is Shining".as_bytes());
        items.extend_from_slice(b"asar");
        items.extend_from_slice(&1000u32.to_be_bytes()); // claims far more than follows
        items.extend_from_slice(b"Bob");

        let metadata = parse_metadata(&body(&items));

        assert_eq!(metadata.title.as_deref(), Some("Sun Is Shining"));
        assert_eq!(metadata.artist, None);
    }

    /// Three RTP timestamps at 44100 Hz: 44100 frames in is one second in.
    #[test]
    fn parses_a_progress_line() {
        assert_eq!(
            parse_progress("1000/45100/9394000"),
            Some(Progress {
                position_ms: 1_000,
                duration_ms: 212_993,
            })
        );
        // Right at the start of the track.
        assert_eq!(
            parse_progress("1000/1000/45100"),
            Some(Progress {
                position_ms: 0,
                duration_ms: 1_000,
            })
        );
    }

    #[test]
    fn nonsense_progress_lines_report_nothing() {
        assert_eq!(parse_progress(""), None);
        assert_eq!(parse_progress("1000/45100"), None);
        assert_eq!(parse_progress("1000/45100/9394000/7"), None);
        assert_eq!(parse_progress("a/b/c"), None);
        // Timestamps running backwards would underflow the subtraction.
        assert_eq!(parse_progress("45100/1000/9394000"), None);
        assert_eq!(parse_progress("45100/45100/1000"), None);
    }

    #[test]
    fn image_types_follow_the_bytes() {
        assert_eq!(sniff_image_mime(&[0xff, 0xd8, 0xff, 0xe0]), "image/jpeg");
        assert_eq!(sniff_image_mime(b"\x89PNG\r\n"), "image/png");
        assert_eq!(sniff_image_mime(b"GIF89a"), "image/gif");
        assert_eq!(sniff_image_mime(b"RIFF____WEBPVP8 "), "image/webp");
        assert_eq!(sniff_image_mime(b"nope"), "application/octet-stream");
    }
}
