#!/usr/bin/env python3
"""Drive the coversync UI in headless Firefox through geckodriver.

Takes screenshots of the main views and prints browser console errors.
Needs `geckodriver` and python-websocket-client (for console logs).

    tools/ui_smoke.py http://127.0.0.1:8770/ shots/ [--light] [--save]

--save clicks Save in the tag editor and library setup: only use it against a scratch library.
--cover-size tries Appearance's cover size slider instead of the full tour.
--suggest ALBUM [--superset ALBUM] tries Smart suggestions on an Inbox album
    (scratch daemons only: it imports the album); --superset names one that
    AcoustID matches to a bigger release, to check the warning.
--romanize ALBUM tries Prefer English the same way, on an album in another script.
--art FOLDER and --import FOLDER try album art rules and importing, with FOLDER
    as the scratch Library.
"""

import base64
import json
import socket
import subprocess
import sys
import threading
import time
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
            ["geckodriver", "--port", str(self.port), "--log", "error"],
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

    def click(self, css):
        el = self.s("POST", "/element", {"using": "css selector", "value": css})
        self.s("POST", f"/element/{el[ELEMENT]}/click", {})

    def click_text(self, scope, text):
        xpath = f"//{scope}//button[normalize-space(.)='{text}' or starts-with(normalize-space(.), '{text} ')]"
        el = self.s("POST", "/element", {"using": "xpath", "value": xpath})
        self.s("POST", f"/element/{el[ELEMENT]}/click", {})

    def click_xpath(self, xpath):
        el = self.s("POST", "/element", {"using": "xpath", "value": xpath})
        self.s("POST", f"/element/{el[ELEMENT]}/click", {})

    def click_sort(self, label):
        """Pick a sort tab in the top bar."""
        self.click_xpath(f"//div[@role='radiogroup'][@aria-label='Sort albums by']//button[normalize-space(.)='{label}']")

    def open_setup(self, label):
        """Open a setup screen (Inbox setup, Library setup, Appearance) from the top bar's menu."""
        self.click(".settings")
        self.wait("return !!document.querySelector('[role=menu]')")
        self.click_text("div[@role='menu']", label)

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


def appearance_flow(d, out, tag):
    """Add Label and Genre sort tabs, move Label up; save only with --save."""
    tabs = "return [...document.querySelectorAll('.sort [role=radio]')].map(b => b.textContent.trim())"
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
    print("top bar preview:", d.js(tabs), "| changes:", d.js("return document.querySelector('.savebar .summary')?.textContent.trim()"))
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


def requests_json(d, path):
    """GET an API path from the page under test, as the browser would."""
    return d.js("const r = new XMLHttpRequest(); r.open('GET', new URL(arguments[0].slice(1), document.baseURI), false);"
                "r.send(); return JSON.parse(r.responseText)", path)


def autofill_flow(d, out, tag, save):
    """Inbox setup's autofill card, and its effect on the Needs attention count."""
    count = "return document.querySelector('.sort .count.warn')?.textContent.trim() ?? '0'"
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
    d.wait(f"return document.querySelector('.sort .count.warn')?.textContent.trim() !== '{before}'")
    off = d.js(count)
    d.click_xpath(switch.format("Track number from the file name"))
    d.click_xpath(switch.format("Album artist from artist"))
    d.click_text("main", "Save")
    d.wait(f"return document.querySelector('.sort .count.warn')?.textContent.trim() === '{before}'")
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


def option(name):
    """The value given after a flag, e.g. the album after --suggest."""
    if name in sys.argv and sys.argv.index(name) + 1 < len(sys.argv):
        return sys.argv[sys.argv.index(name) + 1]
    raise SystemExit(f"{name} needs a value; see the usage at the top of this file.")


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
              "| dirty dot:", d.js("return !!document.querySelector('.settings .dot')"))
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
        time.sleep(0.8)

        d.click(".cell:first-child .cover")
        d.wait("return !!document.querySelector('.tracks li')")
        time.sleep(1.2)
        d.shot(out / f"2-album-{tag}.png")

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
