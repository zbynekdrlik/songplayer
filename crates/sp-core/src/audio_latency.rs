//! #289: the program audio's low-latency switch, `audio_low_latency`.
//!
//! ONE setting, a boolean, DEFAULT OFF: absent, blank or anything but
//! `"true"` (trimmed, any case) reads OFF ([`low_latency`]). The outputs task
//! reads it live on its 5 s pass; a change rebuilds every VBAN output (its
//! send latency L = 15 ms instead of two slots, a converting destination no
//! longer padded to `rate / 60`). The settings PATCH takes `"true"`,
//! `"false"` or `""` only ([`checked`]); Nastavenia "Zvukové výstupy" sends it
//! only when its checkbox differs from the value the page loaded
//! ([`low_latency_to_send`]).

/// The setting's key.
pub const SETTING_AUDIO_LOW_LATENCY: &str = "audio_low_latency";

/// The checkbox's label in Nastavenia "Zvukové výstupy".
pub const LABEL_SK: &str = "Nízka latencia zvuku (~15 ms)";

/// The checkbox's tooltip: which outputs change and by how much, and what must
/// be measured before it stays on (the owner, #289: LED wall against FOH).
pub const TOOLTIP_SK: &str = "Výstupy VBAN odosielajú zvuk programu skôr voči obrazu: 15 ms \
     namiesto 67 ms pri 48 kHz, 18 ms namiesto 83 ms pri 96 kHz. ASIO a Dante sa nemenia. \
     Kým zostane zapnutá, treba premerať LED stenu voči FOH.";

/// The stored `audio_low_latency` read: `"true"` (trimmed, any case) = ON;
/// unset, blank or any other value = OFF.
pub fn low_latency(raw: Option<&str>) -> bool {
    raw.is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
}

/// The settings PATCH check: `audio_low_latency` takes `"true"`, `"false"`
/// or `""` (trimmed, any case; stored lower case), anything else is refused
/// with the reason. Every other key passes unchanged.
pub fn checked(key: &str, value: &str) -> Result<String, String> {
    if key != SETTING_AUDIO_LOW_LATENCY {
        return Ok(value.to_string());
    }
    let v = value.trim().to_ascii_lowercase();
    match v.as_str() {
        "" | "true" | "false" => Ok(v),
        _ => Err(format!("{SETTING_AUDIO_LOW_LATENCY} must be true or false")),
    }
}

/// What a save sends for the switch: `"true"` / `"false"` only when the
/// checkbox differs from the value the page loaded (`loaded`, read by
/// [`low_latency`]), else nothing.
pub fn low_latency_to_send(loaded: Option<&str>, checked: bool) -> Option<String> {
    (low_latency(loaded) != checked).then(|| checked.to_string())
}

#[cfg(test)]
#[path = "audio_latency_tests.rs"]
mod tests;
