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
