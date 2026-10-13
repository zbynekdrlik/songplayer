import { test, expect } from "@playwright/test";
import {
  FIRST_CHECK_WAIT_MS,
  MMCSS_PRODUCTION,
  environmentChecked,
  environmentFailures,
  environmentNotes,
  type AudioProfile,
  type EnvironmentStatus,
} from "./environment-gate";

// #233: the post-deploy production-environment gate's pure decision
// (`environment-gate.ts`), run in the mock suite. The box gate itself is
// `post-deploy-environment.spec.ts` (SNV and PP).

/** #255: PP's audio profile as it must be: the SoundGrid adapter on its
 * NDIS driver with every setting off, automatic updates off, MMCSS for Pro
 * Audio; the Dante adapter on NetAdapterCx and the missing driver exclusion
 * are report-only notes. */
const DVS_NOTE =
  "sieťová karta Dante (DVS) „Ethernet 4“ beží na ovládači NetAdapterCx (vplyv sa ešte meria)";
const WU_NOTE =
  "Windows Update nemá vylúčené ovládače (ExcludeWUDriversInQualityUpdate; nastaví ho príprava PC)";
const audioHolds = (over: Partial<AudioProfile> = {}): AudioProfile => ({
  soundgrid_installed: true,
  soundgrid_nics: [
    {
      guid: "{DBFC2CF4-1A2B-4C3D-8E9F-0123456789AB}",
      name: "soundgrid-pcie",
      service: "rt640x64",
      driver_model: "ndis",
      kmdf_library_version: null,
      settings_ok: true,
      settings: [],
    },
  ],
  dante: {
    installed: true,
    interface_address: "6c1ff773c7d9",
    interface_name: "Ethernet 4",
    nic: { guid: "{0B5E1C2D-3E4F-4A5B-9C6D-7E8F90A1B2C3}", name: "Ethernet 4", driver_model: "netadaptercx" },
  },
  windows_update: { no_auto_update: true, drivers_excluded: false },
  mmcss: {
    system_responsiveness: 0,
    network_throttling_index: 4294967295,
    pro_audio_priority: 6,
    pro_audio_scheduling_category: "High",
  },
  power: {
    scheme_guid: "8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c",
    scheme_name: "High performance",
    core_parking_min_cores_pct: 100,
  },
  notes: [DVS_NOTE, WU_NOTE],
  ...over,
});

/** PP as it must be: IoT Enterprise LTSC, Defender real-time and passive
 * mode off, no other antivirus, every firewall profile off. */
const holds = (over: Partial<EnvironmentStatus> = {}): EnvironmentStatus => ({
  checked: true,
  ok: true,
  note: null,
  edition_id: "IoTEnterpriseS",
  product_name: "Windows 10 IoT Enterprise LTSC 2024",
  build_number: 26100,
  ltsc: true,
  edition_allowed: true,
  defender_realtime: false,
  defender_passive: false,
  defender_mode: "Normal",
  defender_antivirus_enabled: true,
  antivirus_on: [],
  firewall: { domain: false, private: false, public: false },
  audio: audioHolds(),
  problems: [],
  read_errors: [],
  checked_at_ms: 1791608681366,
  ...over,
});

test.describe("production environment gate (#233)", () => {
  test("an LTSC box and a Windows 11 IoT Enterprise box hold", () => {
    expect(environmentFailures(holds())).toEqual([]);
    expect(
      environmentFailures(
        holds({ edition_id: "IoTEnterprise", build_number: 22631, ltsc: false, edition_allowed: true }),
      ),
    ).toEqual([]);
    expect(FIRST_CHECK_WAIT_MS).toBe(60_000);
  });

  test("every violation fails, named by its code and text and by its fact", () => {
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
            "edícia Windows „Professional“ (zostava 22631) nie je povolená (povolené sú len Windows LTSC a Windows 11 IoT Enterprise)",
        },
        { code: "defender_realtime_on", text_sk: "ochrana Windows Defender v reálnom čase je zapnutá" },
        { code: "firewall_on:domain", text_sk: "brána firewall je zapnutá (doménový profil)" },
      ],
    });
    expect(environmentFailures(env)).toEqual([
      "edition_not_allowed: edícia Windows „Professional“ (zostava 22631) nie je povolená (povolené sú len Windows LTSC a Windows 11 IoT Enterprise)",
      "defender_realtime_on: ochrana Windows Defender v reálnom čase je zapnutá",
      "firewall_on:domain: brána firewall je zapnutá (doménový profil)",
      "edition_allowed is false, not true",
      "defender_realtime is true, not false",
      "firewall.domain is true, not false",
    ]);
  });

  test("a fact that cannot be read fails, with why", () => {
    const env = holds({
      ok: false,
      defender_realtime: null,
      defender_passive: null,
      problems: [
        {
          code: "unreadable:defender",
          text_sk: "stav ochrany Windows Defender v reálnom čase sa nepodarilo zistiť",
        },
      ],
      read_errors: ["Defender's status could not be read (0x80070005); the WinDefend service is not stopped (state 4)"],
    });
    expect(environmentFailures(env)).toEqual([
      "unreadable:defender: stav ochrany Windows Defender v reálnom čase sa nepodarilo zistiť",
      "defender_realtime is null, not false",
      "defender_passive is null, not false",
      "read error: Defender's status could not be read (0x80070005); the WinDefend service is not stopped (state 4)",
    ]);
  });

  // Review finding 9: the gate never trusts `ok` alone. A status that reads
  // ok with no problem, yet whose facts are not the production ones (a
  // server bug, a field the server forgot), fails on every such fact.
  test("ok with a fact that is not the production one fails, naming the fact", () => {
    expect(environmentFailures(holds({ edition_allowed: false }))).toEqual([
      "edition_allowed is false, not true",
    ]);
    expect(environmentFailures(holds({ edition_allowed: null }))).toEqual([
      "edition_allowed is null, not true",
    ]);
    expect(environmentFailures(holds({ defender_realtime: true }))).toEqual([
      "defender_realtime is true, not false",
    ]);
    expect(environmentFailures(holds({ defender_realtime: null }))).toEqual([
      "defender_realtime is null, not false",
    ]);
    expect(environmentFailures(holds({ defender_passive: true }))).toEqual([
      "defender_passive is true, not false",
    ]);
    for (const profile of ["domain", "private", "public"] as const) {
      const firewall = { domain: false, private: false, public: false, [profile]: true };
      expect(environmentFailures(holds({ firewall }))).toEqual([`firewall.${profile} is true, not false`]);
    }
    expect(environmentFailures(holds({ firewall: { domain: false, private: null, public: false } }))).toEqual([
      "firewall.private is null, not false",
    ]);
    expect(environmentFailures(holds({ antivirus_on: ["ESET Security"] }))).toEqual([
      'antivirus_on is ["ESET Security"], not []',
    ]);
    expect(environmentFailures(holds({ antivirus_on: null }))).toEqual(["antivirus_on is null, not []"]);
  });

  test("a status from before the guard knew a fact fails on it", () => {
    // An older server: no passive mode, no antivirus list, no firewall.
    const old = holds();
    delete old.defender_passive;
    delete old.antivirus_on;
    delete old.firewall;
    expect(environmentFailures(old)).toEqual([
      "defender_passive is null, not false",
      "firewall.domain is null, not false",
      "firewall.private is null, not false",
      "firewall.public is null, not false",
      "antivirus_on is null, not []",
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

  test("read errors are listed only next to a failure", () => {
    expect(environmentFailures(holds({ read_errors: ["ProductName is missing"] }))).toEqual([]);
  });

  test("#255: a box with no SoundGrid adapter holds (SNV)", () => {
    expect(
      environmentFailures(holds({ audio: audioHolds({ soundgrid_installed: false, soundgrid_nics: [] }) })),
    ).toEqual([]);
    expect(MMCSS_PRODUCTION).toEqual({
      system_responsiveness: 0,
      network_throttling_index: 0xffffffff,
      pro_audio_priority: 6,
    });
  });

  test("#255: every audio-profile violation fails, named by its fact", () => {
    const sg = audioHolds().soundgrid_nics![0];
    const env = holds({
      audio: audioHolds({
        soundgrid_nics: [
          { ...sg, driver_model: "netadaptercx" },
          { ...sg, guid: "{G}", name: null, settings_ok: false },
          { ...sg, name: "third", driver_model: null, settings_ok: null },
        ],
        windows_update: { no_auto_update: false, drivers_excluded: true },
        mmcss: {
          system_responsiveness: 20,
          network_throttling_index: 10,
          pro_audio_priority: 1,
          pro_audio_scheduling_category: "Medium",
        },
      }),
    });
    expect(environmentFailures(env)).toEqual([
      'audio.soundgrid_nics["soundgrid-pcie"].driver_model is "netadaptercx", not "ndis"',
      'audio.soundgrid_nics["{G}"].settings_ok is false, not true',
      'audio.soundgrid_nics["third"].driver_model is null, not "ndis"',
      'audio.soundgrid_nics["third"].settings_ok is null, not true',
      "audio.windows_update.no_auto_update is false, not true",
      "audio.mmcss.system_responsiveness is 20, not 0",
      "audio.mmcss.network_throttling_index is 10, not 4294967295",
      "audio.mmcss.pro_audio_priority is 1, not 6",
      'audio.mmcss.pro_audio_scheduling_category is "Medium", not "High"',
    ]);
    // The scheduling category is compared as Windows does: case and padding
    // ignored.
    const relaxed = audioHolds({
      mmcss: { ...audioHolds().mmcss, pro_audio_scheduling_category: " high " },
    });
    expect(environmentFailures(holds({ audio: relaxed }))).toEqual([]);
  });

  test("#255: a status from before the audio profile fails on it", () => {
    const old = holds();
    delete old.audio;
    expect(environmentFailures(old)).toEqual([
      "audio.soundgrid_nics is null, not a list",
      "audio.windows_update.no_auto_update is null, not true",
      "audio.mmcss.system_responsiveness is null, not 0",
      "audio.mmcss.network_throttling_index is null, not 4294967295",
      "audio.mmcss.pro_audio_priority is null, not 6",
      'audio.mmcss.pro_audio_scheduling_category is null, not "High"',
    ]);
    // The SoundGrid binding could not be read: null, never "no adapter".
    expect(environmentFailures(holds({ audio: audioHolds({ soundgrid_nics: null }) }))).toEqual([
      "audio.soundgrid_nics is null, not a list",
    ]);
  });

  test("#255: the report-only facts never fail and are listed as notes", () => {
    const env = holds({
      audio: audioHolds({
        windows_update: { no_auto_update: true, drivers_excluded: null },
        power: { scheme_guid: null, scheme_name: null, core_parking_min_cores_pct: 50 },
        dante: { installed: null },
      }),
    });
    expect(environmentFailures(env)).toEqual([]);
    expect(environmentNotes(holds())).toEqual([DVS_NOTE, WU_NOTE]);
    expect(environmentNotes(undefined)).toEqual([]);
    expect(environmentNotes(holds({ audio: undefined }))).toEqual([]);
  });

  test("the gate waits for the first check", () => {
    expect(environmentChecked(holds())).toBe(true);
    expect(environmentChecked(holds({ checked: false }))).toBe(false);
    expect(environmentChecked(undefined)).toBe(false);
    expect(environmentChecked(null)).toBe(false);
  });
});
