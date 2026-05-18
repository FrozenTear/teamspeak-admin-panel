#!/usr/bin/env python3
"""PURA-359 — persistent yt-dlp resolver service.

A long-lived process that imports ``yt_dlp`` **once** at boot and keeps its
extractor registry warm, then resolves tracks on demand. PURA-355 measured
~2.0 s of every ``!play`` resolution as pure yt-dlp *process startup*
(extractor-registry import) — local CPU/disk, paid fresh on every track.
Embedding ``yt_dlp.YoutubeDL`` in a warm process moves that cost to a
one-time boot expense.

This script is embedded in the manager binary (``include_str!``) and written
to disk + spawned by ``crates/music-bot-audio/src/resolver.rs``, so the
script and the binary that supervises it never drift.

Protocol — one JSON request per connection, one JSON response, newline
terminated, connection then closed by the server:

    request  : {"op":"resolve","url":"...","cookie_file":"/path"|null}
               {"op":"ping"}
    response : {"ok":true,"direct_url":"...","title":"...","duration":N}
               {"ok":true,"pong":true,"yt_dlp_version":"..."}
               {"ok":false,"error":"..."}
"""

import json
import os
import socketserver
import sys


def _load_yt_dlp():
    """Import the ``yt_dlp`` module, warming its extractor registry once.

    The container ships the yt-dlp *zipapp* (a zip-importable Python
    archive) at ``/usr/local/bin/yt-dlp`` — the exact artifact the
    subprocess fallback (``source/url.rs``) runs. Importing the module from
    that same zipapp keeps the warm resolver and the fallback on
    byte-identical yt-dlp, and a manager restart after an image upgrade
    re-imports the new zipapp for free. A dev host with a pip-installed
    ``yt_dlp`` is served by the bare-import branch.
    """
    try:
        import yt_dlp  # noqa: F401

        return yt_dlp
    except ImportError:
        pass
    zipapp = os.environ.get("YT_DLP_ZIPAPP", "/usr/local/bin/yt-dlp")
    sys.path.insert(0, zipapp)
    import yt_dlp  # noqa: F401

    return yt_dlp


yt_dlp = _load_yt_dlp()


def _yt_dlp_version():
    try:
        return yt_dlp.version.__version__
    except Exception:  # noqa: BLE001
        return "unknown"


def resolve(url, cookie_file):
    """Resolve ``url`` to a direct, ffmpeg-consumable ``bestaudio`` URL.

    Mirrors the subprocess fallback's ``yt-dlp -f bestaudio -g``: extraction
    (including YouTube's nsig/signature challenge) without the download.
    """
    opts = {
        "format": "bestaudio",
        "quiet": True,
        "no_warnings": True,
        "noplaylist": True,
        "skip_download": True,
    }
    if cookie_file:
        opts["cookiefile"] = cookie_file
    with yt_dlp.YoutubeDL(opts) as ydl:
        info = ydl.sanitize_info(ydl.extract_info(url, download=False))
    direct = info.get("url")
    if not direct:
        # Some extractors surface the picked format under requested_formats /
        # requested_downloads rather than the top-level `url`.
        picked = info.get("requested_formats") or info.get("requested_downloads")
        if picked:
            direct = picked[0].get("url")
    if not direct:
        raise RuntimeError("yt-dlp returned no direct media URL")
    return {
        "ok": True,
        "direct_url": direct,
        "title": info.get("title"),
        "duration": info.get("duration"),
    }


def dispatch(payload):
    op = payload.get("op", "resolve")
    if op == "ping":
        return {"ok": True, "pong": True, "yt_dlp_version": _yt_dlp_version()}
    if op == "resolve":
        url = payload.get("url")
        if not url:
            return {"ok": False, "error": "request has no url"}
        try:
            return resolve(url, payload.get("cookie_file"))
        except Exception as exc:  # noqa: BLE001 — any extractor failure → error reply
            return {"ok": False, "error": str(exc)}
    return {"ok": False, "error": "unknown op %r" % (op,)}


class Handler(socketserver.StreamRequestHandler):
    def handle(self):
        line = self.rfile.readline()
        if not line:
            return
        try:
            payload = json.loads(line)
        except json.JSONDecodeError as exc:
            resp = {"ok": False, "error": "bad json: %s" % (exc,)}
        else:
            resp = dispatch(payload)
        self.wfile.write((json.dumps(resp) + "\n").encode())


class Server(socketserver.ThreadingUnixStreamServer):
    # One thread per connection so concurrent !play calls from different
    # music bots are resolved in parallel; a fresh YoutubeDL per request
    # keeps the (cheap) per-call object isolated while the expensive module
    # import stays shared and warm.
    daemon_threads = True
    allow_reuse_address = True


def main():
    if len(sys.argv) != 2:
        print("usage: yt_resolver.py <socket-path>", file=sys.stderr)
        sys.exit(2)
    sock_path = sys.argv[1]
    try:
        os.unlink(sock_path)
    except FileNotFoundError:
        pass
    server = Server(sock_path, Handler)
    print(
        "yt-resolver ready on %s (yt_dlp %s)" % (sock_path, _yt_dlp_version()),
        file=sys.stderr,
        flush=True,
    )
    server.serve_forever()


if __name__ == "__main__":
    main()
