//! #233 (the owner, 8.10.2026, comment 6053701047: "chýba mi resample
//! informácia…"): a running ASIO output's figures as the dashboard shows
//! them, in Slovak, each a labelled chip with its own tooltip.
//!
//! The first line: the state, the latency, the underruns and the hard
//! re-centres (faults). The second line, the resampling:
//! - the conversion (the program's 48 kHz → the driver's rate);
//! - the card's clock against SongPlayer's (`rate_ppm`), and whether the
//!   estimate is locked;
//! - the resampler's correction (`ppm`), with what its sign means;
//! - the offset the ratio is draining, and the time it still needs;
//! - the last hard re-centre, if there was one.
//!
//! Pure (sp-ui has no unit tests): numbers are rounded here, half away from
//! zero, and written the Slovak way (a decimal comma, a true minus sign).

use crate::audio_outputs::PROGRAM_RATE;
use crate::audio_outputs_save::latency_sk;

/// One labelled figure: its test id, its text, its tooltip (`None`: the
/// row's own tooltip shows through).
#[derive(Clone, Debug, PartialEq)]
pub struct Chip {
    pub key: &'static str,
    pub text: String,
    pub title: Option<String>,
}

/// The last hard re-centre (`outputs[i].asio.last_hard_recentre`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LastFault {
    /// `deficit` or `excess`.
    pub cause: String,
    /// Inserted (> 0) or skipped (< 0), ms.
    pub ms: f64,
    /// The block's hand-off lateness, ms.
    pub lateness_ms: f64,
    pub ago_s: f64,
}

/// What a running ASIO output's chips are made of (`outputs[i]` and its
/// `asio`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AsioFigures {
    pub latency_ms: f64,
    pub underruns: u64,
    pub hard_recentres: u64,
    pub driver_rate: u32,
    pub rate_ppm: f64,
    pub locked: bool,
    pub ppm: f64,
    pub offset_ms: f64,
    pub slew_eta_s: Option<f64>,
    pub last_fault: Option<LastFault>,
}

pub const LATENCY_TIP: &str = "Čas od hranice programu SongPlayera po výstup z karty: cieľ 66,7 ms (+ nastavené oneskorenie výstupu), k tomu resampler a vlastné oneskorenie ovládača.";
pub const UNDERRUNS_TIP: &str = "Koľkokrát karta nedostala zvuk včas (podtečenie zásobníka), odkedy výstup beží — každé je krátka medzera v zvuku.";
pub const FAULTS_TIP: &str = "Koľkokrát musel výstup skokom vložiť ticho alebo preskočiť zvuk (s 5 ms prelínaním), lebo v zásobníku karty bolo zvuku primálo (vyschol by, alebo by oneskorený výstup hral o viac ako 133,3 ms skôr) alebo priveľa (pretiekol by). Je to porucha a je počuť; bežne 0 — rozdiely dorovnáva resampler plynule.";
pub const CONVERSION_TIP: &str = "Prevod frekvencie: program SongPlayera má 48 kHz, karta (ovládač ASIO) beží na svojej frekvencii; prevádza ho pásmovo obmedzený sinc resampler (256 koeficientov, BlackmanHarris², tabuľka 256×).";
pub const CARD_TIP: &str = "O koľko milióntin (ppm) tiknú hodiny karty rýchlejšie (+) alebo pomalšie (−) než hodiny SongPlayera. Odhad sa zamkne po minúte meraní (30 bodov).";
pub const CORRECTION_TIP: &str = "O koľko milióntin (ppm) resampler práve mení počet vzoriek, aby zvuk zo SongPlayera držal krok s kartou: + pridáva vzorky, − uberá. Plynulo a nepočuteľne; rozpočet ±300 ppm (asi pol centu).";
pub const SLEW_TIP: &str = "Trvalá odchýlka oneskorenia od cieľa (+ neskôr, − skôr), ktorú resampler plynule dorovnáva zmenou pomeru (najviac o 5 ppm za sekundu), a kedy bude v cieli. Bez skoku, nepočuteľne.";

/// `v` to tenths, signed: `+0,4`, `−0,7`, `0,0` (half away from zero).
pub fn tenths_sk(v: f64) -> String {
    let t = (v * 10.0).round() as i64;
    let sign = match t.signum() {
        1 => "+",
        -1 => "\u{2212}",
        _ => "",
    };
    let a = t.unsigned_abs();
    format!("{sign}{},{}", a / 10, a % 10)
}

/// A rate in kHz, the Slovak way: `48`, `44,1`, `22,05`.
pub fn khz_sk(hz: u32) -> String {
    let rest = hz % 1000;
    if rest == 0 {
        return (hz / 1000).to_string();
    }
    let decimals = format!("{rest:03}");
    format!("{},{}", hz / 1000, decimals.trim_end_matches('0'))
}

/// How long ago: whole seconds under a minute, whole minutes under an hour,
/// else whole hours.
pub fn ago_sk(s: f64) -> String {
    let s = s.max(0.0).floor() as u64;
    if s < 60 {
        format!("pred {s} s")
    } else if s < 3_600 {
        format!("pred {} min", s / 60)
    } else {
        format!("pred {} h", s / 3_600)
    }
}

/// The time a slew still needs: seconds under two minutes, else minutes
/// (each rounded, half away from zero).
pub fn eta_sk(s: f64) -> String {
    let s = s.max(0.0).round() as u64;
    if s < 120 {
        format!("ešte asi {s} s")
    } else {
        format!("ešte asi {} min", (s + 30) / 60)
    }
}

/// What a hard re-centre's cause means.
pub fn fault_cause_sk(cause: &str) -> &'static str {
    match cause {
        "deficit" => "v zásobníku chýbal zvuk",
        "excess" => "zásobník by pretiekol",
        _ => "neznámy dôvod",
    }
}

fn chip(key: &'static str, text: String, title: &str) -> Chip {
    Chip {
        key,
        text,
        title: Some(title.to_string()),
    }
}

/// The first line: the state, the latency ("meria sa" while the server reads
/// it 0, before the servo's first window), the underruns, the faults.
pub fn asio_state_chips(state: &str, f: &AsioFigures) -> Vec<Chip> {
    let latency = latency_sk(f.latency_ms);
    vec![
        Chip {
            key: "asio-state",
            text: state.to_string(),
            title: None,
        },
        chip("asio-latency", latency, LATENCY_TIP),
        chip(
            "asio-underruns",
            format!("výpadky {}", f.underruns),
            UNDERRUNS_TIP,
        ),
        chip(
            "asio-faults",
            format!("núdzové skoky {}", f.hard_recentres),
            FAULTS_TIP,
        ),
    ]
}

/// The second line: the conversion, the card's clock, the correction, the
/// slew, and the last fault when there was one.
pub fn asio_resampling_chips(f: &AsioFigures) -> Vec<Chip> {
    let program = khz_sk(PROGRAM_RATE);
    let conversion = if f.driver_rate == PROGRAM_RATE {
        format!("{program} kHz bez prevodu")
    } else {
        format!("{program} → {} kHz", khz_sk(f.driver_rate))
    };
    let card = if f.locked {
        format!(
            "karta {} ppm voči SongPlayeru (odhad zamknutý)",
            tenths_sk(f.rate_ppm)
        )
    } else {
        "karta voči SongPlayeru: odhad sa ešte meria".to_string()
    };
    let gloss = match (f.ppm * 10.0).round() as i64 {
        t if t > 0 => " (pridáva vzorky)",
        t if t < 0 => " (uberá vzorky)",
        _ => "",
    };
    let correction = format!("korekcia {} ppm{gloss}", tenths_sk(f.ppm));
    let slew = match f.slew_eta_s {
        Some(s) => format!(
            "dorovnáva odchýlku {} ms · {}",
            tenths_sk(f.offset_ms),
            eta_sk(s)
        ),
        None => "oneskorenie v cieli".to_string(),
    };
    let mut chips = vec![
        chip("asio-conversion", conversion, CONVERSION_TIP),
        chip("asio-card", card, CARD_TIP),
        chip("asio-correction", correction, CORRECTION_TIP),
        chip("asio-slew", slew, SLEW_TIP),
    ];
    if let Some(l) = &f.last_fault {
        let why = fault_cause_sk(&l.cause);
        chips.push(Chip {
            key: "asio-last-fault",
            text: format!(
                "posledný núdzový skok {}: {} ms ({why})",
                ago_sk(l.ago_s),
                tenths_sk(l.ms)
            ),
            title: Some(format!(
                "Núdzový skok: {} ms (+ vložené ticho, − preskočený zvuk), lebo {why}. Blok programu vtedy prišiel {} ms po svojej hranici.",
                tenths_sk(l.ms),
                tenths_sk(l.lateness_ms)
            )),
        });
    }
    chips
}

#[cfg(test)]
#[path = "asio_resampling_tests.rs"]
mod tests;
