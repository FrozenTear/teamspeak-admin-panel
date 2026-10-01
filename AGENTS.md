# Notes for coding agents

Facts every agent (Claude Code, Grok, Paperclip agents) needs before touching the music bot or the Contabo deploy. The evidence is in the linked issues.

## Where music runs

Live music is Docker on Floki (`185.146.234.42`), not the Contabo pod. Older notes that put music on Contabo, or that refuse moving it to Floki, are stale.

| | |
|---|---|
| Container | `ts6-manager-music-floki` |
| Image | `v1.6.24`, host network, listen `127.0.0.1:3002`, restart `unless-stopped`. Not recreated after start `2026-09-30T20:21:44Z`. |
| Data | `/home/scuffedspeak/ts6-floki-test/data` (identities and `yt-cookies.txt`). Panel bot 7 (DJ-Bot) is `bot-1.identity`. |
| Contabo copy | `ts6-manager-music` is stopped (exited 137). Its volume still has the same `bot-1.identity`. Starting it takes port 3002 and the TeamSpeak server sees a second copy of that client. |
| Panel / API | Stay on Contabo. `MUSIC_RUNTIME_URL` stays `http://127.0.0.1:3002`. No bearer on that hop, so it stays on Contabo's loopback. Do not publish `:3002` or `:7080`. Do not retarget the URL. |
| Hop | SSH reverse tunnel that Floki opens to Contabo user `ts6dev`, binding only `127.0.0.1:3002`. `ts6-music-tunnel.service` is installed and enabled, and inactive while a one-off `ssh -f` holds the port. On a Floki reboot the unit should start the hop. |
| Cut order | Hop listening, then the Contabo API. The music process comes back from the restart policy. The client is pushed only when that API starts. If the hop is down, rehydrate fails and the music page is 404. |
| What stays | Sidecar on Contabo. Soft pin packing B. Do not restart TeamSpeak on either host. |

`scripts/update.sh` skips the Contabo music container unless `TS6_CONTABO_MUSIC=play`. The script prints the mode and the command that turns it back on. It refuses to restart the API when `127.0.0.1:3002` is not answering.

Issue #93 stays open. The same `v1.6.24` image and the same track had 93 late frames on Contabo and 0 on Floki. That is a host finding, not a code fix. Do not close #93 and do not add an audio change.

## Music bot defaults: keep them unless you have measurements

These live in `crates/voice` and `crates/music-bot-audio`.

| Setting (env on the music container) | Default | Why | Turn off / change |
|---|---|---|---|
| `VOICE_SPLIT_WIRE_TASK` | **on**: split wire/control loop | Keeps chat, queue and yt-dlp work off the 20 ms voice send (PURA-396, #93) | `VOICE_SPLIT_WIRE_TASK=0` selects the single loop |
| `VOICE_ENCODE_HEADROOM_DB` | **-6** | At 0 dB, Opus decoding of mastered music overshoots full scale by 6–8 dB, and clients clip it: the "bass crackle" (#93) | `VOICE_ENCODE_HEADROOM_DB=0` restores the old levels |
| `VOICE_SEND_LEAD_MS` | **0** (off) | The TeamSpeak 6 desktop client already hides our stalls, and a large lead gets trimmed into audible skips (#93) | Only test a small lead on a phone client |

Runtime overrides need no restart and apply from the next track. They go to the music container's loopback API: `GET/POST 127.0.0.1:3002/v1/voice/encode-headroom` (`{"db": -6}`) and `GET/POST 127.0.0.1:3002/v1/voice/send-lead` (`{"ms": 0}`). On Contabo that address is the Floki hop, not the stopped `ts6-manager-music` container. Each play logs `stage=sibling_config` with the values in force.

## Measuring audio problems
- **Judge a fix by what a listener receives.** `frame_underrun` and `pacer_wakeup` describe the sending side only.
- **Steal is never reported on the Contabo VM.** The VM pauses as a whole for 50–270 ms at a time, and `host_steal_ms` is always 0 there. A 0 is not evidence against the host.
- **Decode received Opus to float, in packet order.** int16 decoding soft-clips and hides overshoot. Decoding reordered packets in arrival order invents +1–2 dBFS spikes, so reorder and conceal losses the way a real client does.

## Contabo deploy
- Deploy the panel, API, and sidecar only with `scripts/update.sh vX.Y.Z`, run from `/root/github/teamspeak-admin-panel`. It renders the manifest; the committed `@UNRELEASED` image placeholders fail if you play the file directly.
- The default is `TS6_CONTABO_MUSIC=skip`. Do not start `ts6-manager-music` on Contabo while `ts6-manager-music-floki` is the live client. `TS6_CONTABO_MUSIC=play` is the turn-back, and only after the Floki container is stopped.
- The hop (`ts6-music-tunnel.service` on Floki, or the one-off `ssh -f` that is holding it) must be listening on Contabo `127.0.0.1:3002` before `update.sh` brings the API back.
- **Never `podman kube down --force`.** It wipes the `ts6-data`, `ts6-db` and `ts6-music` volumes.
- DJ-Bot's channel is not persisted. After a redeploy it lands in the server's default channel, so move it back.
- Do not restart TeamSpeak on Contabo or Floki as part of a music or panel cut.
