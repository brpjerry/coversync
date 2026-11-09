#!/usr/bin/env python3
"""Check the player's buffering spinner while its audio stalls, in headless Firefox.

    COVERSYNC_PASSWORD=... tools/player_stall.py http://127.0.0.1:8770/

Puts a small proxy in front of the daemon at the URL and opens the UI through
it (signing in as tools/ui_smoke.py does). The proxy answers every track's
stream itself, with five minutes of generated 16-bit PCM WAV at a fixed
bitrate, so where a time lies in the file is known whatever the library
holds; it can hold a stream back, or stop it for good. Everything else goes
to the daemon. The first album's first track is played four times. Each
time, the page itself notes the spinner, the play button's label and the
time every 100 ms, and the run stops at the first check that fails:

1. A stall before any playable data: the stream's first bytes are held back
   for 4 s. The spinner shows while nothing plays, and goes once the audio
   does.
2. A seek into audio that arrives: a second later there's no spinner, and the
   time moves on from where it landed.
3. A seek beyond the bytes supplied: the stream stops for good after 10 s of
   audio, and the seek, 2 minutes on, needs a request of its own, held back
   for 5 s. (Firefox waits for the stream it has instead, rather than asking
   again, for a seek less than about 1 MiB past it.) Once that request is
   seen, the spinner stays while it waits and the time doesn't move.
4. Pausing: the spinner goes, and the button reads Play.

Read-only: it only plays. Like ui_smoke.py, it needs geckodriver.
"""

import array
import contextlib
import importlib.util
import io
import math
import re
import sys
import threading
import time
import urllib.parse
import wave
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

spec = importlib.util.spec_from_file_location("ui_smoke", Path(__file__).with_name("ui_smoke.py"))
ui_smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ui_smoke)

RATE = 16_000
BYTES_PER_SECOND = RATE * 2  # mono, 16-bit
LENGTH = 300  # seconds
HEADER = 44  # a plain WAV header


def tone():
    """LENGTH seconds of a quiet 400 Hz tone, as a WAV file."""
    period = RATE // 400
    one = array.array("h", (int(3000 * math.sin(2 * math.pi * i / period)) for i in range(period)))
    out = io.BytesIO()
    with wave.open(out, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(one.tobytes() * (RATE * LENGTH // period))
    data = out.getvalue()
    assert len(data) == HEADER + BYTES_PER_SECOND * LENGTH
    return data


def offset(seconds):
    """Where `seconds` into the audio lies in the file."""
    return HEADER + seconds * BYTES_PER_SECOND


class Stalls:
    """How the proxy serves streams, and the stream requests it has had. The
    checks change it as they go."""

    def __init__(self):
        self.lock = threading.Lock()
        self.requests = []
        self.set()

    def set(self, hold_start=0.0, supply=None, hold_seek=0.0):
        """A stream request from the start waits `hold_start` seconds before its
        first byte, and stops for good after `supply` bytes (None: it doesn't);
        one from elsewhere (a seek) waits `hold_seek` seconds first."""
        with self.lock:
            self.hold_start, self.supply, self.hold_seek = hold_start, supply, hold_seek

    def take(self, start):
        """A stream request from byte `start`: records it, and says how to serve it."""
        with self.lock:
            hold = self.hold_start if start == 0 else self.hold_seek
            self.requests.append((time.monotonic(), start, hold))
            return hold, self.supply if start == 0 else None

    def seen(self, since):
        """The stream requests since `since` (time.monotonic()): [(start, held)]."""
        with self.lock:
            return [(start, hold) for t, start, hold in self.requests if t >= since]


STREAM = re.compile(r"/api/tracks/\d+/stream(\?|$)")
RANGE = re.compile(r"bytes=(\d+)-(\d*)$")


def stalling_proxy(upstream, stalls, audio):
    """Serves on a free port: streams from `audio`, the rest from `upstream` (host, port)."""

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def stream(self):
            start, end = 0, len(audio) - 1
            if m := RANGE.match(self.headers.get("Range", "")):
                start = int(m[1])
                end = min(int(m[2]), end) if m[2] else end
            hold, supply = stalls.take(start)
            self.send_response(206 if "Range" in self.headers else 200)
            self.send_header("Content-Type", "audio/wav")
            self.send_header("Accept-Ranges", "bytes")
            self.send_header("Content-Length", str(end + 1 - start))
            if "Range" in self.headers:
                self.send_header("Content-Range", f"bytes {start}-{end}/{len(audio)}")
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.flush()
            time.sleep(hold)
            stop = end + 1 if supply is None else min(end + 1, supply)
            try:
                for at in range(start, stop, 64 * 1024):
                    self.wfile.write(audio[at : min(at + 64 * 1024, stop)])
                self.wfile.flush()
                if stop <= end:
                    # Stopped for good: the connection stays open, with nothing on it.
                    time.sleep(120)
            except (BrokenPipeError, ConnectionResetError):
                pass
            self.close_connection = True

        def forward(self, method):
            length = int(self.headers.get("Content-Length") or 0)
            body = self.rfile.read(length) if length else None
            conn = HTTPConnection(*upstream)
            headers = {k: v for k, v in self.headers.items() if k.lower() != "connection"}
            conn.request(method, self.path, body=body, headers=headers)
            res = conn.getresponse()
            self.send_response(res.status)
            for k, v in res.getheaders():
                if k.lower() not in ("connection", "transfer-encoding"):
                    self.send_header(k, v)
            self.send_header("Connection", "close")
            self.end_headers()
            try:
                while chunk := res.read(64 * 1024):
                    self.wfile.write(chunk)
            except (BrokenPipeError, ConnectionResetError):
                pass
            self.close_connection = True

        def do_GET(self):
            if STREAM.search(self.path):
                self.stream()
            else:
                self.forward("GET")

        def do_POST(self):
            self.forward("POST")

        def log_message(self, *args):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server.server_address[1]


SAMPLER = """
window.__samples = [];
const t0 = performance.now();
window.__mark = (what) => window.__samples.push([(performance.now() - t0) / 1000, what]);
clearInterval(window.__sampler);
window.__sampler = setInterval(() => {
  const p = document.querySelector('.player');
  if (!p) return;
  const s = [!!p.querySelector('.main .spin'), p.querySelector('.main').getAttribute('aria-label'),
             p.querySelector('.t').textContent];
  const last = window.__last;
  if (!last || last.some((v, i) => v !== s[i])) {
    window.__samples.push([(performance.now() - t0) / 1000, s]);
    window.__last = s;
  }
}, 100);
"""


def seconds(clock):
    m, s = clock.split(":")[-2:]
    return int(m) * 60 + int(s)


class Run:
    """One play of the first album's first track, with the page noting what the
    player shows."""

    def __init__(self, d, url):
        self.d = d
        self.marks = {}
        d.go(url)
        ui_smoke.sign_in(d)
        d.wait("return document.querySelectorAll('.card').length > 0")
        d.click(".cell:first-child .cover")
        d.wait("return !!document.querySelector('.tracks li')")
        time.sleep(0.5)
        d.js("window.__last = null;" + SAMPLER)
        self.mark("play")
        # A script's click: the sheet may still be settling from its opening.
        d.js("document.querySelector('.actions .btn.primary').click()")

    def mark(self, what):
        """Notes `what` in the page's samples, and when it was here."""
        self.marks[what] = time.monotonic()
        self.d.js("window.__mark(arguments[0])", what)

    def seek(self, ahead):
        self.mark("seek")
        self.d.js(
            "const b = document.querySelector('.player .bar'); b.focus();"
            "for (let i = 0; i < arguments[0] / 5; i++)"
            "  b.dispatchEvent(new KeyboardEvent('keydown', {key: 'ArrowRight', bubbles: true}));",
            ahead,
        )

    def pause(self):
        self.mark("pause")
        self.d.click(".player .main")

    def samples(self):
        """[(t, spinner, button, time)], with marks as (t, name, None, None)."""
        out = []
        for t, s in self.d.js("return window.__samples"):
            out.append((t, s, None, None) if isinstance(s, str) else (t, *s))
        return out

    def at(self, mark):
        return next(t for t, s, _, _ in self.samples() if s == mark)

    def state_at(self, t):
        """What the player showed at `t`: the last change before it."""
        states = [s for s in self.samples() if s[2] is not None and s[0] <= t]
        return states[-1] if states else None

    def between(self, start, end):
        """What the player showed from `start` to `end`: the state it was in at
        `start` (the page notes only changes), then each change."""
        states = [s for s in self.samples() if s[2] is not None]
        before = [s for s in states if s[0] < start][-1:]
        return before + [s for s in states if start <= s[0] <= end]

    def show(self, title):
        print(f"-- {title}")
        for t, s, button, clock in self.samples():
            if button is None:
                print(f"{t:6.2f}s  [{s}]")
            else:
                print(f"{t:6.2f}s  spinner={'yes' if s else 'no ':<3} button={button:<5} time={clock}")


def check(ok, what):
    print(("ok   " if ok else "FAIL ") + what)
    if not ok:
        raise SystemExit(1)


def main():
    url = sys.argv[1]
    parts = urllib.parse.urlsplit(url)
    stalls = Stalls()
    port = stalling_proxy((parts.hostname, parts.port or 80), stalls, tone())
    via = urllib.parse.urlunsplit(("http", f"127.0.0.1:{port}", parts.path or "/", "", ""))
    try:
        # 1. The stream's first bytes held back for 4 s.
        stalls.set(hold_start=4)
        with fresh() as d:
            run = Run(d, via)
            time.sleep(7)
            run.show("a stall before any playable data")
            seen = stalls.seen(run.marks["play"])
            print(f"   stream requests (from byte, held for): {seen}")
            check((0, 4) in seen, "the stream's first request was held back")
            start = run.at("play")
            stalled = run.between(start + 0.5, start + 3.5)
            check(any(s[1] for s in stalled) and all(s[3] == "0:00" for s in stalled),
                  "the spinner shows while nothing plays")
            last = run.samples()[-1]
            check(not last[1] and seconds(last[3]) > 0, "it goes once the audio plays")

        # 2. Nothing held back; seek 30 s ahead.
        stalls.set()
        with fresh() as d:
            run = Run(d, via)
            time.sleep(3)
            before = seconds(run.samples()[-1][3])
            run.seek(30)
            time.sleep(3)
            run.show("a seek into audio that arrives")
            seek = run.at("seek")
            at = run.state_at(seek + 1.0)
            check(at is not None and at[0] > seek and not at[1],
                  "a second after the seek, there's no spinner")
            check(seconds(run.samples()[-1][3]) > before + 30, "the time moves on past the seek")

        # 3. 10 s of audio, then nothing; the seek, 2 minutes on (3.6 MB past what
        # was supplied), waits 5 s for its own request.
        supplied = offset(10)
        stalls.set(supply=supplied, hold_seek=5)
        with fresh() as d:
            run = Run(d, via)
            time.sleep(3)
            run.seek(120)
            time.sleep(4)
            run.pause()
            time.sleep(1)
            run.show("a seek beyond the bytes supplied, then a pause")
            seen = stalls.seen(run.marks["seek"])
            print(f"   stream requests since the seek (from byte, held for): {seen}")
            check(any(start > supplied and held == 5 for start, held in seen),
                  f"the seek asked for bytes beyond the {supplied} supplied, and was held back")
            seek, pause = run.at("seek"), run.at("pause")
            waiting = run.between(seek + 0.5, pause - 0.2)
            check(bool(waiting) and all(s[1] for s in waiting),
                  "the spinner stays while the seek waits")
            check(len({s[3] for s in waiting}) == 1, "the time doesn't move meanwhile")

            # 4. The pause above.
            last = run.samples()[-1]
            check(not last[1] and last[2] == "Play",
                  "pausing hides the spinner, and the button reads Play")
    finally:
        stalls.set()


@contextlib.contextmanager
def fresh():
    """A browser with a profile of its own: Firefox's media cache would
    otherwise keep the track from one run to the next."""
    d = ui_smoke.Driver(dark=True)
    try:
        yield d
    finally:
        d.quit()


if __name__ == "__main__":
    main()
