//! Decodes recorded segments and reports the speed:
//!
//!     cargo run --release -p argus-detect --example decode_bench -- \
//!         INIT.mp4 SEGMENT.m4s... [--fps 5] [--save DIR]
//!
//! `--save` writes a few decoded pictures there as PPM, to check by eye.
//! Keep them out of the repository: they are camera images.

use std::time::Instant;

use argus_detect::decode::{AvcC, Decoder, Sampler};
use argus_media::{Frame, VideoConfig};

fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut opt = |name: &str| {
        let i = args.iter().position(|a| a == name)?;
        let v = args.get(i + 1).cloned();
        args.drain(i..(i + 2).min(args.len()));
        v
    };
    let fps: f64 = opt("--fps").map_or(5.0, |v| v.parse().unwrap());
    let save = opt("--save");
    let (init, segs) = args.split_first().expect("usage: INIT.mp4 SEGMENT.m4s...");
    let init = std::fs::read(init)?;
    let avcc = AvcC::find(&init).expect("no avcC in the init segment");
    let mut rec = vec![
        1,
        avcc.sps[0][1],
        avcc.sps[0][2],
        avcc.sps[0][3],
        0xFC | (avcc.length_size as u8 - 1),
        0xE1,
    ];
    rec.extend_from_slice(&(avcc.sps[0].len() as u16).to_be_bytes());
    rec.extend_from_slice(&avcc.sps[0]);
    rec.push(1);
    rec.extend_from_slice(&(avcc.pps[0].len() as u16).to_be_bytes());
    rec.extend_from_slice(&avcc.pps[0]);
    let cfg = VideoConfig::from_avcc(&rec, 0, 0)?;

    let mut frames = Vec::new();
    for s in segs {
        let b = bytes::Bytes::from(std::fs::read(s)?);
        let mut i = 0;
        let mut moof = None;
        while i + 8 <= b.len() {
            let size = u32::from_be_bytes(b[i..i + 4].try_into()?) as usize;
            if size < 8 || i + size > b.len() {
                break;
            }
            match &b[i + 4..i + 8] {
                b"moof" => moof = Some(i),
                b"mdat" => {
                    if let Some(m) = moof.take() {
                        for s in argus_media::fmp4::read_video_samples(&b.slice(m..i + size))
                            .unwrap_or_default()
                        {
                            frames.push(Frame {
                                pts_90k: s.dts_90k as i64,
                                wall_us: (s.dts_90k as i64) * 100 / 9,
                                keyframe: s.keyframe,
                                data: s.data,
                            });
                        }
                    }
                }
                _ => {}
            }
            i += size;
        }
    }
    let secs = (frames.last().map_or(0, |f| f.wall_us) - frames.first().map_or(0, |f| f.wall_us))
        as f64
        / 1e6;
    println!(
        "{} frames, {:.0} s of video ({:.1} fps), {} keyframes",
        frames.len(),
        secs,
        frames.len() as f64 / secs.max(1e-9),
        frames.iter().filter(|f| f.keyframe).count()
    );

    let mut d = Decoder::new()?;
    d.configure(&cfg)?;
    let mut sampler = Sampler::new(fps);
    let (mut pictures, mut errors, mut saved) = (0, 0, 0);
    let mut size = (0, 0);
    let t0 = Instant::now();
    for f in &frames {
        let want = sampler.take(f.wall_us);
        match d.decode(f, want) {
            Ok(Some(p)) => {
                pictures += 1;
                size = (p.width, p.height);
                if let Some(dir) = &save
                    && saved < 3
                    && pictures % 50 == 1
                {
                    let mut ppm = format!("P6\n{} {}\n255\n", p.width, p.height).into_bytes();
                    ppm.extend_from_slice(&p.rgb);
                    std::fs::write(format!("{dir}/picture-{saved}.ppm"), ppm)?;
                    saved += 1;
                }
            }
            Ok(None) => {}
            Err(e) => {
                errors += 1;
                if errors < 5 {
                    eprintln!("decode error: {e}");
                }
            }
        }
    }
    let el = t0.elapsed().as_secs_f64();
    println!(
        "{}x{}: decoded {} frames in {:.2} s = {:.2} ms a frame ({} RGB pictures at {fps} fps, {errors} errors)",
        size.0,
        size.1,
        frames.len(),
        el,
        el * 1e3 / frames.len().max(1) as f64,
        pictures
    );
    println!(
        "one camera in real time: {:.1}% of a core",
        el / secs.max(1e-9) * 100.0
    );
    Ok(())
}
