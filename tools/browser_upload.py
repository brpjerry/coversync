#!/usr/bin/env python3
"""Upload folders through coversync's web UI in headless Firefox or Chromium, and time them.

    COVERSYNC_PASSWORD=... tools/browser_upload.py URL FOLDER... [options]

It signs in, hands the folders to the page the way a person would, and
prints each upload's phases from the uploads panel (sending, finishing, done)
with its rate over the whole upload, finishing included. It is what WEB-01's
speed numbers were measured with. Needs geckodriver or chromedriver.

How the folders reach the page (--via):
  drop   dropped on the Inbox, as from a file manager: a real drop through
         DevTools (Chromium only), so the page walks real folders.
  pick   the Upload button's folder picker, one folder at a time (Chromium only).
  files  the Upload button's Files… picker, with every file in the folders
         (no folder structure; the only way in for Firefox, whose WebDriver
         can't hand a page a folder).

--raw K skips the page's own uploader: the page sends the same files with K
plain XMLHttpRequests in flight, in 16 MiB chunks, then finishes. That is the
browser's own pace, to compare the uploader against.

--disrupt SECONDS COMMAND runs a shell command partway through, such as
restarting coversyncd or the reverse proxy; --cancel-after presses Cancel;
--leave-after navigates away and --close-after closes the tab, after which
the server should have nothing left of the upload (check its uploads
folder's .coversync-uploads). Run those against a scratch daemon only.
"""

import argparse
import base64
import json
import os
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

ELEMENT = "element-6066-11e4-a52e-4f735466cecf"
# The longest an upload, or a script running one, may take.
SCRIPT_MS = 1_800_000


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Driver:
    def __init__(self, browser, http1, profile):
        self.port = free_port()
        if browser == "firefox":
            argv = ["geckodriver", "--port", str(self.port), "--log", "error"]
        else:
            argv = ["chromedriver", f"--port={self.port}", "--log-level=SEVERE"]
        self.proc = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.base = f"http://127.0.0.1:{self.port}"
        for _ in range(100):
            try:
                urllib.request.urlopen(self.base + "/status", timeout=1)
                break
            except OSError:
                time.sleep(0.1)
        if browser == "firefox":
            prefs = {"network.http.http2.enabled": not http1, "network.http.http3.enable": False}
            caps = {"acceptInsecureCerts": True, "moz:firefoxOptions": {
                "args": ["-headless", "--width=1440", "--height=1000"], "prefs": prefs}}
        else:
            args = ["--headless=new", "--window-size=1440,1000", f"--user-data-dir={profile}",
                    "--no-first-run", "--disable-gpu"]
            if http1:
                # HTTP/3 too, which a profile that has seen the server may try.
                args += ["--disable-http2", "--disable-quic"]
            caps = {"acceptInsecureCerts": True, "goog:chromeOptions": {"args": args},
                    "goog:loggingPrefs": {"browser": "ALL"}}
        # --raw uploads in one script, which may take longer than the default 30 s.
        caps["timeouts"] = {"script": SCRIPT_MS}
        self.sid = self.cmd("POST", "/session", {"capabilities": {"alwaysMatch": caps}})["sessionId"]
        self.browser = browser

    def cmd(self, method, path, body=None):
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(self.base + path, data=data, method=method,
                                     headers={"content-type": "application/json"})
        try:
            with urllib.request.urlopen(req, timeout=SCRIPT_MS / 1000 + 60) as r:
                return json.load(r)["value"]
        except urllib.error.HTTPError as e:
            raise RuntimeError(f"{method} {path}: {e.code} {e.read()[:400]!r}") from None

    def s(self, method, path, body=None):
        return self.cmd(method, f"/session/{self.sid}{path}", body)

    def js(self, script, *args):
        return self.s("POST", "/execute/sync", {"script": script, "args": list(args)})

    def run(self, script, *args):
        """An async script: it calls its last argument with the result."""
        return self.s("POST", "/execute/async", {"script": script, "args": list(args)})

    def wait(self, script, timeout=30.0):
        end = time.time() + timeout
        while time.time() < end:
            value = self.js(script)
            if value:
                return value
            time.sleep(0.1)
        raise TimeoutError(script)

    def set_files(self, css, paths):
        el = self.s("POST", "/element", {"using": "css selector", "value": css})
        self.s("POST", f"/element/{el[ELEMENT]}/value", {"text": "\n".join(paths)})

    def drag(self, kind, paths):
        """A drag of files over the middle of the window, through DevTools."""
        data = {"items": [], "files": paths, "dragOperationsMask": 1}
        self.s("POST", "/goog/cdp/execute", {"cmd": "Input.dispatchDragEvent", "params": {
            "type": kind, "x": 700, "y": 500, "data": data}})

    def shot(self, path):
        Path(path).write_bytes(base64.b64decode(self.s("GET", "/screenshot")))

    def quit(self):
        try:
            self.s("DELETE", "")
        finally:
            self.proc.terminate()


# Records each upload's phases and their times, from the panel's list items.
WATCH = """
window.__phases = [];
const seen = new WeakMap(), ids = new WeakMap();
let next = 0;
const look = () => document.querySelectorAll('.panel li').forEach((li) => {
  if (!ids.has(li)) ids.set(li, next++);
  const phase = [...li.classList].find((c) => /^(queued|starting|sending|retrying|finishing|done|failed|cancelled)$/.test(c));
  if (seen.get(li) === phase) return;
  seen.set(li, phase);
  window.__phases.push({id: ids.get(li), name: li.querySelector('.name')?.textContent, phase,
    t: performance.now(), text: li.querySelector('.line')?.textContent.replace(/\\s+/g, ' ').trim()});
});
new MutationObserver(look).observe(document.body, {subtree: true, childList: true, attributes: true, attributeFilter: ['class']});
"""

# The same files, sent by plain XMLHttpRequests, K in flight, then finished.
RAW = """
const [base, K, paths, done] = arguments;
const files = [...document.querySelector('#stash').files];
const CHUNK = 16 << 20;
(async () => {
  const t0 = performance.now();
  const start = await fetch(base + 'api/uploads', {method: 'POST', headers: {'content-type': 'application/json'},
    body: JSON.stringify({folder: 'raw', files: files.map((f, i) => ({path: paths[i], size: f.size}))})}).then((r) => r.json());
  if (!start.id) throw JSON.stringify(start);
  const jobs = [];
  files.forEach((f, i) => { for (let o = 0; o < f.size; o += CHUNK) jobs.push([i, o, Math.min(CHUNK, f.size - o)]); });
  const put = ([i, o, n]) => new Promise((ok, fail) => {
    const x = new XMLHttpRequest();
    x.open('PUT', `${base}api/uploads/${start.id}/files/${i}?offset=${o}`);
    x.onload = () => (x.status < 300 ? ok() : fail(x.status));
    x.onerror = () => fail(0);
    x.send(files[i].slice(o, o + n));
  });
  const worker = async () => { while (jobs.length) await put(jobs.shift()); };
  await Promise.all(Array.from({length: K}, worker));
  const sent = performance.now();
  const finished = await fetch(`${base}api/uploads/${start.id}/finish`, {method: 'POST'});
  if (!finished.ok) throw `finishing: ${finished.status} ${await finished.text()}`;
  const end = performance.now();
  const bytes = files.reduce((n, f) => n + f.size, 0);
  done({total: (end - t0) / 1000, sending: (sent - t0) / 1000, finishing: (end - sent) / 1000, bytes});
})().catch((e) => done({error: String(e)}));
"""


def files_in(folders, extensions):
    """The files uploads take under `folders`: no hidden names, and an accepted extension."""
    found = []
    for folder in folders:
        for root, dirs, names in os.walk(folder):
            dirs[:] = sorted(d for d in dirs if not d.startswith("."))
            for name in sorted(names):
                if not name.startswith(".") and name.rsplit(".", 1)[-1].lower() in extensions:
                    found.append(os.path.join(root, name))
    return found


def rate(bytes_, seconds):
    return f"{bytes_ / seconds / 1e6:.0f} MB/s" if seconds else "?"


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("url", help="the UI's address, such as https://n5pro.internal/coversync/")
    ap.add_argument("folders", nargs="+")
    ap.add_argument("--browser", default="firefox", choices=["firefox", "chromium"])
    ap.add_argument("--http1", action="store_true", help="keep the browser to HTTP/1.1")
    ap.add_argument("--via", default="files", choices=["drop", "pick", "files"])
    ap.add_argument("--raw", type=int, metavar="K", help="plain XMLHttpRequests instead of the page's uploader")
    ap.add_argument("--album", action="store_true", help="then wait for the album to show, and open it with Show")
    ap.add_argument("--shot", help="save a screenshot here once done")
    ap.add_argument("--disrupt", nargs=2, metavar=("SECONDS", "COMMAND"))
    ap.add_argument("--cancel-after", type=float, metavar="SECONDS")
    ap.add_argument("--leave-after", type=float, metavar="SECONDS")
    ap.add_argument("--close-after", type=float, metavar="SECONDS")
    ap.add_argument("--profile", default=os.path.expanduser("~/.cache/coversync-browser-upload-chromium"),
                    help="Chromium's profile folder")
    args = ap.parse_args()
    if args.browser == "firefox" and args.via != "files" and not args.raw:
        ap.error("Firefox's WebDriver can't hand a page a folder: use --via files")
    password = os.environ.get("COVERSYNC_PASSWORD")
    if not password and os.environ.get("COVERSYNC_PASSWORD_FILE"):
        password = Path(os.environ["COVERSYNC_PASSWORD_FILE"]).read_text().strip()
    if not password:
        ap.error("set COVERSYNC_PASSWORD (or COVERSYNC_PASSWORD_FILE)")
    folders = [os.path.abspath(f) for f in args.folders]

    d = Driver(args.browser, args.http1, args.profile)
    try:
        d.s("POST", "/url", {"url": args.url})
        path = d.js("return new URL(location.href).pathname")
        status = d.run("""const [password, path, done] = arguments;
            fetch(path + 'api/login', {method: 'POST', headers: {'content-type': 'application/json'},
              body: JSON.stringify({password, path})}).then((r) => done(r.status), (e) => done(String(e)));""",
                       password, path)
        password = None
        if status != 204:
            sys.exit(f"signing in: {status}")
        d.s("POST", "/url", {"url": args.url})
        # The Upload button's pickers, there in an empty Inbox too, once the
        # grid has loaded rather than while its loading competes.
        d.wait("return !!document.querySelector('input[webkitdirectory]')"
               " && !document.querySelector('[aria-busy=true]')", 60)
        info = d.run("const [path, done] = arguments; fetch(path + 'api/uploads').then((r) => r.json()).then(done)", path)
        # Each file with its path under its folder, numbered by folder, from
        # one walk: for --raw, so CD1/01.flac and CD2/01.flac stay apart.
        selected = [(f, f"{i}/{os.path.relpath(f, folder)}") for i, folder in enumerate(folders)
                    for f in files_in([folder], set(info["extensions"]))]
        files = [f for f, _ in selected]
        relative = [rel for _, rel in selected]
        size = sum(os.path.getsize(f) for f in files)
        proto = d.js("return performance.getEntriesByType('navigation')[0].nextHopProtocol")
        print(f"{args.browser} over {proto}: {len(files)} files, {size} bytes", flush=True)

        if args.raw:
            d.js("const i = document.createElement('input'); i.type = 'file'; i.multiple = true; i.id = 'stash'; document.body.append(i)")
            d.set_files("#stash", files)
            r = d.run(RAW, path, args.raw, relative)
            if "error" in r:
                sys.exit(f"raw: {r['error']}")
            print(f"plain XHRs, {args.raw} in flight: {r['total']:.2f} s, {rate(r['bytes'], r['total'])}"
                  f" (sending {r['sending']:.2f} s, finishing {r['finishing']:.2f} s)")
            return

        d.js(WATCH)
        began = time.time()
        if args.via == "drop":
            d.drag("dragEnter", folders)
            d.drag("dragOver", folders)
            print("drop overlay shown:", d.js("return !!document.querySelector('.drop')"))
            d.drag("drop", folders)
        elif args.via == "pick":
            for folder in folders:
                d.set_files("input[webkitdirectory]", [folder])
        else:
            d.set_files("input[type=file][multiple]", files)
        expected = 1 if args.via == "files" else len(folders)

        if args.disrupt:
            def later():
                time.sleep(float(args.disrupt[0]))
                print(f"[{time.time() - began:.2f} s] {args.disrupt[1]}", flush=True)
                subprocess.run(args.disrupt[1], shell=True)
            threading.Thread(target=later, daemon=True).start()
        for after, what in ((args.leave_after, "leave"), (args.close_after, "close")):
            if after is None:
                continue
            time.sleep(after)
            if what == "leave":
                d.s("POST", "/url", {"url": "about:blank"})
            else:
                d.s("POST", "/window/new", {"type": "tab"})
                d.s("DELETE", "/window")
            print(f"[{time.time() - began:.2f} s] {'left the page' if what == 'leave' else 'closed the tab'}")
            # Time for the page's last requests to arrive.
            time.sleep(3)
            return
        if args.cancel_after:
            time.sleep(args.cancel_after)
            d.js("document.querySelector('.panel li button[aria-label^=\"Cancel\"]').click()")
            print(f"[{time.time() - began:.2f} s] pressed Cancel", flush=True)

        over = "['done', 'failed', 'cancelled'].includes(p.phase)"
        d.wait(f"return (window.__phases || []).filter((p) => {over}).length >= {expected}", SCRIPT_MS / 1000)
        by = {}
        for p in d.js("return window.__phases"):
            by.setdefault(p["id"], []).append(p)
        for ps in by.values():
            t0 = ps[0]["t"]
            at = {p["phase"]: (p["t"] - t0) / 1000 for p in ps}
            print(f"  {ps[-1]['name']}: " + ", ".join(f"{p['phase']} +{(p['t'] - t0) / 1000:.2f} s" for p in ps))
            if "done" in at and expected == 1:
                finishing = at["done"] - at.get("finishing", at["done"])
                print(f"    {at['done']:.2f} s, {rate(size, at['done'])}"
                      f" (sending {at['done'] - finishing - at.get('sending', 0):.2f} s, finishing {finishing:.2f} s)")
            print(f"    {ps[-1]['text']}")
        failed = [ps[-1] for ps in by.values() if ps[-1]["phase"] == "failed"]
        if failed:
            sys.exit("failed: " + "; ".join(f"{p['name']}: {p['text']}" for p in failed))
        if args.album:
            began = time.time()
            d.wait(f"return document.querySelectorAll('.panel li.done .act').length >= {expected}", 300)
            print(f"albums shown {time.time() - began:.2f} s after the uploads were done")
            d.js("document.querySelector('.panel li.done .act').click()")
            title = d.wait("return document.querySelector('.sheet-wrap h2')?.textContent", 20)
            print(f"  Show opened “{title.strip()}”")
        if args.shot:
            d.shot(args.shot)
        if args.browser == "chromium":
            logs = d.s("POST", "/se/log", {"type": "browser"})
            bad = [entry["message"] for entry in logs if entry["level"] in ("SEVERE", "WARNING")]
            print("console errors:", bad or "none")
    finally:
        d.quit()


if __name__ == "__main__":
    main()
