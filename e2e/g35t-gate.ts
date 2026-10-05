/**
 * The decision of the Gemini 3.5 Transcribe live gate (#144,
 * `post-deploy-g35t.spec.ts`), as a pure function over the probe's answer so
 * the mock suite tests it (`g35t-gate.spec.ts`).
 *
 * `POST /api/v1/lyrics/g35t/probe` sends one short real clip from the box
 * through the lyrics worker's own call (`lyrics::g35t_probe`): the same
 * upload, request body, language hint and Gemini key rotation as a song.
 * The deploy fails unless the model answered the clip with words, under the
 * model and the language hint the lyrics worker sends. It also fails on any
 * key in `refused_keys` refused for a reason other than a 429 (a dead or
 * invalid key), even when a later key answered: a dead Gemini key must never
 * go unnoticed with CI green (#144 comment 5999711400). A 429 there passes:
 * it is a quota state, not a dead key. Keys after the answering one are not
 * tried.
 */

/** The model every transcription request names (`g35t_client::MODEL_SLUG`). */
export const G35T_MODEL = "gemini-3.5-transcribe";

/** The language hint every request carries (`g35t_client::LANGUAGE_CODES`,
 * #144: the catalogue sings in English and Spanish). */
export const G35T_LANGUAGE_CODES = ["en-US", "es-419"];

/** The clip the probe sent (`g35t_probe::ClipInfo`). */
export interface G35tClip {
  youtube_id: string;
  /** "isolated_vocal" (the `{yt}_vocals16k.wav` the worker uploads),
   * "vocal_stem" (the karaoke vocal stem) or "mix" (the song's audio). */
  source: string;
  start_ms: number;
  duration_ms: number;
}

/** A Gemini key the probe moved past (`g35t_client::KeyRefusal`). */
export interface G35tKeyRefusal {
  /** 0-based place in the key list. */
  key_index: number;
  /** A 429 (out of quota now); false = a key refusal (a 403, or a 400 naming
   * the key: dead, invalid, or not allowed this model or API; read
   * `error`). */
  rate_limited: boolean;
  /** Why (a 429 or a key refusal), never the key. */
  error: string;
}

/** `POST /api/v1/lyrics/g35t/probe`'s answer (`g35t_probe::G35tProbeReport`). */
export interface G35tProbe {
  ok: boolean;
  model: string;
  /** The Gemini key (0-based place in the list) that answered, or whose
   * answer ended the call; null when no key was tried, or the call was cut
   * by the probe's bound. */
  key_index: number | null;
  language_codes: string[];
  word_count: number;
  latency_ms: number;
  /** Why the probe failed (never a key); null when ok. */
  error: string | null;
  /** Keys refused before the one that decided the outcome; every key when
   * all were refused. */
  refused_keys: G35tKeyRefusal[];
  clip: G35tClip | null;
  /** The first words heard. */
  sample: string;
}

/** Every reason `probe` fails the gate; `[]` = the gate passes. */
export function g35tGateFailures(probe: G35tProbe): string[] {
  const failures: string[] = [];
  if (!probe.ok) {
    failures.push(`the probe failed: ${probe.error ?? "(no error text)"}`);
  }
  if (probe.word_count <= 0) {
    failures.push("the model transcribed no words");
  }
  if (probe.model !== G35T_MODEL) {
    failures.push(`the build sends model ${JSON.stringify(probe.model)}, not ${G35T_MODEL}`);
  }
  if (JSON.stringify(probe.language_codes) !== JSON.stringify(G35T_LANGUAGE_CODES)) {
    failures.push(
      `the build hints ${JSON.stringify(probe.language_codes)}, ` +
        `not ${JSON.stringify(G35T_LANGUAGE_CODES)}`,
    );
  }
  for (const refusal of probe.refused_keys) {
    if (!refusal.rate_limited) {
      failures.push(
        `key ${refusal.key_index + 1} was refused, not rate-limited ` +
          `(a dead or invalid key): ${refusal.error}`,
      );
    }
  }
  return failures;
}
