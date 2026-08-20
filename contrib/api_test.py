#!/usr/bin/env python3
"""Test client for the TCP control API of this librespot fork (see src/server.rs).

Connects, subscribes and prints every pushed event. Commands typed on stdin are forwarded
to the server, so this doubles as a remote control and as a reference for what a client
has to do.

    ./api_test.py [host] [port]
    ./api_test.py 192.168.2.10        # the device on its own machine
    ./api_test.py --raw                # print the JSON as it comes in
    ./api_test.py --covers ./art       # write every pushed cover to a directory

Commands to type once it runs:

    next / pause / resume / volup / voldown
    setvol {"volume": 32768}
    status / current_track / getvol
    subscribe / unsubscribe

# The shape of a client

Three things, and nothing else:

  1. connect, and send `subscribe` — the only line a display client ever needs to send.
     `unsubscribe` stops the pushes without closing the connection; simply hanging up does
     the same thing, so a client that just closes the socket needs no goodbye.
  2. loop forever reading lines, and act on each one. Every change is pushed — track,
     playback, volume, artwork. There is nothing to request, nothing to poll, no answer to
     match up with a question.
  3. when the connection ends, reconnect and go back to step 1. The `snapshot` that answers
     `subscribe` re-establishes the whole state.

## Do not do this on the UI thread

The read loop blocks. In a console program like this one the main thread is a fine place
for it (stdin gets the background thread instead), but in a GUI — Delphi's VCL, or anything
else with a message loop — it is the opposite: **the reader belongs on a worker thread**, or
the window freezes for as long as nothing is arriving, which here is most of the time. Then
never touch a widget from that thread: hand the parsed event to the UI thread the way your
framework wants it (in Delphi, `TThread.Queue` for events you can drop under load,
`TThread.Synchronize` when you must not). Sending is a different matter — see `Remote`
below on why every write should go through one place.

# Reading the wire

`LineReader` below is the part worth porting, and it is deliberately written the long way —
recv into a buffer, look for \\n, cut — instead of with Python's `makefile()`. A line reader
that comes with a language or a component library hides two things a client here cannot
afford to have hidden: its own line length limit (Indy's `MaxLineLength` is 16 kB, and its
`maSplit` mode silently hands you half a message), and what a timeout means. The algorithm
is:

  * append whatever `recv` returns to a byte buffer — never assume one read is one line, or
    that one line is one read. A cover arrives over dozens of reads.
  * a line ends at the first \\n. Cut it off the front, strip a trailing \\r, decode UTF-8.
    Whatever is left in the buffer is the start of the next line: keep it. More than one
    whole line can arrive in a single read, so loop over the buffer rather than parsing one
    line per read.
  * refuse to buffer past MAX_LINE_BYTES. A line has no length field, so without a cap the
    peer decides how much memory this process uses. See below for where the number is from.
  * a read timeout with an *empty* buffer is not an error. The server sends nothing while
    nothing is happening, so silence is the normal state of an idle connection.
  * a read timeout with a *partial* line held is a stall: the rest of the message was
    promised and hasn't come. After STALL_SECONDS of that, treat the connection as dead.
  * `recv` returning zero bytes is the server closing the connection. Reconnect.

# Sizing the buffer

MAX_LINE_BYTES is 2.8 MB, and that is a published property of the server rather than a
guess: the longest line it can send is a `cover` event carrying the largest cover it will
accept (2 MB), which base64 grows by a third before the JSON envelope goes round it. The
server's `the_longest_line_is_bounded` test fails if that stops holding. Every other line
is a few hundred bytes.

# Noticing a server that vanished

TCP only reports a peer that closes the socket. A Pi that loses power, or a Wi-Fi drop,
leaves this end blocked on a connection that is never coming back — for hours, until the
kernel's own keepalive gets around to it. So this client sends `status` when the connection
has been quiet for PROBE_EVERY seconds and expects an answer within PROBE_DEADLINE. That is
a client-side choice; the server keeps no timer and no state for it, and `status` is an
ordinary command.

# The rest of what a client has to get right

  * one connection, kept open. Newline-delimited both ways: one JSON object per line coming
    back, one `<command>[ <json payload>]` line going out.
  * **subscribe, then only read.** That is the entire client side. There is no lease to
    renew, nothing to poll, nothing to request and no answer to correlate with a request —
    every change is pushed, artwork included. Nothing is lost or reordered on a stream, so
    the `snapshot` answering `subscribe` plus every event after it is the whole state.
  * reconnect when the connection goes, and subscribe again. The server closes a client that
    stopped reading (its queue filled), and restarts happen. The fresh snapshot is the
    re-sync.
  * `track_changed` means the artwork on screen is stale. Drop it. If the new track has a
    picture, a `cover` event follows by itself a moment later — it is fetched, so it does
    not arrive with the track. If it has none, if the fetch fails, or if it is still
    running, nothing arrives at all: all three look the same, and drawing nothing until a
    `cover` shows up is the right answer to every one of them. There is nothing to ask
    about and no negative to interpret.
  * the `cover` event carries the whole picture, base64, exactly as the source published it
    and never re-encoded. It only ever arrives with a picture in it.
  * the query commands (`status`, `current_track`, `getvol`) are for something else
    entirely — a script that connects, asks one question and leaves. A subscriber has no
    reason to send any of them. This client still forwards them when you type them, because
    that is what makes it a remote control as well.
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

# The longest line the server can send: a `cover` answer at its 2 MB cover limit, grown by
# base64 and wrapped in JSON. Pinned server-side by `the_longest_line_is_bounded`.
MAX_LINE_BYTES = 2_800_000

# How long one `recv` waits before giving up on this round. Short, because it is also what
# paces the liveness check below — not because data is expected this often.
READ_TIMEOUT_SECONDS = 2

# How long the rest of a half-received line may fail to arrive before the connection counts
# as dead. Only ever applies mid-line; an idle connection is silent by design.
STALL_SECONDS = 15

# How quiet the connection may be before this client pokes it with `status`, and how long
# that answer may take. Neither is a protocol requirement — see the module docstring.
PROBE_EVERY = 30
PROBE_DEADLINE = 10

# How much to ask for per read. Only an efficiency knob: the loop copes with any amount.
RECV_BYTES = 64 * 1024

# How long to wait before trying a lost connection again.
RECONNECT_SECONDS = 2

EXTENSIONS = {"image/jpeg": ".jpg", "image/png": ".png", "image/gif": ".gif", "image/webp": ".webp"}


class LineReader:
    """Newline-delimited lines off a socket, with a byte cap and a timeout that means something.

    See the module docstring — this is the part to port, and the comments are the spec.
    """

    def __init__(self, sock):
        self.sock = sock
        self.sock.settimeout(READ_TIMEOUT_SECONDS)
        # bytearray rather than bytes: this is appended to once per read and cut from the
        # front once per line, and a 2.8 MB line makes the difference between the two worth
        # having
        self.buffer = bytearray()
        # when the buffer last stopped growing while holding a partial line
        self.silent_since = None

    def read_line(self):
        """One line, `None` if nothing arrived this round, raising if the connection is done.

        `None` is not an error: it means the server had nothing to say, which is its normal
        state. The caller uses those rounds to run its liveness check.
        """
        while True:
            end = self.buffer.find(b"\n")

            if end >= 0:
                line = bytes(self.buffer[:end])
                # everything after the newline belongs to the next line
                del self.buffer[: end + 1]
                self.silent_since = None

                # a client may well send CRLF, and so might a proxy in between
                return line.rstrip(b"\r").decode("utf-8", errors="replace")

            if len(self.buffer) > MAX_LINE_BYTES:
                raise ConnectionError(
                    f"a line went past {MAX_LINE_BYTES} bytes without ending"
                )

            try:
                chunk = self.sock.recv(RECV_BYTES)
            except socket.timeout:
                if not self.buffer:
                    # nothing owed, nothing wrong: an idle connection is simply quiet
                    return None

                # a partial line is a promise the server has not kept yet
                now = time.monotonic()
                if self.silent_since is None:
                    self.silent_since = now
                elif now - self.silent_since > STALL_SECONDS:
                    raise ConnectionError(
                        f"{len(self.buffer)} bytes of a line arrived and then nothing "
                        f"for {STALL_SECONDS}s"
                    )
                continue

            if not chunk:
                raise ConnectionError("the server closed the connection")

            self.silent_since = None
            self.buffer.extend(chunk)


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

    if event == "cover":
        return f"cover: {payload.get('mime', '?')}, {payload.get('bytes', 0) / 1024:.1f} kB"

    if "song_uri" in payload:  # the answer to current_track
        return f"current_track: {format_track(payload)}"

    return f"{event}: {json.dumps(payload)}"


def shorten(payload):
    """The payload with a cover's base64 replaced by its size, so --raw stays readable."""
    if payload.get("event") == "cover" and payload.get("data"):
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


class Remote:
    """Whichever connection is current, for the stdin thread to send over.

    One of these outlives the connections, rather than a sender thread per connection:
    reconnecting would otherwise leave the previous thread blocked on the same queue,
    stealing every other typed command.
    """

    def __init__(self):
        self.lock = threading.Lock()
        self.sock = None
        self.stopped = False

    def attach(self, sock):
        with self.lock:
            self.sock = sock

    def detach(self):
        with self.lock:
            self.sock = None

    def send(self, command):
        with self.lock:
            if self.sock is None:
                print("    not connected, command dropped", flush=True)
                return

            try:
                self.sock.sendall(f"{command}\n".encode())
            except OSError as e:
                print(f"    could not send: {e}", flush=True)

    def stop(self):
        """Ends the client, unblocking the reader that is sitting in recv."""
        with self.lock:
            self.stopped = True

            if self.sock is not None:
                try:
                    self.sock.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass


def forward_commands(commands, remote):
    """Reads what was typed and sends it, for as long as there is a connection to send on."""
    while True:
        command = commands.get()

        if command is None:  # stdin ended
            remote.stop()
            return

        if command:
            remote.send(command)


class Client:
    """One connection to the server, for as long as it lasts."""

    def __init__(self, sock, remote, covers_dir):
        self.sock = sock
        self.reader = LineReader(sock)
        # Everything this client sends goes through `Remote` as well, rather than straight to
        # the socket: the stdin thread writes there too, and two `sendall`s racing can
        # interleave and hand the server half of one command inside another.
        self.remote = remote
        self.covers_dir = covers_dir
        self.track = {}
        self.last_seen = time.monotonic()
        self.probe_sent_at = None

    def close(self):
        self.sock.close()

    def send(self, command):
        self.remote.send(command)

    def check_liveness(self):
        """Notices a server that went away without closing the socket. See the module docstring."""
        now = time.monotonic()

        if self.probe_sent_at is not None:
            if now - self.probe_sent_at > PROBE_DEADLINE:
                raise ConnectionError(f"no answer to `status` in {PROBE_DEADLINE}s")
            return

        if now - self.last_seen > PROBE_EVERY:
            self.send("status")
            self.probe_sent_at = now

    def on_line(self):
        """Anything arriving is proof the connection is alive, whatever it was."""
        self.last_seen = time.monotonic()
        self.probe_sent_at = None

    def on_track(self, track):
        """A new track means the artwork on screen is stale — drop it and wait.

        Nothing to request: if this track has a picture, a `cover` event follows on its own
        once the server has it (a few hundred ms, since it is fetched). If it has none, none
        arrives, and there is nothing to keep waiting for or ask about.
        """
        self.track = track

    def on_cover(self, payload):
        """A `cover` event always carries a picture — the server sends none otherwise."""
        self.save_cover(base64.b64decode(payload.get("data", "")), payload.get("mime", ""))

    def save_cover(self, cover, mime):
        if not self.covers_dir:
            return

        track = self.track
        name = track.get("song_id") or track.get("song_name") or "cover"
        name = "".join(c if c.isalnum() or c in "-_" else "_" for c in name)
        path = self.covers_dir / f"{name}{EXTENSIONS.get(mime, '.bin')}"
        path.write_bytes(cover)

        print(f"    saved {len(cover)} bytes to {path}", flush=True)


def run_connection(client, raw):
    """Pumps one connection until it closes. Returns when it does."""
    client.send("subscribe")

    while True:
        line = client.reader.read_line()

        if line is None:
            # nothing arrived this round, which is the normal state of an idle connection
            client.check_liveness()
            continue

        client.on_line()

        if not line:
            continue

        try:
            payload = json.loads(line)
        except json.JSONDecodeError:
            print(f"not json: {line!r}", flush=True)
            continue

        print(json.dumps(shorten(payload)) if raw else format_event(payload), flush=True)

        # The whole client: one switch on `event`. Nothing here sends anything back.
        event = payload.get("event")
        if event == "cover":
            client.on_cover(payload)
        else:
            track = track_of(payload)
            if track is not None:
                client.on_track(track)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("host", nargs="?", default="127.0.0.1")
    parser.add_argument("port", nargs="?", type=int, default=DEFAULT_PORT)
    parser.add_argument(
        "--raw", action="store_true", help="print the JSON instead of a summary"
    )
    parser.add_argument(
        "--covers", type=Path, help="write every pushed cover to this directory"
    )
    args = parser.parse_args()

    if args.covers:
        args.covers.mkdir(parents=True, exist_ok=True)

    commands = queue.Queue()
    remote = Remote()
    threading.Thread(target=read_stdin, args=(commands,), daemon=True).start()
    threading.Thread(target=forward_commands, args=(commands, remote), daemon=True).start()

    print(
        f"connecting to {args.host}:{args.port} — type commands, ctrl-c to quit",
        flush=True,
    )

    try:
        while not remote.stopped:
            try:
                sock = socket.create_connection((args.host, args.port))
            except OSError as e:
                print(f"could not connect: {e}, retrying in {RECONNECT_SECONDS}s", flush=True)
                time.sleep(RECONNECT_SECONDS)
                continue

            remote.attach(sock)
            client = Client(sock, remote, args.covers)

            try:
                run_connection(client, args.raw)
            except (ConnectionError, OSError) as e:
                print(f"connection lost: {e}", flush=True)
            finally:
                remote.detach()
                client.close()

            if remote.stopped:
                break

            print(f"reconnecting in {RECONNECT_SECONDS}s", flush=True)
            time.sleep(RECONNECT_SECONDS)
    except KeyboardInterrupt:
        pass

    print("\nbye", flush=True)


if __name__ == "__main__":
    main()
