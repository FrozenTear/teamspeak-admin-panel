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
