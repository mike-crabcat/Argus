//! Grid layout: how to arrange N video tiles in a rectangle.

/// The column count that makes `n` tiles of aspect `tile_aspect`
/// (width / height) as large as possible inside `width` × `height` with
/// `gap` between tiles. Returns (columns, rows, tile width, tile height).
pub fn grid(
    n: usize,
    width: f32,
    height: f32,
    gap: f32,
    tile_aspect: f32,
) -> (usize, usize, f32, f32) {
    if n == 0 {
        return (0, 0, 0.0, 0.0);
    }
    let mut best = (1, n, 0.0f32, 0.0f32);
    for cols in 1..=n {
        let rows = n.div_ceil(cols);
        let cell_w = (width - gap * (cols as f32 - 1.0)) / cols as f32;
        let cell_h = (height - gap * (rows as f32 - 1.0)) / rows as f32;
        if cell_w <= 0.0 || cell_h <= 0.0 {
            continue;
        }
        // The largest tile of the right aspect that fits the cell.
        let (w, h) = if cell_w / cell_h > tile_aspect {
            (cell_h * tile_aspect, cell_h)
        } else {
            (cell_w, cell_w / tile_aspect)
        };
        if w * h > best.2 * best.3 {
            best = (cols, rows, w, h);
        }
    }
    best
}

/// The largest rectangle of aspect `aspect` centred in `w` × `h`, as
/// (x offset, y offset, width, height).
pub fn fit(w: f32, h: f32, aspect: f32) -> (f32, f32, f32, f32) {
    if w / h > aspect {
        let fw = h * aspect;
        ((w - fw) / 2.0, 0.0, fw, h)
    } else {
        let fh = w / aspect;
        (0.0, (h - fh) / 2.0, w, fh)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn five_cameras_on_a_wide_screen_use_three_columns() {
        let (cols, rows, w, h) = grid(5, 1600.0, 900.0, 8.0, 4.0 / 3.0);
        assert_eq!((cols, rows), (3, 2));
        assert!((w / h - 4.0 / 3.0).abs() < 1e-4);
        assert!(w * 3.0 + 16.0 <= 1600.0 + 1e-3 && h * 2.0 + 8.0 <= 900.0 + 1e-3);
    }

    #[test]
    fn portrait_phone_stacks_tiles() {
        let (cols, rows, _, _) = grid(5, 400.0, 860.0, 8.0, 4.0 / 3.0);
        assert_eq!((cols, rows), (1, 5));
        assert_eq!(grid(1, 800.0, 600.0, 8.0, 4.0 / 3.0).0, 1);
        assert_eq!(grid(0, 800.0, 600.0, 8.0, 1.0), (0, 0, 0.0, 0.0));
    }

    #[test]
    fn fit_letterboxes() {
        assert_eq!(fit(200.0, 100.0, 1.0), (50.0, 0.0, 100.0, 100.0));
        assert_eq!(fit(100.0, 200.0, 1.0), (0.0, 50.0, 100.0, 100.0));
    }
}
