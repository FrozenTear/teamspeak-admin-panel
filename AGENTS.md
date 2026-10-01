# Notes for coding agents

Facts every agent (Claude Code, Grok, Paperclip agents) needs before touching the music bot or the panel-host deploy. The evidence is in the linked issues.

## Where music runs

The panel and API run on the host where `scripts/update.sh` runs. Music runs on a different host. Older notes that put the live music process on the panel host, or that refuse moving it, are stale. The present hosts are written once in [`deploy/contabo/README.md`](deploy/contabo/README.md).

| | |
|---|---|
| Switch | `TS6_LOCAL_MUSIC=skip` (default) or `play`. It chooses whether `update.sh` starts the music container on the host where the script runs. |
| Why skip | Music is already running on the other host. Starting the local container takes `127.0.0.1:3002` and clones the client identity. `play` is the turn-back, and only after the remote music process is stopped. |
| Panel URL | `MUSIC_RUNTIME_URL` stays `http://127.0.0.1:3002`. No bearer on that hop, so it stays on the panel host's loopback. Do not publish `:3002` or `:7080`. Do not retarget the URL. |
| Hop | Reverse tunnel the music host opens, binding only `127.0.0.1:3002` on the panel host. `ts6-music-tunnel.service` on the music host should start it after a reboot. A one-off `ssh -f` may be what holds the port until then. |
| Data | Identities and `yt-cookies.txt` live on the music host. An empty cookie file fails the resolve and puts nothing on the wire. |
| Cut order | Hop listening, then the panel API. The music process comes back from its restart policy. The client is pushed only when that API starts. If the API boots while `127.0.0.1:3002` is not answering, the saved row is never pushed and the music page is 404 (empty supervisor). A hop that dies after the client is already connected is a 5xx from the API. The TeamSpeak client stays in the channel, and the panel cannot drive it until the hop is back. |
| Pin | Packing B (send CPUs 0-1, decode 2-5, nice -5) is the pin on the panel host. The remote process had no cpuset and no nice. Its send thread was on both of that host's cores. Sidecar stays on the panel host. Do not restart TeamSpeak on either host. |

`scripts/update.sh` prints the mode and the command that turns it back on. It refuses to restart the API when `127.0.0.1:3002` is not answering.

Issue #93 stays open as the panel-host finding. The same image and the same track had late frames on the panel host and none on the remote music host. That remote track is not packing B, and it is not an unpinned loop. Do not close #93 and do not add an audio change.

## Music bot defaults: keep them unless you have measurements

These live in `crates/voice` and `crates/music-bot-audio`.

| Setting (env on the music container) | Default | Why | Turn off / change |
|---|---|---|---|
| `VOICE_SPLIT_WIRE_TASK` | **on**: split wire/control loop | Keeps chat, queue and yt-dlp work off the 20 ms voice send (PURA-396, #93) | `VOICE_SPLIT_WIRE_TASK=0` selects the single loop |
| `VOICE_ENCODE_HEADROOM_DB` | **-6** | At 0 dB, Opus decoding of mastered music overshoots full scale by 6–8 dB, and clients clip it: the "bass crackle" (#93) | `VOICE_ENCODE_HEADROOM_DB=0` restores the old levels |
| `VOICE_SEND_LEAD_MS` | **0** (off) | The TeamSpeak 6 desktop client already hides our stalls, and a large lead gets trimmed into audible skips (#93) | Only test a small lead on a phone client |

Runtime overrides need no restart and apply from the next track. They go to the music container's loopback API: `GET/POST 127.0.0.1:3002/v1/voice/encode-headroom` (`{"db": -6}`) and `GET/POST 127.0.0.1:3002/v1/voice/send-lead` (`{"ms": 0}`). On the panel host that address is the hop to the remote music process, not the stopped local music container. Each play logs `stage=sibling_config` with the values in force.

## Measuring audio problems
- **Judge a fix by what a listener receives.** `frame_underrun` and `pacer_wakeup` describe the sending side only.
- **Steal is never reported on the panel-host VM.** The VM pauses as a whole for 50–270 ms at a time, and `host_steal_ms` is always 0 there. A 0 is not evidence against the host.
- **Decode received Opus to float, in packet order.** int16 decoding soft-clips and hides overshoot. Decoding reordered packets in arrival order invents +1–2 dBFS spikes, so reorder and conceal losses the way a real client does.

## Panel-host deploy
- Deploy the panel, API, and sidecar only with `scripts/update.sh vX.Y.Z`, run from the panel-host checkout. It renders the manifest; the committed `@UNRELEASED` image placeholders fail if you play the file directly.
- The default is `TS6_LOCAL_MUSIC=skip`. Do not start the local music container while the remote process is the live client. `TS6_LOCAL_MUSIC=play` is the turn-back, and only after that remote process is stopped.
- The hop (`ts6-music-tunnel.service` on the music host, or the one-off `ssh -f` that is holding it) must be listening on the panel host's `127.0.0.1:3002` before `update.sh` brings the API back.
- **Never `podman kube down --force`.** It wipes the `ts6-data`, `ts6-db` and `ts6-music` volumes.
- DJ-Bot's channel is not persisted. After a redeploy it lands in the server's default channel, so move it back.
- Do not restart TeamSpeak on either host as part of a music or panel cut.
