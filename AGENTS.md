# Notes for coding agents

Facts every agent (Claude Code, Grok, Paperclip agents) needs before touching the music bot or the Contabo deploy. The evidence is in the linked issues.

## Music bot defaults: keep them unless you have measurements

These live in `crates/voice` and `crates/music-bot-audio`.

| Setting (env on the music container) | Default | Why | Turn off / change |
|---|---|---|---|
| `VOICE_SPLIT_WIRE_TASK` | **on**: split wire/control loop | Keeps chat, queue and yt-dlp work off the 20 ms voice send (PURA-396, #93) | `VOICE_SPLIT_WIRE_TASK=0` selects the single loop |
| `VOICE_ENCODE_HEADROOM_DB` | **-6** | At 0 dB, Opus decoding of mastered music overshoots full scale by 6–8 dB, and clients clip it: the "bass crackle" (#93) | `VOICE_ENCODE_HEADROOM_DB=0` restores the old levels |
| `VOICE_SEND_LEAD_MS` | **0** (off) | The TeamSpeak 6 desktop client already hides our stalls, and a large lead gets trimmed into audible skips (#93) | Only test a small lead on a phone client |

Runtime overrides need no restart and apply from the next track. They go to the music container's loopback API: `GET/POST 127.0.0.1:3002/v1/voice/encode-headroom` (`{"db": -6}`) and `GET/POST 127.0.0.1:3002/v1/voice/send-lead` (`{"ms": 0}`). Each play logs `stage=sibling_config` with the values in force.

## Measuring audio problems
- **Judge a fix by what a listener receives.** `frame_underrun` and `pacer_wakeup` describe the sending side only.
- **Steal is never reported on the Contabo VM.** The VM pauses as a whole for 50–270 ms at a time, and `host_steal_ms` is always 0 there. A 0 is not evidence against the host.
- **Decode received Opus to float, in packet order.** int16 decoding soft-clips and hides overshoot. Decoding reordered packets in arrival order invents +1–2 dBFS spikes, so reorder and conceal losses the way a real client does.

## Contabo deploy
- Deploy production only with `scripts/update.sh vX.Y.Z`, run from `/root/github/teamspeak-admin-panel`. It renders the manifest; the committed `@UNRELEASED` image placeholders fail if you play the file directly.
- **Never `podman kube down --force`.** It wipes the `ts6-data`, `ts6-db` and `ts6-music` volumes.
- DJ-Bot's channel is not persisted. After a redeploy it lands in the server's default channel, so move it back.
