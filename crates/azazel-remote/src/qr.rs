//! QR codes for pairing links.

use qrcodegen::{QrCode, QrCodeEcc};

/// The modules of the code for `text`, `true` for a dark one, without the quiet zone.
pub fn qr_matrix(text: &str) -> Vec<Vec<bool>> {
    // A link is far below the capacity of the largest code; the fallback keeps the signature total.
    let Ok(code) = QrCode::encode_text(text, QrCodeEcc::Medium) else {
        return Vec::new();
    };
    (0..code.size())
        .map(|y| (0..code.size()).map(|x| code.get_module(x, y)).collect())
        .collect()
}

/// The code as an SVG image with a four-module quiet zone, in black on white.
pub fn qr_svg(text: &str) -> String {
    let matrix = qr_matrix(text);
    let size = matrix.len() + 8;
    let mut path = String::new();
    for (y, row) in matrix.iter().enumerate() {
        for (x, dark) in row.iter().enumerate() {
            if *dark {
                path.push_str(&format!("M{},{}h1v1h-1z", x + 4, y + 4));
            }
        }
    }
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {size} {size}\" shape-rendering=\"crispEdges\">\
<rect width=\"{size}\" height=\"{size}\" fill=\"#fff\"/><path d=\"{path}\" fill=\"#000\"/></svg>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_matrix_is_square_with_the_finder_patterns_in_three_corners() {
        let matrix = qr_matrix("https://example.com/#pair=abc");
        let size = matrix.len();
        assert!(size >= 21 && matrix.iter().all(|row| row.len() == size));
        for (x, y) in [(0, 0), (size - 7, 0), (0, size - 7)] {
            assert!(matrix[y][x] && matrix[y + 6][x + 6] && !matrix[y + 1][x + 1]);
        }
    }

    #[test]
    fn the_svg_draws_every_dark_module() {
        let svg = qr_svg("hello");
        let dark: usize = qr_matrix("hello").iter().flatten().filter(|d| **d).count();
        assert_eq!(svg.matches('M').count(), dark);
        assert!(svg.starts_with("<svg"));
    }
}
