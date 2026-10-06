//! Generates assets/menubar-template.png, the macOS menu bar template icon.
//!
//! Run from the repository root: cargo run --example gen_menubar_icon

use std::fs::File;
use std::io::BufWriter;

const OUT_SIZE: usize = 64;
const SCALE: usize = 8;
const SIZE: usize = OUT_SIZE * SCALE;

/// A rounded C monogram in a 256-unit design space. Its open center and broad
/// terminals stay legible when macOS reduces it to menu bar size.
const MONOGRAM: [(f64, f64); 12] = [
    (198.0, 45.0),
    (100.0, 45.0),
    (78.0, 51.0),
    (61.0, 66.0),
    (49.0, 88.0),
    (45.0, 106.0),
    (45.0, 150.0),
    (49.0, 168.0),
    (61.0, 190.0),
    (78.0, 205.0),
    (100.0, 211.0),
    (198.0, 211.0),
];
const STROKE_RADIUS: f64 = 11.0;
const UNIT: f64 = SIZE as f64 / 256.0;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut big = vec![0u8; SIZE * SIZE];
    for y in 0..SIZE {
        for x in 0..SIZE {
            big[y * SIZE + x] = alpha_at(x as f64 + 0.5, y as f64 + 0.5);
        }
    }
    let mut rgba = vec![0u8; OUT_SIZE * OUT_SIZE * 4];
    for y in 0..OUT_SIZE {
        for x in 0..OUT_SIZE {
            let mut sum = 0u64;
            for sy in 0..SCALE {
                for sx in 0..SCALE {
                    sum += u64::from(big[(y * SCALE + sy) * SIZE + x * SCALE + sx]);
                }
            }
            rgba[(y * OUT_SIZE + x) * 4 + 3] = (sum / (SCALE * SCALE) as u64) as u8;
        }
    }
    let file = BufWriter::new(File::create("assets/menubar-template.png")?);
    let mut encoder = png::Encoder::new(file, OUT_SIZE as u32, OUT_SIZE as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&rgba)?;
    eprintln!("wrote assets/menubar-template.png ({OUT_SIZE}x{OUT_SIZE})");
    Ok(())
}

fn alpha_at(x: f64, y: f64) -> u8 {
    let p = (x / UNIT, y / UNIT);
    let distance = MONOGRAM.windows(2).map(|w| segment(p, w[0], w[1])).fold(f64::INFINITY, f64::min) - STROKE_RADIUS;
    // Supersampling handles most edge smoothing; this narrow band softens steps.
    if distance <= -0.35 {
        255
    } else if distance >= 0.35 {
        0
    } else {
        (255.0 * (0.5 - distance / 0.7)) as u8
    }
}

/// Distance from `p` to the line segment ab.
fn segment(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (abx, aby) = (b.0 - a.0, b.1 - a.1);
    let (apx, apy) = (p.0 - a.0, p.1 - a.1);
    let length_sq = abx * abx + aby * aby;
    let t = if length_sq > 0.0 { ((apx * abx + apy * aby) / length_sq).clamp(0.0, 1.0) } else { 0.0 };
    (apx - t * abx).hypot(apy - t * aby)
}
