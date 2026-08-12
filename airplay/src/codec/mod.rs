//! Decodes ALAC RTP payloads into normalized `f64` PCM samples — the same shape
//! `librespot_playback::decoder::AudioPacket::Samples(Vec<f64>)` already uses, so the
//! integration layer in `src/main.rs` can hand decoded frames to
//! `librespot_playback::convert::Converter` and a `Sink` without a second conversion path. This
//! crate does not depend on `librespot-playback` directly (to avoid coupling the protocol
//! implementation to the playback pipeline's types), but reuses the exact conversion mechanism
//! `playback/src/decoder/symphonia_decoder.rs` already does: a `symphonia::core::audio::
//! SampleBuffer<f64>`'s `copy_interleaved_ref`/`samples()`, not a hand-rolled one.
//!
//! **Where the ALAC parameters come from.** A classic session's `ANNOUNCE` carries them in its
//! SDP `a=fmtp:` line, which `legacy::sdp` parses — frame length, bit depth, channels and sample
//! rate all arrive negotiated, with no lookup table involved.
//!
//! The remaining fixed fields the ALAC magic cookie needs beyond frame length/bit depth/channels
//! (`pb`=40, `mb`=10, `kb`=14, `max_run`=255) are Apple's standard encoder defaults, taken from
//! shairport-sync's *classic RAOP* `fmtp`-parsing fallback array in the same file (`rtsp.c`,
//! around its `pfmtp` handling) — a sender never states them, and this reference confirms they
//! are the values every real Apple sender has been observed to use, not a guess made up for this
//! port.

use symphonia::core::{
    audio::SampleBuffer,
    codecs::{CODEC_TYPE_ALAC, CodecParameters, Decoder, DecoderOptions},
    formats::Packet,
};

/// The ALAC parameters of one session, as its `ANNOUNCE` negotiated them — see the module docs
/// for which of the magic cookie's fields are sender-stated and which are fixed.
pub(crate) struct AlacFormat {
    pub(crate) frame_length: u32,
    pub(crate) bit_depth: u8,
    pub(crate) num_channels: u8,
    pub(crate) sample_rate: u32,
}

/// Apple's standard ALAC encoder parameters (Rice coding: `pb`/`mb`/`kb`), constant across
/// essentially every real-world Apple ALAC stream — not sender-negotiated, taken from
/// shairport-sync's classic-RAOP `fmtp` fallback values (see module docs).
const ALAC_PB: u8 = 40;
const ALAC_MB: u8 = 10;
const ALAC_KB: u8 = 14;
const ALAC_MAX_RUN: u16 = 255;

/// Builds the 24-byte ALAC magic cookie (`ALACSpecificConfig`, a publicly documented Apple
/// structure — see e.g. Apple's own reference ALAC decoder source) `symphonia_codec_alac`
/// expects as `CodecParameters::extra_data`, from an [`AlacFormat`].
fn magic_cookie(format: &AlacFormat) -> Box<[u8]> {
    let mut cookie = Vec::with_capacity(24);
    cookie.extend_from_slice(&format.frame_length.to_be_bytes());
    cookie.push(0); // compatible_version
    cookie.push(format.bit_depth);
    cookie.push(ALAC_PB);
    cookie.push(ALAC_MB);
    cookie.push(ALAC_KB);
    cookie.push(format.num_channels);
    cookie.extend_from_slice(&ALAC_MAX_RUN.to_be_bytes());
    cookie.extend_from_slice(&0u32.to_be_bytes()); // max_frame_bytes: 0 = unbounded/unknown
    cookie.extend_from_slice(&0u32.to_be_bytes()); // avg_bit_rate: 0 = lossless/unknown
    cookie.extend_from_slice(&format.sample_rate.to_be_bytes());
    cookie.into_boxed_slice()
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CodecError {
    #[error("failed to initialize the ALAC decoder: {0}")]
    Init(String),
    #[error("failed to decode an ALAC packet: {0}")]
    Decode(String),
}

/// Decodes successive ALAC RTP payloads (already stripped of the RTP header and decrypted — see
/// `rtp::receive_loop`) into interleaved, normalized `f64` PCM samples.
pub(crate) struct AlacDecoder {
    decoder: Box<dyn Decoder>,
    sample_buffer: Option<SampleBuffer<f64>>,
}

impl AlacDecoder {
    pub(crate) fn new(format: &AlacFormat) -> Result<Self, CodecError> {
        let mut params = CodecParameters::new();
        params
            .for_codec(CODEC_TYPE_ALAC)
            .with_extra_data(magic_cookie(format));

        let decoder =
            symphonia::default::codecs::AlacDecoder::try_new(&params, &DecoderOptions::default())
                .map_err(|err| CodecError::Init(err.to_string()))?;

        Ok(Self {
            decoder: Box::new(decoder),
            sample_buffer: None,
        })
    }

    /// `payload` is one RTP packet's decrypted audio frame (no RTP header, no trailer).
    pub(crate) fn decode(&mut self, payload: &[u8]) -> Result<Vec<f64>, CodecError> {
        let packet = Packet::new_from_slice(0, 0, 0, payload);
        let decoded = self
            .decoder
            .decode(&packet)
            .map_err(|err| CodecError::Decode(err.to_string()))?;

        let sample_buffer = match self.sample_buffer.as_mut() {
            Some(buffer) => buffer,
            None => {
                let spec = *decoded.spec();
                let duration = decoded.capacity() as u64;
                self.sample_buffer.insert(SampleBuffer::new(duration, spec))
            }
        };
        sample_buffer.copy_interleaved_ref(decoded);
        Ok(sample_buffer.samples().to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a real classic `ANNOUNCE` describes: `a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100`.
    const CLASSIC_FORMAT: AlacFormat = AlacFormat {
        frame_length: 352,
        bit_depth: 16,
        num_channels: 2,
        sample_rate: 44100,
    };

    /// Round-trips a real ALAC encode through our decoder using `symphonia`'s own ALAC
    /// *encoder*... except symphonia doesn't ship one. Instead this constructs a decoder for
    /// the standard format and confirms it initializes and reports the expected spec — the
    /// actual bitstream decode correctness is Symphonia's own tested code, not this crate's; what
    /// this crate owns and must verify is the magic-cookie construction and plumbing.
    #[test]
    fn decoder_initializes_with_the_standard_format() {
        let decoder = AlacDecoder::new(&CLASSIC_FORMAT);
        assert!(decoder.is_ok(), "{:?}", decoder.err());
    }

    #[test]
    fn magic_cookie_is_24_bytes_and_matches_the_format() {
        let cookie = magic_cookie(&CLASSIC_FORMAT);
        assert_eq!(cookie.len(), 24);
        assert_eq!(u32::from_be_bytes(cookie[0..4].try_into().unwrap()), 352); // frame_length
        assert_eq!(cookie[5], 16); // bit_depth
        assert_eq!(cookie[6], ALAC_PB);
        assert_eq!(cookie[7], ALAC_MB);
        assert_eq!(cookie[8], ALAC_KB);
        assert_eq!(cookie[9], 2); // num_channels
        assert_eq!(
            u32::from_be_bytes(cookie[20..24].try_into().unwrap()),
            44100
        ); // sample_rate
    }

    #[test]
    fn handles_a_malformed_packet_without_panicking() {
        let mut decoder = AlacDecoder::new(&CLASSIC_FORMAT).unwrap();
        // Not a valid ALAC bitstream. Symphonia doesn't guarantee an `Err` for arbitrary garbage
        // bytes (this particular input happens to hit ALAC's "end of frame" element tag
        // immediately and decodes as silence) — the only invariant this crate can actually rely
        // on, and the one that matters given `rtp::receive_loop` feeds this decoder untrusted
        // network data, is that decoding never panics.
        let _ = decoder.decode(&[0xFF; 8]);
    }

    fn decode_hex(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    /// Decodes one ALAC frame from a real, independent encoder (`ffmpeg -c:a alac`, not
    /// Symphonia and not anything in this codebase), extracted from an actual `.m4a`'s `mdat`
    /// by walking its MP4 boxes (`stsd` → `alac` sample entry → nested `alac` cookie box,
    /// `stsz`/`stco` for the frame's offset/size). This is the strongest available check that
    /// this crate's magic-cookie construction and Symphonia wiring produce *correct* audio, not
    /// just a well-formed `Ok(...)` — and a nice bonus: ffmpeg's own cookie for this encode
    /// independently confirms `pb`/`mb`/`kb`/`bit_depth`/`num_channels`/`sample_rate` (40/10/14/
    /// 16/2/44100) match this module's hardcoded values exactly, cross-checking the
    /// shairport-sync-derived constants above against a real, separate ALAC implementation.
    #[test]
    fn decodes_a_real_ffmpeg_encoded_alac_frame() {
        // ffmpeg's file-oriented ALAC encoder defaults to 4096-sample frames (unlike the
        // 352-sample frames real AirPlay senders use for low latency) — irrelevant here since
        // this test is about bitstream decode correctness, not frame-size negotiation, and
        // `format` below matches what ffmpeg actually produced.
        let format = AlacFormat {
            frame_length: 4096,
            bit_depth: 16,
            num_channels: 2,
            sample_rate: 44100,
        };
        let mut decoder = AlacDecoder::new(&format).unwrap();

        let frame_hex = "20000000020f0c0168004dffabff83ffc000600f0801000000000000000ff805a7fe0168ff805a3ea3e8be700000808288001030808020020082062188653a1052804501823452291191acc180315213756c4ca766f7845646909c89b88718462018931c43121021db69034aae9468ae34c234da8e38e5401cbd44ee0c43720eb43855d5b59103687748699540927138e5412a49842b4c23556c438c80c32b57719131015a1cad55cc8271b8e03b69132379124e081de5ac12a8edb55aad55d28152ad92854544243b2a0a9a8055b20c7137004d0e206caa428d1284d00d34d0c8c2055b004d27043d4969a25380c04d8992adb80d558eda6a34cde55f20429276d0d34e568698106082a03519454901a49cad55d48c1156399756da071a2300abd5c4871db841a71a2b4d1565455a4d34938e3ca01a8271a89b7215a69821ac90154690e034d5cb072920600e34c4d55d5b01822a39401525a92935189a596dc62696d0e4a42690a9385213d44a911cab70ad39563134524980a909a0464b88403d09d6989ca912c8ee90218e22920624206934954415a0ab2b437132b400e38e1453428954693d3455b4a924d840626254aa005288720d00ed3189a6989a62ab6d394ec701b310ed3881caba8200620609898aa348704da4934840e196644d0d65b95aa824d8389838cded27c121ab6d340d0c8310e3714a411bb00077a4380eda69a1881826989898e268762a92c04984a05565480c4d572aca2268a1098dba569ead0e3b7562a82698263838c8e0d2a8374e4529c43988115757bbabc809b438c434ec636e483b1c1c62ababa8206aa4061189a1348a77a56806129377481a72aeac75a686d151c69a6dcb913486955b00ae3556d362189b8d314a06c7765c68549052bc43415619756d8e3551a06eb135100e0d2018965e455aabab6d38d27018931f7dea38e541b6804eb4c8c0556c64a01a43554ae3b8e389a715412abc8edb55752310c4930665020495059481340c431190c8f58e351a01c10e38e269524e355655b55751313951a04e4514608589c00621c62ab62ab6c4342a134ef9c88072a3657018106d2abc8000c8d151e14a400000c70d0d34d0da8d558e342760d54923886898000d56ab4dc0189826934d229bd72e22a66804da63839975aa906aadd471b4866600ed5cc562a2ea2ad568189a68a9045406ee34a9451c293b155e5d5bab1822aeada6269dca069b0ab51c62005908668626380e0dc6934d44c734ac4380e4549038c23013624c690c4332a6a047562692aba51a2b55aabad30ab5481c7018f9cd4770621b9075a1c2aeadac821d58ee90d32a811292701a69524c2156c20dc0438c80c2ab9771a51bb02b8395aab991c1369c69db48551bc42949207796524aa3b6d56ab55aa502a3b64a4a8a8888a8c49535132ad906389b80034240db693bd30940262ababa91bbc4e2756035281c72b0a20e3b549034e0c4c8c04d35563b72921a29c96812a49c20d34e55b1301318931263b65352e2292072f234aa08ab1acbab6d0e340980c92003493841a72b4c4d15630ad269c138e3aa426a03b6a22890ad0d55b4d648d0eda43831d0947127076c01c6991aabab698d3b1c72841577a5152702a2165b7189a32db8a909892a4d1484ee45411cab70ad3956310d524980a909a0464b88403d09d6989ca912c8ee90218e22920a12006a50954415a0ab2b437132b400349c194d0924edc1e9a2ada54926c234c4c4a95400a510e41a01da63134d3134c556da729d8e036621da6a0395750400c40c1302f4690e09b49269083432cc8869acb72aea09360e260e337b49e8486adb040d0c8310e3694a111bb00077a4389a40d3431030b00b37591ac5525bbd454e2685565480c4c778951134c0131d3a5155c1c6a538954134c131c59752380e540600ec771a1821544571c6aac600980351c65524876389c6d34d35560d3710c23001382628ee241493b6e2aba89c60875a71521547290d9bab96810e15134e571aad38da13686818a50055ea169a438052803415615756c1a191a70a626a201c1a40312cbc8ab55756da71a4e031263efbd471ca836d009d699180aad8c9403486aa95c771c71355a5504aaf23b6d55d48c43124c1994081254165204d0310c464323d638e46849c10e38e269524e355655b59751313951a71655de93fe1fffc";
        let frame = decode_hex(frame_hex);

        let samples = decoder
            .decode(&frame)
            .expect("a real ALAC frame must decode successfully");
        assert_eq!(
            samples.len(),
            4096 * 2,
            "expected one full stereo frame's worth of samples"
        );
        assert!(
            samples.iter().any(|&s| s.abs() > 0.01),
            "decoded a 440Hz sine wave, samples should not all be near-silent"
        );
        assert!(
            samples.iter().all(|&s| (-1.0..=1.0).contains(&s)),
            "samples must be normalized to [-1.0, 1.0]"
        );
    }
}
