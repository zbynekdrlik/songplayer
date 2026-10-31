//! #285: a running Dante output's figures and texts. The output runs on the
//! ASIO worker (`asio_out`), so its servo figures are `AsioFigures`; this
//! module adds the stack's own (`outputs[i].dante`) and words every chip and
//! reason for an output that has no card and no driver.

use serde::{Deserialize, Serialize};

use crate::asio_resampling::{
    AsioFigures, Chip, asio_resampling_chips, asio_state_chips, tenths_sk,
};

/// `outputs[i].dante`: dstack's status line, its TX flows summed.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct DanteFigures {
    /// The media clock runs: dantesync answers and PTP is locked.
    pub clock_ready: bool,
    /// The flows a receiver holds now.
    pub live_flows: u32,
    /// Packets by how late they left: under 0.25, 0.5, 1, 2, 5 ms, and more.
    pub late_bins: Vec<u64>,
    pub late_max_us: u64,
    pub send_max_us: u64,
    pub pump_late: u64,
    pub pump_missed: u64,
    pub pump_overruns: u64,
}

const CLOCK_TIP: &str = "Hodiny siete Dante: SongPlayer ich číta z dantesync. Bez nich zariadenie ostáva ohlásené v sieti, ale neposiela zvuk.";
const FLOWS_TIP: &str =
    "Koľko prúdov zvuku odoberá zariadenie v sieti Dante (nastavuje sa v Dante Controlleri).";
const LATE_TIP: &str = "Najväčšie oneskorenie paketu pri odchode a najdlhšie jedno odoslanie, odkedy výstup beží. Musí ostať hlboko pod latenciou zariadenia 10 ms.";
const PUMP_TIP: &str = "Bloky zvuku, ktoré vlákno zariadenia spracovalo neskoro, vynechalo alebo nestihlo, odkedy výstup beží.";

/// The ASIO chips' phrases for a card or a driver, as a Dante output's, whole
/// so the grammar holds (`karta` is feminine, `zariadenie` neuter).
const WORDS: [(&str, &str); 12] = [
    ("výstup z karty", "výstup do siete Dante"),
    (
        "vlastné oneskorenie ovládača",
        "oneskorenie zariadenia Dante",
    ),
    (
        "Koľkokrát karta nedostala",
        "Koľkokrát zariadenie Dante nedostalo",
    ),
    ("v zásobníku karty", "v zásobníku zariadenia Dante"),
    (
        "Koľkokrát ovládač karty (ASIO) požiadal",
        "Koľkokrát zariadenie Dante požiadalo",
    ),
    ("ovládač sa nezatvára", "zariadenie sa nezatvára"),
    (
        "karta (ovládač ASIO) beží na svojej frekvencii",
        "zariadenie Dante beží na frekvencii siete",
    ),
    ("hodiny karty", "hodiny siete Dante"),
    ("s kartou", "so sieťou Dante"),
    (
        "resynchronizácie ovládača",
        "resynchronizácie zariadenia Dante",
    ),
    ("karta voči", "sieť Dante voči"),
    ("karta ", "sieť Dante "),
];

fn dante_words(text: &str) -> String {
    WORDS
        .iter()
        .fold(text.to_string(), |t, (from, to)| t.replace(from, to))
}

fn worded(chips: Vec<Chip>) -> Vec<Chip> {
    chips
        .into_iter()
        .map(|c| Chip {
            key: c.key,
            text: dante_words(&c.text),
            title: c.title.as_deref().map(dante_words),
        })
        .collect()
}

fn chip(key: &'static str, text: String, title: &str) -> Chip {
    Chip {
        key,
        text,
        title: Some(title.to_string()),
    }
}

/// `us` as ms to tenths, unsigned.
fn ms_sk(us: u64) -> String {
    tenths_sk(us as f64 / 1_000.0)
}

/// The first line: the ASIO worker's chips, then the clock, the flows, the
/// lateness (only when the stack answered), and the pump once it slipped.
pub fn dante_state_chips(state: &str, f: &AsioFigures, d: Option<&DanteFigures>) -> Vec<Chip> {
    let mut chips = worded(asio_state_chips(state, f));
    let Some(d) = d else { return chips };
    let clock = if d.clock_ready {
        "hodiny Dante v poriadku"
    } else {
        "čaká na hodiny Dante"
    };
    chips.push(chip("dante-clock", clock.to_string(), CLOCK_TIP));
    chips.push(chip(
        "dante-flows",
        format!("prúdy {}", d.live_flows),
        FLOWS_TIP,
    ));
    chips.push(chip(
        "dante-late",
        format!(
            "paket neskoro max {} ms · odoslanie max {} ms",
            ms_sk(d.late_max_us),
            ms_sk(d.send_max_us)
        ),
        LATE_TIP,
    ));
    if d.pump_late + d.pump_missed + d.pump_overruns > 0 {
        chips.push(chip(
            "dante-pump",
            format!(
                "bloky: neskoro {} · vynechané {} · nestihnuté {}",
                d.pump_late, d.pump_missed, d.pump_overruns
            ),
            PUMP_TIP,
        ));
    }
    chips
}

/// The second line: the ASIO worker's resampling, in a Dante output's words.
pub fn dante_resampling_chips(f: &AsioFigures) -> Vec<Chip> {
    worded(asio_resampling_chips(f))
}

/// The Slovak reason of a waiting Dante output, by the worker's stable code.
pub fn dante_reason_sk(code: &str) -> &'static str {
    match code {
        "refused" => "zariadenie Dante sa nedá spustiť — skontroluj názov a adresu IP",
        "failed" => "chyba zariadenia Dante",
        "reset" => "zariadenie Dante sa reštartuje",
        "rate_changed" => "zariadenie Dante zmenilo frekvenciu",
        "clock_lost" => "zariadenie Dante stratilo hodiny",
        "stalled" => "zariadenie Dante neodpovedá",
        "held" => "predchádzajúci výstup ešte neuvoľnil zariadenie Dante",
        "parked" => "zariadenie Dante zamrzlo — pomôže len reštart SongPlayera",
        "no_clock" => "zariadenie Dante nemá hodiny — dantesync nebeží alebo v sieti nie je PTP",
        _ => "neznámy dôvod",
    }
}

/// The dashboard's line for a Dante output that is not running: as
/// `asio_waiting_text`, with no next try for a parked device.
pub fn dante_waiting_text(
    state: &str,
    reason_code: Option<&str>,
    retry_in_s: Option<f64>,
) -> String {
    let mut parts = vec![state.to_string()];
    if let Some(code) = reason_code {
        parts.push(dante_reason_sk(code).to_string());
    }
    if let Some(s) = retry_in_s
        && reason_code != Some("parked")
    {
        parts.push(format!("ďalší pokus o {s:.0} s"));
    }
    parts.join(" · ")
}

#[cfg(test)]
#[path = "dante_status_tests.rs"]
mod tests;
