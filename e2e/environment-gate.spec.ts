import { test, expect } from "@playwright/test";
import {
  FIRST_CHECK_WAIT_MS,
  MMCSS_PRODUCTION,
  environmentChecked,
  environmentFailures,
  environmentNotes,
  type AudioProfile,
  type EnvironmentStatus,
  type TwoNetworks,
} from "./environment-gate";

// #233: the post-deploy production-environment gate's pure decision
// (`environment-gate.ts`), run in the mock suite. The box gate itself is
// `post-deploy-environment.spec.ts` (SNV and PP).

/** #255: PP's audio profile as it must be: the adapter the SoundGrid driver
 * chose (soundgrid-pcie) on its NDIS driver with every setting off,
 * automatic updates off and drivers excluded from Windows Update, MMCSS for
 * Pro Audio; an economy power scheme is a report-only note. */
const SCHEME_NOTE = "aktívny plán napájania je úsporný: Balanced";
const PARKING_NOTE = "parkovanie jadier je zapnuté (minimum 50 % jadier)";
const audioHolds = (over: Partial<AudioProfile> = {}): AudioProfile => ({
  soundgrid: "found",
  soundgrid_port_mac: "6c2408e59c91",
  soundgrid_nic: {
    guid: "{DBFC2CF4-C91F-4C0C-A2D8-042330E1CD05}",
    name: "soundgrid-pcie",
    service: "rt640x64",
    driver_model: "ndis",
    kmdf_library_version: null,
    settings_ok: true,
    settings: [],
  },
  windows_update: { no_auto_update: true, drivers_excluded: true, store_auto_updates_off: true },
  wired_nics: [
    { name: "soundgrid-pcie", guid: "{A}", ok: true },
    { name: "Dante", guid: "{B}", ok: true },
  ],
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
  notes: [SCHEME_NOTE],
  ...over,
});

/** #255 item D: PP has no Dante output, so nothing is judged. */
const NO_DANTE_LEG: TwoNetworks = {
  dante_ips: [],
  physical_enabled: ["soundgrid-pcie", "Ethernet 4"],
  physical_up: ["soundgrid-pcie", "Ethernet 4"],
  default_routes: ["Ethernet 4"],
  legs: [],
};

/** #255 item D: resolume-snv's two networks as dstack set them up on
 * 10.11.2026 (dstack `docs/two-networks.md` §6 items 1–3). */
const SNV_TWO_NETWORKS: TwoNetworks = {
  dante_ips: ["10.77.7.107"],
  physical_enabled: ["Ethernet 4"],
  physical_up: ["Ethernet 4"],
  default_routes: ["Ethernet 4"],
  legs: [
    {
      ip: "10.77.7.107",
      nic: "Dante70",
      gateways: [],
      metric: 900,
      automatic_metric: false,
      dns_registration: false,
      netbios_options: 2,
      file_sharing_bound: false,
      client_bound: false,
    },
  ],
};

/** #255: the firewall of a PC with no Dante leg (PP): every profile off. */
const FIREWALL_OFF = {
  domain: false,
  private: false,
  public: false,
  mode: "off",
  defaults: [
    { inbound: 4, outbound: 2 },
    { inbound: 4, outbound: 2 },
    { inbound: 4, outbound: 2 },
  ],
  dante_leg_group_rules: 0,
  block_rules_outside_group: [],
};

/** #322 (dstack#264): dstack's generic Dante-leg filter, as dante-leg.ps1
 * leaves resolume-snv and resolume-pp (the group "dstack Dante leg", 10;
 * both read 19.11.2026). */
const FIREWALL_DANTE_LEG = {
  domain: true,
  private: true,
  public: true,
  mode: "dante_leg",
  defaults: [
    { inbound: 2, outbound: 2 },
    { inbound: 2, outbound: 2 },
    { inbound: 2, outbound: 2 },
  ],
  dante_leg_group_rules: 10,
  block_rules_outside_group: [],
};

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
  defender_protections_on: [],
  defender_mode: "Normal",
  defender_antivirus_enabled: true,
  antivirus_on: [],
  firewall: FIREWALL_OFF,
  audio: audioHolds(),
  two_networks: NO_DANTE_LEG,
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
      firewall: { ...FIREWALL_OFF, domain: true, mode: "other" },
      problems: [
        {
          code: "edition_not_allowed",
          text_sk:
            "edícia Windows „Professional“ (zostava 22631) nie je povolená (povolené sú len Windows LTSC a Windows 11 IoT Enterprise)",
        },
        { code: "defender_realtime_on", text_sk: "ochrana Windows Defender v reálnom čase je zapnutá" },
        { code: "firewall_on:domain", text_sk: "brána firewall je zapnutá (doménový profil) a nie je nastavená podľa štandardu dstack (skupina pravidiel „dstack Dante leg“)" },
      ],
    });
    expect(environmentFailures(env)).toEqual([
      "edition_not_allowed: edícia Windows „Professional“ (zostava 22631) nie je povolená (povolené sú len Windows LTSC a Windows 11 IoT Enterprise)",
      "defender_realtime_on: ochrana Windows Defender v reálnom čase je zapnutá",
      "firewall_on:domain: brána firewall je zapnutá (doménový profil) a nie je nastavená podľa štandardu dstack (skupina pravidiel „dstack Dante leg“)",
      "edition_allowed is false, not true",
      "defender_realtime is true, not false",
      'firewall.mode is "other", not "off" or "dante_leg"',
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
      const firewall = { ...FIREWALL_OFF, [profile]: true };
      expect(environmentFailures(holds({ firewall }))).toEqual([`firewall.${profile} is true, not false`]);
    }
    expect(environmentFailures(holds({ firewall: { ...FIREWALL_OFF, private: null } }))).toEqual([
      "firewall.private is null, not false",
    ]);
    expect(environmentFailures(holds({ antivirus_on: ["ESET Security"] }))).toEqual([
      'antivirus_on is ["ESET Security"], not []',
    ]);
    expect(environmentFailures(holds({ antivirus_on: null }))).toEqual(["antivirus_on is null, not []"]);
    expect(environmentFailures(holds({ defender_protections_on: ["ioav"] }))).toEqual([
      'defender_protections_on is ["ioav"], not []',
    ]);
    expect(environmentFailures(holds({ defender_protections_on: null }))).toEqual([
      "defender_protections_on is null, not []",
    ]);
  });

  // #255 (dstack#155): a PC with a Dante leg keeps its firewall on, as
  // dstack's two-network standard wants it; every fact of it by name.
  test("dstack's two-network standard holds, and each of its facts is checked", () => {
    // #322 (dstack#264): the generic filter holds, each of its facts by name.
    expect(environmentFailures(holds({ firewall: FIREWALL_DANTE_LEG }))).toEqual([]);
    expect(environmentFailures(holds({ firewall: { ...FIREWALL_DANTE_LEG, private: false } }))).toEqual([
      "firewall.private is false, not true",
    ]);
    expect(environmentFailures(holds({ firewall: { ...FIREWALL_DANTE_LEG, public: false } }))).toEqual([
      "firewall.public is false, not true",
    ]);
    const defaults: ({ inbound: number; outbound: number } | null)[] = [...FIREWALL_DANTE_LEG.defaults];
    defaults[2] = { inbound: 4, outbound: 2 };
    expect(environmentFailures(holds({ firewall: { ...FIREWALL_DANTE_LEG, defaults } }))).toEqual([
      'firewall.defaults.public is {"inbound":4,"outbound":2}, not {"inbound":2,"outbound":2}',
    ]);
    expect(
      environmentFailures(holds({ firewall: { ...FIREWALL_DANTE_LEG, defaults: [null, null, null] } })),
    ).toEqual([
      'firewall.defaults.domain is null, not {"inbound":2,"outbound":2}',
      'firewall.defaults.private is null, not {"inbound":2,"outbound":2}',
      'firewall.defaults.public is null, not {"inbound":2,"outbound":2}',
    ]);
    for (const [count, shown] of [
      [9, "9"],
      [11, "11"],
      [0, "0"],
      [null, "null"],
    ] as const) {
      expect(
        environmentFailures(holds({ firewall: { ...FIREWALL_DANTE_LEG, dante_leg_group_rules: count } })),
      ).toEqual([`firewall.dante_leg_group_rules is ${shown}, not 10`]);
    }
    expect(
      environmentFailures(holds({ firewall: { ...FIREWALL_DANTE_LEG, block_rules_outside_group: ["Block X"] } })),
    ).toEqual(['firewall.block_rules_outside_group is ["Block X"], not []']);
    expect(
      environmentFailures(holds({ firewall: { ...FIREWALL_DANTE_LEG, block_rules_outside_group: null } })),
    ).toEqual(["firewall.block_rules_outside_group is null, not []"]);
    // #322: the old per-box mode "dstack155" is no longer accepted, whatever
    // else the status says (an older server on a PC that never switched).
    expect(
      environmentFailures(
        holds({ firewall: { ...FIREWALL_DANTE_LEG, mode: "dstack155", dante_leg_group_rules: 0 } }),
      ),
    ).toEqual(['firewall.mode is "dstack155", not "off" or "dante_leg"']);
    // A leftover rule of the old group is a Block rule outside the group.
    expect(
      environmentFailures(
        holds({ firewall: { ...FIREWALL_DANTE_LEG, block_rules_outside_group: ["dstack155 obs64.exe in"] } }),
      ),
    ).toEqual(['firewall.block_rules_outside_group is ["dstack155 obs64.exe in"], not []']);
    // A firewall that is on and follows nothing of the standard.
    const on = { ...FIREWALL_OFF, domain: true, private: true, public: true, mode: "other" };
    expect(environmentFailures(holds({ firewall: on }))).toEqual([
      'firewall.mode is "other", not "off" or "dante_leg"',
    ]);
  });

  // #340: resolume-pp ran a Dante leg with every firewall profile off from
  // 18.11.2026 and the gate read it clean. The gate has no per-code list (it
  // fails on every problem); this pins what it prints, and that the fact
  // fails by name even from a server that names no problem.
  test("#340: every firewall profile off fails on a PC with a Dante leg, and only there", () => {
    const text =
      "brána firewall je vypnutá a sieť Dante nie je filtrovaná — zapni ju podľa štandardu dstack: " +
      "všetky profily zapnuté, predvolená akcia povoliť (prichádzajúce aj odchádzajúce), " +
      "skupina pravidiel „dstack Dante leg“";
    const fact =
      'firewall.mode is "off" on a PC with a Dante leg (two_networks.dante_ips ["10.77.7.107"]), not "dante_leg"';
    const status = holds({
      ok: false,
      firewall: FIREWALL_OFF,
      two_networks: SNV_TWO_NETWORKS,
      problems: [{ code: "firewall_off_with_dante_leg", text_sk: text }],
    });
    expect(environmentFailures(status)).toEqual([`firewall_off_with_dante_leg: ${text}`, fact]);
    // An older server that names no problem for it: the fact alone fails.
    expect(environmentFailures(holds({ firewall: FIREWALL_OFF, two_networks: SNV_TWO_NETWORKS }))).toEqual([fact]);
    // No Dante leg (PP before its leg): off holds.
    expect(environmentFailures(holds({ firewall: FIREWALL_OFF, two_networks: NO_DANTE_LEG }))).toEqual([]);
    // A Dante leg on dstack's standard (both PCs today) holds.
    expect(environmentFailures(holds({ firewall: FIREWALL_DANTE_LEG, two_networks: SNV_TWO_NETWORKS }))).toEqual([]);
    // The outputs unread: the leg is not known, only that is named.
    expect(
      environmentFailures(holds({ firewall: FIREWALL_OFF, two_networks: { ...NO_DANTE_LEG, dante_ips: null } })),
    ).toEqual(["two_networks.dante_ips is null, not a list"]);
  });

  test("a status from before the guard knew a fact fails on it", () => {
    // An older server: no passive mode, no antivirus list, no firewall.
    const old = holds();
    delete old.defender_passive;
    delete old.antivirus_on;
    delete old.firewall;
    delete old.defender_protections_on;
    expect(environmentFailures(old)).toEqual([
      "defender_passive is null, not false",
      'firewall.mode is null, not "off" or "dante_leg"',
      "antivirus_on is null, not []",
      "defender_protections_on is null, not []",
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
      environmentFailures(
        holds({ audio: audioHolds({ soundgrid: "not_installed", soundgrid_port_mac: null, soundgrid_nic: null }) }),
      ),
    ).toEqual([]);
    expect(MMCSS_PRODUCTION).toEqual({
      system_responsiveness: 0,
      network_throttling_index: 0xffffffff,
      pro_audio_priority: 6,
    });
  });

  test("#255: every audio-profile violation fails, named by its fact", () => {
    const sg = audioHolds().soundgrid_nic!;
    const env = holds({
      audio: audioHolds({
        soundgrid_nic: { ...sg, driver_model: "netadaptercx", settings_ok: false },
        windows_update: { no_auto_update: false, drivers_excluded: false, store_auto_updates_off: false },
        mmcss: {
          system_responsiveness: 20,
          network_throttling_index: 10,
          pro_audio_priority: 1,
          pro_audio_scheduling_category: "Medium",
        },
      }),
    });
    expect(environmentFailures(env)).toEqual([
      'audio.soundgrid_nic.driver_model is "netadaptercx", not "ndis"',
      "audio.soundgrid_nic.settings_ok is false, not true",
      "audio.windows_update.no_auto_update is false, not true",
      "audio.windows_update.drivers_excluded is false, not true",
      "audio.windows_update.store_auto_updates_off is false, not true",
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
    // A model or settings that could not be read fail too.
    expect(
      environmentFailures(holds({ audio: audioHolds({ soundgrid_nic: { ...sg, driver_model: null, settings_ok: null } }) })),
    ).toEqual([
      "audio.soundgrid_nic.driver_model is null, not \"ndis\"",
      "audio.soundgrid_nic.settings_ok is null, not true",
    ]);
  });

  test("#255: SoundGrid installed with no port chosen, a missing port or an unreadable one fails", () => {
    for (const soundgrid of ["port_unset", "port_missing", "unreadable"] as const) {
      expect(
        environmentFailures(holds({ audio: audioHolds({ soundgrid, soundgrid_nic: null }) })),
        soundgrid,
      ).toEqual([`audio.soundgrid is "${soundgrid}", not "found" or "not_installed"`]);
    }
    // Found with no adapter on the status (a server bug) fails on the adapter.
    expect(environmentFailures(holds({ audio: audioHolds({ soundgrid_nic: null }) }))).toEqual([
      'audio.soundgrid_nic.driver_model is null, not "ndis"',
      "audio.soundgrid_nic.settings_ok is null, not true",
    ]);
  });

  test("#285: every wired adapter must hold, named by the adapter", () => {
    const failing = (wired_nics: AudioProfile["wired_nics"]) =>
      environmentFailures(holds({ audio: audioHolds({ wired_nics }) }));
    expect(failing([{ name: "Dante", guid: "{B}", ok: false }])).toEqual([
      "audio.wired_nics[Dante].ok is false, not true",
    ]);
    expect(failing([{ guid: "{B}", ok: null }])).toEqual([
      "audio.wired_nics[{B}].ok is null, not true",
    ]);
    expect(failing([])).toEqual(["audio.wired_nics is [], not a list of adapters"]);
    expect(failing(null)).toEqual(["audio.wired_nics is null, not a list of adapters"]);
  });

  // A contract pin of what the gate prints for the code (the gate has no
  // per-code list: it fails on every problem). The code and the text
  // themselves are pinned in sp-core's and sp-server's tests.
  test("#337: a Dante adapter on a driver too old fails by its code and its adapter", () => {
    const text =
      "sieťová karta „Ethernet 4“ nesie Dante a jej ovládač je príliš starý: neponúka " +
      "nastavenie vysielacej fronty — nainštaluj ovládač Realtek RTL8156 verzie 11.19 alebo novšej";
    const status = holds({
      ok: false,
      problems: [{ code: "dante_nic_driver_old", text_sk: text }],
      audio: audioHolds({
        wired_nics: [
          { name: "soundgrid-pcie", guid: "{A}", ok: true },
          { name: "Ethernet 4", guid: "{B}", ok: false },
        ],
      }),
    });
    expect(environmentFailures(status)).toEqual([
      `dante_nic_driver_old: ${text}`,
      "audio.wired_nics[Ethernet 4].ok is false, not true",
    ]);
  });

  test("#255: a status from before the audio profile fails on it", () => {
    const old = holds();
    delete old.audio;
    expect(environmentFailures(old)).toEqual([
      'audio.soundgrid is null, not "found" or "not_installed"',
      "audio.windows_update.no_auto_update is null, not true",
      "audio.windows_update.drivers_excluded is null, not true",
      "audio.windows_update.store_auto_updates_off is null, not true",
      "audio.mmcss.system_responsiveness is null, not 0",
      "audio.mmcss.network_throttling_index is null, not 4294967295",
      "audio.mmcss.pro_audio_priority is null, not 6",
      'audio.mmcss.pro_audio_scheduling_category is null, not "High"',
      "audio.wired_nics is null, not a list of adapters",
    ]);
    // An older server without SoundGrid's state: null, never "not installed".
    expect(environmentFailures(holds({ audio: audioHolds({ soundgrid: null }) }))).toEqual([
      'audio.soundgrid is null, not "found" or "not_installed"',
    ]);
  });

  test("#255 lane 2: the Windows Update driver exclusion gates, missing or unread", () => {
    // SNV before its first deploy with scripts/setup-prod-pc.ps1: the
    // exclusion missing fails; the deploy applies it before the start.
    for (const drivers_excluded of [false, null] as const) {
      expect(
        environmentFailures(
          holds({
            audio: audioHolds({
              windows_update: { no_auto_update: true, drivers_excluded, store_auto_updates_off: true },
            }),
          }),
        ),
        String(drivers_excluded),
      ).toEqual([`audio.windows_update.drivers_excluded is ${String(drivers_excluded)}, not true`]);
    }
  });

  test("#333: the Store's automatic app updates gate, on or unread", () => {
    for (const store_auto_updates_off of [false, null] as const) {
      expect(
        environmentFailures(
          holds({
            audio: audioHolds({
              windows_update: { no_auto_update: true, drivers_excluded: true, store_auto_updates_off },
            }),
          }),
        ),
        String(store_auto_updates_off),
      ).toEqual([
        `audio.windows_update.store_auto_updates_off is ${String(store_auto_updates_off)}, not true`,
      ]);
    }
  });

  test("#255: the report-only facts never fail and are listed as notes", () => {
    const env = holds({
      audio: audioHolds({
        power: { scheme_guid: null, scheme_name: null, core_parking_min_cores_pct: 50 },
        notes: [SCHEME_NOTE, PARKING_NOTE],
      }),
    });
    expect(environmentFailures(env)).toEqual([]);
    expect(environmentNotes(holds())).toEqual([SCHEME_NOTE]);
    expect(environmentNotes(env)).toEqual([SCHEME_NOTE, PARKING_NOTE]);
    expect(environmentNotes(undefined)).toEqual([]);
    expect(environmentNotes(holds({ audio: undefined }))).toEqual([]);
  });

  test("#255 item D: SNV's two networks hold and every fact of §6 1-3 is checked", () => {
    expect(environmentFailures(holds({ firewall: FIREWALL_DANTE_LEG, two_networks: SNV_TWO_NETWORKS }))).toEqual([]);
    const snv = (over: Partial<TwoNetworks>) =>
      environmentFailures(holds({ firewall: FIREWALL_DANTE_LEG, two_networks: { ...SNV_TWO_NETWORKS, ...over } }));
    const leg = SNV_TWO_NETWORKS.legs![0];
    const withLeg = (over: object) => snv({ legs: [{ ...leg, ...over }] });
    expect(snv({ physical_enabled: ["Ethernet 4", "Ethernet"] })).toEqual([
      'two_networks.physical_enabled is ["Ethernet 4","Ethernet"], not exactly one adapter',
    ]);
    expect(snv({ physical_up: [] })).toEqual(['two_networks.physical_up is [], not ["Ethernet 4"]']);
    expect(snv({ physical_enabled: null })).toEqual([
      "two_networks.physical_enabled is null, not exactly one adapter",
    ]);
    expect(snv({ default_routes: ["Ethernet 4", "Dante70"] })).toEqual([
      'two_networks.default_routes is ["Ethernet 4","Dante70"], not exactly one',
      "two_networks.legs[10.77.7.107]: a default route goes through Dante70",
    ]);
    expect(snv({ default_routes: ["Dante70"] })).toEqual([
      "two_networks.legs[10.77.7.107]: a default route goes through Dante70",
    ]);
    expect(withLeg({ nic: null })).toEqual(["two_networks.legs[10.77.7.107].nic is null: no interface holds it"]);
    expect(withLeg({ gateways: ["10.77.7.1"] })).toEqual([
      'two_networks.legs[10.77.7.107].gateways is ["10.77.7.1"], not []',
    ]);
    expect(withLeg({ metric: 25, automatic_metric: true, dns_registration: true })).toEqual([
      "two_networks.legs[10.77.7.107].metric is 25, not 900",
      "two_networks.legs[10.77.7.107].automatic_metric is true, not false",
      "two_networks.legs[10.77.7.107].dns_registration is true, not false",
    ]);
    expect(withLeg({ netbios_options: null, file_sharing_bound: true, client_bound: null })).toEqual([
      "two_networks.legs[10.77.7.107].netbios_options is null, not 2",
      "two_networks.legs[10.77.7.107].file_sharing_bound is true, not false",
      "two_networks.legs[10.77.7.107].client_bound is null, not false",
    ]);
    expect(snv({ legs: [] })).toEqual(["two_networks.legs has 0, not 1"]);
  });

  test("#255 item D: no Dante leg is not judged; missing or unread facts fail", () => {
    // PP: two physical adapters on and no Dante output: nothing judged.
    expect(environmentFailures(holds())).toEqual([]);
    expect(environmentFailures(holds({ two_networks: null }))).toEqual([
      "two_networks is missing (a server without #255 item D?)",
    ]);
    const old = holds();
    delete old.two_networks;
    expect(environmentFailures(old)).toEqual(["two_networks is missing (a server without #255 item D?)"]);
    expect(environmentFailures(holds({ two_networks: { ...NO_DANTE_LEG, dante_ips: null } }))).toEqual([
      "two_networks.dante_ips is null, not a list",
    ]);
  });

  test("the gate waits for the first check", () => {
    expect(environmentChecked(holds())).toBe(true);
    expect(environmentChecked(holds({ checked: false }))).toBe(false);
    expect(environmentChecked(undefined)).toBe(false);
    expect(environmentChecked(null)).toBe(false);
  });
});
