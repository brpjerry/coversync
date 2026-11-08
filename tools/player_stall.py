#!/usr/bin/env python3
"""Check the player's buffering spinner while its audio stalls, in headless Firefox.

    COVERSYNC_PASSWORD=... tools/player_stall.py http://127.0.0.1:8770/

Puts a small proxy that can stall a track's stream in front of the daemon at
the URL, opens the UI through it (signing in as tools/ui_smoke.py does), and
plays the first album four times. Each time, the page itself notes the
spinner, the play button's label and the time every 100 ms, and the run
stops at the first check that fails:

1. A stall before playback starts: the spinner shows while nothing plays,
   and goes once the audio does.
2. A seek into audio that arrives: the spinner goes within a second, and the
   time moves on from where it landed.
3. A seek whose request stalls: the spinner stays while the time doesn't move.
4. Pausing: the spinner goes, and the button reads Play.

Read-only: it only plays. Like ui_smoke.py, it needs geckodriver.
"""

import contextlib
import importlib.util
import sys
import threading
import time
import urllib.parse
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

spec = importlib.util.spec_from_file_location("ui_smoke", Path(__file__).with_name("ui_smoke.py"))
ui_smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ui_smoke)


class Stalls:
    """How the proxy stalls streams; the checks change it as they go."""

    def __init__(self):
        self.lock = threading.Lock()
        self.set()

    def set(self, start_after=None, start_for=0.0, seek_for=0.0):
        """A stream from its start pauses `start_for` seconds after `start_after`
        bytes; one from elsewhere (a seek) waits `seek_for` seconds first."""
        with self.lock:
            self.start_after, self.start_for, self.seek_for = start_after, start_for, seek_for

    def get(self):
        with self.lock:
            return self.start_after, self.start_for, self.seek_for


def stalling_proxy(upstream, stalls):
    """Serves on a free port, forwarding to `upstream` (host, port)."""

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def forward(self, method):
            length = int(self.headers.get("Content-Length") or 0)
            body = self.rfile.read(length) if length else None
            conn = HTTPConnection(*upstream)
            headers = {k: v for k, v in self.headers.items() if k.lower() not in ("connection",)}
            conn.request(method, self.path, body=body, headers=headers)
            res = conn.getresponse()
            self.send_response(res.status)
            for k, v in res.getheaders():
                if k.lower() not in ("connection", "transfer-encoding"):
                    self.send_header(k, v)
            self.send_header("Connection", "close")
            self.end_headers()
            stream = "/stream" in self.path
            from_start = self.headers.get("Range", "bytes=0-").startswith("bytes=0-")
            start_after, start_for, seek_for = stalls.get()
            if stream and not from_start and seek_for:
                self.wfile.flush()
                time.sleep(seek_for)
            sent, paused = 0, False
            try:
                while chunk := res.read(64 * 1024):
                    self.wfile.write(chunk)
                    sent += len(chunk)
                    if stream and from_start and start_after is not None and not paused and sent >= start_after:
                        self.wfile.flush()
                        paused = True
                        time.sleep(start_for)
            except (BrokenPipeError, ConnectionResetError):
                pass
            self.close_connection = True

        def do_GET(self):
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
    """One play of the first album, with the page noting what the player shows."""

    def __init__(self, d, url):
        self.d = d
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
    port = stalling_proxy((parts.hostname, parts.port or 80), stalls)
    via = urllib.parse.urlunsplit(("http", f"127.0.0.1:{port}", parts.path or "/", "", ""))
    try:
        # 1. Nothing to play for 4 s.
        stalls.set(start_after=256 * 1024, start_for=4)
        with fresh() as d:
            run = Run(d, via)
            time.sleep(7)
            run.show("a stall before playback starts")
            start = run.at("play")
            stalled = run.between(start + 0.5, start + 3.5)
            check(any(s[1] for s in stalled) and all(s[3] == "0:00" for s in stalled),
                  "the spinner shows while nothing plays")
            last = run.samples()[-1]
            check(not last[1] and seconds(last[3]) > 0, "it goes once the audio plays")

        # 2. No stalls; seek 30 s ahead.
        stalls.set()
        with fresh() as d:
            run = Run(d, via)
            time.sleep(3)
            before = seconds(run.samples()[-1][3])
            run.seek(30)
            time.sleep(3)
            run.show("a seek into audio that arrives")
            seek = run.at("seek")
            check(any(not s[1] for s in run.between(seek, seek + 1.0)),
                  "the spinner goes within a second")
            check(seconds(run.samples()[-1][3]) > before + 30, "the time moves on past the seek")

        # 3. The first request hangs after 2 MB; the seek's waits 5 s.
        stalls.set(start_after=2 * 1024 * 1024, start_for=60, seek_for=5)
        with fresh() as d:
            run = Run(d, via)
            time.sleep(3)
            run.seek(30)
            time.sleep(4)
            run.pause()
            time.sleep(1)
            run.show("a seek whose request stalls, then a pause")
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
