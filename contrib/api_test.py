#!/usr/bin/env python3
"""Test client for the UDP control API of this librespot fork (see src/server.rs).

Subscribes, keeps the lease alive and prints every pushed event. Commands typed on stdin
are forwarded to the server, so this doubles as a remote control and as a reference for
what a client has to do.

    ./api_test.py [host] [port]
    ./api_test.py 172.30.2.10          # the device on its own machine
    ./api_test.py --raw                # print the JSON as it comes in
    ./api_test.py --covers ./art       # write every cover received to a directory

Commands to type once it runs:

    next / pause / resume / volup / voldown
    setvol {"volume": 32768}
    status / current_track / getvol

What a client has to get right, all of which this script demonstrates:

  * subscribe once, then renew with `subscribe_refresh`. The refresh answers with the same
    snapshot minus the cover bytes, which keeps a keepalive to one small datagram.
  * a track carries its cover as bytes (`cover_data`, base64) and by no other means. When
    the server reports a `song_uri` you have no cover for — which happens when the
    `track_changed` push was lost, it being the only fragmented datagram — ask
    `current_track` to get the picture.
  * receive into a buffer of ~128 kB. A track event carrying a cover is tens of kB and
    arrives fragmented; on Windows a short buffer does not truncate like on Linux, it fails
    the whole `recvfrom` with WSAEMSGSIZE and the event is lost.
  * `cover_data` is simply empty when there is no picture to be had. There are no
    substitutes and no urls to fall back to.
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

# The server drops a subscription after 30s. Refreshing every 10s survives two lost
# keepalives, and each refresh answers with a snapshot, which re-syncs the client after a
# dropped event.
KEEPALIVE_SECONDS = 10

# Big enough for a track event with its cover; see the note about WSAEMSGSIZE above.
RECV_BUFFER = 128 * 1024

# Room for a few of those events, in case this client is busy when they arrive.
SOCKET_BUFFER = 1024 * 1024

EXTENSIONS = {"image/jpeg": ".jpg", "image/png": ".png", "image/gif": ".gif", "image/webp": ".webp"}


def format_position(position_ms, duration_ms=0):
    position = f"{position_ms // 60000}:{position_ms // 1000 % 60:02d}"

    if duration_ms:
        return f"{position} / {duration_ms // 60000}:{duration_ms // 1000 % 60:02d}"

    return position


def decoded_size(data):
    """How many bytes of picture a base64 string holds, without decoding it."""
    return len(data) * 3 // 4 - data.count("=")


def format_track(track):
    name = track.get("song_name") or "(nothing playing)"
    artists = ", ".join(track.get("song_artists", []))
    album = track.get("album", "")

    line = name
    if artists:
        line += f" - {artists}"
    if album:
        line += f" [{album}]"

    cover = track.get("cover_data") or ""
    if cover:
        line += (
            f"\n    cover: {track.get('cover_mime', '?')} "
            f"{track.get('cover_width', 0)}x{track.get('cover_height', 0)}, "
            f"{decoded_size(cover) / 1024:.1f} kB"
        )

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

    if "song_uri" in payload:  # the answer to current_track
        return f"current_track: {format_track(payload)}"

    return f"{event}: {json.dumps(payload)}"


def shorten(payload):
    """The payload with the cover replaced by its size — 27 kB of base64 unreadable."""
    payload = dict(payload)
    track = payload.get("track")

    if isinstance(track, dict):
        payload["track"] = shorten(track)
    elif payload.get("cover_data"):
        payload["cover_data"] = f"<{decoded_size(payload['cover_data'])} bytes>"

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


class Client:
    def __init__(self, sock, server, covers_dir):
        self.sock = sock
        self.server = server
        self.covers_dir = covers_dir
        # what we hold a picture for, and what we last asked about, so that a track without
        # a cover on the server is asked about once rather than at every keepalive
        self.cover_of = None
        self.asked_about = None

    def send(self, command):
        self.sock.sendto(command.encode(), self.server)

    def on_track(self, track):
        uri = track.get("song_uri") or ""
        cover = track.get("cover_data") or ""

        if cover:
            self.cover_of = uri
            self.asked_about = None
            self.save_cover(track, base64.b64decode(cover))
        elif uri and uri != self.cover_of and uri != self.asked_about:
            # the push that carried it must have been lost; this is the repair path
            self.asked_about = uri
            print("    no cover for this track, asking for it", flush=True)
            self.send("current_track")

    def save_cover(self, track, cover):
        if not self.covers_dir:
            return

        name = track.get("song_id") or track.get("song_uri", "cover").replace(":", "_")
        path = self.covers_dir / f"{name}{EXTENSIONS.get(track.get('cover_mime'), '.bin')}"
        path.write_bytes(cover)

        print(f"    saved {len(cover)} bytes to {path}", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("host", nargs="?", default="127.0.0.1")
    parser.add_argument("port", nargs="?", type=int, default=DEFAULT_PORT)
    parser.add_argument(
        "--raw", action="store_true", help="print the JSON instead of a summary"
    )
    parser.add_argument(
        "--covers", type=Path, help="write every cover received to this directory"
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
                # what a buffer shorter than RECV_BUFFER would get you on Windows
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

            now = time.monotonic()

            if now - last_keepalive >= KEEPALIVE_SECONDS:
                # not `subscribe`: renewing must not drag the cover along every ten seconds
                client.send("subscribe_refresh")
                last_keepalive = now
    except KeyboardInterrupt:
        client.send("unsubscribe")
        print("\nunsubscribed", flush=True)


if __name__ == "__main__":
    main()
