#!/usr/bin/env python3
"""Upload an album folder into coversync's Inbox through the upload API.

A small reference client, for trying the API and measuring it:

    tools/upload.py https://n5pro.internal/coversync/ ~/Music/Album
    tools/upload.py URL FOLDER --resume ID     # carry on with an upload
    tools/upload.py URL FOLDER --parallel 8 --chunk 16M --keep   # don't finish

It signs in with COVERSYNC_PASSWORD (or --password), or sends an app token
from COVERSYNC_TOKEN (or --token). Files coversync doesn't take (and hidden
ones) are left out. Several chunks are in flight at once, one connection
per worker; a resumed upload sends only the byte ranges the server lacks.
"""

import argparse
import http.client
import json
import os
import queue
import ssl
import sys
import threading
import time
import urllib.parse


def size_arg(text):
    units = {"k": 1 << 10, "m": 1 << 20, "g": 1 << 30}
    text = text.strip().lower()
    if text and text[-1] in units:
        return int(float(text[:-1]) * units[text[-1]])
    return int(text)


class Client:
    def __init__(self, url, insecure):
        parts = urllib.parse.urlsplit(url)
        self.https = parts.scheme == "https"
        self.host = parts.netloc
        self.base = parts.path.rstrip("/")
        self.context = ssl._create_unverified_context() if insecure else None
        self.headers = {}

    def connect(self):
        if self.https:
            return http.client.HTTPSConnection(self.host, context=self.context, timeout=120)
        return http.client.HTTPConnection(self.host, timeout=120)

    def call(self, method, path, body=None, conn=None, raw=False):
        own = conn is None
        conn = conn or self.connect()
        headers = dict(self.headers)
        if body is not None and not raw:
            body = json.dumps(body).encode()
            headers["Content-Type"] = "application/json"
        elif raw:
            headers["Content-Type"] = "application/octet-stream"
        try:
            conn.request(method, self.base + path, body=body, headers=headers)
            res = conn.getresponse()
            data = res.read()
        finally:
            if own:
                conn.close()
        value = json.loads(data) if data else None
        if res.status >= 400:
            message = value.get("error") if isinstance(value, dict) else data[:200]
            raise RuntimeError(f"{method} {path}: {res.status} {message}")
        return res, value

    def sign_in(self, password, token):
        if token:
            self.headers["Authorization"] = f"Bearer {token}"
            return
        res, _ = self.call("POST", "/api/login", {"password": password, "path": self.base + "/"})
        cookie = res.getheader("Set-Cookie", "").split(";", 1)[0]
        self.headers["Cookie"] = cookie


def album_files(folder, extensions):
    """(relative path, absolute path, size) of each file to upload."""
    found = []
    for dirpath, dirnames, filenames in os.walk(folder):
        dirnames[:] = sorted(d for d in dirnames if not d.startswith("."))
        for name in sorted(filenames):
            ext = name.rsplit(".", 1)[-1].lower() if "." in name else ""
            if name.startswith(".") or ext not in extensions:
                continue
            path = os.path.join(dirpath, name)
            rel = os.path.relpath(path, folder).replace(os.sep, "/")
            found.append((rel, path, os.path.getsize(path)))
    return found


def missing(size, ranges, chunk):
    """Chunks (offset, length) covering what `ranges` leave out of `size` bytes."""
    gaps, at = [], 0
    for start, end in ranges:
        if start > at:
            gaps.append((at, start))
        at = max(at, end)
    if at < size:
        gaps.append((at, size))
    for start, end in gaps:
        for offset in range(start, end, chunk):
            yield offset, min(chunk, end - offset)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("url", help="coversync's address, such as https://n5pro.internal/coversync/")
    ap.add_argument("folder", help="the album folder to upload")
    ap.add_argument("--name", help="the folder's name on the server (default: the folder's own)")
    ap.add_argument("--password", default=os.environ.get("COVERSYNC_PASSWORD"))
    ap.add_argument("--token", default=os.environ.get("COVERSYNC_TOKEN"))
    ap.add_argument("--parallel", type=int, default=6, help="chunks in flight at once (default 6)")
    ap.add_argument("--chunk", type=size_arg, help="chunk size, such as 16M (default: the server's)")
    ap.add_argument("--resume", metavar="ID", help="carry on with this upload")
    ap.add_argument("--keep", action="store_true", help="send the files but don't finish")
    ap.add_argument("--insecure", action="store_true", help="don't check the TLS certificate")
    args = ap.parse_args()
    if not args.password and not args.token:
        ap.error("set COVERSYNC_PASSWORD or COVERSYNC_TOKEN (or --password / --token)")

    client = Client(args.url, args.insecure)
    client.sign_in(args.password, args.token)
    _, info = client.call("GET", "/api/uploads")
    folder = os.path.abspath(args.folder)
    files = album_files(folder, set(info["extensions"]))
    if not files:
        sys.exit("nothing to upload")
    chunk = args.chunk or info["chunk_size"]

    if args.resume:
        _, upload = client.call("GET", f"/api/uploads/{args.resume}")
        if [f["path"] for f in upload["files"]] != [f[0] for f in files]:
            sys.exit("that upload holds other files")
    else:
        body = {
            "folder": args.name or os.path.basename(folder),
            "files": [{"path": rel, "size": size} for rel, _, size in files],
        }
        _, upload = client.call("POST", "/api/uploads", body)
    upload_id = upload["id"]
    print(f"upload {upload_id}: {len(files)} files, {upload['size']} bytes", flush=True)

    jobs = queue.Queue()
    to_send = 0
    for (rel, path, size), status in zip(files, upload["files"]):
        for offset, length in missing(size, status["ranges"], chunk):
            jobs.put((status["index"], path, offset, length))
            to_send += length
    sent = [0]
    errors = []
    lock = threading.Lock()

    def worker():
        conn = client.connect()
        fds = {}
        try:
            while not errors:
                try:
                    index, path, offset, length = jobs.get_nowait()
                except queue.Empty:
                    return
                fd = fds.get(path)
                if fd is None:
                    fd = fds[path] = os.open(path, os.O_RDONLY)
                data = os.pread(fd, length, offset)
                route = f"/api/uploads/{upload_id}/files/{index}?offset={offset}"
                client.call("PUT", route, data, conn=conn, raw=True)
                with lock:
                    sent[0] += len(data)
        except Exception as e:  # noqa: BLE001 - reported below
            errors.append(e)
        finally:
            conn.close()
            for fd in fds.values():
                os.close(fd)

    began = time.monotonic()
    threads = [threading.Thread(target=worker) for _ in range(max(1, args.parallel))]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    took = time.monotonic() - began
    rate = sent[0] / took / 1e6 if took else 0
    print(f"sent {sent[0]} of {to_send} bytes in {took:.2f} s: {rate:.1f} MB/s", flush=True)
    if errors:
        sys.exit(f"failed: {errors[0]} (resume with --resume {upload_id})")
    if args.keep:
        return
    began = time.monotonic()
    _, done = client.call("POST", f"/api/uploads/{upload_id}/finish")
    print(f"finished in {time.monotonic() - began:.2f} s: {done['path']}")


if __name__ == "__main__":
    main()
