/**
 * The decision of the OpenRouter live gate (#321,
 * `post-deploy-openrouter.spec.ts`), as a pure function over the probe's
 * answer so the mock suite tests it (`openrouter-gate.spec.ts`).
 *
 * `POST /api/v1/ai/openrouter/probe {}` sends ONE tiny completion through
 * the very call every text AI task's OpenRouter route makes
 * (`AiClient::openrouter_chat`: the paid-AI switch, `openrouter_api_key`
 * read live, the client and its retries). The deploy fails unless the
 * default model answered with text. Paid AI off or no key = `ok: false`
 * with the reason (nothing sent), and that fails too: SNV runs paid AI and
 * holds the key, so either is a broken box.
 */

/** The model a task sends when its `ai_model_<task>` is blank
 * (`sp_core::ai_tasks::DEFAULT_OPENROUTER_MODEL`). */
export const OPENROUTER_DEFAULT_MODEL = "google/gemini-4-hydrogen";

/** `POST /api/v1/ai/openrouter/probe`'s answer (`openrouter_probe::ProbeReport`). */
export interface OpenRouterProbe {
  ok: boolean;
  /** The model asked for. */
  model: string;
  /** The model OpenRouter says answered (it may name a dated version). */
  answered_model: string | null;
  latency_ms: number;
  /** Why it failed (never the key); null when ok. */
  error: string | null;
  /** The answer's first characters. */
  sample: string;
}

/** Every reason `probe` fails the gate; `[]` = the gate passes. */
export function openRouterGateFailures(probe: OpenRouterProbe): string[] {
  const failures: string[] = [];
  if (!probe.ok) {
    failures.push(`the probe failed: ${probe.error ?? "(no error text)"}`);
  }
  if (probe.ok && probe.sample.trim() === "") {
    failures.push("the model answered no text");
  }
  if (probe.model !== OPENROUTER_DEFAULT_MODEL) {
    failures.push(
      `the build probes ${JSON.stringify(probe.model)}, not the default ${OPENROUTER_DEFAULT_MODEL}`,
    );
  }
  return failures;
}
