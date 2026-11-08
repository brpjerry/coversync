#!/usr/bin/env python3
"""Drive the coversync UI in headless Firefox through geckodriver.

Takes screenshots of the main views and prints browser console errors.
Needs `geckodriver` and python-websocket-client (for console logs).

    COVERSYNC_PASSWORD=... tools/ui_smoke.py http://127.0.0.1:8770/ shots/ [--light] [--save]

It signs in with the password in COVERSYNC_PASSWORD, or the one after --password
(which other users can see in the process list: prefer the variable).
--auth checks the login view instead of the tour, and stops at the first failure: a
    wrong password, signing in, a session that ends under unsaved tag edits (Escape in
    the login view must keep them), and signing out (asked first while edits are unsaved).
--devices pairs a device instead of the tour (scratch daemons only: it adds, revokes and
    removes one): Add device shows a code and a QR code, the script redeems the code at
    /api/pair as an app would, the dialog shows the device, its token works, and revoking it
    in Sync setup's list gets the token 401. Then the dialog by keyboard (focus stays in it,
    Escape closes only it), and New code double-clicked after the device to replace was
    removed. Stops at the first failure.
--pairing-link only checks the link Add device makes (its server address with the page's
    path prefix, and ca= when Sync setup has a CA certificate), and makes no device: for a
    UI built with BASE_PATH behind a proxy.
--save clicks Save in the tag editor and library setup: only use it against a scratch library.
--cover-size tries Appearance's cover size slider instead of the full tour.
--suggest ALBUM [--superset ALBUM] tries Smart suggestions on an Inbox album
    (scratch daemons only: it imports the album); --superset names one that
    AcoustID matches to a bigger release, to check the warning.
--romanize ALBUM tries Prefer English the same way, on an album in another script.
--lookup-again ALBUM opens the import dialog on an album identified before, which
    should say when it was looked up, then presses Look up again (nothing is imported).
--art FOLDER and --import FOLDER try album art rules and importing, with FOLDER
    as the scratch Library.
"""

import base64
import json
import os
import re
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

try:
    import websocket  # python-websocket-client
except ImportError:
    websocket = None

ELEMENT = "element-6066-11e4-a52e-4f735466cecf"


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Driver:
    def __init__(self, dark=True):
        self.port = free_port()
        self.proc = subprocess.Popen(
            # A BiDi port of its own, so several smoke tests can run at once.
            ["geckodriver", "--port", str(self.port), "--websocket-port", str(free_port()),
             "--log", "error"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        self.base = f"http://127.0.0.1:{self.port}"
        for _ in range(50):
            try:
                urllib.request.urlopen(self.base + "/status", timeout=1)
                break
            except OSError:
                time.sleep(0.1)
        caps = {
            "alwaysMatch": {
                "webSocketUrl": True,
                "moz:firefoxOptions": {
                    "args": ["-headless", "--width=1440", "--height=1000"],
                    "prefs": {
                        "ui.systemUsesDarkTheme": 1 if dark else 0,
                        "media.autoplay.default": 0,
                        "media.autoplay.blocking_policy": 0,
                        "security.enterprise_roots.enabled": True,
                    },
                },
            }
        }
        res = self.cmd("POST", "/session", {"capabilities": caps})
        self.sid = res["sessionId"]
        self.logs = []
        ws_url = res["capabilities"].get("webSocketUrl")
        if ws_url and websocket:
            self.ws = websocket.create_connection(ws_url, suppress_origin=True)
            self.ws.send(json.dumps({
                "id": 1,
                "method": "session.subscribe",
                "params": {"events": ["log.entryAdded"]},
            }))
            threading.Thread(target=self._read_logs, daemon=True).start()

    def _read_logs(self):
        while True:
            try:
                msg = json.loads(self.ws.recv())
            except Exception:
                return
            if msg.get("method") == "log.entryAdded":
                p = msg["params"]
                self.logs.append(f"[{p.get('level')}] {p.get('text')}")

    def cmd(self, method, path, body=None):
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(self.base + path, data=data, method=method,
                                     headers={"content-type": "application/json"})
        with urllib.request.urlopen(req, timeout=60) as r:
            return json.load(r)["value"]

    def s(self, method, path, body=None):
        return self.cmd(method, f"/session/{self.sid}{path}", body)

    def go(self, url):
        self.s("POST", "/url", {"url": url})

    def js(self, script, *args):
        return self.s("POST", "/execute/sync", {"script": script, "args": list(args)})

    def wait(self, script, timeout=15.0, *args):
        end = time.time() + timeout
        while time.time() < end:
            if self.js(script, *args):
                return True
            time.sleep(0.2)
        raise TimeoutError(script)

    def _click(self, el):
        # WebDriver scrolls the element just into view, which can leave it under
        # the sticky save bar at the bottom edge; centre it first.
        self.js("arguments[0].scrollIntoView({block: 'center'})", el)
        self.s("POST", f"/element/{el[ELEMENT]}/click", {})

    def click(self, css):
        self._click(self.s("POST", "/element", {"using": "css selector", "value": css}))

    def click_text(self, scope, text):
        """Click a button by its text; switches (such as Save lookups for "Save") are left alone."""
        xpath = f"//{scope}//button[not(@role='switch')][normalize-space(.)='{text}' or starts-with(normalize-space(.), '{text} ')]"
        self._click(self.s("POST", "/element", {"using": "xpath", "value": xpath}))

    def click_xpath(self, xpath):
        self._click(self.s("POST", "/element", {"using": "xpath", "value": xpath}))

    def click_sort(self, label):
        """Pick a sort from the top bar's sort menu."""
        self.click(".sort-btn")
        self.wait("return !!document.querySelector('[role=menu]')")
        self.click_xpath(f"//div[@role='menu']//button[@role='menuitemradio'][.//span[contains(concat(' ', normalize-space(@class), ' '), ' label ')][normalize-space(.)='{label}']]")

    def open_setup(self, label):
        """Open a setup screen (Inbox setup, Library setup, Appearance, Sync setup) from the sidebar."""
        if self.js("return document.querySelector('.sidebar').classList.contains('collapsed')"):
            self.click(".toggle")
            time.sleep(0.5)
        short = {"Inbox setup": "Inbox", "Library setup": "Library", "Sync setup": "Sync"}.get(label, label)
        self.click_xpath(f"//nav[@aria-label='Setup']//button[normalize-space(.)='{short}']")

    def right_click(self, css):
        el = self.s("POST", "/element", {"using": "css selector", "value": css})
        origin = {ELEMENT: el[ELEMENT]}
        self.s("POST", "/actions", {"actions": [{
            "type": "pointer", "id": "mouse", "parameters": {"pointerType": "mouse"},
            "actions": [
                {"type": "pointerMove", "origin": origin, "x": 0, "y": 0},
                {"type": "pointerDown", "button": 2},
                {"type": "pointerUp", "button": 2},
            ],
        }]})
        self.s("DELETE", "/actions")

    def clear_and_type(self, css, text):
        self.js("const el = document.querySelector(arguments[0]); el.value = '';"
                "el.dispatchEvent(new Event('input', {bubbles: true}))", css)
        self.type(css, text)

    def toasts(self):
        return self.js("return [...document.querySelectorAll('.toast')].map(t => t.textContent.replace(/\\s+/g, ' ').trim())")

    def resize(self, width, height):
        self.s("POST", "/window/rect", {"width": width, "height": height})

    def type(self, css, text):
        el = self.s("POST", "/element", {"using": "css selector", "value": css})
        self.s("POST", f"/element/{el[ELEMENT]}/value", {"text": text})

    def shot(self, path):
        Path(path).write_bytes(base64.b64decode(self.s("GET", "/screenshot")))
        print("screenshot", path)

    def quit(self):
        try:
            self.s("DELETE", "")
        finally:
            self.proc.terminate()


def grid_intro_check(d):
    """Cards keep the entrance they mounted with: when the grid's intro ends,
    no card's animation restarts or changes timing, including cards that a
    scroll mounted during the intro. Remounts the grid of the collection shown
    by way of Appearance, which has none, so the intro starts on a known frame."""
    collection = d.js("return document.querySelector('.tabs button[aria-selected=true]')"
                      "?.textContent.includes('Library') ? 'Library' : 'Inbox'")
    d.open_setup("Appearance")
    d.wait("return !document.querySelector('.vgrid')")
    res = d.s("POST", "/execute/async", {"args": [collection], "script": """
        const [collection, done] = arguments, seen = new Map();
        [...document.querySelectorAll('.tabs button')].find(b => b.textContent.includes(collection)).click();
        let t0 = null, scrolled = false, restarts = 0, retimed = 0, scrolledIn = 0, scrolledInRising = 0;
        const timing = a => { const t = a.effect.getComputedTiming(); return t.duration + '/' + t.delay; };
        const tick = () => {
            if (t0 === null && document.querySelector('.vgrid .cell')) t0 = performance.now();
            const now = t0 === null ? -1 : performance.now() - t0;
            // Mount cards well inside the intro (1.4s), while the first ones still rise.
            if (!scrolled && now > 500) { scrolled = true; scrollBy(0, innerHeight * 2); }
            for (const c of document.querySelectorAll('.vgrid .cell')) {
                const a = c.getAnimations().find(a => a.animationName);
                const was = seen.get(c);
                if (!was) {
                    seen.set(c, { a, timing: a && timing(a) });
                    // The intro's entrance is the 560ms one.
                    if (scrolled) { scrolledIn++; if (a?.effect.getComputedTiming().duration === 560) scrolledInRising++; }
                } else if (a) {
                    if (was.a && a !== was.a) restarts++;
                    else if (was.timing && timing(a) !== was.timing) retimed++;
                    was.a = a; was.timing = timing(a);
                }
            }
            if (now < 2600) requestAnimationFrame(tick);
            else { scrollTo(0, 0); done({ cells: seen.size, scrolledIn, scrolledInRising, restarts, retimed }); }
        };
        requestAnimationFrame(tick);
    """})
    ok = res["restarts"] == 0 and res["retimed"] == 0
    print("grid intro:", "ok" if ok else "FAIL", res)
    if not res["scrolledInRising"]:
        print("  (no card mounted during the intro, so that case went unchecked)")


def appearance_flow(d, out, tag):
    """Add Label and Genre to the sort menu, move Label up; save only with --save."""
    tabs = "return [...document.querySelectorAll('[aria-labelledby=tabs-title] .tab-label')].map(b => b.textContent.trim())"
    d.open_setup("Appearance")
    d.wait("return !!document.querySelector('.chips')")
    # The tabs card sits below Cover art size; keep it clear of the save bar.
    d.js("document.querySelector('#tabs-title').scrollIntoView({block: 'start'})")
    time.sleep(0.6)
    print("tabs before:", d.js(tabs))
    print("appearance chips shown:", d.js("return document.querySelectorAll('.chip').length"))
    d.click_text("main", "Show all tags")
    time.sleep(0.4)
    print("after show all:", d.js("return document.querySelectorAll('.chip').length"))
    d.click_xpath("//button[contains(@class,'chip')][normalize-space(.)='Label']")
    d.click_xpath("//button[contains(@class,'chip')][normalize-space(.)='Genre']")
    time.sleep(0.4)
    d.click_xpath("//li[.//span[contains(@class,'tab-label')][normalize-space(.)='Label']]//button[contains(@aria-label,'up')]")
    time.sleep(0.6)
    print("sort menu edits:", d.js(tabs), "| changes:", d.js("return document.querySelector('.savebar .summary')?.textContent.trim()"))
    d.shot(out / f"9a-appearance-{tag}.png")
    if "--save" in sys.argv:
        d.click_text("main", "Save")
        d.wait("return [...document.querySelectorAll('.toast')].some(t => t.textContent.includes('Appearance saved'))")
        d.click_text("header", "Inbox")
        d.wait("return document.querySelectorAll('.card').length > 0")
        time.sleep(0.6)
        d.click_sort("Label")
        time.sleep(0.8)
        print("sorted by label, first albums:", d.js("return [...document.querySelectorAll('.card .title')].slice(0, 4).map(t => t.textContent)"))
        d.shot(out / f"9b-sorted-by-label-{tag}.png")
        # Put the defaults back for later steps and runs.
        d.open_setup("Appearance")
        d.wait("return !!document.querySelector('.chips')")
        d.click_text("main", "Reset to defaults")
        d.click_text("main", "Save")
        d.wait("return [...document.querySelectorAll('.toast')].filter(t => t.textContent.includes('Appearance saved')).length >= 1")
        time.sleep(0.5)
    else:
        d.click_text("main", "Revert")
        time.sleep(0.4)
    print("tabs after:", d.js(tabs))
    print("back button:", d.js("return document.querySelector('.setup .back')?.title"))
    d.click(".setup .back")
    d.wait("return document.querySelectorAll('.card').length > 0")
    print("after back:", repr(d.js("return location.search")))


def sync_flow(d, out, tag):
    """Flip the Sync switch; with --save, check the sidebar's Sync section follows, then flip it back."""
    section = "return !!document.querySelector('#sidebar-sync-title')"
    switch = "return document.querySelector('main [role=switch]').getAttribute('aria-checked')"
    d.open_setup("Sync setup")
    d.wait("return !!document.querySelector('#sync-title')")
    time.sleep(0.6)
    initially = d.js(switch) == "true"
    print("sync section:", d.js(section), "| switch:", d.js(switch))
    d.click("main [role=switch]")
    time.sleep(0.4)
    print("switch:", d.js(switch), "| dirty dot:", d.js("return !!document.querySelector('nav .dot')"),
          "| changes:", d.js("return document.querySelector('.savebar .summary')?.textContent.trim()"))
    d.shot(out / f"9c-sync-setup-{tag}.png")
    if "--save" in sys.argv:
        d.click_text("main", "Save")
        d.wait("return [...document.querySelectorAll('.toast')].some(t => t.textContent.includes('Sync setup saved'))")
        d.wait(f"{section} === {str(not initially).lower()}")
        print("sync section after saving:", d.js(section))
        d.shot(out / f"9d-sync-saved-{tag}.png")
        # Put the setting back for later steps and runs.
        d.click("main [role=switch]")
        d.click_text("main", "Save")
        d.wait(f"{section} === {str(initially).lower()}")
        print("sync section after restoring:", d.js(section))
    else:
        d.click_text("main", "Revert")
        d.wait(f"{switch} === '{str(initially).lower()}'")
        print("after revert:", d.js(switch))
    d.click(".setup .back")
    d.wait("return document.querySelectorAll('.card').length > 0")


def filter_flow(d, out, tag):
    """The sidebar's tag filters: pick values of two tags, search a list, then clear them."""
    cards = "return document.querySelectorAll('.card').length"
    stats = "return document.querySelector('.stats')?.textContent.trim()"
    values = ("return [...document.querySelectorAll('aside .value')]"
              ".map(v => v.querySelector('.name').textContent + ' ' + v.querySelector('.n').textContent)")
    print("filter tags:", d.js("return [...document.querySelectorAll('aside option')].map(o => o.textContent)"),
          "| values:", d.js(values)[:8])
    d.click("aside .value")
    time.sleep(0.6)
    print("picked:", d.js(cards), "cards |", d.js(stats), "| chips:",
          d.js("return [...document.querySelectorAll('aside .chip')].map(c => c.textContent.trim())"))
    # Another tag's counts only cover the albums the first one leaves.
    d.js("const s = document.querySelector('aside select'); s.value = s.options[s.options.length - 1].value;"
         "s.dispatchEvent(new Event('change', {bubbles: true}))")
    time.sleep(0.4)
    print("other tag:", d.js("return document.querySelector('aside select').value"), "| values:", d.js(values)[:8])
    d.shot(out / f"1b-filter-{tag}.png")
    d.click(".toggle")
    time.sleep(0.7)
    print("closed sidebar width:", d.js("return document.querySelector('.sidebar').getBoundingClientRect().width"),
          "| toggle badge:", d.js("return document.querySelector('.toggle .filtered')?.textContent"))
    d.shot(out / f"1b-filter-closed-{tag}.png")
    d.click(".toggle")
    time.sleep(0.7)
    first = d.js("return document.querySelector('aside .value .name')?.textContent ?? ''")
    d.type("aside input[type=search]", first[:2])
    time.sleep(0.4)
    print("searched values:", d.js(values)[:8])
    d.click("aside .field .clear")
    d.click("aside .clear-all")
    time.sleep(0.4)
    print("cleared:", d.js(cards), "cards |", d.js(stats))


def cover_size_flow(d, out, tag, save):
    """Appearance's cover size slider, its preview, and the grid it sizes; saves only with --save."""
    grid = ("const cs = [...document.querySelectorAll('.vgrid .cell')].map(c => c.getBoundingClientRect());"
            "const top = Math.min(...cs.map(r => r.top));"
            "const row = cs.filter(r => Math.abs(r.top - top) < 1);"
            "return {cols: row.length, width: Math.round(row[0].width)}")
    readout = "return document.querySelector('.readout')?.textContent.replace(/\\s+/g, ' ').trim()"
    preview = ("const c = document.querySelector('.cover-preview .cell');"
               "return c && {width: Math.round(c.getBoundingClientRect().width),"
               " cards: document.querySelectorAll('.cover-preview .card').length,"
               " inert: document.querySelector('.cover-preview').inert}")
    summary = "return document.querySelector('.savebar .summary')?.textContent.trim() ?? null"
    srcs = ("return [...new Set([...document.querySelectorAll('.vgrid .card img')]"
            ".filter(i => i.currentSrc).map(i => i.currentSrc.split('size=')[1]))]")

    def set_size(px):
        d.js("const s = document.querySelector('.slider'); s.value = arguments[0];"
             "s.dispatchEvent(new Event('input', {bubbles: true}))", px)
        time.sleep(0.7)
        return d.js("return +document.querySelector('.slider').value")

    def measure_grid(label):
        d.wait("return document.querySelectorAll('.vgrid .card').length > 0")
        d.wait("return [...document.querySelectorAll('.card img')]"
               ".filter(i => i.getBoundingClientRect().top < innerHeight).every(i => i.complete)")
        time.sleep(0.8)
        print(f"grid {label}:", d.js(grid), "| cover sizes fetched:", d.js(srcs))

    measure_grid("before")
    d.open_setup("Appearance")
    d.wait("return !!document.querySelector('.slider')")
    time.sleep(0.8)
    print("default:", d.js("return +document.querySelector('.slider').value"), "|", d.js(readout),
          "| preview:", d.js(preview), "| Default disabled:",
          d.js("return [...document.querySelectorAll('.sizing button')].find(b => b.textContent.includes('Default')).disabled"))
    d.shot(out / f"10a-cover-size-default-{tag}.png")

    # Arrow keys move it a step at a time.
    el = d.s("POST", "/element", {"using": "css selector", "value": ".slider"})
    d.s("POST", f"/element/{el[ELEMENT]}/value", {"text": "\ue014\ue014"})
    time.sleep(0.4)
    print("after two right arrows:", d.js("return +document.querySelector('.slider').value"),
          "| changes:", d.js(summary))

    for px in (120, 288):
        got = set_size(px)
        print(f"size {got}:", d.js(readout), "| preview:", d.js(preview), "| changes:", d.js(summary))
        d.shot(out / f"10b-cover-size-{got}-{tag}.png")

    if not save:
        d.click_text("main", "Revert")
        time.sleep(0.4)
        print("reverted:", d.js("return +document.querySelector('.slider').value"), "| changes:", d.js(summary))
        d.click(".setup .back")
        measure_grid("after revert")
        return

    set_size(240)
    predicted = d.js(readout)
    d.click_text("main", "Save")
    d.wait("return [...document.querySelectorAll('.toast')].some(t => t.textContent.includes('Appearance saved'))")
    print("saved:", requests_json(d, "/api/appearance")["setup"])
    d.click(".setup .back")
    print("predicted:", predicted)
    measure_grid("at 240")
    d.shot(out / f"10c-grid-240-{tag}.png")

    # Phones get covers a fifth smaller than chosen.
    d.resize(500, 900)
    time.sleep(1.0)
    measure_grid("at 240, 500 px window")
    d.shot(out / f"10d-grid-240-narrow-{tag}.png")
    d.resize(1440, 1000)
    time.sleep(1.0)

    # A reload lays the grid out at the saved size straight away.
    d.go(d.js("return location.href"))
    measure_grid("after reload")

    d.open_setup("Appearance")
    d.wait("return !!document.querySelector('.slider')")
    time.sleep(0.6)
    d.click_text("main", "Default")
    time.sleep(0.3)
    d.click_text("main", "Save")
    d.wait("return [...document.querySelectorAll('.toast')].filter(t => t.textContent.includes('Appearance saved')).length >= 1")
    print("restored:", requests_json(d, "/api/appearance")["setup"])
    d.click(".setup .back")
    measure_grid("after restoring")


def attention_flow(d, out, tag):
    """The Needs attention view: its banner, and "Show all albums" going back to the sort before."""
    before = d.js("return document.querySelector('.sort-btn .label').textContent.trim()")
    d.click_sort("Needs attention")
    d.wait("return !!document.querySelector('.attention-note')")
    time.sleep(0.8)
    print("attention banner:", d.js("return document.querySelector('.attention-note').textContent.replace(/\\s+/g, ' ').trim()"),
          "| cards:", d.js("return document.querySelectorAll('.card').length"),
          "| sort button:", d.js("return document.querySelector('.sort-btn').textContent.replace(/\\s+/g, ' ').trim()"))
    d.shot(out / f"1g-attention-{tag}.png")
    d.click_text("main", "Show all albums")
    d.wait("return !document.querySelector('.attention-note')")
    time.sleep(0.6)
    after = d.js("return document.querySelector('.sort-btn .label').textContent.trim()")
    print("show all albums:", d.js("return document.querySelectorAll('.card').length"), "cards | sort back to:", after,
          "" if after == before else f"(expected {before})")


def phone_flow(d, out, tag):
    """Phone width: the compact sort button, the sidebar drawer, and the save bar docked at the bottom."""
    d.resize(390, 844)
    time.sleep(0.8)
    print("phone width:", d.js("return innerWidth"),
          "| sort label hidden:", d.js("return getComputedStyle(document.querySelector('.sort-btn .label')).display === 'none'"),
          "| page overflows:", d.js("return document.documentElement.scrollWidth > document.documentElement.clientWidth"))
    d.shot(out / f"1h-phone-grid-{tag}.png")
    d.open_setup("Appearance")
    d.wait("return !!document.querySelector('.cover-preview')")
    time.sleep(0.8)
    # An unsaved change docks the save bar along the bottom edge.
    d.js("document.querySelector('.chips .chip:not(.on)').click()")
    d.wait("return !!document.querySelector('.savebar')")
    time.sleep(0.6)
    print("phone save bar:", d.js("const el = document.querySelector('.savebar'); const r = el.getBoundingClientRect();"
                                  "return `${getComputedStyle(el).position}, ${Math.round(innerHeight - r.bottom)}px from the bottom, ${Math.round(r.width)}px wide`"),
          "| overflows:", d.js("return document.documentElement.scrollWidth > document.documentElement.clientWidth"))
    d.shot(out / f"1i-phone-savebar-{tag}.png")
    d.click_text("main", "Revert")
    time.sleep(0.4)
    d.resize(1440, 1000)
    d.click_text("header", "Inbox")
    d.wait("return document.querySelectorAll('.card').length > 0")
    time.sleep(0.6)


def requests_json(d, path):
    """GET an API path from the page under test, as the browser would."""
    return d.js("const r = new XMLHttpRequest(); r.open('GET', new URL(arguments[0].slice(1), document.baseURI), false);"
                "r.send(); return JSON.parse(r.responseText)", path)


def autofill_flow(d, out, tag, save):
    """Inbox setup's autofill card, and its effect on the Needs attention count."""
    # The sort menu shows the count, but it's out of reach on setup screens: ask the daemon.
    attention = ("const r = new XMLHttpRequest(); r.open('GET', new URL('api/albums?collection=inbox', document.baseURI), false);"
                 "r.send(); const n = String(JSON.parse(r.responseText).filter(a => a.attention).length);")
    count = attention + " return n"
    switch = "//button[@role='switch'][.//span[contains(@class, 'label')][normalize-space(.)='{}']]"
    summary = "return document.querySelector('.savebar .summary')?.textContent.trim()"
    d.js("document.getElementById('autofill-title').scrollIntoView({block: 'start'})")
    time.sleep(0.8)
    d.shot(out / f"1j-autofill-{tag}.png")
    before = d.js(count)
    d.click_xpath(switch.format("Track number from the file name"))
    d.click_xpath(switch.format("Album artist from artist"))
    time.sleep(0.5)
    print("autofill switches:", d.js("return document.querySelectorAll('button[role=switch]').length"),
          "| various disabled:", d.js("return [...document.querySelectorAll('button[role=switch]')]"
                                      ".find(b => b.textContent.includes('Various Artists')).disabled"),
          "| savebar:", d.js(summary))
    d.shot(out / f"1k-autofill-off-{tag}.png")
    if not save:
        d.click_text("main", "Revert")
        return
    saved = "return [...document.querySelectorAll('.toast')].filter(t => t.textContent.includes('Inbox setup saved')).length"
    d.click_text("main", "Save")
    d.wait(saved + " > 0")
    d.wait(attention + f" return n !== '{before}'")
    off = d.js(count)
    d.click_xpath(switch.format("Track number from the file name"))
    d.click_xpath(switch.format("Album artist from artist"))
    d.click_text("main", "Save")
    d.wait(attention + f" return n === '{before}'")
    print("needs attention: autofill on", before, "| off", off, "| on again", d.js(count))


def import_flow(d, out, tag, library_folder):
    """Scratch libraries only: set up a library folder and import two albums."""
    text = "return document.querySelector(arguments[0])?.textContent.replace(/\\s+/g, ' ').trim()"
    d.open_setup("Library setup")
    d.wait("return !!document.querySelector('#library-folder')")
    d.clear_and_type("#library-folder", library_folder)
    d.wait("return /created on the first import|Ready for imports/.test(document.querySelector('.folder-field').textContent)")
    print("folder verdict:", d.js(text, ".folder-field .verdict"))
    d.click(".suggestion:nth-child(2)")
    d.wait("return !!document.querySelector('.example .path')")
    time.sleep(0.4)
    print("structure preview:", d.js(text, ".example .to"))
    d.shot(out / f"8a-library-location-{tag}.png")
    d.click_text("main", "Save")
    d.wait("return [...document.querySelectorAll('.toast')].some(t => t.textContent.includes('Library setup saved'))")

    d.click_text("header", "Inbox")
    d.wait("return document.querySelectorAll('.card').length > 0")
    time.sleep(0.6)
    titles = d.js("return [...document.querySelectorAll('.card .title')].map(t => t.textContent)")
    print("inbox:", titles)
    for i in range(1, len(titles) + 1):
        d.right_click(f".cell:nth-child(1) .cover")
        d.wait("return !!document.querySelector('[role=menu]')")
        if i == 1:
            time.sleep(0.3)
            d.shot(out / f"8b-context-menu-{tag}.png")
        album = d.js(text, "[role=menu] .title")
        d.click_text("*[@role='menu']", "Import to Library")
        d.wait("return !!document.querySelector('.dialog') || [...document.querySelectorAll('.toast')].some(t => /Imported|failed|Choose/.test(t.textContent))", 20)
        if d.js("return !!document.querySelector('.dialog')"):
            time.sleep(0.8)
            print(f"editor for {album}:", d.js(text, ".status-text"),
                  "| missing inputs:", d.js("return document.querySelectorAll('.dialog input.missing').length"))
            d.shot(out / f"8c-import-editor-{tag}.png")
            for el in d.js("return [...document.querySelectorAll('.dialog input.missing')].map((e, i) => i)"):
                d.type(".dialog input.missing", "Maltine Records")
            time.sleep(0.9)
            print("after filling:", d.js(text, ".status-text"), "| dest:", d.js(text, ".dest-path"))
            d.shot(out / f"8d-import-ready-{tag}.png")
            d.click(".dialog .btn.primary")
        d.wait("return [...document.querySelectorAll('.toast')].some(t => t.textContent.includes('Imported'))", 20)
        print("toasts:", [t for t in d.toasts() if 'Imported' in t][-1:])
        time.sleep(0.8)
    d.click_text("header", "Library")
    d.wait("return document.querySelectorAll('.card').length > 0")
    time.sleep(1.0)
    print("library:", d.js("return [...document.querySelectorAll('.card')].map(c => c.querySelector('.title').textContent + ' / ' + c.querySelector('.artist').textContent)"))
    d.shot(out / f"8e-library-{tag}.png")


def art_flow(d, out, tag, library_folder):
    """Scratch libraries only: require album art, then import an album without any."""
    text = "return document.querySelector(arguments[0])?.textContent.replace(/\\s+/g, ' ').trim()"
    d.open_setup("Library setup")
    d.wait("return !!document.querySelector('#library-folder')")
    d.clear_and_type("#library-folder", library_folder)
    d.wait("return /created on the first import|Ready for imports/.test(document.querySelector('.folder-field').textContent)")
    d.js("[...document.querySelectorAll('h3')].find(h => h.textContent === 'Album art').scrollIntoView({block: 'center'})")
    time.sleep(0.6)
    d.shot(out / f"9a-art-rule-{tag}.png")
    require = "//button[@role='switch'][.//span[contains(@class, 'label')][normalize-space(.)='Require album art']]"
    if not d.js("return document.evaluate(arguments[0], document, null, 9, null).singleNodeValue.getAttribute('aria-checked') === 'true'", require):
        d.click_xpath(require)
    placement = "//label[contains(@class, 'placement')][.//span[normalize-space(.)='{}']]"
    d.click_xpath(placement.format("Embedded"))
    d.wait("return !document.querySelector('.name-input')")
    time.sleep(0.4)
    d.shot(out / f"9b-art-embedded-{tag}.png")
    d.click_xpath(placement.format("Both"))
    d.wait("return !!document.querySelector('.name-input input')")
    d.clear_and_type(".name-input input", "folder.JPG")
    time.sleep(0.3)
    print("art hint:", d.js(text, ".name-hint"), "| selected chip:", d.js(text, ".name-suggestions .on"))
    d.clear_and_type(".name-input input", "art/x")
    print("bad name:", d.js(text, ".name-hint"))
    d.click_xpath("//div[contains(@class, 'name-suggestions')]/button[normalize-space(.)='cover']")
    time.sleep(0.5)
    print("art description:", d.js(text, ".placement.on .placement-desc"), "| savebar:", d.js(text, ".savebar .summary"))
    d.shot(out / f"9c-art-both-{tag}.png")
    d.click_text("main", "Save")
    d.wait("return [...document.querySelectorAll('.toast')].some(t => t.textContent.includes('Library setup saved'))")

    d.click_text("header", "Inbox")
    d.wait("return document.querySelectorAll('.card').length > 0")
    d.click_sort("Needs attention")
    d.wait("return !!document.querySelector('.card .flag')")
    time.sleep(0.8)
    print("needs attention:", d.js("return [...document.querySelectorAll('.card')].map(c => c.querySelector('.title').textContent + ' [' + c.querySelector('.flag')?.textContent.trim() + ']')"))
    d.shot(out / f"9d-attention-art-{tag}.png")

    index = d.js("return [...document.querySelectorAll('.cell')].findIndex(c => c.querySelector('.title')?.textContent === 'No Art') + 1")
    d.right_click(f".cell:nth-child({index}) .cover")
    d.wait("return !!document.querySelector('[role=menu]')")
    d.click_text("*[@role='menu']", "Import to Library")
    d.wait("return !!document.querySelector('.art-drop')", 20)
    time.sleep(0.8)
    print("dialog:", d.js(text, ".status-text"), "| import enabled:", d.js("return !document.querySelector('.dialog .btn.primary').disabled"))
    d.shot(out / f"9e-import-needs-art-{tag}.png")
    # Drop a generated image, as dragging a file from the desktop would.
    d.s("POST", "/execute/async", {"args": [], "script": """
        const done = arguments[arguments.length - 1];
        const c = document.createElement('canvas'); c.width = c.height = 400;
        const g = c.getContext('2d'); const grad = g.createLinearGradient(0, 0, 400, 400);
        grad.addColorStop(0, '#ff3d7f'); grad.addColorStop(1, '#3d5afe');
        g.fillStyle = grad; g.fillRect(0, 0, 400, 400);
        c.toBlob(blob => {
            const dt = new DataTransfer();
            dt.items.add(new File([blob], 'chosen.png', {type: 'image/png'}));
            document.querySelector('.art-drop').dispatchEvent(new DragEvent('drop', {dataTransfer: dt, bubbles: true, cancelable: true}));
            done();
        }, 'image/png');"""})
    d.wait("return !!document.querySelector('.art-preview')")
    time.sleep(0.8)
    print("after drop:", d.js(text, ".status-text"), "|", d.js(text, ".art-text"), "| note:", d.js(text, ".autofilled"),
          "| import enabled:", d.js("return !document.querySelector('.dialog .btn.primary').disabled"))
    d.shot(out / f"9f-import-art-chosen-{tag}.png")
    d.click(".dialog .btn.primary")
    d.wait("return [...document.querySelectorAll('.toast')].some(t => t.textContent.includes('Imported'))", 20)
    print("toast:", [t for t in d.toasts() if 'Imported' in t][-1:])
    time.sleep(1.0)
    print("still flagged:", d.js("return [...document.querySelectorAll('.card .title')].map(t => t.textContent)"))


SUGGEST_TEXT = "return document.querySelector(arguments[0])?.textContent.replace(/\\s+/g, ' ').trim()"


def open_import(d, title):
    """Right-click an Inbox album found by searching, and choose Import to Library."""
    d.click(".setup .back") if d.js("return !!document.querySelector('.setup .back')") else None
    d.clear_and_type(".search input", title)
    d.wait("return document.querySelectorAll('.vgrid .card').length > 0")
    time.sleep(0.8)
    index = d.js("return [...document.querySelectorAll('.cell')].findIndex(c => c.querySelector('.title')?.textContent === arguments[0]) + 1", title)
    d.right_click(f".cell:nth-child({index}) .cover")
    d.wait("return !!document.querySelector('[role=menu]')")
    d.click_text("*[@role='menu']", "Import to Library")
    d.wait("return !!document.querySelector('.suggest')", 20)


def wait_suggestions(d):
    d.wait("return !!document.querySelector('.suggest .match') || !!document.querySelector('.suggest .error')", 60)
    time.sleep(0.8)


def describe_suggestions(d, label, rows=8):
    text = SUGGEST_TEXT
    print(f"{label}:", d.js(text, ".suggest .match"), "|", d.js(text, ".suggest .facts"))
    print("   toolbar:", d.js(text, ".suggest .toolbar"), "| script:", d.js(text, ".suggest .script"), "| notes:",
          d.js("return [...document.querySelectorAll('.suggest .note')].map(n => n.textContent.replace(/\\s+/g, ' ').trim())"))
    print("   rows:", d.js("return [...document.querySelectorAll('.suggest .sugg')].slice(0, arguments[0]).map(r => (r.querySelector('input').checked ? '[x] ' : '[ ] ') + r.textContent.replace(/\\s+/g, ' ').trim())", rows))


def api_call(url, token=None, body=None):
    """An app's request: the status and the JSON answer (None if there isn't one)."""
    headers = {"content-type": "application/json"}
    if token:
        headers["authorization"] = f"Bearer {token}"
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, headers=headers, method="POST" if data else "GET")
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            status, raw = r.status, r.read()
    except urllib.error.HTTPError as e:
        status, raw = e.code, e.read()
    try:
        return status, json.loads(raw)
    except ValueError:
        return status, None


def devices_flow(d, out, tag, url):
    """Pair a device from Sync setup, use its token, and revoke it there.
    Stops at the first failed check."""
    text = "return document.querySelector(arguments[0])?.textContent.replace(/\\s+/g, ' ').trim()"
    name = "Smoke test phone"
    row = f"//li[contains(@class,'device')][.//b[normalize-space(.)='{name}']]"
    has = "return document.evaluate(arguments[0], document, null, XPathResult.BOOLEAN_TYPE, null).booleanValue"
    base = url if url.endswith("/") else url + "/"

    d.open_setup("Sync setup")
    d.wait("return !!document.querySelector('#devices-title')")
    time.sleep(0.8)
    d.shot(out / f"10a-devices-{tag}.png")
    d.click_text("main", "Add device")
    d.wait("return document.activeElement === document.querySelector('.dialog .field input')")
    d.type(".dialog .field input", name)
    time.sleep(0.5)
    d.shot(out / f"10b-add-device-{tag}.png")
    d.click_text("div[@role='dialog']", "Show code")
    d.wait("return !!document.querySelector('.dialog .code')")
    code = d.js(text, ".dialog .code")
    check(re.fullmatch(r"[0-9A-HJKMNP-TV-Z]{4}-[0-9A-HJKMNP-TV-Z]{4}", code or ""), f"code shown: {code}")
    check(d.js("return (document.querySelector('.dialog .qr path')?.getAttribute('d') ?? '').length > 100"),
          "QR code drawn")
    check(d.js(text, ".dialog .server code") == base, f"server address: {base}")
    check_pairing_link(d)
    time.sleep(0.6)
    d.shot(out / f"10c-code-{tag}.png")

    # The app's side: the code as typed, in lowercase.
    status, paired = api_call(base + "api/pair", body={
        "code": code.lower(), "platform": "android", "app_version": "0.1.0", "hostname": "smoke-phone"})
    check(status == 200 and paired["token"].startswith("cst_"), f"/api/pair: {status}")
    token = paired["token"]
    check(api_call(base + "api/status", token)[0] == 200, "the token opens /api/status")
    d.wait("return !!document.querySelector('.dialog .result h3')")
    shown = d.js(text, ".dialog .result")
    check("is paired" in shown and "smoke-phone" in shown, f"the dialog shows it: {shown}")
    time.sleep(0.4)
    d.shot(out / f"10d-paired-{tag}.png")
    d.click_text("div[@role='dialog']", "Done")
    d.wait("return !document.querySelector('.dialog')")
    d.wait(has, 15.0, row)
    time.sleep(0.8)
    print("listed:", d.js("return [...document.querySelectorAll('.device .info')]"
                          ".map(i => i.textContent.replace(/\\s+/g, ' ').trim())"))
    d.shot(out / f"10e-device-list-{tag}.png")

    d.click_xpath(row + "//button[normalize-space(.)='Revoke']")
    d.wait("return !!document.querySelector('.device .confirm')")
    d.click_xpath(row + "//div[contains(@class,'confirm')]//button[normalize-space(.)='Revoke']")
    d.wait(has, 15.0, row + "//span[normalize-space(.)='Revoked']")
    status, body = api_call(base + "api/status", token)
    check(status == 401 and body["code"] == "invalid_token", f"revoked: /api/status answers {status}")
    time.sleep(0.6)
    d.shot(out / f"10f-revoked-{tag}.png")
    d.click_xpath(row + "//button[normalize-space(.)='Remove']")
    d.click_xpath(row + "//div[contains(@class,'confirm')]//button[normalize-space(.)='Remove']")
    d.wait(f"return !({has.removeprefix('return ')})", 15.0, row)
    check(True, "removed from the list")
    add_device_keyboard(d)
    add_device_new_code(d, out, tag, base)
    device_edits_keep_newer_ones(d)
    pairing_setup_follows_saves_elsewhere(d)
    pairing_save_keeps_later_edits(d)


def pairing_setup_follows_saves_elsewhere(d):
    """Sync setup's Pairing, while another browser saves it: a field left as
    it was follows the save, and one being edited keeps its draft."""
    switch = ("return [...document.querySelectorAll('main [role=switch]')]"
              ".find(s => s.textContent.includes('plain http'))?.getAttribute('aria-checked')")
    save_elsewhere = """
        const done = arguments[1];
        fetch('api/pairing/setup', {method: 'PUT', headers: {'content-type': 'application/json'},
            body: JSON.stringify({ca_path: null, allow_http: arguments[0]})})
            .then(r => done(r.status), e => done(String(e)));
    """
    d.wait(f"{switch} === 'false'")
    d.clear_and_type("#ca-path", "~/draft.pem")
    status = d.s("POST", "/execute/async", {"args": [True], "script": save_elsewhere})
    check(status == 200, f"saved elsewhere: {status}")
    # Through the event stream, or the poll while it's down.
    d.wait(f"{switch} === 'true'", 20.0)
    check(d.js("return document.querySelector('#ca-path').value") == "~/draft.pem",
          "the switch followed the save made elsewhere; the CA path draft stayed")
    d.s("POST", "/execute/async", {"args": [False], "script": save_elsewhere})
    d.wait(f"{switch} === 'false'", 20.0)
    d.click_text("main", "Revert")
    d.wait("return document.querySelector('#ca-path').value === ''")


def device_edits_keep_newer_ones(d):
    """A slow save of one device's name doesn't close another device's editor
    opened meanwhile."""
    # Two devices, paired from this machine as an app would.
    d.s("POST", "/execute/async", {"args": [], "script": """
        const done = arguments[0];
        (async () => {
            for (const name of ['Edit A', 'Edit B']) {
                const made = await (await fetch('api/pairings', {method: 'POST',
                    headers: {'content-type': 'application/json'},
                    body: JSON.stringify({name, scope: 'manage'})})).json();
                await fetch('api/pair', {method: 'POST', headers: {'content-type': 'application/json'},
                    body: JSON.stringify({code: made.code})});
                await fetch('api/pairings/' + made.id, {method: 'DELETE'});
            }
        })().then(() => done(true), e => done(String(e)));
    """})
    # Load the list afresh.
    d.click(".setup .back")
    d.wait("return !document.querySelector('#devices-title')")
    d.open_setup("Sync setup")
    row = lambda n: f"//li[contains(@class,'device')][.//b[normalize-space(.)='{n}']]"
    has = "return document.evaluate(arguments[0], document, null, XPathResult.BOOLEAN_TYPE, null).booleanValue"
    d.wait(has, 15.0, row("Edit B"))
    # Device saves answer a second late.
    d.js("const f = window.fetch; window.fetch = async (u, i) => {"
         " const r = await f(u, i);"
         " if (i?.method === 'PUT' && String(u).includes('/api/devices/')) await new Promise(t => setTimeout(t, 1000));"
         " return r; }")
    d.click_xpath(row("Edit A") + "//button[normalize-space(.)='Edit']")
    d.wait("return !!document.querySelector('.device .edit input')")
    d.clear_and_type(".device .edit input", "Edit A2")
    d.click_xpath("//li[contains(@class,'device')]//form[contains(@class,'edit')]//button[normalize-space(.)='Save']")
    d.click_xpath(row("Edit B") + "//button[normalize-space(.)='Edit']")
    d.wait(has, 15.0, row("Edit A2"))
    time.sleep(0.3)
    check(d.js("return document.querySelector('.device .edit input')?.value") == "Edit B",
          "B's editor stayed open when A's save answered")
    d.click_xpath("//form[contains(@class,'edit')]//button[normalize-space(.)='Cancel']")
    for name in ["Edit A2", "Edit B"]:
        d.click_xpath(row(name) + "//button[normalize-space(.)='Revoke']")
        d.click_xpath(row(name) + "//div[contains(@class,'confirm')]//button[normalize-space(.)='Revoke']")
        d.wait(has, 15.0, row(name) + "//span[normalize-space(.)='Revoked']")
        d.click_xpath(row(name) + "//button[normalize-space(.)='Remove']")
        d.click_xpath(row(name) + "//div[contains(@class,'confirm')]//button[normalize-space(.)='Remove']")
        d.wait(f"return !({has.removeprefix('return ')})", 15.0, row(name))
    check(True, "both removed")


def check_pairing_link(d):
    """The link in Add device's QR code: the page's own address up to its
    path prefix, the code, and ca= when Sync setup has a CA certificate."""
    link = d.js("return document.querySelector('.dialog [data-link]')?.dataset.link")
    want = d.s("POST", "/execute/async", {"args": [], "script": """
        const done = arguments[0];
        fetch('api/pairing/setup').then(r => r.json()).then(s => {
            const server = location.origin + location.pathname.replace(/[^/]*$/, '');
            const code = document.querySelector('.dialog .code').textContent.trim().replace('-', '');
            const ca = s.ca ? s.ca.fingerprint : null;
            done({server, ca, link: 'coversync://pair?server=' + encodeURIComponent(server)
                + '&code=' + code + (ca ? '&ca=' + ca : '')});
        }, e => done({error: String(e)}));
    """})
    check(link == want.get("link"), f"the pairing link: {link}")
    return want


def pairing_link_flow(d):
    d.open_setup("Sync setup")
    d.wait("return !!document.querySelector('#devices-title')")
    d.click_text("main", "Add device")
    d.wait("return document.activeElement === document.querySelector('.dialog .field input')")
    d.type(".dialog .field input", "Link check")
    d.click_text("div[@role='dialog']", "Show code")
    d.wait("return !!document.querySelector('.dialog .code')")
    want = check_pairing_link(d)
    print("server in the link:", want["server"], "| ca=:", want["ca"] or "none (no CA certificate set)")
    press(d, ESCAPE)
    d.wait("return !document.querySelector('.dialog')")


def pairing_save_keeps_later_edits(d):
    """A pairing switch put back to its saved value while a save of it is
    under way stays as it was put, and is left to save."""
    switch_el = ("[...document.querySelectorAll('main [role=switch]')]"
                 ".find(s => s.textContent.includes('plain http'))")
    switch = f"return {switch_el}?.getAttribute('aria-checked')"
    d.wait(f"{switch} === 'false'")
    # Pairing setup saves answer a second late.
    d.js("const f = window.fetch; window.fetch = async (u, i) => {"
         " const r = await f(u, i);"
         " if (i?.method === 'PUT' && String(u).includes('/api/pairing/setup')) await new Promise(t => setTimeout(t, 1000));"
         " return r; }")
    d.js(f"{switch_el}.click()")
    d.wait(f"{switch} === 'true'")
    d.click_text("main", "Save")
    d.js(f"{switch_el}.click()")
    d.wait(f"{switch} === 'false'")
    d.wait("return [...document.querySelectorAll('.toast')].some(t => t.textContent.includes('Sync setup saved'))")
    time.sleep(0.3)
    summary = "return document.querySelector('.savebar .summary')?.textContent.trim() ?? ''"
    check(d.js(switch) == "false" and "1" in d.js(summary),
          f"put back during the save, it stayed off, unsaved: {d.js(summary)!r}")
    d.click_text("main", "Save")
    d.wait(f"return !document.querySelector('.savebar .summary')", 15.0)
    check(d.js(switch) == "false", "saved off again")


def press(d, key):
    """Press a key (a WebDriver key code, such as Tab) wherever focus is."""
    d.s("POST", "/actions", {"actions": [{
        "type": "key", "id": "keyboard",
        "actions": [{"type": "keyDown", "value": key}, {"type": "keyUp", "value": key}],
    }]})
    d.s("DELETE", "/actions")


TAB, ENTER, ESCAPE = "", "", ""


def add_device_keyboard(d):
    """Add device by keyboard: focus starts in the name, stays in the dialog
    through each step and around Tab's ends, and Escape closes only the
    dialog, giving focus back to Add device."""
    focus_in_dialog = "return !!document.activeElement?.closest('.dialog')"
    d.click_text("main", "Add device")
    d.wait("return document.activeElement === document.querySelector('.dialog .field input')")
    d.type(".dialog .field input", "Keyboard phone")
    press(d, ENTER)
    d.wait("return !!document.querySelector('.dialog .code')")
    d.wait(focus_in_dialog)
    check(d.js("return document.activeElement.textContent.trim()") == "Copy link",
          "after Show code, focus is on Copy link")
    # Past the last control (Cancel) Tab comes back to the first (Close), and back.
    d.js("[...document.querySelectorAll('.dialog .foot button')].pop().focus()")
    press(d, TAB)
    check(d.js("return document.activeElement.getAttribute('aria-label')") == "Close",
          "Tab past the last control wraps to the first")
    d.s("POST", "/actions", {"actions": [{
        "type": "key", "id": "keyboard",
        "actions": [{"type": "keyDown", "value": ""}, {"type": "keyDown", "value": TAB},
                    {"type": "keyUp", "value": TAB}, {"type": "keyUp", "value": ""}],
    }]})
    d.s("DELETE", "/actions")
    check(d.js("return document.activeElement.textContent.trim()") == "Cancel",
          "Shift+Tab before the first wraps to the last")
    # Even with focus lost to the page, Escape closes the dialog and nothing else.
    d.js("document.activeElement.blur()")
    press(d, ESCAPE)
    d.wait("return !document.querySelector('.dialog')")
    check("view=sync-setup" in d.js("return location.search"), "Escape left Sync setup open")
    check(d.js("return document.activeElement?.textContent.trim()") == "Add device",
          "focus is back on Add device")


def add_device_new_code(d, out, tag, base):
    """New code once the device to replace is removed: the dialog says why the
    code ended, a double click asks for one code only, and its failure shows."""
    text = "return document.querySelector(arguments[0])?.textContent.replace(/\\s+/g, ' ').trim()"
    # A device to replace, paired as an app would.
    d.click_text("main", "Add device")
    d.wait("return document.activeElement === document.querySelector('.dialog .field input')")
    d.type(".dialog .field input", "Old laptop")
    d.click_text("div[@role='dialog']", "Show code")
    d.wait("return !!document.querySelector('.dialog .code')")
    status, paired = api_call(base + "api/pair", body={"code": d.js(text, ".dialog .code")})
    check(status == 200, "a device to replace paired")
    d.wait("return !!document.querySelector('.dialog .result h3')")
    d.click_text("div[@role='dialog']", "Done")
    d.wait("return !document.querySelector('.dialog')")
    d.wait("return [...document.querySelectorAll('.device .name b')].some(b => b.textContent === 'Old laptop')")

    # Count the codes asked for from here on.
    d.js("window.__codes = 0; const f = window.fetch;"
         "window.fetch = (u, i) => { if (String(u).endsWith('/api/pairings') && i?.method === 'POST')"
         " window.__codes++; return f(u, i); }")
    d.click_text("main", "Add device")
    d.wait("return !!document.querySelector('.dialog .check input')")
    d.click(".dialog .check input")
    d.wait("return !!document.querySelector('.dialog select')")
    d.js("const s = document.querySelector('.dialog select');"
         "s.value = [...s.options].find(o => o.text.startsWith('Old laptop')).value;"
         "s.dispatchEvent(new Event('change', {bubbles: true}))")
    d.click_text("div[@role='dialog']", "Show code")
    d.wait("return !!document.querySelector('.dialog .code')")
    # Removed elsewhere (another tab) while its replacement code waits.
    d.js("fetch(arguments[0], {method: 'DELETE'})", f"{base}api/devices/{paired['device_id']}")
    d.wait("return !!document.querySelector('.dialog .result h3')", 15.0)
    shown = d.js(text, ".dialog .result")
    check("would replace was removed" in shown, f"the dialog says why: {shown}")
    d.wait("return ![...document.querySelectorAll('.sidebar .device-name')]"
           ".some(n => n.textContent === 'Old laptop')")
    check(True, "the sidebar no longer lists it")
    # Double-clicked: one request. It's refused, and the form comes back with
    # why, the device to replace cleared, to choose another or none.
    d.js("const b = document.querySelector('.dialog .new-code'); b.click(); b.click()")
    d.wait("return !!document.querySelector('.dialog form .error')")
    time.sleep(0.5)
    check(d.js("return window.__codes") == 2, f"codes asked for: {d.js('return window.__codes')} (2 expected)")
    print("error shown:", d.js(text, ".dialog form .error"))
    check(d.js("return !document.querySelector('.dialog .check input')?.checked"),
          "the device to replace is cleared, and Replace unticked")
    d.shot(out / f"10g-new-code-failed-{tag}.png")
    # As a new device, a code comes.
    d.click_text("div[@role='dialog']", "Show code")
    d.wait("return !!document.querySelector('.dialog .code')")
    check(True, "a new code once Replace is unticked")
    press(d, ESCAPE)
    d.wait("return !document.querySelector('.dialog')")


def option(name):
    """The value given after a flag, e.g. the album after --suggest."""
    if name in sys.argv and sys.argv.index(name) + 1 < len(sys.argv):
        return sys.argv[sys.argv.index(name) + 1]
    raise SystemExit(f"{name} needs a value; see the usage at the top of this file.")


def password():
    if "--password" in sys.argv:
        return option("--password")
    if os.environ.get("COVERSYNC_PASSWORD"):
        return os.environ["COVERSYNC_PASSWORD"]
    raise SystemExit("The UI asks for a password: set COVERSYNC_PASSWORD or pass --password.")


LOGIN_FIELD = ".login input[type=password]"


def sign_in(d, pw=None):
    """Sign in on the login view if it's up, and wait for the app."""
    d.wait("return !!document.querySelector('.login, .page')")
    if not d.js(f"return !!document.querySelector('{LOGIN_FIELD}')"):
        if d.js("return !!document.querySelector('.login')"):
            raise SystemExit("The server has no password yet: run coversyncd set-password.")
        return
    d.type(LOGIN_FIELD, pw or password())
    d.click(".login button[type=submit]")
    d.wait("return !document.querySelector('.login') && !!document.querySelector('.page')")


def check(ok, what):
    """Stop the run unless `ok`; say what was checked."""
    if not ok:
        raise AssertionError(what)
    print("  ok:", what)


def auth_flow(d, out, tag):
    """The login view: a wrong password, signing in, a session that ends under
    an album with unsaved tag edits (Escape in the login view must not close
    it), signing back in, and signing out (asked first while a setup screen
    has unsaved changes). Starts signed out; stops at the first failed check."""
    text = "return document.querySelector(arguments[0])?.textContent.replace(/\\s+/g, ' ').trim()"
    d.wait(f"return !!document.querySelector('{LOGIN_FIELD}')")
    time.sleep(0.6)
    print("login view:", d.js(text, ".login h1"), "|", d.js(text, ".login .card > p"))
    check(d.js(f"return document.activeElement === document.querySelector('{LOGIN_FIELD}')"),
          "the password field has focus")
    d.shot(out / f"a1-login-{tag}.png")
    d.type(LOGIN_FIELD, "not the password")
    d.click(".login button[type=submit]")
    d.wait("return !!document.querySelector('.login .problem')")
    time.sleep(0.4)
    check("isn't right" in d.js(text, ".login .problem"), "a wrong password is refused")
    d.shot(out / f"a2-wrong-{tag}.png")
    d.clear_and_type(LOGIN_FIELD, password())
    d.click(".login button[type=submit]")
    d.wait("return !document.querySelector('.login') && document.querySelectorAll('.card').length > 0")
    d.wait("return [...document.querySelectorAll('.card img')]"
           ".filter(i => i.getBoundingClientRect().top < innerHeight).every(i => i.complete)")
    shown = d.js("return [...document.querySelectorAll('.card img')].filter(i => i.naturalWidth).length")
    check(shown > 0, f"signed in, {shown} covers shown through the cookie")
    d.shot(out / f"a3-signed-in-{tag}.png")

    # An album with unsaved tag edits, then the session ends under it (as when
    # another tab signs out, or the password is reset).
    time.sleep(1.2)
    d.click(".cell:first-child .cover")
    d.wait("return !!document.querySelector('.tracks li')")
    time.sleep(1.2)
    d.click(".mode button:nth-child(3)")
    d.wait("return !!document.querySelector('.editor')")
    field = ".album-grid .field:nth-child(4) input"
    d.type(field, " (edited)")
    time.sleep(0.5)
    summary = "return document.querySelector('.editor .summary')?.textContent.replace(/\\s+/g, ' ').trim()"
    edited = d.js(summary)
    value = d.js("return document.querySelector(arguments[0]).value", field)
    check(value.endswith(" (edited)") and "change" in edited and "No changes" not in edited,
          f"an unsaved tag edit: {edited!r}")
    d.js("fetch(new URL('api/logout', document.baseURI), {method: 'POST'})")
    started = time.time()
    d.wait("return !!document.querySelector('.login.over input[type=password]')", 20)
    print(f"session ended: login view over the app after {time.time() - started:.1f}s")
    check(d.js(text, ".login h1") == "You’ve been signed out", "it says the session ended")
    check(d.js("return !!document.querySelector('.page').closest('[inert]')"), "the app under it is inert")
    check(d.js("return document.querySelectorAll('.toast.error').length") == 0, "no error toasts")
    # Escape typed in the login view stays there.
    el = d.s("POST", "/element", {"using": "css selector", "value": LOGIN_FIELD})
    d.s("POST", f"/element/{el[ELEMENT]}/value", {"text": "\ue00c"})  # Escape
    time.sleep(0.6)
    check(d.js("return !!document.querySelector('.sheet .editor')") and d.js(summary) == edited,
          "Escape in the login view leaves the album and its tag edit open")
    d.shot(out / f"a4-session-ended-{tag}.png")
    sign_in(d)
    time.sleep(0.8)
    check(d.js(summary) == edited and d.js("return document.querySelector(arguments[0]).value", field) == value,
          "signed back in, the tag edit is still there")
    check(d.js("const i = document.querySelector('.hero-cover img'); return !i || (i.complete && i.naturalWidth > 0)"),
          "the album's cover shows")
    # Now Escape closes the album again (dropping the edit, as it always has).
    d.js("window.dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape'}))")
    d.wait("return !document.querySelector('.sheet')")
    time.sleep(0.8)

    # A track whose stream was refused while signed out plays once signed back in.
    d.click(".cell:first-child .cover")
    d.wait("return !!document.querySelector('.tracks li')")
    time.sleep(1.2)
    # The seek below goes 150s in, and playing on is watched for a few more.
    long_track = d.js("return [...document.querySelectorAll('.tracks li .row')].findIndex(row =>"
                      " row.querySelector('.dur').textContent.trim().split(':')"
                      ".reduce((n, part) => n * 60 + Number(part), 0) >= 180)")
    if long_track < 0:
        print("  skip: replaying a refused stream needs a track of 3 minutes or more on this album")
        d.js("window.dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape'}))")
        d.wait("return !document.querySelector('.sheet')")
        time.sleep(0.8)
    else:
        d.js("document.querySelectorAll('.tracks li .row')[arguments[0]].click()", long_track)
        d.wait("return !!document.querySelector('.player')")
        time.sleep(2)
        play = ".player button.main"
        d.click(play)
        d.js("window.dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape'}))")
        d.wait("return !document.querySelector('.sheet')")
        time.sleep(0.8)
        d.js("fetch(new URL('api/logout', document.baseURI), {method: 'POST'})")
        d.wait("return !!document.querySelector('.login.over input[type=password]')", 20)
        # Seek well ahead under the login view, past what was fetched: refused.
        d.js("const s = document.querySelector('.player [role=slider]');"
             "for (let i = 0; i < 30; i++) s.dispatchEvent(new KeyboardEvent('keydown', {key: 'ArrowRight', bubbles: true}))")
        time.sleep(1.5)
        sign_in(d)
        time.sleep(0.5)

        def position():
            shown = d.js("return document.querySelector('.player .t').textContent").strip()
            return sum(int(p) * 60 ** i for i, p in enumerate(reversed(shown.split(":"))))

        before = position()
        d.click(play)
        time.sleep(3)
        check(d.js(f"return document.querySelector('{play}').getAttribute('aria-label')") == "Pause"
              and position() > before >= 60,
              f"Play after signing back in plays on from {before}s (now {position()}s)")
        d.click(play)

    # Signing out asks first while a setup screen has unsaved changes.
    d.open_setup("Appearance")
    d.wait("return !!document.querySelector('.slider')")
    d.js("const s = document.querySelector('.slider');"
         "s.value = +s.value === +s.min ? s.max : s.min;"
         "s.dispatchEvent(new Event('input', {bubbles: true}))")
    time.sleep(0.6)
    unsaved = "return document.querySelector('.savebar .summary')?.textContent.trim() ?? null"
    before = d.js(unsaved)
    check(before is not None, f"an unsaved Appearance change: {before!r}")
    # The question is answered "no" here, and noted.
    d.js("window.asked = []; window.confirm = (m) => (window.asked.push(m), false)")
    d.click_xpath("//aside//button[normalize-space(.)='Sign out']")
    time.sleep(0.5)
    asked = d.js("return window.asked")
    check(len(asked) == 1 and "unsaved" in asked[0], f"Sign out asks first: {asked!r}")
    check(not d.js("return !!document.querySelector('.login')") and d.js(unsaved) == before,
          "declining keeps the session and the change")
    d.click_text("main", "Revert")
    time.sleep(0.4)

    # With nothing unsaved, Sign out starts the page afresh, signed out.
    d.click_xpath("//aside//button[normalize-space(.)='Sign out']")
    d.wait("return !!document.querySelector('.login:not(.over) input[type=password]')")
    time.sleep(0.6)
    check(d.js("return !document.querySelector('.page')"), "signed out, nothing of the library on screen")
    status = d.js("const r = new XMLHttpRequest(); r.open('GET', new URL('api/status', document.baseURI), false);"
                  "r.send(); return r.status")
    check(status == 401, "the API answers 401 afterwards")
    d.shot(out / f"a5-signed-out-{tag}.png")


def romanize_flow(d, out, tag, album):
    """Scratch daemons with Prefer English on: romanized suggestions, the script switch, and an import."""
    d.open_setup("Inbox setup")
    d.wait("return !!document.querySelector('#suggest-title')")
    d.js("document.querySelector('#suggest-title').scrollIntoView({block: 'start'})")
    time.sleep(0.6)
    print("prefer english:", d.js("return [...document.querySelectorAll('[role=switch]')].filter(b => /Prefer English|Suggest tags/.test(b.textContent)).map(b => b.querySelector('.label').textContent + '=' + b.getAttribute('aria-checked'))"))
    d.shot(out / f"12a-prefer-english-setup-{tag}.png")
    open_import(d, album)
    wait_suggestions(d)
    describe_suggestions(d, "romanized", 12)
    d.shot(out / f"12b-romanized-{tag}.png")
    d.click_xpath("//div[contains(@class,'segmented')]/button[normalize-space(.)='Original script']")
    d.wait("return document.querySelector('.segmented .on')?.textContent.trim() === 'Original script'", 30)
    time.sleep(0.8)
    describe_suggestions(d, "original script", 6)
    d.shot(out / f"12c-original-script-{tag}.png")
    d.click_xpath("//div[contains(@class,'segmented')]/button[normalize-space(.)='Romanized']")
    d.wait("return document.querySelector('.segmented .on')?.textContent.trim() === 'Romanized' && !!document.querySelector('.suggest .toolbar')", 30)
    time.sleep(0.8)
    describe_suggestions(d, "romanized again", 3)
    print("dest:", d.js(SUGGEST_TEXT, ".dest-path"))
    d.click(".dialog .btn.primary")
    d.wait("return [...document.querySelectorAll('.toast')].some(t => t.textContent.includes('Imported'))", 30)
    print("toast:", [t for t in d.toasts() if 'Imported' in t][-1:])


def lookup_again_flow(d, out, tag, album):
    """An album identified before: the saved lookup, then Look up again, at desktop and phone widths."""
    open_import(d, album)
    wait_suggestions(d)
    describe_suggestions(d, "saved", 3)
    print("   actions:", d.js("return [...document.querySelectorAll('.suggest .head-actions > *')].map(b => b.getAttribute('aria-label') || b.textContent.trim())"))
    d.shot(out / f"13a-saved-lookup-{tag}.png")
    d.resize(390, 844)
    time.sleep(0.6)
    d.shot(out / f"13b-saved-lookup-phone-{tag}.png")
    d.resize(1440, 1000)
    time.sleep(0.4)
    # A ticked suggestion is taken back before looking up again.
    print("ticked:", d.js(SUGGEST_TEXT, ".suggest .toolbar"))
    ticked = d.js("const i = document.querySelector('.suggest .sugg.change input[type=checkbox]:not(:checked):not(:disabled)'); i?.click(); return !!i")
    if ticked:
        time.sleep(0.3)
        print("after ticking a change:", d.js(SUGGEST_TEXT, ".suggest .toolbar"))
    else:
        print("no unticked change to tick")
    d.click(".suggest .head-actions button[aria-label='Look up again']")
    d.wait("return !!document.querySelector('.suggest .stage')", 10)
    print("looking up:", d.js(SUGGEST_TEXT, ".suggest .stage"))
    d.wait("return !!document.querySelector('.suggest .match') || !!document.querySelector('.suggest .error')", 60)
    time.sleep(0.8)
    describe_suggestions(d, "looked up again", 3)
    d.shot(out / f"13c-looked-up-again-{tag}.png")


def suggest_flow(d, out, tag, album, superset=None):
    """Scratch daemons only: Smart suggestions in Inbox setup, then importing with them."""
    text = "return document.querySelector(arguments[0])?.textContent.replace(/\\s+/g, ' ').trim()"
    summary = "return document.querySelector('.savebar .summary')?.textContent.trim() ?? null"
    d.open_setup("Inbox setup")
    d.wait("return !!document.querySelector('#suggest-title')")
    d.js("document.querySelector('#suggest-title').scrollIntoView({block: 'start'})")
    time.sleep(0.6)
    print("key:", d.js(text, ".key-row"), "| fpcalc:", d.js(text, ".fpcalc"))
    d.shot(out / f"11a-suggest-setup-{tag}.png")
    d.click_text("main", "Replace")
    d.wait("return !!document.querySelector('.key-input')")
    d.type(".key-input", "notarealkey")
    time.sleep(0.3)
    print("replacing:", d.js(summary))
    d.shot(out / f"11b-suggest-replace-{tag}.png")
    d.click_text("main", "Keep the saved key")
    time.sleep(0.3)
    d.click_text("main", "Remove")
    time.sleep(0.3)
    print("removing:", d.js(text, ".key-row"), "|", d.js(summary))
    d.click_text("main", "Keep it")
    time.sleep(0.3)
    print("kept:", d.js(text, ".key-row"), "|", d.js(summary))

    open_import(d, album)
    d.wait("return /Fingerprint|Asking|Reading|Fetching/.test(document.querySelector('.suggest .stage')?.textContent ?? '')", 10)
    print("loading:", d.js(text, ".suggest .stage"))
    d.shot(out / f"11c-suggest-loading-{tag}.png")
    d.wait("return !!document.querySelector('.suggest .match') || !!document.querySelector('.suggest .error')", 60)
    time.sleep(0.8)
    describe_suggestions(d, "matched")
    d.shot(out / f"11d-suggest-matched-{tag}.png")
    # Tick a change: the first unticked title row.
    d.js("const r = [...document.querySelectorAll('.suggest .sugg.change')].find(r => !r.querySelector('input').checked); r.querySelector('input').click()")
    time.sleep(0.3)
    print("after ticking a change:", d.js(text, ".suggest .toolbar"))
    d.click_text("*[contains(@class,'suggest')]", "Show all suggestions")
    time.sleep(0.6)
    d.js("document.querySelector('.suggest .more')?.scrollIntoView({block: 'start'})")
    time.sleep(0.4)
    d.shot(out / f"11e-suggest-more-{tag}.png")
    other = d.js("const s = document.querySelector('.suggest select'); return [...s.options].map(o => o.textContent)")
    print("candidates:", other)
    d.js("const s = document.querySelector('.suggest select'); s.value = s.options[1].value; s.dispatchEvent(new Event('change', {bubbles: true}))")
    d.wait("return !!document.querySelector('.suggest .match') && /Picked by you/.test(document.querySelector('.suggest .facts').textContent)", 30)
    time.sleep(0.6)
    describe_suggestions(d, "switched")
    d.shot(out / f"11f-suggest-switched-{tag}.png")
    print("status:", d.js(text, ".status-text"), "| import enabled:", d.js("return !document.querySelector('.dialog .btn.primary').disabled"))
    d.click(".dialog .btn.primary")
    d.wait("return [...document.querySelectorAll('.toast')].some(t => t.textContent.includes('Imported'))", 30)
    print("toast:", [t for t in d.toasts() if 'Imported' in t][-1:])
    time.sleep(1.0)

    if not superset:
        return
    open_import(d, superset)
    d.wait("return !!document.querySelector('.suggest .match') || !!document.querySelector('.suggest .error')", 60)
    time.sleep(0.8)
    describe_suggestions(d, "superset")
    d.shot(out / f"11g-suggest-superset-{tag}.png")
    d.click_text("footer", "Cancel")
    time.sleep(0.5)


def main():
    url, out = sys.argv[1], Path(sys.argv[2])
    dark = "--light" not in sys.argv
    out.mkdir(parents=True, exist_ok=True)
    d = Driver(dark=dark)
    tag = "dark" if dark else "light"
    try:
        d.go(url)
        if "--auth" in sys.argv:
            auth_flow(d, out, tag)
            return
        sign_in(d)
        if "--devices" in sys.argv:
            devices_flow(d, out, tag, url)
            return
        if "--pairing-link" in sys.argv:
            pairing_link_flow(d)
            return
        d.wait("return document.querySelectorAll('.card').length > 0")
        grid_intro_check(d)
        d.wait("return document.querySelectorAll('.card').length > 0")
        # Only covers in the viewport load; the rest are lazy.
        d.wait("return [...document.querySelectorAll('.card img')]"
               ".filter(i => i.getBoundingClientRect().top < innerHeight).every(i => i.complete)")
        time.sleep(1.2)
        print("cards:", d.js("return document.querySelectorAll('.card').length"))
        d.shot(out / f"1-grid-{tag}.png")
        if "--art" in sys.argv:
            art_flow(d, out, tag, option("--art"))
            return
        if "--romanize" in sys.argv:
            romanize_flow(d, out, tag, option("--romanize"))
            return
        if "--lookup-again" in sys.argv:
            lookup_again_flow(d, out, tag, option("--lookup-again"))
            return
        if "--suggest" in sys.argv:
            superset = option("--superset") if "--superset" in sys.argv else None
            suggest_flow(d, out, tag, option("--suggest"), superset)
            return
        if "--cover-size" in sys.argv:
            cover_size_flow(d, out, tag, "--save" in sys.argv)
            return

        d.click_sort("Year")
        time.sleep(0.8)
        print("first by year:", d.js("return document.querySelector('.card .artist').textContent.trim()"))
        filter_flow(d, out, tag)
        attention_flow(d, out, tag)
        phone_flow(d, out, tag)
        d.click_sort("Album artist")
        time.sleep(0.6)

        d.click_text("header", "Library")
        d.wait("return !!document.querySelector('.fan')")
        time.sleep(1.2)
        print("library url:", d.js("return location.search"))
        d.shot(out / f"1c-library-{tag}.png")

        d.click_text("main", "Set up rules")
        d.wait("return !!document.querySelector('.impact-text b')")
        time.sleep(1.0)
        print("setup url:", d.js("return location.search"))
        print("impact:", d.js("return document.querySelector('.impact-text').textContent.replace(/\\s+/g, ' ').trim()"))
        d.shot(out / f"1d-setup-{tag}.png")
        d.click_text("main", "Essentials")
        # Toggling one more tag guarantees a change whatever was saved before.
        d.click_xpath("//label[contains(@class, 'kind')][.//span[normalize-space(.)='Track total']]")
        time.sleep(1.0)
        print("essentials:", d.js("return document.querySelector('.impact-text').textContent.replace(/\\s+/g, ' ').trim()"),
              "|", d.js("return document.querySelector('.setup .summary').textContent.trim()"))
        print("rule tags shown:", d.js("return document.querySelectorAll('.kind').length"),
              "| show-all:", d.js("return document.querySelector('.show-all .toggle').textContent.replace(/\\s+/g, ' ').trim()"))
        d.click_text("main", "Show all tags")
        time.sleep(0.4)
        d.click_text("main", "Credits")
        time.sleep(0.6)
        d.shot(out / f"1e-setup-essentials-{tag}.png")
        d.type(".search input", "musicbrainz")
        time.sleep(0.6)
        print("filtered kinds:", d.js("return document.querySelectorAll('.kind').length"))
        d.shot(out / f"1f-setup-filter-{tag}.png")
        d.click(".search .clear")
        if "--save" in sys.argv:
            d.click_text("main", "Save")
            d.wait("return !!document.querySelector('.toast')")
            print("toast:", d.js("return document.querySelector('.toast').textContent.trim()"))
        else:
            d.click_text("main", "Revert")
        time.sleep(0.4)

        d.click_text("header", "Library")
        d.wait("return !!document.querySelector('.fan')")
        d.click_text("main", "Browse the Inbox")
        d.wait("return document.querySelectorAll('.card').length > 0")

        d.open_setup("Inbox setup")
        d.wait("return !!document.querySelector('.modes')")
        time.sleep(0.8)
        text = "return document.querySelector(arguments[0])?.textContent.replace(/\\s+/g, ' ').trim()"
        print("inbox setup url:", d.js("return location.search"))
        print("hidden:", d.js(text, ".hidden-stat"), "| inbox badge:", d.js(text, ".tabs .badge"))
        d.wait("return !!document.querySelector('.folder-field .verdict:not(.muted)')")
        print("inbox folder:", d.js(text, ".folder-field .verdict"))
        d.shot(out / f"1g-inbox-setup-{tag}.png")
        # Pick whichever mode isn't saved, so there is always a change.
        d.click(".mode:not(.on)")
        time.sleep(0.7)
        print("selected:", d.js("return document.querySelector('.mode input:checked').value"),
              "| warning:", d.js("return !!document.querySelector('.warn')"),
              "| dirty dot:", d.js("return !!document.querySelector('nav .dot')"))
        d.shot(out / f"1h-inbox-move-{tag}.png")
        if "--save" in sys.argv:
            d.click_text("main", "Save")
            d.wait("return [...document.querySelectorAll('.toast')].some(t => t.textContent.includes('Inbox setup saved'))")
            if d.js("return !document.querySelector('.hidden-row > .btn').disabled"):
                d.click(".hidden-row > .btn")
                d.wait("return !!document.querySelector('.confirm')")
                d.shot(out / f"1i-inbox-confirm-{tag}.png")
                d.click(".confirm .btn.primary")
                d.wait("return [...document.querySelectorAll('.toast')].some(t => t.textContent.includes('back in the Inbox'))")
                time.sleep(0.8)
                print("after unhide:", d.js(text, ".hidden-stat"), "| inbox badge:", d.js(text, ".tabs .badge"))
        else:
            d.click_text("main", "Revert")
        autofill_flow(d, out, tag, "--save" in sys.argv)
        d.click_text("header", "Inbox")
        d.wait("return document.querySelectorAll('.card').length > 0")

        appearance_flow(d, out, tag)
        print("back to inbox url:", repr(d.js("return location.search")))
        sync_flow(d, out, tag)
        time.sleep(0.8)

        d.click(".cell:first-child .cover")
        d.wait("return !!document.querySelector('.tracks li')")
        time.sleep(1.2)
        d.shot(out / f"2-album-{tag}.png")
        # The top bar is inert behind the sheet: "/" mustn't focus the search field.
        d.js("window.dispatchEvent(new KeyboardEvent('keydown', {key: '/', bubbles: true}))")
        print("top bar inert:", d.js("return !!document.querySelector('.bar').closest('[inert]')"),
              "| '/' focused search:", d.js("return document.activeElement === document.querySelector('.search input')"))

        d.click(".actions .btn.primary")
        d.wait("return !!document.querySelector('.player')")
        time.sleep(2.5)
        print("player:", d.js(
            "const p = document.querySelector('.player');"
            "return p && [p.querySelector('.np-title').textContent, [...p.querySelectorAll('.t')].map(t => t.textContent)]"))
        d.shot(out / f"3-playing-{tag}.png")

        d.click(".mode button:nth-child(3)")
        d.wait("return !!document.querySelector('.editor')")
        d.js("const i = document.querySelector('.album-grid .field:nth-child(4) input'); i.focus();")
        d.type(".album-grid .field:nth-child(4) input", " (edited)")
        d.type(".tr:nth-child(2) .col-title", " — remix")
        time.sleep(0.8)
        print("changes:", d.js("return document.querySelector('.summary').textContent.trim()"))
        d.shot(out / f"4-editor-{tag}.png")
        # Save is disabled on servers without --allow-writes.
        # Save appears only where in-place edits are allowed (Library, or --allow-writes).
        save = "return [...document.querySelectorAll('.editor .savebar button')].find(b => b.textContent.trim() === 'Save' && !b.disabled) ?? null"
        if "--save" in sys.argv and d.js(save):
            d.click_xpath("//div[contains(@class,'editor')]//div[contains(@class,'savebar')]//button[normalize-space(.)='Save']")
            d.wait("return !!document.querySelector('.toast')")
            print("toast:", d.js("return document.querySelector('.toast').textContent.trim()"))
            time.sleep(0.6)
            d.shot(out / f"4b-saved-{tag}.png")

        d.js("document.querySelector('.sheet-wrap').scrollTo(0, 0)")
        d.js("window.dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape'}))")
        d.wait("return !document.querySelector('.sheet')")
        d.type(".search input", "kano")
        time.sleep(1.0)
        print("filtered:", d.js("return document.querySelectorAll('.card').length"))
        d.shot(out / f"5-search-{tag}.png")

        if "--import" in sys.argv:
            import_flow(d, out, tag, option("--import"))
            return

        d.resize(500, 900)
        time.sleep(0.8)
        print("drawer hidden:", d.js("return document.querySelector('.sidebar').getBoundingClientRect().right <= 0"))
        d.shot(out / f"6-narrow-{tag}.png")
        d.click(".toggle")
        time.sleep(0.6)
        d.shot(out / f"7-drawer-{tag}.png")
        # Picking a tag keeps the drawer open for more; Escape closes it.
        d.click("aside .value")
        time.sleep(0.6)
        print("drawer open after pick:", d.js("return !!document.querySelector('.scrim')"),
              "| cards:", d.js("return document.querySelectorAll('.card').length"))
        d.js("window.dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape'}))")
        time.sleep(0.6)
        print("drawer closed by Escape:", d.js("return !document.querySelector('.scrim')"))
    finally:
        errors = [l for l in d.logs if l.startswith(("[error]", "[warn]"))]
        print(f"console: {len(d.logs)} entries, {len(errors)} warnings/errors")
        for line in errors[:20]:
            print("  ", line[:300])
        d.quit()


if __name__ == "__main__":
    main()
