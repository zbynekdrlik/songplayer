/**
 * #233 post-deploy gates (pure; unit-tested in the mock suite by
 * audio-outputs-gate.spec.ts): FOH still gets SongPlayer's 48 kHz INT24
 * `sp-program` through the output list (the migration kept it byte-identical),
 * a VBAN destination at another rate carries that rate's index with a
 * contiguous frame counter, and (lane 3) an ASIO output runs a minute at its
 * driver's rate with no underrun and no reopen.
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
  /** #233 lane 3: an ASIO output's telemetry. */
  asio?: AsioTelemetry;
  /** A running driver off the network's rate or with a long buffer. */
  note?: string;
}

/** `outputs[i].asio` (the fields the gate reads). */
export interface AsioTelemetry {
  driver: string;
  channels: number[];
  driver_rate: number;
  sample_type: string;
  ppm: number;
  underruns: number;
  resets: number;
  /** The owner's "fault": a faded skip or insert (#233 review round 2). */
  hard_recentres: number;
  latency_ms: number;
  /** While waiting: the reason's stable code (`no_clock`: the driver gives no clock). */
  reason_code?: string | null;
}

/** The driver SNV's and PP's ASIO outputs play on. */
export const DVS_DRIVER = "Dante Virtual Soundcard (x64)";
/** One minute of program blocks (30 a second). */
export const WINDOW_BLOCKS = 1800;
/** The drift servo's bound, ppm (`asrc_servo::MAX_PPM`). */
export const MAX_PPM = 300;
/**
 * An output's latency is two grid slots + its own delay + the driver's
 * output latency: it must stay under a second ABOVE the delay.
 */
export const LATENCY_OVER_DELAY_MS = 1000;

/**
 * The ASIO outputs the gate counts and measures: the enabled ones, except
 * one waiting for its driver's clock (reason `no_clock`). The owner's
 * ruling, #233, 8.10.2026: at PP, DVS opens but gives no clock while PP's
 * network has no Dante PTP clock; the output waits calmly and is not
 * expected to run, so a box expecting no ASIO output does not fail on it.
 * A box that expects one (SNV, "1") still fails when its DVS gives no
 * clock: the count is then 0.
 */
export function gatedAsioOutputs(list: OutputStatus[]): OutputStatus[] {
  const waiting = waitingForClock(list);
  return list.filter((o) => o.type === "asio" && o.enabled && !waiting.includes(o));
}

/** The enabled ASIO outputs waiting for their driver's clock (`no_clock`). */
export function waitingForClock(list: OutputStatus[]): OutputStatus[] {
  return list.filter(
    (o) => o.type === "asio" && o.enabled && o.state === "waiting" && o.asio?.reason_code === "no_clock",
  );
}

/** Why an ASIO output's minute (two reads of `outputs[i]`) fails. */
export function asioGateFailures(first: OutputStatus, second: OutputStatus): string[] {
  const f: string[] = [];
  if (second.state !== "running") {
    f.push(`the ASIO output is ${second.state}${second.reason ? ` (${second.reason})` : ""}`);
  }
  const a = first.asio;
  const b = second.asio;
  if (!a || !b) return [...f, "no ASIO telemetry"];
  if (b.driver_rate <= 0 || second.rate !== b.driver_rate) {
    f.push(`it runs at ${second.rate} Hz, the driver at ${b.driver_rate} Hz`);
  }
  if (b.underruns > a.underruns) f.push(`${b.underruns - a.underruns} underruns in the window`);
  if (b.resets > a.resets) f.push(`the driver was reopened ${b.resets - a.resets} times in the window`);
  if (b.hard_recentres > a.hard_recentres) {
    f.push(`${b.hard_recentres - a.hard_recentres} hard re-centres in the window`);
  }
  if (Math.abs(b.ppm) > MAX_PPM) f.push(`its correction is ${b.ppm} ppm (bound ${MAX_PPM})`);
  const latencyBound = second.delay_ms + LATENCY_OVER_DELAY_MS;
  if (!(second.latency_ms > 0 && second.latency_ms < latencyBound)) f.push(`its latency is ${second.latency_ms} ms`);
  const blocks = second.blocks_sent - first.blocks_sent;
  if (blocks < WINDOW_BLOCKS) f.push(`only ${blocks} blocks in the window, want ${WINDOW_BLOCKS}`);
  return f;
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
