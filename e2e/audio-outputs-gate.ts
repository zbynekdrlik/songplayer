/**
 * #233 post-deploy gates (pure; unit-tested in the mock suite by
 * audio-outputs-gate.spec.ts): lv1 gets SongPlayer's `LV1_RATE` INT24
 * `sp-program` through the output list,
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
  /** #285: a running native Dante output's stack figures. */
  dante?: DanteTelemetry;
}

/**
 * `outputs[i].dante` (`sp_core::dante_status::DanteFigures`, dstack's status
 * line with its TX flows summed). `late_bins` counts packets by how late they
 * left past their due time, at dstack's `LATE_EDGES_NS`: under 0.25, 0.5, 1,
 * 2 and 5 ms, then 5 ms or more — the last two bins are the packets 2 ms or
 * more late.
 */
export interface DanteTelemetry {
  clock_ready: boolean;
  live_flows: number;
  late_bins: number[];
  late_max_us: number;
  send_max_us: number;
  pump_late: number;
  pump_missed: number;
  pump_overruns: number;
}

/** How many of `late_bins`' last bins are "late" (2 ms or more). */
export const DANTE_LATE_TOP_BINS = 2;

/**
 * Why a native Dante output's minute fails on the stack's own figures (#285,
 * design 6809328088): the FOH console's per-window rule. Only a `type: dante`
 * output is judged (anything else: no failure). At both reads the media clock
 * runs; at the second a receiver holds a flow (FOH); the packets sent grew
 * (the `late_bins` sum); no packet 2 ms or more late (the last two bins) and
 * no pump late / missed / overrun in the minute. `late_max_us` is NOT gated:
 * it is a since-start maximum, so a start-up spike would fail every deploy.
 * Each failure names the figure and both reads.
 */
export function danteGateFailures(first: OutputStatus, second: OutputStatus): string[] {
  if (second.type !== "dante") return [];
  const a = first.dante;
  const b = second.dante;
  if (!a || !b) {
    const missing = [!a && "first", !b && "second"].filter(Boolean).join(" and ");
    return [`Dante output ${second.id}: no Dante figures (outputs[i].dante) at the ${missing} read`];
  }
  const f: string[] = [];
  const at = `Dante output ${second.id}`;
  if (!a.clock_ready || !b.clock_ready) {
    f.push(`${at}: clock_ready ${a.clock_ready} -> ${b.clock_ready}, want true at both reads`);
  }
  if (!(b.live_flows >= 1)) f.push(`${at}: live_flows ${a.live_flows} -> ${b.live_flows}, want at least 1`);
  const sum = (bins: number[]) => bins.reduce((s, n) => s + n, 0);
  if (!(sum(b.late_bins) > sum(a.late_bins))) {
    f.push(`${at}: packets sent (late_bins sum) ${sum(a.late_bins)} -> ${sum(b.late_bins)}, want growth`);
  }
  const top = (bins: number[]) => sum(bins.slice(-DANTE_LATE_TOP_BINS));
  if (b.late_bins.length !== a.late_bins.length || b.late_bins.length < DANTE_LATE_TOP_BINS) {
    f.push(`${at}: late_bins ${JSON.stringify(a.late_bins)} -> ${JSON.stringify(b.late_bins)}, want the same bins at both reads`);
  } else if (top(b.late_bins) > top(a.late_bins)) {
    f.push(`${at}: packets 2 ms or more late (last two late_bins) ${top(a.late_bins)} -> ${top(b.late_bins)}`);
  }
  for (const k of ["pump_late", "pump_missed", "pump_overruns"] as const) {
    if (b[k] > a[k]) f.push(`${at}: ${k} ${a[k]} -> ${b[k]}`);
  }
  return f;
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
  /** The driver's resync requests since the output was built (#233). */
  resyncs: number;
  latency_ms: number;
  /** While waiting: the reason's stable code (`no_clock`: the driver gives no clock;
   *  `clock_off_rate`: its card takes audio over 1 % off its rate, #229). */
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
 * The ASIO outputs the gate measures: the enabled ones, except one in a
 * calm wait for its driver's clock (`waitingForClock`). The owner's ruling,
 * #233, 8.10.2026: at PP, DVS opens but gives no clock while PP's network
 * has no Dante PTP clock; the output waits calmly and is not expected to
 * run. Since #229 (11.11.2026) such an output still counts toward
 * SP_ASIO_OUTPUTS_EXPECTED (`asioCount`), named and unmeasured.
 */
export function gatedAsioOutputs(list: OutputStatus[]): OutputStatus[] {
  const waiting = waitingForClock(list);
  return list.filter((o) => onDriverWorker(o) && o.enabled && !waiting.includes(o));
}

/** The outputs on the ASIO worker: a native Dante output runs on it too
 *  (#285) and reports the same `asio` status, so the same gates judge it. */
function onDriverWorker(o: OutputStatus): boolean {
  return o.type === "asio" || o.type === "dante";
}

/** The waits for a usable clock: none (`no_clock`), or a card that takes
 *  audio over 1 % off its rate (`clock_off_rate`, #229: PP's SoundGrid with
 *  its network off, by the owner's choice outside services). */
const CLOCK_WAITS = ["no_clock", "clock_off_rate"];

/** The enabled ASIO outputs waiting for a usable clock from their driver. */
export function waitingForClock(list: OutputStatus[]): OutputStatus[] {
  return list.filter(
    (o) =>
      onDriverWorker(o) &&
      o.enabled &&
      o.state === "waiting" &&
      CLOCK_WAITS.includes(o.asio?.reason_code ?? ""),
  );
}

/** What the live gate counts against SP_ASIO_OUTPUTS_EXPECTED (#229). */
export interface AsioCount {
  /** The outputs counted toward SP_ASIO_OUTPUTS_EXPECTED. */
  counted: number;
  /** The ids the gate measures (`gatedAsioOutputs`): each must run. */
  measured: string[];
  /** The ids in a calm wait for their driver's clock (`waitingForClock`),
   *  with the wait's code: counted, named, never measured. */
  waiting: string[];
}

/**
 * The count the live gate compares with SP_ASIO_OUTPUTS_EXPECTED (#229,
 * ROZHODNUTÉ 11.11.2026): every enabled ASIO / Dante output. One in a calm
 * clock wait (`no_clock`, `clock_off_rate`: its device or network is off,
 * e.g. PP's SoundGrid outside services) is an enabled output behaving
 * correctly, so it counts, is named and is not measured; every other one is
 * measured and must run (a waiting one with another reason fails at the
 * running poll). A disabled entry, a VBAN output and a missing output count
 * nothing.
 *
 * `clockWaitOk` (env SP_ASIO_CLOCK_WAIT_OK, set by deploy-pp.yml alone,
 * review F1): only there does a calm wait count. On resolume-snv a DVS or
 * native Dante output with no clock is a fault: it counts nothing, is still
 * named, and the deploy fails.
 */
export function asioCount(list: OutputStatus[], clockWaitOk: boolean): AsioCount {
  const measured = gatedAsioOutputs(list).map((o) => o.id);
  const waiting = waitingForClock(list).map((o) => `${o.id} (${o.asio?.reason_code})`);
  return { counted: measured.length + (clockWaitOk ? waiting.length : 0), measured, waiting };
}

/** The count poll's value: the count when it is the expected one, else the
 *  count with the measured and the waiting outputs named. */
export function asioCountReading(list: OutputStatus[], expected: number, clockWaitOk: boolean): number | string {
  const c = asioCount(list, clockWaitOk);
  if (c.counted === expected) return c.counted;
  return `${c.counted} (measured: ${c.measured.join(", ") || "none"}; waiting for a clock: ${c.waiting.join(", ") || "none"})`;
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

/** SNV's lv1 destination. FOH has no VBAN since 5.11.2026 (the owner, #257): it
 * takes SongPlayer over Dante, gated by `post-deploy-audio-asio.spec.ts`. */
export const LV1_TARGET = "lv1.lan:6980";
/** lv1's rate. */
export const LV1_RATE = 48000;
/** Blocks that must go out between the two reads (30 per second). */
export const MIN_BLOCKS = 25;

function lv1Of(list: OutputStatus[]): OutputStatus | undefined {
  return list.find((o) => o.type === "vban" && o.vban?.targets[0]?.target === LV1_TARGET);
}

/** Why lv1's path is broken, between two reads of `outputs[]`. */
export function lv1PathFailures(first: OutputStatus[], second: OutputStatus[]): string[] {
  const a = lv1Of(first);
  const b = lv1Of(second);
  if (!a || !b) return [`no VBAN output to ${LV1_TARGET}`];
  const f: string[] = [];
  if (b.state !== "running") f.push(`the lv1 output is ${b.state}${b.reason ? ` (${b.reason})` : ""}`);
  if (b.rate !== LV1_RATE) f.push(`the lv1 output runs at ${b.rate} Hz, not ${LV1_RATE}`);
  if (b.format !== "int24") f.push(`the lv1 output sends ${b.format}, not int24`);
  if (b.vban?.stream_name !== "sp-program") f.push(`the lv1 stream is ${b.vban?.stream_name}, not sp-program`);
  if (b.delay_ms !== 0) f.push(`the lv1 output is delayed ${b.delay_ms} ms`);
  const sent = b.blocks_sent - a.blocks_sent;
  if (sent < MIN_BLOCKS) f.push(`only ${sent} lv1 blocks went out between the reads`);
  const errors = (b.vban?.send_errors ?? 0) - (a.vban?.send_errors ?? 0);
  if (errors > 0) f.push(`${errors} lv1 send errors between the reads`);
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
