/**
 * #233 post-deploy gates (pure; unit-tested in the mock suite by
 * audio-outputs-gate.spec.ts): FOH still gets SongPlayer's 48 kHz INT24
 * `sp-program` through the output list (the migration kept it byte-identical),
 * and a VBAN destination at another rate carries that rate's index with a
 * contiguous frame counter.
 */

export interface VbanTelemetry {
  stream_name: string;
  packets_sent: number;
  send_errors: number;
  targets: { target: string; addr: string | null; error: string | null }[];
}

/** `GET /api/v1/program` → `outputs[i]` (the fields the gates read). */
export interface OutputStatus {
  id: string;
  type: string;
  name: string;
  enabled: boolean;
  state: string;
  reason: string | null;
  rate: number;
  format: string;
  channels: number;
  delay_ms: number;
  latency_ms: number;
  blocks_sent: number;
  blocks_dropped: number;
  vban?: VbanTelemetry;
}

/** SNV's FOH destination (#210's first target, migrated as out-1). */
export const FOH_TARGET = "fohabl.lan:6980";
/** Blocks that must go out between the two reads (30 per second). */
export const MIN_BLOCKS = 25;

function fohOf(list: OutputStatus[]): OutputStatus | undefined {
  return list.find((o) => o.type === "vban" && o.vban?.targets[0]?.target === FOH_TARGET);
}

/** Why FOH's path is not #210's any more, between two reads of `outputs[]`. */
export function fohPathFailures(first: OutputStatus[], second: OutputStatus[]): string[] {
  const a = fohOf(first);
  const b = fohOf(second);
  if (!a || !b) return [`no VBAN output to ${FOH_TARGET}`];
  const f: string[] = [];
  if (b.state !== "running") f.push(`the FOH output is ${b.state}${b.reason ? ` (${b.reason})` : ""}`);
  if (b.rate !== 48000) f.push(`the FOH output runs at ${b.rate} Hz, not 48000`);
  if (b.format !== "int24") f.push(`the FOH output sends ${b.format}, not int24`);
  if (b.vban?.stream_name !== "sp-program") f.push(`the FOH stream is ${b.vban?.stream_name}, not sp-program`);
  if (b.delay_ms !== 0) f.push(`the FOH output is delayed ${b.delay_ms} ms`);
  const sent = b.blocks_sent - a.blocks_sent;
  if (sent < MIN_BLOCKS) f.push(`only ${sent} FOH blocks went out between the reads`);
  const errors = (b.vban?.send_errors ?? 0) - (a.vban?.send_errors ?? 0);
  if (errors > 0) f.push(`${errors} FOH send errors between the reads`);
  return f;
}

export interface VbanHeader {
  srIndex: number;
  subProtocol: number;
  frames: number;
  channels: number;
  formatBit: number;
  stream: string;
  counter: number;
}

/** One datagram's VBAN header, `null` when it is not VBAN. */
export function parseVbanHeader(b: Uint8Array): VbanHeader | null {
  if (b.length < 28 || b[0] !== 0x56 || b[1] !== 0x42 || b[2] !== 0x41 || b[3] !== 0x4e) return null;
  const name = b.subarray(8, 24);
  const end = name.indexOf(0);
  return {
    srIndex: b[4] & 0x1f,
    subProtocol: b[4] >> 5,
    frames: b[5] + 1,
    channels: b[6] + 1,
    formatBit: b[7],
    stream: String.fromCharCode(...(end < 0 ? name : name.subarray(0, end))),
    counter: new DataView(b.buffer, b.byteOffset, b.length).getUint32(24, true),
  };
}

export interface ReceiverWant {
  srIndex: number;
  frames: number;
  formatBit: number;
  stream: string;
  minPackets: number;
}

/** A receiver's verdict on a destination's datagrams, in arrival order. */
export function receiverFailures(packets: Uint8Array[], want: ReceiverWant): string[] {
  const f: string[] = [];
  const headers = packets.map(parseVbanHeader);
  if (headers.some((h) => h === null)) f.push("a datagram is not VBAN");
  const ok = headers.filter((h): h is VbanHeader => h !== null);
  if (ok.length < want.minPackets) f.push(`${ok.length} packets, want at least ${want.minPackets}`);
  const bad = ok.find(
    (h) =>
      h.srIndex !== want.srIndex ||
      h.subProtocol !== 0 ||
      h.frames !== want.frames ||
      h.channels !== 2 ||
      h.formatBit !== want.formatBit ||
      h.stream !== want.stream,
  );
  if (bad) f.push(`a packet carries ${JSON.stringify(bad)}`);
  for (let i = 1; i < ok.length; i++) {
    if (ok[i].counter !== ((ok[i - 1].counter + 1) >>> 0)) {
      f.push(`the frame counter jumps ${ok[i - 1].counter} -> ${ok[i].counter}`);
      break;
    }
  }
  return f;
}
