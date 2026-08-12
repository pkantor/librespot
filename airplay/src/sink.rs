//! Hands decoded PCM off to a real `librespot_playback` audio backend.
//!
//! **Its own sink, not one shared with Spotify Connect.** This opens an independent
//! `Box<dyn Sink>` — the same backend/device/format the user picked for Spotify Connect via
//! `--backend`/`--device`/`--format` (see [`SinkConfig`]), but a *separate instance*, not a
//! shared one. Two consequences, both flagged rather than silently wrong:
//! - If Spotify Connect is already playing through an exclusive-access backend (e.g. `alsa` on a
//!   hardware device that only one process/handle can own), opening a second sink for AirPlay
//!   will fail — `Sink::start`'s `Err` is logged and playback silently drops rather than panicking,
//!   but no attempt is made to pause Spotify first. For backends that mix in software (e.g.
//!   `rodio` via the system's default output), both would instead be audible at once, talking
//!   over each other. Nothing here arbitrates between the two sources; in practice a person
//!   playing to this device from a phone is not also driving it from Spotify.
//! - No resampling: every backend derives its output rate from `librespot_playback::SAMPLE_RATE`
//!   (44100 Hz, hardcoded — confirmed in `rodio.rs`'s `create_sink`, which builds its `cpal`
//!   stream config from that constant directly, not from anything passed into `Sink::write`).
//!   Classic AirPlay is 44100 Hz — that is what `discovery` advertises (`sr=44100`) and what
//!   every observed sender negotiates in its `a=fmtp:` line, so the rates match exactly. A
//!   sender that negotiated some other rate would decode correctly and play back pitched, since
//!   nothing here resamples.
//!
//! The sink runs on its own dedicated OS thread, not inside the async `decode_audio` task: real
//! backends block. `RodioSink::write` in particular calls `thread::sleep` in a loop to throttle
//! its internal buffer (`playback/src/audio_backend/rodio.rs`) — calling that directly from an
//! async task would stall the tokio runtime worker it's running on. This mirrors
//! `playback/src/player.rs`'s own `PlayerInternal`, which likewise owns its `Sink` from a
//! dedicated thread rather than an async task, for the same reason.

use librespot_playback::{
    audio_backend::{Sink, SinkBuilder},
    config::AudioFormat,
    convert::Converter,
    decoder::AudioPacket,
};
use log::warn;
use std::sync::mpsc;

/// The backend/device/format the user selected for Spotify Connect (`Setup.backend`/`.device`/
/// `.format` in `src/main.rs`), reused here rather than adding separate AirPlay-specific flags —
/// there is exactly one physical audio output to configure, even before the two sources share it.
#[derive(Clone)]
pub(crate) struct SinkConfig {
    pub(crate) backend: SinkBuilder,
    pub(crate) device: Option<String>,
    pub(crate) format: AudioFormat,
}

/// A running audio-output thread. Dropping this drops the channel `Sender`, which ends the
/// thread's loop and lets it call `Sink::stop` on its way out — the thread itself is left
/// detached (not joined) so dropping never blocks the caller.
pub(crate) struct AudioOutputHandle {
    tx: mpsc::Sender<Vec<f64>>,
    // Only read by tests (destructured to join deterministically before asserting on ordering,
    // see `tests` below) — production code just drops the whole handle and lets the thread finish
    // detached, so this field is otherwise write-only outside `#[cfg(test)]`.
    #[allow(dead_code)]
    thread: std::thread::JoinHandle<()>,
}

impl AudioOutputHandle {
    pub(crate) fn spawn(config: SinkConfig) -> Self {
        Self::spawn_with(
            move |device, format| (config.backend)(device, format),
            config.device,
            config.format,
        )
    }

    /// Split out from [`Self::spawn`] so tests can supply a fake [`Sink`] without a `SinkBuilder`
    /// function pointer (which, being a plain `fn`, can't close over test state like a channel).
    fn spawn_with<F>(build: F, device: Option<String>, format: AudioFormat) -> Self
    where
        F: FnOnce(Option<String>, AudioFormat) -> Box<dyn Sink> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel::<Vec<f64>>();
        let thread = std::thread::spawn(move || {
            let mut sink = build(device, format);
            let mut converter = Converter::new(None);
            let mut started = false;

            while let Ok(samples) = rx.recv() {
                if !started {
                    if let Err(err) = sink.start() {
                        warn!("airplay: failed to start the audio sink: {err}");
                        break;
                    }
                    started = true;
                }
                if let Err(err) = sink.write(AudioPacket::Samples(samples), &mut converter) {
                    warn!("airplay: failed to write to the audio sink: {err}");
                }
            }

            if started {
                let _ = sink.stop();
            }
        });

        Self { tx, thread }
    }

    /// Queues decoded PCM for playback. Never blocks the caller — if the output thread has
    /// already died (e.g. `Sink::start` failed), this silently drops the samples, matching this
    /// crate's established "no fallback, just an empty/absent result" pattern (see e.g. the
    /// cover-art fetch in `src/server.rs`'s docs) rather than propagating an error nothing here
    /// would act on differently.
    pub(crate) fn write(&self, samples: Vec<f64>) {
        let _ = self.tx.send(samples);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    enum Event {
        Start,
        Write(usize),
        Stop,
    }

    struct FakeSink {
        tx: mpsc::Sender<Event>,
    }

    impl Sink for FakeSink {
        fn start(&mut self) -> librespot_playback::audio_backend::SinkResult<()> {
            let _ = self.tx.send(Event::Start);
            Ok(())
        }

        fn stop(&mut self) -> librespot_playback::audio_backend::SinkResult<()> {
            let _ = self.tx.send(Event::Stop);
            Ok(())
        }

        fn write(
            &mut self,
            packet: AudioPacket,
            _converter: &mut Converter,
        ) -> librespot_playback::audio_backend::SinkResult<()> {
            let len = packet.samples().expect("test always sends Samples").len();
            let _ = self.tx.send(Event::Write(len));
            Ok(())
        }
    }

    #[test]
    fn starts_writes_in_order_and_stops_on_shutdown() {
        let (events_tx, events_rx) = mpsc::channel();
        let handle = AudioOutputHandle::spawn_with(
            move |_device, _format| Box::new(FakeSink { tx: events_tx }) as Box<dyn Sink>,
            None,
            AudioFormat::default(),
        );

        handle.write(vec![0.0; 4]);
        handle.write(vec![0.0; 8]);

        // Drop the sender explicitly (not the whole handle) so the thread's `rx.recv()` sees a
        // closed channel and runs its shutdown path, then join it so the events below are all
        // guaranteed to have been sent already — avoids a race against a detached thread.
        let AudioOutputHandle { tx, thread } = handle;
        drop(tx);
        thread.join().unwrap();

        assert!(matches!(events_rx.recv().unwrap(), Event::Start));
        assert!(matches!(events_rx.recv().unwrap(), Event::Write(4)));
        assert!(matches!(events_rx.recv().unwrap(), Event::Write(8)));
        assert!(matches!(events_rx.recv().unwrap(), Event::Stop));
        assert!(events_rx.recv().is_err(), "no more events expected");
    }

    #[test]
    fn never_started_means_never_stopped() {
        // A sink that always fails to start must not have `stop` called on it — nothing to
        // tear down, and some real backends (e.g. StdoutSink) treat a stop-without-start as an
        // error condition of its own.
        struct FailsToStart {
            tx: mpsc::Sender<Event>,
        }
        impl Sink for FailsToStart {
            fn start(&mut self) -> librespot_playback::audio_backend::SinkResult<()> {
                Err(
                    librespot_playback::audio_backend::SinkError::ConnectionRefused(
                        "nope".to_string(),
                    ),
                )
            }
            fn stop(&mut self) -> librespot_playback::audio_backend::SinkResult<()> {
                let _ = self.tx.send(Event::Stop);
                Ok(())
            }
            fn write(
                &mut self,
                _packet: AudioPacket,
                _converter: &mut Converter,
            ) -> librespot_playback::audio_backend::SinkResult<()> {
                unreachable!("must not write without a successful start")
            }
        }

        let (events_tx, events_rx) = mpsc::channel();
        let handle = AudioOutputHandle::spawn_with(
            move |_device, _format| Box::new(FailsToStart { tx: events_tx }) as Box<dyn Sink>,
            None,
            AudioFormat::default(),
        );
        handle.write(vec![0.0; 4]);

        let AudioOutputHandle { tx, thread } = handle;
        drop(tx);
        thread.join().unwrap();

        assert!(
            events_rx.recv().is_err(),
            "stop must not be called when start failed"
        );
    }
}
