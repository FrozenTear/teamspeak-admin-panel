// moq-spike WS-0 reference player — no build step, no framework.
//
// Implements a minimal moq-lite (draft-04) subscriber over vanilla WebTransport.
// Media format: raw VP8 frames → WebCodecs VideoDecoder → <canvas>.
//
// Wire protocol reference: https://moq-dev.github.io/drafts/draft-lcurley-moq-lite.html
// See also: docs/adr/0007-moq-flavor-and-draft-pin.md

// ---------------------------------------------------------------------------
// QUIC/moq-lite varint helpers
// ---------------------------------------------------------------------------

function varintSize(n) {
  if (n < 0x40) return 1;
  if (n < 0x4000) return 2;
  if (n < 0x40000000) return 4;
  return 8;
}

function writeVarint(buf, offset, n) {
  if (n < 0x40) {
    buf[offset] = n;
    return 1;
  }
  if (n < 0x4000) {
    buf[offset]     = 0x40 | (n >> 8);
    buf[offset + 1] = n & 0xff;
    return 2;
  }
  if (n < 0x40000000) {
    buf[offset]     = 0x80 | (n >> 24);
    buf[offset + 1] = (n >> 16) & 0xff;
    buf[offset + 2] = (n >>  8) & 0xff;
    buf[offset + 3] =  n        & 0xff;
    return 4;
  }
  // 8-byte — n must be a BigInt or a safe integer in 0..2^62-1
  const hi = Math.floor(n / 0x100000000);
  const lo = n >>> 0;
  buf[offset]     = 0xc0 | ((hi >> 24) & 0x3f);
  buf[offset + 1] = (hi >> 16) & 0xff;
  buf[offset + 2] = (hi >>  8) & 0xff;
  buf[offset + 3] =  hi        & 0xff;
  buf[offset + 4] = (lo >> 24) & 0xff;
  buf[offset + 5] = (lo >> 16) & 0xff;
  buf[offset + 6] = (lo >>  8) & 0xff;
  buf[offset + 7] =  lo        & 0xff;
  return 8;
}

// StreamReader: reads from a WebTransport ReadableStream in buffered chunks.
class StreamReader {
  constructor(readable) {
    this._reader = readable.getReader();
    this._buf = new Uint8Array(0);
    this._done = false;
  }

  async _fill() {
    if (this._done) return false;
    const { value, done } = await this._reader.read();
    if (done) { this._done = true; return false; }
    if (!value?.byteLength) throw new Error("empty chunk");
    const tmp = new Uint8Array(this._buf.length + value.length);
    tmp.set(this._buf);
    tmp.set(value, this._buf.length);
    this._buf = tmp;
    return true;
  }

  async _fillTo(n) {
    while (this._buf.length < n) {
      if (!await this._fill()) throw new Error("unexpected end of stream");
    }
  }

  async readBytes(n) {
    await this._fillTo(n);
    const out = this._buf.slice(0, n);
    this._buf = this._buf.slice(n);
    return out;
  }

  async readAll() {
    while (await this._fill()) {}
    const out = this._buf;
    this._buf = new Uint8Array(0);
    return out;
  }

  async isDone() {
    if (this._buf.length > 0) return false;
    return !(await this._fill());
  }

  async u8() {
    const b = await this.readBytes(1);
    return b[0];
  }

  async uvarint() {
    // QUIC variable-length integer (RFC 9000 §16)
    const first = (await this.readBytes(1))[0];
    const prefix = first >> 6;
    let value = first & 0x3f;
    if (prefix === 0) return value;
    if (prefix === 1) {
      const b = (await this.readBytes(1))[0];
      return (value << 8) | b;
    }
    if (prefix === 2) {
      const rest = await this.readBytes(3);
      return (value << 24) | (rest[0] << 16) | (rest[1] << 8) | rest[2];
    }
    // 8-byte (BigInt path, then convert to Number if safe)
    const rest = await this.readBytes(7);
    let n = BigInt(value);
    for (const b of rest) n = (n << 8n) | BigInt(b);
    return Number(n); // safe for subscribe IDs in our spike
  }

  async string() {
    const len = await this.uvarint();
    const bytes = await this.readBytes(len);
    return new TextDecoder().decode(bytes);
  }
}

// StreamWriter: wraps a WritableStream and provides moq-lite encoding helpers.
class StreamWriter {
  constructor(writable) {
    this._w = writable.getWriter();
  }

  async write(bytes) {
    await this._w.write(bytes);
  }

  async close() {
    await this._w.close();
  }

  // Encodes a length-prefixed message.
  async writeMessage(buildFn) {
    const chunks = [];
    let size = 0;
    const fakeWriter = {
      async write(b) { chunks.push(b); size += b.length; },
    };
    await buildFn(fakeWriter);
    const buf = new Uint8Array(8 + size);
    const varintLen = writeVarint(buf, 0, size);
    let off = varintLen;
    for (const chunk of chunks) {
      buf.set(chunk, off);
      off += chunk.length;
    }
    await this._w.write(buf.subarray(0, varintLen + size));
  }

  async writeVarintBytes(n) {
    const buf = new Uint8Array(8);
    const len = writeVarint(buf, 0, n);
    await this._w.write(buf.subarray(0, len));
  }

  async writeString(s) {
    const enc = new TextEncoder().encode(s);
    await this.writeVarintBytes(enc.length);
    await this._w.write(enc);
  }

  async writeBool(b) {
    await this._w.write(new Uint8Array([b ? 1 : 0]));
  }

  async writeU8(n) {
    await this._w.write(new Uint8Array([n & 0xff]));
  }
}

// ---------------------------------------------------------------------------
// moq-lite subscribe logic
// ---------------------------------------------------------------------------

// Stream type IDs (bidi streams opened by subscriber).
const STREAM_SUBSCRIBE = 2; // 0x02

// Subscribe message fields (draft-04+).
async function sendSubscribe(writer, id, broadcast, track) {
  // Write stream type (u53 varint, not length-prefixed).
  await writer.writeVarintBytes(STREAM_SUBSCRIBE);

  // Write the SUBSCRIBE message (length-prefixed body).
  await writer.writeMessage(async (w) => {
    const enc = (s) => new TextEncoder().encode(s);
    const broadcastBytes = enc(broadcast);
    const trackBytes = enc(track);

    // id (u62 = QUIC varint)
    const idBuf = new Uint8Array(8);
    const idLen = writeVarint(idBuf, 0, id);
    await w.write(idBuf.subarray(0, idLen));

    // broadcast path (length-prefixed string)
    const blen = new Uint8Array(8); writeVarint(blen, 0, broadcastBytes.length);
    await w.write(blen.subarray(0, varintSize(broadcastBytes.length)));
    await w.write(broadcastBytes);

    // track name (length-prefixed string)
    const tlen = new Uint8Array(8); writeVarint(tlen, 0, trackBytes.length);
    await w.write(tlen.subarray(0, varintSize(trackBytes.length)));
    await w.write(trackBytes);

    // priority (u8 = 0 = default)
    await w.write(new Uint8Array([0]));

    // ordered (bool = true = ascending)
    await w.write(new Uint8Array([1]));

    // maxLatency (varint = 0 = no limit)
    await w.write(new Uint8Array([0]));

    // startGroup (varint = 0 = latest group)
    await w.write(new Uint8Array([0]));

    // endGroup (varint = 0 = unbounded)
    await w.write(new Uint8Array([0]));
  });
}

// Read SUBSCRIBE_OK (draft-04: type varint 0, then length-prefixed body).
async function readSubscribeOk(reader) {
  const typ = await reader.uvarint(); // 0x0 = ok
  if (typ !== 0) throw new Error(`unexpected subscribe response type: ${typ}`);
  const bodyLen = await reader.uvarint(); // body length
  // Drain the body so the stream cursor is past it.
  if (bodyLen > 0) await reader.readBytes(bodyLen);
}

// ---------------------------------------------------------------------------
// WebCodecs VP8 decode + canvas render
// ---------------------------------------------------------------------------

class Renderer {
  constructor(canvas) {
    this._canvas = canvas;
    this._ctx = canvas.getContext("2d");
    this._decoder = null;
    this._frameSeq = 0;
  }

  init(width, height) {
    this._canvas.width = width;
    this._canvas.height = height;

    this._decoder = new VideoDecoder({
      output: (frame) => {
        this._ctx.drawImage(frame, 0, 0);
        frame.close();
      },
      error: (e) => log(`VideoDecoder error: ${e.message}`),
    });

    this._decoder.configure({ codec: "vp8" });
  }

  decode(data, isKeyframe) {
    if (!this._decoder || this._decoder.state === "closed") return;
    const chunk = new EncodedVideoChunk({
      type: isKeyframe ? "key" : "delta",
      timestamp: this._frameSeq * 33333, // ~30fps in microseconds
      data,
    });
    this._frameSeq++;
    this._decoder.decode(chunk);
  }

  close() {
    if (this._decoder && this._decoder.state !== "closed") {
      this._decoder.close();
    }
  }
}

// Determine if a raw VP8 bitstream frame is a keyframe.
// VP8 frame: byte[0] bit 0 == 0 → keyframe.
function vp8IsKeyframe(data) {
  return (data[0] & 0x01) === 0;
}

// ---------------------------------------------------------------------------
// Main session
// ---------------------------------------------------------------------------

class MoqSession {
  constructor(relayUrl, namespace, canvas) {
    this._relayUrl = relayUrl;
    this._namespace = namespace;
    this._canvas = canvas;
    this._wt = null;
    this._stopped = false;
  }

  async start() {
    log(`connecting to ${this._relayUrl}`);

    // Fetch the relay's self-signed certificate hash for WebTransport trust.
    // The relay serves the hex SHA-256 of the raw DER certificate at /certificate.sha256.
    // We convert to a binary ArrayBuffer for the WebTransport serverCertificateHashes API.
    const certOpts = await this._fetchCertHash();

    // moq-lite-04 uses ALPN-based version negotiation (no SETUP message).
    // The "protocols" field sets the WebTransport subprotocol; the relay reads it
    // via session.protocol() and dispatches to the moq-lite-04 handler, which
    // expects bare ControlType + Subscribe with no prior SETUP exchange.
    this._wt = new WebTransport(this._relayUrl, {
      protocols: ["moq-lite-04"],
      ...certOpts,
    });

    await this._wt.ready;
    log("WebTransport connected");

    // Subscribe to the video track.
    const subscribeStream = await this._wt.createBidirectionalStream();
    const subWriter = new StreamWriter(subscribeStream.writable);
    const subReader = new StreamReader(subscribeStream.readable);

    await sendSubscribe(subWriter, 0, this._namespace, "video");
    log(`SUBSCRIBE sent → ${this._namespace}/video`);

    // Wait for SUBSCRIBE_OK.
    await readSubscribeOk(subReader);
    log("SUBSCRIBE_OK received");

    // Renderer init — we'll set dimensions on first keyframe.
    const renderer = new Renderer(this._canvas);

    // Drain incoming unidirectional streams (group data streams).
    await this._receiveGroups(renderer);
  }

  async _receiveGroups(renderer) {
    const uniStreams = this._wt.incomingUnidirectionalStreams;
    const reader = uniStreams.getReader();

    let initialized = false;

    for (;;) {
      if (this._stopped) break;
      const { value: stream, done } = await reader.read();
      if (done) break;

      // Handle each group stream in its own async task.
      this._handleGroup(stream, renderer, initialized)
        .then((wasKey) => {
          if (wasKey) initialized = true;
        })
        .catch((err) => {
          if (!this._stopped) log(`group error: ${err.message}`);
        });
    }
  }

  async _handleGroup(readable, renderer, initialized) {
    const r = new StreamReader(readable);

    // Unidirectional stream first varint = DataType (0 = group).
    const streamType = await r.uvarint();
    if (streamType !== 0x00) {
      log(`unknown uni stream type: ${streamType}, skipping`);
      return false;
    }

    // GROUP message (length-prefixed): subscribe_id (u62), sequence (u53).
    const _msgLen = await r.uvarint();
    const _subscribeId = await r.uvarint();
    const _sequence = await r.uvarint();

    // Read all FRAME messages until stream closes.
    let firstFrame = true;
    let isGroupKeyframe = false;

    for (;;) {
      const frameLen = await r.uvarint().catch(() => null);
      if (frameLen === null) break; // stream ended

      const frameData = await r.readBytes(frameLen);
      const isKey = vp8IsKeyframe(frameData);

      if (firstFrame) {
        isGroupKeyframe = isKey;
        firstFrame = false;
      }

      if (!initialized && !isKey) continue; // wait for first keyframe

      if (isKey && !initialized) {
        // IVF fixture is 1280×720 (set in fixture/build.sh).
        renderer.init(1280, 720);
        log("VideoDecoder configured (640×360 VP8)");
        initialized = true;
      }

      renderer.decode(frameData, isKey);
    }

    return isGroupKeyframe;
  }

  async _fetchCertHash() {
    // Derive the relay's HTTP origin from its WebTransport URL.
    const u = new URL(this._relayUrl);
    const certUrl = `http://${u.hostname}:${u.port}/certificate.sha256`;
    try {
      const resp = await fetch(certUrl);
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const hex = (await resp.text()).trim();
      log(`cert hash fetched (${hex.slice(0, 8)}…)`);
      const bytes = new Uint8Array(hex.match(/../g).map((b) => parseInt(b, 16)));
      return {
        serverCertificateHashes: [{ algorithm: "sha-256", value: bytes.buffer }],
      };
    } catch (err) {
      log(`cert fetch failed: ${err.message}. Falling back to no-hash (requires --ignore-certificate-errors).`);
      return {};
    }
  }

  stop() {
    this._stopped = true;
    if (this._wt) {
      this._wt.close();
      this._wt = null;
    }
  }
}

// ---------------------------------------------------------------------------
// UI wiring
// ---------------------------------------------------------------------------

const $ = (id) => document.getElementById(id);

function log(msg) {
  const el = $("log");
  const stamp = new Date().toISOString().slice(11, 23);
  el.textContent += `\n[${stamp}] ${msg}`;
  el.scrollTop = el.scrollHeight;
}

function probe() {
  const caps = {
    WebTransport: typeof WebTransport === "function",
    VideoDecoder: typeof VideoDecoder === "function",
    AudioDecoder: typeof AudioDecoder === "function",
  };
  log("capability probe:");
  for (const [k, v] of Object.entries(caps)) log(`  ${v ? "✓" : "✗"} ${k}`);
  const ok = Object.values(caps).every(Boolean);
  if (!ok) log("✗ missing required browser APIs");
  return ok;
}

function init() {
  if (!probe()) {
    $("subscribe").disabled = true;
    return;
  }

  // Apply URL search params to form defaults, then auto-submit if both present.
  const params = new URLSearchParams(location.search);
  if (params.get("relay")) $("relay").value = params.get("relay");
  if (params.get("ns")) $("namespace").value = params.get("ns");

  const canvas = $("video");
  const form = $("connect-form");
  const stopBtn = $("stop");
  let session = null;

  form.addEventListener("submit", async (ev) => {
    ev.preventDefault();
    const relay = $("relay").value.trim();
    const ns = $("namespace").value.trim();
    if (!relay || !ns) { log("✗ relay and namespace required"); return; }

    $("subscribe").disabled = true;
    stopBtn.disabled = false;

    try {
      session = new MoqSession(relay, ns, canvas);
      await session.start();
    } catch (err) {
      if (!session?._stopped) log(`✗ ${err.message ?? err}`);
    } finally {
      $("subscribe").disabled = false;
      stopBtn.disabled = true;
    }
  });

  stopBtn.addEventListener("click", () => {
    session?.stop();
    session = null;
    $("subscribe").disabled = false;
    stopBtn.disabled = true;
    log("stopped.");
  });

  // Auto-subscribe when both params are present in the URL.
  if (params.get("relay") && params.get("ns")) {
    log("auto-subscribing from URL params…");
    form.dispatchEvent(new Event("submit", { cancelable: true, bubbles: false }));
  }
}

init();
