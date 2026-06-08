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

PURA-360 additionally turns on yt-dlp's per-player-revision preprocessed-player
cache (``_enable_preprocessed_player_cache`` below) so the expensive YouTube
``base.js`` parse is paid once per player revision rather than once per
``!play``.

Protocol — one JSON request per connection, one JSON response, newline
terminated, connection then closed by the server:

    request  : {"op":"resolve","url":"...","cookie_file":"/path"|null}
               {"op":"ping"}
    partial  : {"ok":true,"partial":true,"video_id":"...","phase":"search_fetch","ms":N}
               (THE-942 — a search streams this the moment phase 1 resolves the
                video_id, before phase 2 runs; zero or more partials precede the
                final reply. A non-streaming client ignores it.)
    response : {"ok":true,"direct_url":"...","title":"...","duration":N}
               {"ok":true,"pong":true,"yt_dlp_version":"..."}
               {"ok":false,"error":"..."}
"""

import json
import os
import socketserver
import sys
import time


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


def _enable_preprocessed_player_cache():
    """PURA-360 — turn on the per-player-revision preprocessed-player cache.

    Every ``!play`` of a YouTube URL runs a JS challenge: yt-dlp's EJS
    provider hands the player ``base.js`` (~2.7 MB) to ``deno``, which parses
    and *preprocesses* it before solving the ``n`` / signature transforms.
    PURA-355 measured that solve as the largest phase of the resolution
    floor. The preprocessing — not deno's process start (~60 ms) — is the
    cost: parsing a 2.7 MB script in V8 on every track.

    yt-dlp's ``_real_bulk_solve`` *already* knows how to cache the
    preprocessed player, keyed by player URL, and skip the parse on a hit —
    but ships it disabled (``_ENABLE_PREPROCESSED_PLAYER_CACHE = False``,
    upstream rationale: "files are large and we do not support rotation").

    For a long-lived resolver the trade-off inverts:

      * The player revision is embedded in the cache key (the player URL is
        ``.../s/player/<rev>/...base.js``), so a revision bump is automatic
        invalidation — a new key, never a stale solve. "Rotation" is free.
      * The revision is stable for days, so after the first ``!play`` every
        later one reuses the cached preprocessed player.
      * The ~3.7 MB cache files are bounded: one per player revision, on the
        container's ephemeral fs, and the manager restarts on every image
        upgrade — a handful of files per container lifetime.

    Measured on contabo-dev, same player, warm process: enabling this cut
    the deno JS-challenge phase ~2.4 s -> ~1.1 s and the whole resolve
    ~5.1 s -> ~2.4 s on a cache hit.

    Flipping the class flag reuses yt-dlp's own tested load/store path — no
    fork of the solver. ``YT_NSIG_CACHE_DISABLE`` pins playback back to the
    stock cold-solve behaviour. A future yt-dlp that renames or drops the
    flag degrades to stock behaviour rather than erroring: we only set an
    attribute that already exists.
    """
    if os.environ.get("YT_NSIG_CACHE_DISABLE"):
        print(
            "yt-resolver: preprocessed-player cache disabled (YT_NSIG_CACHE_DISABLE)",
            file=sys.stderr,
            flush=True,
        )
        return
    try:
        from yt_dlp.extractor.youtube.jsc._builtin.ejs import EJSBaseJCP
    except Exception as exc:  # noqa: BLE001 — any import failure → stay on stock behaviour
        print(
            "yt-resolver: EJS JS-challenge provider not importable (%s) — "
            "preprocessed-player cache not enabled" % (exc,),
            file=sys.stderr,
            flush=True,
        )
        return
    if not hasattr(EJSBaseJCP, "_ENABLE_PREPROCESSED_PLAYER_CACHE"):
        print(
            "yt-resolver: yt-dlp has no _ENABLE_PREPROCESSED_PLAYER_CACHE flag — "
            "preprocessed-player cache not enabled (yt-dlp internals changed?)",
            file=sys.stderr,
            flush=True,
        )
        return
    EJSBaseJCP._ENABLE_PREPROCESSED_PLAYER_CACHE = True
    print(
        "yt-resolver: per-player-revision preprocessed-player cache enabled (PURA-360)",
        file=sys.stderr,
        flush=True,
    )


_enable_preprocessed_player_cache()


def _yt_dlp_version():
    try:
        return yt_dlp.version.__version__
    except Exception:  # noqa: BLE001
        return "unknown"


def _extract_track(info):
    """Pull a direct ``bestaudio`` URL + metadata out of an extract_info result.

    PURA-368 — a watch URL (``https://youtu.be/<id>``) resolves to a video
    info dict carrying a top-level ``url``. A YouTube *search* query
    (``!play yt:`` → ``ytsearch1:<query>``, PURA-353) or a playlist URL
    resolves instead to a ``_type: playlist`` dict whose per-video info
    dicts live under ``entries``. yt-dlp processes search/playlist entries
    fully — the nsig/signature challenge runs on the picked video as part
    of ``extract_info`` — so the first non-empty entry already carries the
    resolved direct URL; descend into it rather than re-resolving.

    Before this fix the search path looked only at the playlist's
    (absent) top-level ``url``, raised "no direct media URL", and the
    caller fell back to a cold ``yt-dlp`` subprocess that re-ran the whole
    search — the warm resolver did the full (multi-second) work and threw
    it away. Returns ``None`` when no entry yields a playable URL.
    """
    if info is None:
        return None
    entries = info.get("entries")
    if entries is not None:
        for entry in entries:
            track = _extract_track(entry)
            if track is not None:
                return track
        return None
    direct = info.get("url")
    if not direct:
        # Some extractors surface the picked format under requested_formats /
        # requested_downloads rather than the top-level `url`.
        picked = info.get("requested_formats") or info.get("requested_downloads")
        if picked:
            direct = picked[0].get("url")
    if not direct:
        return None
    return {
        "ok": True,
        "direct_url": direct,
        "title": info.get("title"),
        "duration": info.get("duration"),
    }


def resolve(url, cookie_file, send_partial=None):
    """Resolve ``url`` to a direct, ffmpeg-consumable ``bestaudio`` URL.

    Mirrors the subprocess fallback's ``yt-dlp -f bestaudio -g``: extraction
    (including YouTube's nsig/signature challenge) without the download.
    Handles both a direct watch URL and a search/playlist result — see
    [`_extract_track`].

    THE-932 — per-phase timing instrumentation.

    For a ``ytsearch<N>:`` search URL the resolution splits into two phases so
    each can be measured and attributed separately:

    1. **search_fetch** — ``extract_flat`` call: asks the YouTube search API
       for the top result and returns a flat entry dict (id + title only).
       No nsig challenge, no format selection. Typical cost: the YouTube
       search-API round-trip (~1–3 s on a warm prod host).

    2. **nsig_solve** — second ``extract_info`` call on the concrete watch URL
       (``https://www.youtube.com/watch?v=<id>``). This is where yt-dlp runs
       the deno JS challenge and picks the best audio format. Typical cost on a
       warm preprocessed-player cache: ~1.1 s (PURA-360).

    For a direct watch URL both phases collapse into a single **direct_resolve**
    measurement.

    The resolved ``video_id`` is included in the response so the Rust
    supervisor can hand it to the ``yt-dlp`` subprocess fallback as a direct
    URL instead of re-running the search query from scratch — cutting the
    worst-case fallback from ~25 s down to the single-video resolve floor.

    THE-942 — for a search, the ``video_id`` is *streamed* as a partial reply
    the moment phase 1 (``search_fetch``) resolves it, **before** phase 2
    (``nsig_solve``) runs. ``send_partial`` (when supplied) is called with a
    ``{"ok":true,"partial":true,"video_id":...}`` line. This is the THE-931
    failure mode's escape hatch: if phase 2 then stalls and the Rust caller's
    phase-2 budget fires, it already holds the ``video_id`` and hands the
    subprocess a direct watch URL instead of re-running ``ytsearch1:``. The
    final reply still carries ``video_id`` too, so a non-streaming client (or
    a direct-URL resolve) is unaffected.
    """
    base_opts = {
        "format": "bestaudio",
        "quiet": True,
        "no_warnings": True,
        "noplaylist": True,
        "skip_download": True,
        # THE-931 — bound every outbound socket read (search-API fetch,
        # watch page, player JS, …). Parity with the subprocess fallback's
        # `--socket-timeout 10` (`source/url.rs` SOCKET_TIMEOUT_SECS): without
        # it a stalled YouTube search-API socket held the warm resolver for up
        # to RESOLVE_TIMEOUT before failing. yt-dlp installs no socket
        # timeout by default, so a dead-slow read otherwise blocks until the OS
        # gives up.
        "socket_timeout": 10,
    }
    if cookie_file:
        base_opts["cookiefile"] = cookie_file

    is_search = url.startswith("ytsearch")

    if is_search:
        # --- Phase 1: search_fetch ---
        # Use extract_flat to ask YouTube for the top search result without
        # running the nsig/format-selection machinery. This isolates the
        # search-API network cost from the JS-challenge cost.
        flat_opts = dict(base_opts)
        flat_opts["extract_flat"] = True
        flat_opts["noplaylist"] = False  # allow the search-result "playlist"

        t0 = time.monotonic()
        with yt_dlp.YoutubeDL(flat_opts) as ydl:
            flat_info = ydl.extract_info(url, download=False)
        search_ms = int((time.monotonic() - t0) * 1000)

        # Pull the first entry's video ID.
        entries = (flat_info or {}).get("entries") or []
        video_id = entries[0].get("id") if entries else None
        if not video_id:
            raise RuntimeError("ytsearch returned no entries")

        # THE-942 — stream the video_id as a partial reply *before* phase 2.
        # If nsig_solve then stalls and the caller's phase-2 budget fires, it
        # already holds the video_id and can hand the subprocess a direct
        # watch URL instead of re-running the search.
        if send_partial is not None:
            send_partial(
                {
                    "ok": True,
                    "partial": True,
                    "video_id": video_id,
                    "phase": "search_fetch",
                    "ms": search_ms,
                }
            )

        # --- Phase 2: nsig_solve ---
        watch_url = "https://www.youtube.com/watch?v=%s" % video_id
        t1 = time.monotonic()
        with yt_dlp.YoutubeDL(base_opts) as ydl:
            info = ydl.sanitize_info(ydl.extract_info(watch_url, download=False))
        nsig_ms = int((time.monotonic() - t1) * 1000)

        phases = [
            {"name": "search_fetch", "ms": search_ms},
            {"name": "nsig_solve", "ms": nsig_ms},
        ]
    else:
        video_id = None
        t0 = time.monotonic()
        with yt_dlp.YoutubeDL(base_opts) as ydl:
            info = ydl.sanitize_info(ydl.extract_info(url, download=False))
        direct_ms = int((time.monotonic() - t0) * 1000)
        # Try to capture the video ID from the resolved info for cache/fallback use.
        if info:
            video_id = info.get("id")
        phases = [{"name": "direct_resolve", "ms": direct_ms}]

    track = _extract_track(info)
    if track is None:
        raise RuntimeError("yt-dlp returned no direct media URL")
    track["phases"] = phases
    if video_id:
        track["video_id"] = video_id
    return track


def dispatch(payload, send_partial=None):
    op = payload.get("op", "resolve")
    if op == "ping":
        return {"ok": True, "pong": True, "yt_dlp_version": _yt_dlp_version()}
    if op == "resolve":
        url = payload.get("url")
        if not url:
            return {"ok": False, "error": "request has no url"}
        try:
            return resolve(url, payload.get("cookie_file"), send_partial)
        except Exception as exc:  # noqa: BLE001 — any extractor failure → error reply
            return {"ok": False, "error": str(exc)}
    return {"ok": False, "error": "unknown op %r" % (op,)}


class Handler(socketserver.StreamRequestHandler):
    def handle(self):
        line = self.rfile.readline()
        if not line:
            return

        def send_partial(obj):
            # THE-942 — write one newline-terminated partial line and flush it
            # immediately so the Rust caller can read the streamed video_id
            # while phase 2 (nsig_solve) is still running.
            self.wfile.write((json.dumps(obj) + "\n").encode())
            self.wfile.flush()

        try:
            payload = json.loads(line)
        except json.JSONDecodeError as exc:
            resp = {"ok": False, "error": "bad json: %s" % (exc,)}
        else:
            resp = dispatch(payload, send_partial)
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
