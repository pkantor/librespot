#!/usr/bin/env python3
"""Test client for the UDP control API of this librespot fork (see src/server.rs).

Subscribes, keeps the lease alive and prints every pushed event. Commands typed on stdin
are forwarded to the server, so this doubles as a remote control and as a reference for
what a client has to do.

    ./api_test.py [host] [port]
    ./api_test.py 172.30.2.10          # the device on its own machine
    ./api_test.py --raw                # print the JSON as it comes in

Commands to type once it runs:

    next / pause / resume / volup / voldown
    setvol {"volume": 32768}
    status / current_track / getvol
"""

import argparse
import json
import selectors
import socket
import sys
import time

DEFAULT_PORT = 50505

# The server drops a subscription after 30s. Refreshing every 10s survives two lost
# keepalives, and each refresh answers with a full snapshot, which re-syncs the client
# after a dropped event.
KEEPALIVE_SECONDS = 10


def format_position(position_ms, duration_ms=0):
    position = f"{position_ms // 60000}:{position_ms // 1000 % 60:02d}"

    if duration_ms:
        return f"{position} / {duration_ms // 60000}:{duration_ms // 1000 % 60:02d}"

    return position


def format_track(track):
    name = track.get("song_name") or "(nothing playing)"
    artists = ", ".join(track.get("song_artists", []))
    album = track.get("album", "")

    line = name
    if artists:
        line += f" - {artists}"
    if album:
        line += f" [{album}]"
    if track.get("cover_url"):
        line += f"\n    cover: {track['cover_url']}"

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

    return f"{event}: {json.dumps(payload)}"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("host", nargs="?", default="127.0.0.1")
    parser.add_argument("port", nargs="?", type=int, default=DEFAULT_PORT)
    parser.add_argument(
        "--raw", action="store_true", help="print the JSON instead of a summary"
    )
    args = parser.parse_args()

    server = (args.host, args.port)
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)

    selector = selectors.DefaultSelector()
    selector.register(sock, selectors.EVENT_READ, "socket")
    selector.register(sys.stdin, selectors.EVENT_READ, "stdin")

    print(
        f"subscribing to {args.host}:{args.port} — type commands, ctrl-c to quit",
        flush=True,
    )
    sock.sendto(b"subscribe", server)
    last_keepalive = time.monotonic()

    try:
        while True:
            for key, _ in selector.select(timeout=1):
                if key.data == "socket":
                    datagram, _ = sock.recvfrom(65535)

                    try:
                        payload = json.loads(datagram)
                    except json.JSONDecodeError:
                        print(f"not json: {datagram!r}", flush=True)
                        continue

                    if args.raw:
                        print(json.dumps(payload), flush=True)
                    else:
                        print(format_event(payload), flush=True)
                else:
                    command = sys.stdin.readline()

                    if not command:  # stdin closed
                        selector.unregister(sys.stdin)
                        continue

                    if command.strip():
                        sock.sendto(command.strip().encode(), server)

            now = time.monotonic()

            if now - last_keepalive >= KEEPALIVE_SECONDS:
                sock.sendto(b"subscribe", server)
                last_keepalive = now
    except KeyboardInterrupt:
        sock.sendto(b"unsubscribe", server)
        print("\nunsubscribed", flush=True)


if __name__ == "__main__":
    main()
