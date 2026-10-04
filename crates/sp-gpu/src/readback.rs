//! Reading a mapped render target back into packed rows (pure).
//!
//! A mapped staging texture's rows are `RowPitch` bytes apart, and the
//! driver may pad them past the pixels. The readback (`win/textures.rs`)
//! builds a slice of exactly [`mapped_len`] bytes over the mapping, which
//! needs a pitch of at least a row, and [`unpad_rows`] packs the rows.

/// The bytes a mapping of `rows` rows `pitch` bytes apart holds up to the
/// end of its last row of `row` bytes: `pitch·(rows − 1) + row`. `None` when
/// the pitch is shorter than a row (the rows would overlap) or the size
/// overflows. Never more than the mapping's `pitch·rows`.
pub fn mapped_len(pitch: usize, row: usize, rows: usize) -> Option<usize> {
    if pitch < row {
        return None;
    }
    pitch.checked_mul(rows.saturating_sub(1))?.checked_add(row)
}

/// The first `row` bytes of each of `rows` rows of `mapped`, rows `pitch`
/// bytes apart, packed one after another. `None` when `mapped` is shorter
/// than [`mapped_len`] says, or the pitch is shorter than a row.
pub fn unpad_rows(mapped: &[u8], pitch: usize, row: usize, rows: usize) -> Option<Vec<u8>> {
    let needed = mapped_len(pitch, row, rows)?;
    if mapped.len() < needed {
        return None;
    }
    let mut packed = Vec::new();
    // `max(1)`: a zero pitch passes `mapped_len` only with zero-byte rows.
    for line in mapped.chunks(pitch.max(1)).take(rows) {
        packed.extend_from_slice(&line[..row]);
    }
    Some(packed)
}

#[cfg(test)]
#[path = "readback_tests.rs"]
mod tests;
