//! Generates the folder shown in the Store trailer and screenshots.
//!
//! Separate from `make_samples`, which exists to exercise the grid and is
//! deliberately garish. This one has to look like a library worth browsing:
//! varied palettes, real motion, and names that read as content rather than as
//! test data. Everything is synthetic, so nothing personal ends up in a public
//! listing.
//!
//! ```
//! cargo run --release -p mandala-media --example make_demo -- demo
//! ```

#![cfg(windows)]

use std::f32::consts::TAU;
use std::path::{Path, PathBuf};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::HSTRING;

const FPS: u32 = 30;
const FRAME_HNS: i64 = 10_000_000 / FPS as i64;
const W: u32 = 1280;
const H: u32 = 720;

/// Names that read as a photo library rather than as generated files.
///
/// Enough of them that the grid scrolls at a sensible tile size: a trailer that
/// fits its whole folder on one screen has nothing to demonstrate.
const STEMS: &[&str] = &[
    "aurora", "beacon", "cascade", "drift", "ember", "flux", "glimmer", "harbour", "isle", "jetty",
    "kelp", "lantern", "meridian", "nocturne", "opal", "prism", "quarry", "ripple", "solstice",
    "tide", "umbra", "vellum", "willow", "zephyr",
];

fn pack(a: u32, b: u32) -> u64 {
    ((a as u64) << 32) | b as u64
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().unwrap_or_else(|| "demo".into()));
    let count: usize = args.next().and_then(|n| n.parse().ok()).unwrap_or(STEMS.len());
    std::fs::create_dir_all(&dir)?;
    println!("writing {count} items to {}", dir.display());

    // Stems repeat with a numeric suffix once they run out, so a larger demo
    // folder still reads as a library rather than as a list of test files.
    let names: Vec<String> = (0..count)
        .map(|i| {
            let stem = STEMS[i % STEMS.len()];
            if i < STEMS.len() {
                stem.to_owned()
            } else {
                format!("{stem}-{:02}", i / STEMS.len() + 1)
            }
        })
        .collect();

    for (i, name) in names.iter().enumerate() {
        let seed = i as u32;
        // Two clips for every still, since playing video is the thing being
        // demonstrated.
        if i % 3 == 2 {
            let (w, h) = [(1200u32, 800u32), (800, 1200), (1100, 1100)][i % 3];
            write_image(&dir.join(format!("{name}.png")), seed, w, h)?;
        } else {
            let seconds = 4 + (seed % 3);
            write_video(&dir.join(format!("{name}.mp4")), seed, seconds)?;
        }
        print!(".");
        use std::io::Write;
        std::io::stdout().flush().ok();
    }
    println!("\ndone");
    Ok(())
}

/// A palette spread around the colour wheel, so neighbouring tiles differ.
fn palette(seed: u32) -> [(u8, u8, u8); 3] {
    let base = (seed as f32 * 47.0) % 360.0;
    [hsv(base, 0.55, 0.22), hsv(base + 25.0, 0.72, 0.85), hsv(base + 200.0, 0.62, 0.95)]
}

fn hsv(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let h = (h.rem_euclid(360.0)) / 60.0;
    let c = v * s;
    let x = c * (1.0 - ((h % 2.0) - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match h as u32 % 6 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (
        ((r + m) * 255.0).round() as u8,
        ((g + m) * 255.0).round() as u8,
        ((b + m) * 255.0).round() as u8,
    )
}

/// Paints one frame. `t` is seconds since the clip started.
///
/// Four styles, chosen by seed. All of them move something large across the
/// frame, because motion has to be obvious in a tile a couple of hundred
/// pixels wide.
fn paint(pixels: &mut [u8], t: f32, seed: u32, w: u32, h: u32) {
    let [dark, bright, accent] = palette(seed);
    let (fw, fh) = (w as f32, h as f32);

    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32, y as f32);
            let (nx, ny) = (fx / fw - 0.5, fy / fh - 0.5);

            let value = match seed % 4 {
                // Concentric ripples.
                0 => {
                    let d = (nx * nx + ny * ny).sqrt();
                    ((d * 18.0 - t * 3.0).sin() * 0.5 + 0.5).powf(2.0)
                }
                // Diagonal sweep, the clearest motion at thumbnail size.
                1 => {
                    let p = (nx + ny) * 3.0 + t * 1.2;
                    ((p * TAU / 2.0).sin() * 0.5 + 0.5).powf(3.0)
                }
                // Orbiting blobs.
                2 => {
                    let mut v: f32 = 0.0;
                    for k in 0..4 {
                        let a = t * 1.1 + k as f32 * TAU / 4.0;
                        let (cx, cy) = (a.cos() * 0.28, a.sin() * 0.20);
                        let d = ((nx - cx).powi(2) + (ny - cy).powi(2)).sqrt();
                        v = v.max((1.0 - d * 6.0).clamp(0.0, 1.0));
                    }
                    v
                }
                // Interference, the busiest of the four.
                _ => {
                    let a = (nx * 12.0 + t * 2.0).sin();
                    let b = (ny * 12.0 - t * 1.6).sin();
                    ((a + b) * 0.25 + 0.5).clamp(0.0, 1.0)
                }
            };

            // Two-stop ramp: dark to bright to accent.
            let (r, g, b) = if value < 0.5 {
                mix(dark, bright, value * 2.0)
            } else {
                mix(bright, accent, (value - 0.5) * 2.0)
            };

            let at = ((y * w + x) * 4) as usize;
            pixels[at] = b;
            pixels[at + 1] = g;
            pixels[at + 2] = r;
            pixels[at + 3] = 255;
        }
    }
}

fn mix(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    (lerp(a.0, b.0), lerp(a.1, b.1), lerp(a.2, b.2))
}

fn write_image(path: &Path, seed: u32, w: u32, h: u32) -> anyhow::Result<()> {
    let mut bgra = vec![0u8; (w * h * 4) as usize];
    paint(&mut bgra, seed as f32 * 0.6, seed, w, h);

    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for px in bgra.as_chunks::<4>().0 {
        rgb.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    image::RgbImage::from_raw(w, h, rgb)
        .ok_or_else(|| anyhow::anyhow!("bad image dimensions"))?
        .save(path)?;
    Ok(())
}

fn write_video(path: &Path, seed: u32, seconds: u32) -> anyhow::Result<()> {
    let frames = FPS * seconds;

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)?;

        let writer = MFCreateSinkWriterFromURL(&HSTRING::from(path.as_os_str()), None, None)?;

        let output = MFCreateMediaType()?;
        output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        output.SetUINT32(&MF_MT_AVG_BITRATE, 6_000_000)?;
        output.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        output.SetUINT64(&MF_MT_FRAME_SIZE, pack(W, H))?;
        output.SetUINT64(&MF_MT_FRAME_RATE, pack(FPS, 1))?;
        output.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        let stream = writer.AddStream(&output)?;

        let input = MFCreateMediaType()?;
        input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        input.SetUINT64(&MF_MT_FRAME_SIZE, pack(W, H))?;
        input.SetUINT64(&MF_MT_FRAME_RATE, pack(FPS, 1))?;
        input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        input.SetUINT32(&MF_MT_DEFAULT_STRIDE, W * 4)?;
        writer.SetInputMediaType(stream, &input, None)?;
        writer.BeginWriting()?;

        let bytes = (W * H * 4) as usize;
        for index in 0..frames {
            let buffer = MFCreateMemoryBuffer(bytes as u32)?;
            let mut data: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut data, None, None)?;
            paint(
                std::slice::from_raw_parts_mut(data, bytes),
                index as f32 / FPS as f32,
                seed,
                W,
                H,
            );
            buffer.Unlock()?;
            buffer.SetCurrentLength(bytes as u32)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(index as i64 * FRAME_HNS)?;
            sample.SetSampleDuration(FRAME_HNS)?;
            writer.WriteSample(stream, &sample)?;
        }
        writer.Finalize()?;
    }
    Ok(())
}
