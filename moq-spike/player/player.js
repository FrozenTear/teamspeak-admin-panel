// moq-spike WS-0 reference player. No build step, no framework.
// See ../README.md and ../../docs/adr/0007-moq-flavor-and-draft-pin.md.
//
// Heartbeat 1 of PURA-138 lands the page wiring + capability probe.
// Heartbeat 3 lands the moq-lite subscribe wire format + WebCodecs
// VideoDecoder + AudioContext playback.

const $ = (id) => document.getElementById(id);
const log = (msg) => {
  const el = $("log");
  const stamp = new Date().toISOString().slice(11, 23);
  el.textContent += `\n[${stamp}] ${msg}`;
  el.scrollTop = el.scrollHeight;
};

function probe() {
  const capabilities = {
    WebTransport: typeof WebTransport === "function",
    WebCodecs_VideoDecoder: typeof VideoDecoder === "function",
    WebCodecs_AudioDecoder: typeof AudioDecoder === "function",
    AudioContext: typeof AudioContext === "function",
    ReadableStream_BYOB:
      typeof ReadableStream === "function" &&
      ReadableStream.prototype.getReader.length >= 0,
  };
  log("capability probe:");
  for (const [k, v] of Object.entries(capabilities)) {
    log(`  ${v ? "✓" : "✗"} ${k}`);
  }
  return Object.values(capabilities).every(Boolean);
}

async function subscribe(relayUrl, namespace) {
  log(`connecting to ${relayUrl} for namespace ${namespace}...`);

  // Heartbeat-3 work:
  // 1. const wt = new WebTransport(relayUrl);
  // 2. await wt.ready;
  // 3. send moq-lite SETUP frame on a bidi control stream.
  // 4. send moq-lite SUBSCRIBE for `${namespace}/video` and `${namespace}/audio`.
  // 5. for each incoming unidirectional stream:
  //    - parse moq-lite group header (track id, group seq).
  //    - dispatch frames to VideoDecoder / AudioDecoder.
  //    - on VideoDecoder.output → canvas.transferControlToOffscreen() or drawImage.
  //    - on AudioDecoder.output → AudioContext + AudioBufferSourceNode.
  // 6. expose a `stop()` that closes the WebTransport and tears decoders down.
  //
  // Wire format reference:
  //   https://moq-dev.github.io/drafts/draft-lcurley-moq-lite.html
  //   docs/adr/0007-moq-flavor-and-draft-pin.md

  log("subscribe path not yet implemented (see heartbeat 3 of PURA-138).");
  throw new Error("subscribe path not yet implemented");
}

function init() {
  if (!probe()) {
    log("✗ missing required browser APIs; aborting.");
    $("subscribe").disabled = true;
    return;
  }

  const form = $("connect-form");
  const stopBtn = $("stop");
  let session = null;

  form.addEventListener("submit", async (ev) => {
    ev.preventDefault();
    const relay = $("relay").value.trim();
    const ns = $("namespace").value.trim();
    if (!relay || !ns) {
      log("✗ relay and namespace are required.");
      return;
    }
    $("subscribe").disabled = true;
    stopBtn.disabled = false;
    try {
      session = await subscribe(relay, ns);
    } catch (err) {
      log(`✗ ${err.message ?? err}`);
      $("subscribe").disabled = false;
      stopBtn.disabled = true;
    }
  });

  stopBtn.addEventListener("click", () => {
    if (session && typeof session.stop === "function") {
      session.stop();
    }
    session = null;
    $("subscribe").disabled = false;
    stopBtn.disabled = true;
    log("stopped.");
  });
}

init();
