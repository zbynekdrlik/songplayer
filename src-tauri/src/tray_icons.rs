//! Procedural 32x32 RGBA icon generation for the system tray.

/// Raw RGBA icon data.
pub struct IconData {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Gray icon for idle state.
pub fn make_idle_icon() -> IconData {
    make_icon(&[100, 100, 120])
}

/// Green icon for playing state.
#[allow(dead_code)]
pub fn make_playing_icon() -> IconData {
    make_icon(&[76, 175, 80])
}

/// Blue icon for downloading state.
#[allow(dead_code)]
pub fn make_downloading_icon() -> IconData {
    make_icon(&[33, 150, 243])
}

/// Red icon for error state.
#[allow(dead_code)]
pub fn make_error_icon() -> IconData {
    make_icon(&[233, 69, 96])
}

fn make_icon(color: &[u8; 3]) -> IconData {
    let size = 32u32;
    let mut data = vec![0u8; (size * size * 4) as usize];

    let corner_r = 6u32;

    for y in 0..size {
        for x in 0..size {
            let idx = ((y * size + x) * 4) as usize;
            let in_body = (x >= corner_r || y >= corner_r)
                && (x < size - corner_r || y >= corner_r)
                && (x >= corner_r || y < size - corner_r)
                && (x < size - corner_r || y < size - corner_r);

            if in_body || is_in_rounded_corner(x, y, size, corner_r) {
                data[idx] = color[0];
                data[idx + 1] = color[1];
                data[idx + 2] = color[2];
                data[idx + 3] = 255;
            }
        }
    }

    // Draw a white play triangle in the center.
    draw_play_symbol(&mut data, size, &[255, 255, 255]);

    IconData {
        data,
        width: size,
        height: size,
    }
}

/// Check whether (`x`, `y`) falls inside one of the four rounded corners.
fn is_in_rounded_corner(x: u32, y: u32, size: u32, r: u32) -> bool {
    // Determine which corner (if any) this pixel belongs to.
    let (cx, cy) = if x < r && y < r {
        (r, r) // top-left
    } else if x >= size - r && y < r {
        (size - r - 1, r) // top-right
    } else if x < r && y >= size - r {
        (r, size - r - 1) // bottom-left
    } else if x >= size - r && y >= size - r {
        (size - r - 1, size - r - 1) // bottom-right
    } else {
        return false;
    };

    let dx = x as i32 - cx as i32;
    let dy = y as i32 - cy as i32;
    (dx * dx + dy * dy) <= (r as i32 * r as i32)
}

/// Draw a simple play-button triangle (right-pointing) in the center.
fn draw_play_symbol(data: &mut [u8], size: u32, color: &[u8; 3]) {
    let cx = size / 2;
    let cy = size / 2;
    let half = size / 4; // triangle half-height

    for y in (cy - half)..=(cy + half) {
        // Width proportional to vertical position within triangle.
        let dy = (y as i32 - cy as i32).unsigned_abs();
        let width = half - dy;
        let x_start = cx - half / 3;
        for x in x_start..(x_start + width) {
            if x < size && y < size {
                let idx = ((y * size + x) * 4) as usize;
                data[idx] = color[0];
                data[idx + 1] = color[1];
                data[idx + 2] = color[2];
                data[idx + 3] = 255;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_icon_dimensions() {
        let icon = make_idle_icon();
        assert_eq!(icon.width, 32);
        assert_eq!(icon.height, 32);
        assert_eq!(icon.data.len(), 32 * 32 * 4);
    }

    #[test]
    fn playing_icon_has_non_zero_pixels() {
        let icon = make_playing_icon();
        let non_zero = icon.data.iter().filter(|&&b| b != 0).count();
        assert!(non_zero > 0);
    }

    #[test]
    fn all_icon_variants_correct_size() {
        for icon in [
            make_idle_icon(),
            make_playing_icon(),
            make_downloading_icon(),
            make_error_icon(),
        ] {
            assert_eq!(icon.data.len(), (icon.width * icon.height * 4) as usize);
        }
    }
}
