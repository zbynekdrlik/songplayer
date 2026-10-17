/**
 * #256: the decisions of the post-deploy usage-log gate
 * (`post-deploy-ui-usage.spec.ts`), pure so the mock suite tests them
 * (`ui-usage-gate.spec.ts`) without a box.
 */

/** One row of `GET /api/v1/ui-usage/summary` (`sp_core::ui_usage::UsageRow`). */
export interface UsageRow {
  page: string;
  control: string;
  kind: string;
  count: number;
  days_used: number;
  first_ms: number;
  last_ms: number;
  on_air: number;
}

/** `GET /api/v1/ui-usage/summary` (`sp_core::ui_usage::UsageSummary`). */
export interface UsageSummary {
  now_ms: number;
  days: number;
  from_ms: number;
  events: number;
  first_ms: number | null;
  rows: UsageRow[];
}

/** A row's key: page, control, kind. */
export type RowKey = [string, string, string];

const DAY_MS = 86_400_000;
const KINDS = ["click", "change", "submit", "page"];

/** What is wrong with a summary asked for `days`, one line each (none = it
 * answers the server's shape). */
export function summaryProblems(summary: UsageSummary, days: number): string[] {
  const problems: string[] = [];
  if (summary.days !== days) problems.push(`days ${summary.days}, asked ${days}`);
  if (summary.from_ms !== summary.now_ms - days * DAY_MS) {
    problems.push(`from_ms ${summary.from_ms} is not ${days} days before now_ms ${summary.now_ms}`);
  }
  const total = summary.rows.reduce((sum, row) => sum + row.count, 0);
  if (summary.events !== total) problems.push(`events ${summary.events}, the rows count ${total}`);
  const first = summary.rows.length ? Math.min(...summary.rows.map((row) => row.first_ms)) : null;
  if (summary.first_ms !== first) problems.push(`first_ms ${summary.first_ms}, the rows' ${first}`);
  for (const row of summary.rows) {
    const name = `${row.page} ${row.control} ${row.kind}`;
    if (!KINDS.includes(row.kind)) problems.push(`${name}: unknown kind`);
    if (row.count < 1) problems.push(`${name}: count ${row.count}`);
    if (row.days_used < 1 || row.days_used > row.count) {
      problems.push(`${name}: days_used ${row.days_used} of ${row.count}`);
    }
    if (row.on_air < 0 || row.on_air > row.count) {
      problems.push(`${name}: on_air ${row.on_air} of ${row.count}`);
    }
    if (row.last_ms < row.first_ms) problems.push(`${name}: last use before the first`);
  }
  return problems;
}

/** The count of the row `key`, 0 when there is none. */
export function rowCount(summary: UsageSummary, [page, control, kind]: RowKey): number {
  return (
    summary.rows.find((row) => row.page === page && row.control === control && row.kind === kind)
      ?.count ?? 0
  );
}

/** The rows of `keys` that grew by `uses` or more from `before` to
 * `after`: a session that made `uses` of each would have been recorded.
 * An operator's own few uses meanwhile stay below it. */
export function recordedSession(
  before: UsageSummary,
  after: UsageSummary,
  keys: RowKey[],
  uses: number,
): string[] {
  return keys
    .map((key) => ({ key, grew: rowCount(after, key) - rowCount(before, key) }))
    .filter(({ grew }) => grew >= uses)
    .map(({ key, grew }) => `${key.join(" ")} +${grew}`);
}
