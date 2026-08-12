//! RTSP/1.0 request parsing and response encoding.
//!
//! Apple's RTSP variant reuses the RFC 2326 request/status-line and header framing but adds its
//! own methods (`ANNOUNCE` aside, `RECORD`, `FLUSH`, `GET_PARAMETER`, `SET_PARAMETER`, …). The
//! messages are plaintext throughout: classic AirPlay encrypts the audio, not the control
//! channel. `Method` is deliberately a plain string rather than a closed enum: no official method
//! list exists to enumerate confidently, and a string match costs nothing at this scale.
//!
//! `Request::parse` only requires that at least one full request's worth of bytes be present in
//! `data` — reading that much off a real socket (and feeding back how many bytes it actually
//! consumed, so a persistent connection can find the next request) is `rtsp::connection`'s job.

pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) uri: String,
    pub(crate) cseq: Option<u32>,
    /// Parsed but not yet read by any dispatcher — future verbs will likely need e.g. `Range`
    /// (`RECORD`) or `Active-Remote`/`DACP-ID` (remote-control headers, relevant once `mrp`
    /// exists), so this stays populated rather than being dropped at parse time.
    #[allow(dead_code)]
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ParseError {
    #[error("empty request")]
    Empty,
    #[error("malformed request line")]
    MalformedRequestLine,
    #[error("malformed header line")]
    MalformedHeader,
    #[error("declared Content-Length exceeds the data actually received")]
    BodyTooShort,
}

impl Request {
    /// Parses one RTSP request off the front of `data`, returning it along with how many bytes
    /// of `data` it consumed — the caller drains/re-reads based on that rather than assuming the
    /// whole buffer was one request, so pipelined or partially-received-ahead bytes for the
    /// *next* request aren't lost.
    pub(crate) fn parse(data: &[u8]) -> Result<(Request, usize), ParseError> {
        let header_end = find_double_crlf(data).ok_or(ParseError::Empty)?;
        let head = std::str::from_utf8(&data[..header_end])
            .map_err(|_| ParseError::MalformedRequestLine)?;
        let mut lines = head.split("\r\n");

        let request_line = lines.next().ok_or(ParseError::Empty)?;
        let mut parts = request_line.split(' ');
        let method = parts
            .next()
            .ok_or(ParseError::MalformedRequestLine)?
            .to_string();
        let uri = parts
            .next()
            .ok_or(ParseError::MalformedRequestLine)?
            .to_string();
        // Third part is the RTSP version (e.g. "RTSP/1.0") — not validated, since real senders
        // could plausibly send an unexpected-but-still-valid variant and rejecting on that alone
        // isn't worth the fragility.

        let mut headers = Vec::new();
        let mut cseq = None;
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let (name, value) = line.split_once(':').ok_or(ParseError::MalformedHeader)?;
            let name = name.trim().to_string();
            let value = value.trim().to_string();
            if name.eq_ignore_ascii_case("cseq") {
                cseq = value.parse().ok();
            }
            headers.push((name, value));
        }

        let content_length = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.parse::<usize>().ok())
            .unwrap_or(0);

        let body_start = header_end + 4; // skip the \r\n\r\n itself
        let body_end = body_start + content_length;
        let body = data
            .get(body_start..body_end)
            .ok_or(ParseError::BodyTooShort)?
            .to_vec();

        Ok((
            Request {
                method,
                uri,
                cseq,
                headers,
                body,
            },
            body_end,
        ))
    }
}

fn find_double_crlf(data: &[u8]) -> Option<usize> {
    data.windows(4).position(|w| w == b"\r\n\r\n")
}

pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) reason: &'static str,
    pub(crate) cseq: Option<u32>,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl Response {
    pub(crate) fn new(status: u16, reason: &'static str, cseq: Option<u32>) -> Self {
        Self {
            status,
            reason,
            cseq,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub(crate) fn ok(cseq: Option<u32>) -> Self {
        Self::new(200, "OK", cseq)
    }

    pub(crate) fn not_implemented(cseq: Option<u32>) -> Self {
        Self::new(501, "Not Implemented", cseq)
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = format!("RTSP/1.0 {} {}\r\n", self.status, self.reason).into_bytes();
        if let Some(cseq) = self.cseq {
            out.extend_from_slice(format!("CSeq: {cseq}\r\n").as_bytes());
        }
        for (name, value) in &self.headers {
            out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        if !self.body.is_empty() {
            out.extend_from_slice(format!("Content-Length: {}\r\n", self.body.len()).as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&self.body);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_without_body() {
        let raw = b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\nUser-Agent: test\r\n\r\n";
        let (req, consumed) = Request::parse(raw).unwrap();
        assert_eq!(req.method, "OPTIONS");
        assert_eq!(req.uri, "*");
        assert_eq!(req.cseq, Some(1));
        assert!(req.body.is_empty());
        assert_eq!(consumed, raw.len());
        assert!(
            req.headers
                .iter()
                .any(|(k, v)| k == "User-Agent" && v == "test")
        );
    }

    #[test]
    fn parses_request_with_body() {
        let mut raw = b"POST /pair-setup RTSP/1.0\r\nCSeq: 2\r\nContent-Length: 4\r\n\r\n".to_vec();
        raw.extend_from_slice(&[1, 2, 3, 4]);
        let (req, consumed) = Request::parse(&raw).unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.uri, "/pair-setup");
        assert_eq!(req.body, vec![1, 2, 3, 4]);
        assert_eq!(consumed, raw.len());
    }

    #[test]
    fn parse_reports_only_bytes_consumed_leaving_a_pipelined_request_intact() {
        let mut raw = b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n".to_vec();
        let first_len = raw.len();
        raw.extend_from_slice(b"OPTIONS * RTSP/1.0\r\nCSeq: 2\r\n\r\n");

        let (first, consumed) = Request::parse(&raw).unwrap();
        assert_eq!(first.cseq, Some(1));
        assert_eq!(consumed, first_len);

        let (second, _) = Request::parse(&raw[consumed..]).unwrap();
        assert_eq!(second.cseq, Some(2));
    }

    #[test]
    fn rejects_truncated_body() {
        let raw = b"POST /pair-setup RTSP/1.0\r\nCSeq: 2\r\nContent-Length: 100\r\n\r\n\x01\x02";
        assert!(matches!(Request::parse(raw), Err(ParseError::BodyTooShort)));
    }

    #[test]
    fn response_round_trips_through_parse_shaped_bytes() {
        let mut response = Response::ok(Some(5));
        response.body = vec![9, 9, 9];
        response.headers.push((
            "Content-Type".to_string(),
            "application/octet-stream".to_string(),
        ));
        let bytes = response.encode();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.starts_with("RTSP/1.0 200 OK\r\n"));
        assert!(text.contains("CSeq: 5\r\n"));
        assert!(text.contains("Content-Length: 3\r\n"));
        assert!(bytes.ends_with(&[9, 9, 9]));
    }
}
