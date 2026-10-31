import { test, expect } from "@playwright/test";
import {
  recordedSession,
  rowCount,
  summaryProblems,
  type RowKey,
  type UsageRow,
  type UsageSummary,
} from "./ui-usage-gate";

// #256: the post-deploy usage-log gate's pure decisions (`ui-usage-gate.ts`),
// run in the mock suite. The box gate itself is
// `post-deploy-ui-usage.spec.ts` (SNV and PP's subset).

const NOW = 1_791_608_681_366;
const DAY = 86_400_000;

const row = (over: Partial<UsageRow> = {}): UsageRow => ({
  page: "/",
  control: "nav-lyrics",
  kind: "click",
  count: 3,
  days_used: 2,
  first_ms: NOW - 2 * DAY,
  last_ms: NOW - 1000,
  on_air: 1,
  ...over,
});

const summary = (rows: UsageRow[], over: Partial<UsageSummary> = {}): UsageSummary => ({
  now_ms: NOW,
  days: 1,
  from_ms: NOW - DAY,
  events: rows.reduce((sum, r) => sum + r.count, 0),
  first_ms: rows.length ? Math.min(...rows.map((r) => r.first_ms)) : null,
  rows,
  ...over,
});

const SESSION: RowKey[] = [
  ["/", "nav-lyrics", "click"],
  ["/lyrics", "nav-dashboard", "click"],
  ["/lyrics", "", "page"],
];

test("a summary in the server's shape has no problem, an empty one too", () => {
  const rows = [
    row(),
    row({ page: "/lyrics", control: "", kind: "page", count: 1, days_used: 1, on_air: 0 }),
  ];
  expect(summaryProblems(summary(rows), 1)).toEqual([]);
  expect(summaryProblems(summary([]), 1)).toEqual([]);
});

test("every way a summary breaks its shape is named", () => {
  expect(summaryProblems(summary([row()], { days: 7 }), 1)).toEqual(["days 7, asked 1"]);
  expect(summaryProblems(summary([row()], { from_ms: NOW }), 1)).toEqual([
    `from_ms ${NOW} is not 1 days before now_ms ${NOW}`,
  ]);
  expect(summaryProblems(summary([row()], { events: 4 }), 1)).toEqual([
    "events 4, the rows count 3",
  ]);
  expect(summaryProblems(summary([row()], { first_ms: null }), 1)).toEqual([
    `first_ms null, the rows' ${NOW - 2 * DAY}`,
  ]);
  const name = "/ nav-lyrics click";
  expect(summaryProblems(summary([row({ kind: "hover" })]), 1)).toEqual([
    "/ nav-lyrics hover: unknown kind",
  ]);
  expect(summaryProblems(summary([row({ days_used: 4 })]), 1)).toEqual([
    `${name}: days_used 4 of 3`,
  ]);
  expect(summaryProblems(summary([row({ days_used: 0 })]), 1)).toEqual([
    `${name}: days_used 0 of 3`,
  ]);
  expect(summaryProblems(summary([row({ on_air: 4 })]), 1)).toEqual([`${name}: on_air 4 of 3`]);
  expect(summaryProblems(summary([row({ last_ms: NOW - 3 * DAY })]), 1)).toEqual([
    `${name}: last use before the first`,
  ]);
});

test("a row's count is read by page, control and kind, 0 when absent", () => {
  const s = summary([row({ count: 5, days_used: 1 }), row({ kind: "change", count: 2, days_used: 1 })]);
  expect(rowCount(s, ["/", "nav-lyrics", "click"])).toBe(5);
  expect(rowCount(s, ["/", "nav-lyrics", "change"])).toBe(2);
  expect(rowCount(s, ["/live", "nav-lyrics", "click"])).toBe(0);
});

test("the session is recorded only when a row grew by its own uses", () => {
  const before = summary([row({ count: 10, days_used: 1 })]);
  // An operator's click meanwhile: below the session's 4 uses.
  const operator = summary([
    row({ count: 13, days_used: 1 }),
    row({ page: "/lyrics", control: "", kind: "page", count: 1, days_used: 1 }),
  ]);
  expect(recordedSession(before, operator, SESSION, 4)).toEqual([]);
  // The session itself, recorded: every row grew by 4.
  const recorded = summary([
    row({ count: 14, days_used: 1 }),
    row({ page: "/lyrics", control: "nav-dashboard", count: 4, days_used: 1 }),
    row({ page: "/lyrics", control: "", kind: "page", count: 4, days_used: 1 }),
  ]);
  expect(recordedSession(before, recorded, SESSION, 4)).toEqual([
    "/ nav-lyrics click +4",
    "/lyrics nav-dashboard click +4",
    "/lyrics  page +4",
  ]);
});
