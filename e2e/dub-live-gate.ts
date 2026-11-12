/**
 * The decision of the Gemini Live Translate live gate (#232 unit 2,
 * `post-deploy-dub-live.spec.ts`), as a pure function over the probe's answer
 * so the mock suite tests it (`dub-live-gate.spec.ts`).
 *
 * `POST /api/v1/dabing/probe` dubs one short real clip from the box through
 * the dub worker's own child call (`dabing::probe`): the same scripts, the
 * same continuous Live session, the `dub_model` / `dub_voice` settings and the
 * first Gemini key. The deploy fails unless the probe ran (200, never a 409
 * refusal: no key, no venv python, no song, paid AI off), the child finished,
 * the dub holds sound (at least `DUB_MIN_RMS_DBFS`, the probe's own floor) and
 * the session's transcripts make at least one Slovak subtitle line. The model
 * is a setting (an upgrade is a settings change), so the gate does not pin it;
 * the spec logs it.
 */

/** The probe's floor for the dub's RMS (`dabing::probe::MIN_DUB_RMS_DBFS`). */
export const DUB_MIN_RMS_DBFS = -45;

/** The child's session stats (`dabing::child::DubSessionStats`). */
export interface DubSession {
  connections: number | null;
  reconnects: number | null;
  output_to_input_ratio: number | null;
  max_voiced_gap_s: number | null;
  latency_ms: number | null;
  drain_end_reason: string | null;
}

/** `POST /api/v1/dabing/probe`'s answer (`dabing::probe::DubProbeReport`). */
export interface DubProbe {
  ok: boolean;
  model: string;
  voice: string;
  youtube_id: string | null;
  start_ms: number | null;
  clip_ms: number;
  /** "vocal_stem" or "audio". */
  source: string | null;
  elapsed_ms: number;
  dub_ms: number;
  dub_rms_dbfs: number | null;
  sk_lines: number;
  /** The first Slovak line. */
  sample: string;
  session: DubSession | null;
  /** Why the probe failed (never a key); null when ok. */
  error: string | null;
}

/** Every reason the answer `probe` (sent with HTTP `status`) fails the gate;
 * `[]` = the gate passes. */
export function dubLiveGateFailures(status: number, probe: DubProbe): string[] {
  const failures: string[] = [];
  if (status !== 200) {
    failures.push(`the probe was refused (HTTP ${status}): ${probe.error ?? "(no error text)"}`);
  } else if (!probe.ok) {
    failures.push(`the probe failed: ${probe.error ?? "(no error text)"}`);
  }
  if (probe.dub_ms <= 0) {
    failures.push("the probe made no dub");
  }
  if (probe.dub_rms_dbfs === null || probe.dub_rms_dbfs < DUB_MIN_RMS_DBFS) {
    failures.push(
      `the dub holds no sound (${probe.dub_rms_dbfs ?? "not read"} dBFS, floor ${DUB_MIN_RMS_DBFS})`,
    );
  }
  if (probe.sk_lines <= 0) {
    failures.push("the session gave no Slovak transcript line");
  }
  return failures;
}
