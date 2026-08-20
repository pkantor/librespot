#!/usr/bin/env python3
"""Test client for the UDP control API of this librespot fork (see src/server.rs).

Subscribes, keeps the lease alive and prints every pushed event. Commands typed on stdin
are forwarded to the server, so this doubles as a remote control and as a reference for
what a client has to do.

    ./api_test.py [host] [port]
    ./api_test.py 192.168.2.10        # the device on its own machine
    ./api_test.py --raw                # print the JSON as it comes in
    ./api_test.py --covers ./art       # ask for every cover and write it to a directory

Commands to type once it runs:

    next / pause / resume / volup / voldown
    setvol {"volume": 32768}
    status / current_track / getvol / cover

What a client has to get right, all of which this script demonstrates:

  * subscribe once, then renew with `subscribe` again, well inside the 30s lease. Renewing
    is subscribing: the server refreshes a lease that exists and creates one that doesn't,
    so a client that lost its lease keeps going. Every renewal answers with a snapshot,
    which re-syncs the client after a dropped event.
  * no response carries the cover. A track event is small; the picture is asked for with
    `cover` and arrives as a series of `cover_chunk` datagrams to reassemble in `index`
    order. `count` says how many to expect, and `count: 0` means there is none to send.
  * `pending` says whether a `count: 0` is final. The picture is fetched when the track
    changes and takes a moment, and a client watching `track_changed` asks inside that
    window, so the two cases have to be told apart. `pending: true` means not yet: wait
    for `cover_available` and ask again. `pending: false` means this track has no cover
    at all and there is nothing to wait for. Chunks carrying picture are never pending.
  * `cover_available` says a picture is ready to be asked for — the moment to send `cover`
    if you want one. It is pushed to subscribers only.
  * nothing is retransmitted. If a chunk goes missing (the count doesn't add up, or a
    chunk never arrives), ask `cover` again. The server paces the chunks so a receive
    buffer of any reasonable size keeps up, and re-encodes oversized artwork smaller,
    so a picture that never completes means something worse than a busy client.
  * both sources look the same. `source` says `"spotify"` or `"airplay"`; the events and
    the track shape are identical, and fields that don't apply to a source are empty
    rather than missing.
"""

import argparse
import base64
import json
import queue
import socket
import sys
import threading
import time
from pathlib import Path

DEFAULT_PORT = 50505

# The server drops a subscription after 30s. Renewing every 10s survives two lost
# keepalives, and each renewal answers with a snapshot, which re-syncs the client.
KEEPALIVE_SECONDS = 10

# Every datagram is small now that covers travel in chunks; this is roomy.
RECV_BUFFER = 64 * 1024

# Room for a burst of chunks arriving while this client is busy elsewhere. Ask for a lot,
# but don't count on getting it: Linux silently clamps this to net.core.rmem_max, ~208 kB by
# default, which is why the server paces the chunks rather than firing a whole cover at once.
SOCKET_BUFFER = 1024 * 1024

# A cover that never finishes arriving shouldn't be waited on forever.
COVER_TIMEOUT_SECONDS = 5

EXTENSIONS = {"image/jpeg": ".jpg", "image/png": ".png", "image/gif": ".gif", "image/webp": ".webp"}


def format_position(position_ms, duration_ms=0):
    position = f"{position_ms // 60000}:{position_ms // 1000 % 60:02d}"

    if duration_ms:
        return f"{position} / {duration_ms // 60000}:{duration_ms // 1000 % 60:02d}"

    return position


def format_track(track):
    name = track.get("song_name") or "(nothing playing)"
    artists = ", ".join(track.get("song_artists", []))
    album = track.get("album", "")
    source = track.get("source", "")

    line = name
    if artists:
        line += f" - {artists}"
    if album:
        line += f" [{album}]"
    if source:
        line += f" ({source})"

    return line


def format_event(payload):
    """One line per event, so that a track change is easy to spot in the stream."""
    # the answers to getvol / current_track carry no event field
    event = payload.get("event", "reply")

    if event == "snapshot":
        track = payload.get("track", {})
        state = "playing" if payload.get("is_playing") else "paused"
        position = format_position(
            payload.get("position_ms", 0), track.get("duration_ms", 0)
        )
        volume = payload.get("volume", 0)

        return (
            f"snapshot: {format_track(track)}\n"
            f"    {state} at {position}, volume {volume} ({volume / 65535 * 100:.0f}%)"
        )

    if event == "track_changed":
        return f"track_changed: {format_track(payload.get('track', {}))}"

    if event == "playback_changed":
        state = "playing" if payload.get("is_playing") else "paused"
        return f"playback_changed: {state} at {format_position(payload.get('position_ms', 0))}"

    if event == "volume_changed":
        volume = payload.get("volume", 0)
        return f"volume_changed: {volume} ({volume / 65535 * 100:.0f}%)"

    if event == "cover_available":
        return (
            f"cover_available: {payload.get('mime', '?')}, "
            f"{payload.get('bytes', 0) / 1024:.1f} kB"
        )

    if event == "cover_chunk":
        count = payload.get("count", 0)
        if count == 0:
            if payload.get("pending"):
                return "cover_chunk: the cover is still being fetched, ask again"
            return "cover_chunk: this track has no cover"
        return f"cover_chunk: {payload.get('index', 0) + 1} of {count}"

    if "song_uri" in payload:  # the answer to current_track
        return f"current_track: {format_track(payload)}"

    return f"{event}: {json.dumps(payload)}"


def shorten(payload):
    """The payload with a chunk's base64 replaced by its size, so --raw stays readable."""
    if payload.get("event") == "cover_chunk" and payload.get("data"):
        payload = dict(payload)
        payload["data"] = f"<{len(payload['data'])} base64 chars>"

    return payload


def track_of(payload):
    """The track a payload carries, be it an event or the answer to `current_track`."""
    track = payload.get("track")

    if isinstance(track, dict):
        return track

    return payload if "song_uri" in payload else None


def read_stdin(commands):
    """Reads commands on a thread, since selecting on stdin does not work on Windows."""
    for line in sys.stdin:
        commands.put(line.strip())

    commands.put(None)


class CoverAssembly:
    """Chunks of one cover, in `index` order, until they are all there.

    The server sends them in order and does not retransmit, so this holds them by index and
    reports the picture complete once none is missing — a lost chunk simply never
    completes, and is dealt with by asking again.
    """

    def __init__(self):
        self.chunks = {}
        self.count = 0
        self.mime = ""
        self.started = 0.0

    def add(self, payload):
        count = payload.get("count", 0)

        # A `count: 0` never reaches here: it carries no chunk, and with `pending` it is not
        # even a statement about the picture. The caller sorts that out.
        if count != self.count:
            # a new cover: forget a half-received one rather than mixing two pictures
            self.chunks = {}
            self.count = count
            self.started = time.monotonic()

        self.mime = payload.get("mime", "")
        self.chunks[payload.get("index", 0)] = payload.get("data", "")

        if self.count and len(self.chunks) == self.count:
            data = "".join(self.chunks[index] for index in sorted(self.chunks))
            self.chunks = {}
            self.count = 0

            return base64.b64decode(data)

        return None

    def timed_out(self):
        return (
            self.count
            and self.started
            and time.monotonic() - self.started > COVER_TIMEOUT_SECONDS
        )


class Client:
    def __init__(self, sock, server, covers_dir):
        self.sock = sock
        self.server = server
        self.covers_dir = covers_dir
        self.assembly = CoverAssembly()
        # the track a picture was last asked for, so a cover is requested once per track
        self.asked_for = None
        self.track = {}

    def send(self, command):
        self.sock.sendto(command.encode(), self.server)

    def on_track(self, track):
        self.track = track
        # a new track means whatever cover we were collecting is stale
        self.assembly = CoverAssembly()
        self.asked_for = None

    def on_cover_available(self):
        """The server has a picture for what is playing; ask for it if we want one.

        Also how a `pending` answer gets retried: `on_cover_chunk` forgets having asked, and
        the fetch that was still running is what pushes this once it lands.
        """
        if not self.covers_dir:
            return

        uri = self.track.get("song_uri") or self.track.get("song_name") or ""
        if uri == self.asked_for:
            return

        self.asked_for = uri
        self.send("cover")

    def on_cover_chunk(self, payload):
        if payload.get("count", 0) == 0:
            if payload.get("pending"):
                # asked while the fetch was still running, so this says nothing about the
                # track: forget having asked and let `cover_available` ask again
                self.asked_for = None
            # `pending: false` is final — this track has no cover, and asking again at the
            # same one would only ever get the same answer
            self.assembly = CoverAssembly()
            return

        cover = self.assembly.add(payload)

        if cover is not None:
            self.save_cover(cover)

    def save_cover(self, cover):
        if not self.covers_dir:
            return

        track = self.track
        name = track.get("song_id") or track.get("song_name") or "cover"
        name = "".join(c if c.isalnum() or c in "-_" else "_" for c in name)
        path = self.covers_dir / f"{name}{EXTENSIONS.get(self.assembly.mime, '.bin')}"
        path.write_bytes(cover)

        print(f"    saved {len(cover)} bytes to {path}", flush=True)

    def check_cover_timeout(self):
        if self.assembly.timed_out():
            print("    a cover chunk went missing, asking again", flush=True)
            self.assembly = CoverAssembly()
            self.send("cover")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("host", nargs="?", default="127.0.0.1")
    parser.add_argument("port", nargs="?", type=int, default=DEFAULT_PORT)
    parser.add_argument(
        "--raw", action="store_true", help="print the JSON instead of a summary"
    )
    parser.add_argument(
        "--covers", type=Path, help="ask for every cover and write it to this directory"
    )
    args = parser.parse_args()

    if args.covers:
        args.covers.mkdir(parents=True, exist_ok=True)

    server = (args.host, args.port)
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, SOCKET_BUFFER)
    sock.settimeout(1)

    client = Client(sock, server, args.covers)

    commands = queue.Queue()
    threading.Thread(target=read_stdin, args=(commands,), daemon=True).start()

    print(
        f"subscribing to {args.host}:{args.port} — type commands, ctrl-c to quit",
        flush=True,
    )
    client.send("subscribe")
    last_keepalive = time.monotonic()

    try:
        while True:
            try:
                datagram, _ = sock.recvfrom(RECV_BUFFER)
            except socket.timeout:
                datagram = None
            except OSError as e:
                print(f"receive failed: {e}", flush=True)
                datagram = None

            if datagram is not None:
                try:
                    payload = json.loads(datagram)
                except json.JSONDecodeError:
                    print(f"not json: {datagram!r}", flush=True)
                    payload = None

                if payload is not None:
                    print(
                        json.dumps(shorten(payload)) if args.raw else format_event(payload),
                        flush=True,
                    )

                    event = payload.get("event")
                    if event == "cover_available":
                        client.on_cover_available()
                    elif event == "cover_chunk":
                        client.on_cover_chunk(payload)
                    else:
                        track = track_of(payload)
                        if track is not None:
                            client.on_track(track)

            while True:
                try:
                    command = commands.get_nowait()
                except queue.Empty:
                    break

                if command:
                    client.send(command)

            client.check_cover_timeout()

            now = time.monotonic()

            if now - last_keepalive >= KEEPALIVE_SECONDS:
                # renewing is subscribing again; there is nothing cheaper to send
                client.send("subscribe")
                last_keepalive = now
    except KeyboardInterrupt:
        client.send("unsubscribe")
        print("\nunsubscribed", flush=True)


if __name__ == "__main__":
    main()
