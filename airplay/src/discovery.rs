//! `_raop._tcp` mDNS advertisement — how a sender finds this receiver, and how it decides what
//! kind of receiver it is.
//!
//! **One service, and nothing else.** shairport-sync in AirPlay 2 mode registers `_raop._tcp`
//! (`config.regtype`) and `_airplay._tcp` (`config.regtype2`) simultaneously on the same port
//! (`bonjour_strings.c`'s `build_bonjour_strings`, `shairport.c` around `config.regtype`, both
//! fetched and read directly); its classic branch registers only the first. This crate follows
//! the classic branch, deliberately: an `_airplay._tcp` record is exactly what makes a sender
//! take the AirPlay 2 path, and a sender on that path sends no `DACP-ID`/`Active-Remote` — which
//! is to say it offers no way to be told to pause or skip. Discovery is therefore what decides
//! whether this receiver can be a remote control at all. See [`legacy_raop_txt_record`] for the
//! fields.
//!
//! Cannot fully reuse `librespot_discovery`'s existing backend abstraction: its
//! `DnsSdServiceBuilder` bakes Spotify's service type and TXT records into each backend
//! function's body rather than taking them as parameters (verified in `discovery/src/lib.rs`).
//! Registering independently instead touches no existing file, at the cost of duplicating a
//! small amount of backend-selection logic — worth it to avoid risking a regression in the
//! working Spotify Connect discovery path. `librespot_discovery::launch_libmdns` is the
//! reference the `libmdns` path mirrors, down to the `spawn_blocking` + `oneshot`-shutdown
//! shape, so the advertisement's lifetime is tied to holding the handle.
//!
//! **`libmdns` has a confirmed compliance gap that isn't practical to patch around: it never
//! sets the RFC 6762 §10.2 cache-flush bit** on any answer (checked `fsm.rs`/`services.rs`
//! directly — the bit is never referenced at all). Every real device captured against this
//! crate's own advertisement (macOS's native AirPlay Receiver, a Sonos One) sets it on every
//! unique-owner record; ours never does, and `libmdns` offers no way to opt in short of patching
//! the crate. Since this repo already has a proven alternative for exactly this problem —
//! `librespot_discovery`'s `with-dns-sd` backend, which talks to the real system daemon (Bonjour
//! on macOS, avahi's compat layer on Linux) rather than reimplementing mDNS — this module
//! supports that too, behind the `dns-sd` Cargo feature (root `airplay-dns-sd`). **Opt-in, not
//! the default**: switching unconditionally risks silently breaking `rpi-release.yml`'s cross
//! build if the avahi-compat lib isn't in that image, which is untestable here. `dns_sd`'s
//! `DNSServiceRegister` has no interface-restriction knob the way `libmdns` does, so `bind_ip`
//! has no effect on that path — the daemon manages interface exposure itself, which is
//! presumably why `--airplay-bind-ip` was necessary for `libmdns` in the first place.

use std::{net::IpAddr, sync::Arc};

use tokio::sync::oneshot;

use crate::device_id::DeviceId;

const RAOP_SERVICE_TYPE: &str = "_raop._tcp";

/// How long a sender may cache this advertisement, in seconds — Apple's own value for
/// PTR/SRV/TXT (mDNSResponder's `kStandardTTL`, 75 minutes), which is what every real receiver on
/// the network hands out.
///
/// Not a tuning knob: `libmdns::Responder::register` defaults to `libmdns::DEFAULT_TTL`, **60
/// seconds**, and libmdns announces exactly once at registration with nothing periodic after
/// (`send_unsolicited` is called only from `register_with_ttl` and from unregister). The
/// advertisement therefore survives in a sender's list only for as long as that sender keeps
/// re-querying *and* every one of those answers arrives — one lost response inside a one-minute
/// window and the device vanishes from the list while the receiver is still running and still
/// answering. Confirmed on the wire: a `dns-sd -B _raop._tcp` browse logged `Rmv` exactly
/// 60.000s after `Add`.
///
/// This governs PTR/SRV/TXT only — the records whose expiry removes the *service* from a browse
/// list. libmdns hardcodes `DEFAULT_TTL` for the A/AAAA answers it sends in reply to
/// queries (`fsm.rs`'s `add_ip_rr` call sites), so this receiver's address record still refreshes
/// on the minute regardless. That matches Apple, which likewise gives address records a short TTL
/// (120s) and the service records the long one.
///
/// A long TTL leaves a stale entry behind only after an *ungraceful* exit: dropping the
/// registration sends the RFC 6762 §10.1 goodbye (libmdns re-announces with TTL 0), so a clean
/// shutdown still removes the device from every list immediately.
///
/// Only the `libmdns` path has a TTL to set; the system daemon behind the `dns-sd` path picks
/// this same standard value itself, so the constant is `cfg`'d out with the code that reads it.
#[cfg(not(feature = "dns-sd"))]
const SERVICE_TTL_SECS: u32 = 4500;

/// This crate's own version, for `fv` — the *firmware* version, which shairport-sync likewise
/// fills with its own package version (`config.firmware_version`). A sender treats it as
/// descriptive.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Holds the mDNS registration(s) alive until [`shutdown`](Self::shutdown) — dropping this
/// without calling `shutdown` also stops advertising (the spawned task ends when the sender
/// half of the shutdown channel is dropped, same as `oneshot::Receiver::blocking_recv`'s
/// behavior on a closed channel), it just does so without the graceful log-free wait. Shared by
/// both the `libmdns` and `dns-sd` implementations of [`advertise`] — same handle shape either
/// way, only what's held alive inside the spawned task differs.
pub(crate) struct MdnsHandle {
    shutdown_tx: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl MdnsHandle {
    pub(crate) async fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
        let _ = self.task.await;
    }
}

/// The `_raop._tcp` TXT record of a plain AirPlay 1 receiver — shairport-sync's own classic
/// branch (`bonjour_strings.c`, the `else` of `config.service_type == APST_airplay2`), field for
/// field and in its order.
///
/// Everything that would say "AirPlay 2" is absent, and that is the point: no `features`/`ft`,
/// no `pk`, and `vs=105.1` rather than a modern source version. `et=0,1` offers exactly two
/// encryption types — none, and RSA — with no FairPlay among them, which is what gets a modern
/// sender to wrap the audio key with the RSA key `legacy::rsa_key` can actually unwrap.
///
/// This crate did once advertise the AirPlay 2 field set, including shairport-sync's exact
/// `features` bitmask; it got a real iPhone to connect and stream, and to offer no control
/// whatsoever. Adding a field back here is how that returns.
fn legacy_raop_txt_record() -> Vec<String> {
    vec![
        "sf=0x4".to_string(),
        format!("fv={VERSION}"),
        "am=Librespot,1".to_string(),
        "vs=105.1".to_string(),
        "tp=TCP,UDP".to_string(),
        "vn=65537".to_string(),
        "md=0,1,2".to_string(),
        "ss=16".to_string(),
        "sr=44100".to_string(),
        "da=true".to_string(),
        "sv=false".to_string(),
        "et=0,1".to_string(),
        "ek=1".to_string(),
        "cn=0,1".to_string(),
        "ch=2".to_string(),
        "txtvers=1".to_string(),
        "pw=false".to_string(),
    ]
}

/// Starts advertising `_airplay._tcp`/`_raop._tcp` via the system `dns-sd` daemon (Bonjour on
/// macOS, avahi's compat layer on Linux) instead of `libmdns` — see this module's docs for why.
/// `bind_ip` is accepted for signature parity with the `libmdns` path but has no effect: the real
/// `DNSServiceRegister` API has no equivalent restriction, the daemon manages interface exposure
/// itself.
#[cfg(feature = "dns-sd")]
pub(crate) fn advertise(
    device_id: Arc<DeviceId>,
    name: Arc<str>,
    port: u16,
    _bind_ip: Vec<IpAddr>,
) -> MdnsHandle {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let task = tokio::task::spawn_blocking(move || {
        // One service, for the reason given in the `libmdns` path below.
        let txt = legacy_raop_txt_record();
        let txt_refs: Vec<&str> = txt.iter().map(String::as_str).collect();
        let name = raop_service_name(&device_id, &name);
        let service = dns_sd::DNSService::register(
            Some(&name),
            RAOP_SERVICE_TYPE,
            None,
            None,
            port,
            &txt_refs,
        );
        let service = match service {
            Ok(service) => service,
            Err(err) => {
                log::error!("airplay: failed to register _raop._tcp via dns-sd: {err}");
                return;
            }
        };

        let _ = shutdown_rx.blocking_recv();
        drop(service);
    });

    MdnsHandle { shutdown_tx, task }
}

#[cfg(not(feature = "dns-sd"))]
pub(crate) fn advertise(
    device_id: Arc<DeviceId>,
    name: Arc<str>,
    port: u16,
    bind_ip: Vec<IpAddr>,
) -> MdnsHandle {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let task = tokio::task::spawn_blocking(move || {
        let handle = tokio::runtime::Handle::current();
        let spawn_responder = |bind_ip: Vec<IpAddr>| {
            if bind_ip.is_empty() {
                libmdns::Responder::spawn(&handle)
            } else {
                libmdns::Responder::spawn_with_ip_list(&handle, bind_ip)
            }
        };

        // One service, and only ever one — see this module's docs.
        let responder = match spawn_responder(bind_ip) {
            Ok(responder) => responder,
            Err(err) => {
                log::error!("airplay: failed to start the _raop._tcp mDNS responder: {err}");
                return;
            }
        };

        let txt = legacy_raop_txt_record();
        let txt_refs: Vec<&str> = txt.iter().map(String::as_str).collect();
        let name = raop_service_name(&device_id, &name);
        // `register_with_ttl` rather than `register`: the default TTL is a minute — see
        // `SERVICE_TTL_SECS`. The `dns-sd` path above needs no equivalent; the system daemon
        // picks the same standard TTL itself.
        let _service = responder.register_with_ttl(
            RAOP_SERVICE_TYPE,
            &name,
            port,
            &txt_refs,
            SERVICE_TTL_SECS,
        );

        let _ = shutdown_rx.blocking_recv();
    });

    MdnsHandle { shutdown_tx, task }
}

/// `_raop._tcp`'s service *instance name* — shairport-sync's own `<ap1_prefix>@<name>`
/// (`mdns.c`'s `ap1_service_name`), e.g. `"AABBCCDDEEFF@Living Room"`, confirmed against this
/// crate's own real `tcpdump` captures showing the identical shape from other real devices (a
/// Chromecast's `_raop._tcp` entry: `AABBCCDDEEFF@Speaker`). The prefix is what a sender echoes
/// back as its `DACP-ID`'s speaker id, so it has to match [`DeviceId`] exactly.
fn raop_service_name(device_id: &DeviceId, name: &str) -> String {
    format!("{}@{name}", device_id.service_name_prefix())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain AirPlay 1 receiver advertises no AirPlay 2 credentials whatsoever — that is what
    /// makes a sender treat it as one, use RSA key wrapping rather than FairPlay, and send the
    /// headers remote control depends on.
    #[test]
    fn the_legacy_record_offers_no_airplay_2_credentials() {
        let legacy = legacy_raop_txt_record();

        for absent in ["pk=", "ft=", "features=", "ov="] {
            assert!(
                !legacy.iter().any(|kv| kv.starts_with(absent)),
                "an AirPlay 1 receiver advertises no {absent}"
            );
        }

        // "no encryption, or RSA" — with no FairPlay among them.
        assert!(legacy.iter().any(|kv| kv == "et=0,1"));
        // The source version of an AirPlay 1 device, from shairport-sync's own classic branch.
        assert!(legacy.iter().any(|kv| kv == "vs=105.1"));
        // Metadata still asked for: this path pushes it the same way.
        assert!(legacy.iter().any(|kv| kv == "md=0,1,2"));
        // The audio description, which only a classic record carries.
        for present in ["ss=16", "sr=44100", "ch=2", "txtvers=1"] {
            assert!(legacy.iter().any(|kv| kv == present), "missing {present}");
        }
    }

    #[tokio::test]
    async fn advertise_and_shutdown_round_trip() {
        // Not asserting the service is actually visible on the network (no test harness for
        // that here) — this just confirms the spawn/shutdown plumbing itself doesn't panic or
        // hang, the same bar `librespot_discovery`'s own equivalent path is held to.
        let device_id = Arc::new(DeviceId::from_name("test-device"));
        let handle = advertise(device_id, Arc::from("test-device"), 0, Vec::new());
        handle.shutdown().await;
    }
}
