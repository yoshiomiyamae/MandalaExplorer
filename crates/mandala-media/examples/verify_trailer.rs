//! Verifies a finished trailer against what the Store requires.
#![cfg(windows)]
use mandala_media::backend::{Advance, MediaBackend};
use std::path::PathBuf;
use std::time::Duration;

fn main() -> anyhow::Result<()> {
    let path = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "trailer.mp4".into()));
    let backend = mandala_media::default_backend()?;

    // Opened at a size far larger than the file, so no downscale is applied and
    // the reported frame size is the file's own.
    let mut stream = backend.open_video(&path, (4096, 4096))?;
    let duration = stream.duration();

    let Advance::Frame(first) = stream.advance_to(Duration::ZERO)? else {
        anyhow::bail!("no first frame");
    };

    let bytes = std::fs::metadata(&path)?.len();
    println!("file      {}", path.display());
    println!("size      {:.1} MB   (Store limit 2048 MB)", bytes as f64 / 1048576.0);
    println!("frame     {}x{}   (Store requires 1920x1080)", first.width, first.height);
    match duration {
        Some(d) => println!("duration  {:.1}s   (60s or less recommended)", d.as_secs_f64()),
        None => println!("duration  unknown"),
    }

    // Walk it to the end, to prove the whole file decodes rather than just its
    // first frame.
    let mut frames = 1;
    let mut t = Duration::ZERO;
    loop {
        t += Duration::from_millis(100);
        match stream.advance_to(t)? {
            Advance::Frame(_) => frames += 1,
            Advance::Unchanged => {}
            Advance::EndOfStream => break,
        }
        if t > Duration::from_secs(120) {
            anyhow::bail!("did not reach the end within two minutes");
        }
    }
    println!("decoded   {frames} sample points to the end without error");

    let ok = first.width == 1920
        && first.height == 1080
        && bytes < 2 * 1024 * 1024 * 1024
        && duration.is_some_and(|d| d <= Duration::from_secs(60));
    println!("\n{}", if ok { "PASS" } else { "FAILS a Store requirement" });
    Ok(())
}
