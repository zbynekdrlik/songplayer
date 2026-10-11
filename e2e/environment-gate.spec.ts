import { test, expect } from "@playwright/test";
import {
  FIRST_CHECK_WAIT_MS,
  environmentChecked,
  environmentFailures,
  type EnvironmentStatus,
} from "./environment-gate";

// #233: the post-deploy production-environment gate's pure decision
// (`environment-gate.ts`), run in the mock suite. The box gate itself is
// `post-deploy-environment.spec.ts` (SNV and PP).

/** PP as it must be: IoT Enterprise LTSC, Defender real-time off, firewall off. */
const holds = (over: Partial<EnvironmentStatus> = {}): EnvironmentStatus => ({
  checked: true,
  ok: true,
  note: null,
  edition_id: "IoTEnterpriseS",
  product_name: "Windows 10 IoT Enterprise LTSC 2024",
  ltsc: true,
  edition_allowed: true,
  defender_realtime: false,
  firewall: { domain: false, private: false, public: false },
  problems: [],
  read_errors: [],
  checked_at_ms: 1791608681366,
  ...over,
});

test.describe("production environment gate (#233)", () => {
  test("an LTSC box and an IoT Enterprise box hold", () => {
    expect(environmentFailures(holds())).toEqual([]);
    expect(
      environmentFailures(holds({ edition_id: "IoTEnterprise", ltsc: false, edition_allowed: true })),
    ).toEqual([]);
    expect(FIRST_CHECK_WAIT_MS).toBe(60_000);
  });

  test("every violation fails, named by its code and text", () => {
    const env = holds({
      ok: false,
      edition_id: "Professional",
      ltsc: false,
      edition_allowed: false,
      defender_realtime: true,
      firewall: { domain: true, private: false, public: false },
      problems: [
        {
          code: "edition_not_allowed",
          text_sk:
            "edícia Windows „Professional“ nie je povolená (povolené sú len Windows LTSC a Windows IoT Enterprise)",
        },
        { code: "defender_realtime_on", text_sk: "ochrana Windows Defender v reálnom čase je zapnutá" },
        { code: "firewall_on:domain", text_sk: "brána firewall je zapnutá (doménový profil)" },
      ],
    });
    expect(environmentFailures(env)).toEqual([
      "edition_not_allowed: edícia Windows „Professional“ nie je povolená (povolené sú len Windows LTSC a Windows IoT Enterprise)",
      "defender_realtime_on: ochrana Windows Defender v reálnom čase je zapnutá",
      "firewall_on:domain: brána firewall je zapnutá (doménový profil)",
    ]);
  });

  test("a fact that cannot be read fails, with why", () => {
    const env = holds({
      ok: false,
      defender_realtime: null,
      problems: [
        {
          code: "unreadable:defender",
          text_sk: "stav ochrany Windows Defender v reálnom čase sa nepodarilo zistiť",
        },
      ],
      read_errors: ["Defender's status could not be read (0x80070005)"],
    });
    expect(environmentFailures(env)).toEqual([
      "unreadable:defender: stav ochrany Windows Defender v reálnom čase sa nepodarilo zistiť",
      "read error: Defender's status could not be read (0x80070005)",
    ]);
  });

  test("a box is never taken as fine unverified", () => {
    expect(environmentFailures(undefined)).toEqual([
      "/api/v1/status has no environment (a server without the #233 guard?)",
    ]);
    expect(environmentFailures(null)).toHaveLength(1);
    expect(environmentFailures(holds({ checked: false, ok: false }))).toEqual([
      "the production environment has not been checked yet",
    ]);
    expect(
      environmentFailures(
        holds({ note: "not Windows: the production environment is checked on Windows only" }),
      ),
    ).toEqual([
      "the production environment is not checked here: not Windows: the production environment is checked on Windows only",
    ]);
    expect(environmentFailures(holds({ ok: false }))).toEqual([
      "the environment is not ok, but no problem is named",
    ]);
  });

  test("read errors are listed only next to a problem", () => {
    expect(environmentFailures(holds({ read_errors: ["ProductName is missing"] }))).toEqual([]);
  });

  test("the gate waits for the first check", () => {
    expect(environmentChecked(holds())).toBe(true);
    expect(environmentChecked(holds({ checked: false }))).toBe(false);
    expect(environmentChecked(undefined)).toBe(false);
    expect(environmentChecked(null)).toBe(false);
  });
});
