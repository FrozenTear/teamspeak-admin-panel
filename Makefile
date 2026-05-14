# PURA-170 / OD3 — single-command local bring-up of the upstream TS6 server
# fixture used by the verification matrix. Thin wrapper over the
# `ts6-fixture` profile in `podman-compose.yml`; CI runs the same image as
# a service container (see `.github/workflows/ci.yml ts6-fixture-smoke`).
# Operator notes: docs/ts6-fixture.md.

.PHONY: ts6-up ts6-down ts6-logs ts6-apikey

ts6-up:
	@if podman ps --format '{{.Names}}' 2>/dev/null | grep -qx ts6-fixture; then \
	    echo "==> ts6-fixture already running"; \
	else \
	    echo "==> starting ts6-fixture (--profile ts6-fixture, --network=host)"; \
	    podman-compose --profile ts6-fixture up -d ts6-fixture; \
	fi
	@echo "==> waiting up to 90s for the fixture to print its API key…"
	@for _ in $$(seq 1 45); do \
	    if podman logs ts6-fixture 2>&1 | grep -qE 'apikey=[A-Za-z0-9_-]+'; then \
	        break; \
	    fi; \
	    sleep 2; \
	done
	@KEY=$$(podman logs ts6-fixture 2>&1 | grep -oE 'apikey=[A-Za-z0-9_-]+' | head -1 | cut -d= -f2); \
	if [ -z "$$KEY" ]; then \
	    echo "==> fixture is up but no apikey line has been printed yet — run 'make ts6-logs' to inspect"; \
	else \
	    echo "==> fixture API key: $$KEY"; \
	    echo "==> verification env:"; \
	    echo "      export TS6_INTEGRATION_HOST=127.0.0.1"; \
	    echo "      export TS6_INTEGRATION_PORT=10080"; \
	    echo "      export TS6_INTEGRATION_API_KEY=$$KEY"; \
	fi

ts6-down:
	podman-compose --profile ts6-fixture down

ts6-logs:
	podman logs -f ts6-fixture

ts6-apikey:
	@podman logs ts6-fixture 2>&1 | grep -oE 'apikey=[A-Za-z0-9_-]+' | head -1 | cut -d= -f2

# PURA-108 WS-4 / PURA-112 — "two clients can talk" prototype.
#
# Single-command bring-up of the local ts6-fixture + a two-client harness
# that exchanges Opus voice frames end-to-end. Acceptance bar: >=30s of
# stable bidirectional flow with no `ts3error` and no resends. Operator
# notes: docs/voice-prototype.md.

.PHONY: voice-prototype voice-prototype-build voice-prototype-fixture-up \
        voice-prototype-fixture-down voice-prototype-clean

# Override-able knobs.
VOICE_PROTOTYPE_DURATION ?= 30
VOICE_PROTOTYPE_SERVER   ?= 127.0.0.1:9987
VOICE_PROTOTYPE_OUT_DIR  ?= target/voice-prototype

voice-prototype: voice-prototype-build voice-prototype-fixture-up
	@mkdir -p $(VOICE_PROTOTYPE_OUT_DIR)/alice $(VOICE_PROTOTYPE_OUT_DIR)/bob
	@echo "==> giving fixture 5s to settle before the two clients connect"
	@sleep 5
	@echo "==> spawning alice (440Hz tone) + bob (660Hz tone) for $(VOICE_PROTOTYPE_DURATION)s"
	@set -e; \
	./target/release/ts6-voice-prototype \
	    --server $(VOICE_PROTOTYPE_SERVER) \
	    --name alice --send-tone-hz 440 \
	    --duration-secs $(VOICE_PROTOTYPE_DURATION) \
	    --identity-dir $(VOICE_PROTOTYPE_OUT_DIR)/alice \
	    --out-wav $(VOICE_PROTOTYPE_OUT_DIR)/alice.wav \
	    >$(VOICE_PROTOTYPE_OUT_DIR)/alice.log 2>&1 & PID_A=$$!; \
	./target/release/ts6-voice-prototype \
	    --server $(VOICE_PROTOTYPE_SERVER) \
	    --name bob --send-tone-hz 660 \
	    --duration-secs $(VOICE_PROTOTYPE_DURATION) \
	    --identity-dir $(VOICE_PROTOTYPE_OUT_DIR)/bob \
	    --out-wav $(VOICE_PROTOTYPE_OUT_DIR)/bob.wav \
	    >$(VOICE_PROTOTYPE_OUT_DIR)/bob.log 2>&1 & PID_B=$$!; \
	wait $$PID_A; RC_A=$$?; \
	wait $$PID_B; RC_B=$$?; \
	echo "==> alice rc=$$RC_A bob rc=$$RC_B"; \
	echo "==> WAV files:"; \
	ls -lh $(VOICE_PROTOTYPE_OUT_DIR)/*.wav 2>/dev/null || \
	    echo "  (no WAV files emitted — see docs/voice-prototype.md \"no audio?\" section)"; \
	echo "==> client logs: $(VOICE_PROTOTYPE_OUT_DIR)/{alice,bob}.log"; \
	exit $$(( RC_A + RC_B ))

voice-prototype-build:
	cargo build --release -p ts6-voice-prototype

voice-prototype-fixture-up:
	@if podman ps --format '{{.Names}}' 2>/dev/null | grep -qx ts6-fixture; then \
	    echo "==> ts6-fixture already running"; \
	else \
	    echo "==> starting ts6-fixture (--profile ts6-fixture)"; \
	    podman-compose --profile ts6-fixture up -d ts6-fixture; \
	fi

voice-prototype-fixture-down:
	podman-compose --profile ts6-fixture down

voice-prototype-clean:
	rm -rf $(VOICE_PROTOTYPE_OUT_DIR)

# PURA-108 WS-7 / PURA-114 — Phase 3.5 WebRTC bridge translator. Brings up
# the SFU + TURN side of the bridge profile (LiveKit + coturn) co-hosted
# with the existing ts6-fixture so an operator can validate the deployment
# shape from ADR-0006 before the translator daemon ships. Smoke target
# checks the LiveKit health endpoint and STUN binding response. Operator
# notes: docs/voice-translator.md.

.PHONY: voice-translator-up voice-translator-down voice-translator-smoke

voice-translator-up:
	podman-compose --profile voice-translator up -d

voice-translator-down:
	podman-compose --profile voice-translator down

# Smoke test: LiveKit's HTTP server answers `/` with a body that includes
# its name string, and coturn answers a STUN Binding Request on 3478/udp.
# Both checks are deliberately tiny — they only assert "the daemons came
# up and bound their ports", not anything semantic about WebRTC media.
# Run after `make voice-translator-up`.
voice-translator-smoke:
	@echo "==> waiting up to 10s for livekit on :7880"
	@for i in $$(seq 1 10); do \
	    if curl -fsS -m 1 http://127.0.0.1:7880/ >/dev/null 2>&1; then \
	        echo "==> livekit OK"; break; \
	    fi; \
	    sleep 1; \
	    if [ "$$i" = "10" ]; then echo "FAIL: livekit /:7880 unreachable"; exit 1; fi; \
	done
	@echo "==> probing coturn STUN on :3478/udp"
	@python3 -c 'import socket, secrets; \
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.settimeout(3); \
tid = secrets.token_bytes(12); \
s.sendto(b"\x00\x01\x00\x00\x21\x12\xa4\x42" + tid, ("127.0.0.1", 3478)); \
data, _ = s.recvfrom(1500); \
assert data[:2] == b"\x01\x01", "expected STUN Binding Success"; \
print("==> coturn OK"); s.close()'

# PURA-108 WS-7 / PURA-114 slice b — translator daemon scaffold. Connects to
# the running `voice-translator` profile (ts6-fixture + LiveKit + coturn),
# mints a LiveKit access token, drives the TS6 handshake, and exits cleanly
# after the configured duration. Useful as a deeper bring-up smoke test
# than `voice-translator-smoke` even before the audio forwarding lands in
# slices c-e. Operator notes: docs/voice-translator.md.

.PHONY: voice-translator-build voice-translator-run voice-translator-bridge-smoke \
        voice-translator-reverse-smoke voice-translator-test voice-translator-clean

VOICE_TRANSLATOR_DURATION ?= 10
VOICE_TRANSLATOR_OUT_DIR  ?= target/voice-translator
VOICE_TRANSLATOR_TS6      ?= 127.0.0.1:9987
VOICE_TRANSLATOR_LIVEKIT  ?= ws://127.0.0.1:7880
VOICE_TRANSLATOR_ROOM     ?= ts6-bridge

voice-translator-build:
	cargo build --release -p ts6-voice-translator

voice-translator-run: voice-translator-build
	@mkdir -p $(VOICE_TRANSLATOR_OUT_DIR)
	./target/release/ts6-voice-translator \
	    --ts6-server $(VOICE_TRANSLATOR_TS6) \
	    --identity-dir $(VOICE_TRANSLATOR_OUT_DIR) \
	    --livekit-url $(VOICE_TRANSLATOR_LIVEKIT) \
	    --livekit-room $(VOICE_TRANSLATOR_ROOM) \
	    --duration-secs $(VOICE_TRANSLATOR_DURATION)

voice-translator-test:
	cargo test -p ts6-voice-translator

# Slice-c bridge smoke: spin up the translator AND a TS6 prototype "alice"
# that talks 440Hz for 8s, verify the translator counted frames forwarded
# 1:1 from TS6 to LiveKit. Run after `make voice-translator-up`.
# Pass criterion: audio_frames_seen == audio_frames_published, both > 0.
voice-translator-bridge-smoke: voice-translator-build
	@cargo build --release -p ts6-voice-prototype
	@rm -rf $(VOICE_TRANSLATOR_OUT_DIR)/bridge-smoke
	@mkdir -p $(VOICE_TRANSLATOR_OUT_DIR)/bridge-smoke/translator \
	          $(VOICE_TRANSLATOR_OUT_DIR)/bridge-smoke/alice
	@echo "==> spawning translator (15s window) + alice (8s of 440Hz tone)"
	@./target/release/ts6-voice-translator \
	    --ts6-server $(VOICE_TRANSLATOR_TS6) \
	    --identity-dir $(VOICE_TRANSLATOR_OUT_DIR)/bridge-smoke/translator \
	    --livekit-url $(VOICE_TRANSLATOR_LIVEKIT) \
	    --livekit-room $(VOICE_TRANSLATOR_ROOM) \
	    --duration-secs 15 \
	    >$(VOICE_TRANSLATOR_OUT_DIR)/bridge-smoke/translator.log 2>&1 & \
	  TRANS=$$!; \
	  sleep 5; \
	  ./target/release/ts6-voice-prototype \
	      --server $(VOICE_TRANSLATOR_TS6) \
	      --name alice --send-tone-hz 440 --duration-secs 8 \
	      --identity-dir $(VOICE_TRANSLATOR_OUT_DIR)/bridge-smoke/alice \
	      --out-wav $(VOICE_TRANSLATOR_OUT_DIR)/bridge-smoke/alice.wav \
	      >$(VOICE_TRANSLATOR_OUT_DIR)/bridge-smoke/alice.log 2>&1; \
	  wait $$TRANS
	@grep -E "audio_frames_(seen|published)|exited cleanly" \
	    $(VOICE_TRANSLATOR_OUT_DIR)/bridge-smoke/translator.log | tail -3
	@python3 -c 'import re; \
raw = open("$(VOICE_TRANSLATOR_OUT_DIR)/bridge-smoke/translator.log").read(); \
log = re.sub(r"\x1b\[[0-9;]*[mGKH]", "", raw); \
m = re.search(r"exited cleanly.*?audio_frames_seen=(\d+).*?audio_frames_published=(\d+)", log, re.DOTALL); \
assert m, "translator did not exit cleanly with the audio counters line"; \
seen, pub = int(m.group(1)), int(m.group(2)); \
print(f"==> bridge frames: seen={seen} published={pub}"); \
assert seen > 0, f"expected audio_frames_seen > 0, got {seen}"; \
assert seen == pub, f"expected seen == published, got seen={seen} published={pub}"; \
print("==> bridge smoke OK")'


# Slice-d reverse-path smoke. Spawns two translator instances (T1, T2)
# joined to the same LiveKit room with distinct identities; each subscribes
# to the other's published track and forwards remote Opus into TS6 as a
# synthetic-client send. Pass criterion: at least one instance reports
# `reverse_frames_received > 0`, proving the LiveKit → TS6 forwarding
# path fires end-to-end. Looser than the slice-c bridge smoke because
# without a non-TS6 audio source the closed loop is symmetric and prone
# to amplification — slice e's browser demo is the natural acceptance
# gate for the full reverse path.
#
# Operator note: this test brings up two TS6 connections from 127.0.0.1
# in quick succession; if you've recently run other TS6 prototypes on
# the same fixture the IP rate-limiter may ban one of the connections.
# Reset the fixture data volume first if so:
#
#   JWT_SECRET=devjwt podman-compose --profile voice-translator down -v
#   make voice-translator-up
voice-translator-reverse-smoke: voice-translator-build
	@cargo build --release -p ts6-voice-prototype
	@rm -rf $(VOICE_TRANSLATOR_OUT_DIR)/reverse-smoke
	@mkdir -p $(VOICE_TRANSLATOR_OUT_DIR)/reverse-smoke/t1 \
	          $(VOICE_TRANSLATOR_OUT_DIR)/reverse-smoke/t2 \
	          $(VOICE_TRANSLATOR_OUT_DIR)/reverse-smoke/alice
	@echo "==> spawning T1 (28s) + T2 (22s) + alice (12s of 440Hz) in the same LiveKit room"
	@./target/release/ts6-voice-translator \
	    --ts6-server $(VOICE_TRANSLATOR_TS6) \
	    --identity-dir $(VOICE_TRANSLATOR_OUT_DIR)/reverse-smoke/t1 \
	    --ts6-name translator-1 \
	    --livekit-url $(VOICE_TRANSLATOR_LIVEKIT) \
	    --livekit-room $(VOICE_TRANSLATOR_ROOM) \
	    --livekit-identity translator-1 \
	    --duration-secs 28 \
	    >$(VOICE_TRANSLATOR_OUT_DIR)/reverse-smoke/t1.log 2>&1 & \
	  T1=$$!; \
	  sleep 6; \
	  ./target/release/ts6-voice-translator \
	      --ts6-server $(VOICE_TRANSLATOR_TS6) \
	      --identity-dir $(VOICE_TRANSLATOR_OUT_DIR)/reverse-smoke/t2 \
	      --ts6-name translator-2 \
	      --livekit-url $(VOICE_TRANSLATOR_LIVEKIT) \
	      --livekit-room $(VOICE_TRANSLATOR_ROOM) \
	      --livekit-identity translator-2 \
	      --duration-secs 22 \
	      >$(VOICE_TRANSLATOR_OUT_DIR)/reverse-smoke/t2.log 2>&1 & \
	  T2=$$!; \
	  sleep 4; \
	  ./target/release/ts6-voice-prototype \
	      --server $(VOICE_TRANSLATOR_TS6) \
	      --name alice --send-tone-hz 440 --duration-secs 12 \
	      --identity-dir $(VOICE_TRANSLATOR_OUT_DIR)/reverse-smoke/alice \
	      --out-wav $(VOICE_TRANSLATOR_OUT_DIR)/reverse-smoke/alice.wav \
	      >$(VOICE_TRANSLATOR_OUT_DIR)/reverse-smoke/alice.log 2>&1; \
	  wait $$T2; wait $$T1
	@SUM=0; for tag in t1 t2; do \
	    LOG=$(VOICE_TRANSLATOR_OUT_DIR)/reverse-smoke/$$tag.log; \
	    PLAIN=$$(sed 's/\x1b\[[0-9;]*[mGKH]//g' $$LOG); \
	    N=$$(printf '%s' "$$PLAIN" | grep -oE 'exited cleanly.*reverse_frames_received=[0-9]+' \
	        | grep -oE '[0-9]+$$' | head -1); \
	    [ -z "$$N" ] && N=0; \
	    echo "==> $$tag: reverse_frames_received=$$N"; \
	    SUM=$$((SUM + N)); \
	done; \
	if [ "$$SUM" -gt 0 ]; then \
	    echo "==> reverse smoke OK (total received=$$SUM)"; \
	else \
	    echo "FAIL: expected at least one translator to report reverse_frames_received > 0"; \
	    exit 1; \
	fi

voice-translator-clean:
	rm -rf $(VOICE_TRANSLATOR_OUT_DIR)

# PURA-108 WS-7 / PURA-114 slice e — browser demo.
#
# `voice-translator-mint-token` prints a LiveKit JWT to stdout for a
# given identity + room. Operator pastes it into deploy/voice/demo.html
# in a browser. `voice-translator-demo-up` serves deploy/voice as a
# rootless static site on :8080 so the operator opens
# http://localhost:8080/demo.html without an npm/build chain.
#
# Together these close the description's acceptance bar (TS6 client +
# browser tab, ≥30 s bidirectional audible voice), gated by the
# operator's manual ear-check.

.PHONY: voice-translator-mint-token voice-translator-demo-up

# Override-able knobs; `IDENTITY` is the only one you usually pass.
VOICE_TRANSLATOR_IDENTITY ?= browser-demo
VOICE_TRANSLATOR_DEMO_PORT ?= 8080

voice-translator-mint-token: voice-translator-build
	@./target/release/ts6-voice-translator \
	    --livekit-url $(VOICE_TRANSLATOR_LIVEKIT) \
	    --livekit-room $(VOICE_TRANSLATOR_ROOM) \
	    --livekit-identity $(VOICE_TRANSLATOR_IDENTITY) \
	    --duration-secs 3600 \
	    --print-token

voice-translator-demo-up:
	@echo "==> serving deploy/voice on :$(VOICE_TRANSLATOR_DEMO_PORT)"
	@echo "==> open http://localhost:$(VOICE_TRANSLATOR_DEMO_PORT)/demo.html"
	@cd deploy/voice && python3 -m http.server $(VOICE_TRANSLATOR_DEMO_PORT)
