//! Runs the detector on PPM pictures (e.g. from decode_bench --save) and
//! reports what it finds and how long it takes:
//!
//!     cargo run --release -p argus-detect --features cuda --example detect_bench -- \
//!         MODEL.onnx PICTURE.ppm... [--cuda] [--runs 20] [--boxes DIR] [--pose POSE.onnx]
//!
//! `--boxes` writes the pictures with their boxes drawn, to check by eye.
//! Keep them out of the repository: they are camera images.

use std::time::Instant;

use argus_detect::decode::Picture;
use argus_detect::model::{Detector, Device};

fn read_ppm(path: &str) -> anyhow::Result<Picture> {
    let b = std::fs::read(path)?;
    let mut fields = Vec::new();
    let mut i = 0;
    while fields.len() < 4 {
        while b[i].is_ascii_whitespace() {
            i += 1;
        }
        let s = i;
        while !b[i].is_ascii_whitespace() {
            i += 1;
        }
        fields.push(String::from_utf8_lossy(&b[s..i]).into_owned());
    }
    anyhow::ensure!(fields[0] == "P6", "not a binary PPM");
    let (w, h): (u32, u32) = (fields[1].parse()?, fields[2].parse()?);
    Ok(Picture {
        width: w,
        height: h,
        rgb: b[i + 1..i + 1 + (w * h * 3) as usize].to_vec(),
        wall_us: 0,
    })
}

fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |args: &mut Vec<String>, name: &str| {
        let i = args.iter().position(|a| a == name)?;
        args.remove(i);
        Some(())
    };
    let opt = |args: &mut Vec<String>, name: &str| {
        let i = args.iter().position(|a| a == name)?;
        let v = args.remove(i + 1);
        args.remove(i);
        Some(v)
    };
    let cuda = flag(&mut args, "--cuda").is_some();
    let runs: usize = opt(&mut args, "--runs").map_or(10, |v| v.parse().unwrap());
    let boxes = opt(&mut args, "--boxes");
    let pose = opt(&mut args, "--pose");
    let (model, pics) = args
        .split_first()
        .expect("usage: MODEL.onnx PICTURE.ppm...");
    let pictures: Vec<Picture> = pics.iter().map(|p| read_ppm(p)).collect::<Result<_, _>>()?;
    let t = Instant::now();
    let mut d = Detector::open(
        model.as_ref(),
        if cuda { Device::Cuda(0) } else { Device::Cpu },
    )?;
    println!(
        "loaded in {:.2} s on {:?}",
        t.elapsed().as_secs_f64(),
        d.device
    );
    let refs: Vec<&Picture> = pictures.iter().collect();
    let mut found = d.detect(&refs)?; // warm-up, and the answer
    if let Some(path) = &pose {
        let mut pm = argus_detect::pose::PoseModel::open(
            path.as_ref(),
            if cuda { Device::Cuda(0) } else { Device::Cpu },
        )?;
        for (p, f) in pictures.iter().zip(found.iter_mut()) {
            let people: Vec<usize> = (0..f.len()).filter(|&i| f[i].label == "person").collect();
            let bx: Vec<_> = people.iter().map(|&i| f[i].bbox).collect();
            let t = Instant::now();
            let feet = pm.feet(p, &bx)?;
            if !bx.is_empty() {
                println!(
                    "pose: {} people in {:.1} ms",
                    bx.len(),
                    t.elapsed().as_secs_f64() * 1e3
                );
            }
            for (&i, ft) in people.iter().zip(feet) {
                f[i].foot = Some(ft);
            }
        }
    }
    for (name, (p, f)) in pics.iter().zip(pictures.iter().zip(&found)) {
        println!("{name}:");
        for x in f {
            println!(
                "  {:<7} {:<10} {:.2}  [{:.0}, {:.0}, {:.0}, {:.0}]  foot {:?}",
                x.label, x.class, x.score, x.bbox[0], x.bbox[1], x.bbox[2], x.bbox[3], x.foot
            );
        }
        if let Some(dir) = &boxes {
            let mut rgb = p.rgb.clone();
            let w = p.width as usize;
            for x in f {
                let [l, t, r, b] = x.bbox.map(|v| v as usize);
                let b = b.min(p.height as usize - 1);
                let r = r.min(w - 1);
                for xx in l..=r {
                    for yy in [t, b] {
                        rgb[(yy * w + xx) * 3..(yy * w + xx) * 3 + 3]
                            .copy_from_slice(&[255, 0, 255]);
                    }
                }
                for yy in t..=b {
                    for xx in [l, r] {
                        rgb[(yy * w + xx) * 3..(yy * w + xx) * 3 + 3]
                            .copy_from_slice(&[255, 0, 255]);
                    }
                }
            }
            for x in f {
                if let Some(ft) = x.foot {
                    let c = if ft.ankles {
                        [0, 255, 0]
                    } else {
                        [255, 255, 0]
                    };
                    for dy in -4i32..=4 {
                        for dx in -4i32..=4 {
                            let (xx, yy) = (ft.x as i32 + dx, ft.y as i32 + dy);
                            if xx >= 0 && yy >= 0 && (xx as u32) < p.width && (yy as u32) < p.height
                            {
                                let k = (yy as usize * w + xx as usize) * 3;
                                rgb[k..k + 3].copy_from_slice(&c);
                            }
                        }
                    }
                }
            }
            let mut ppm = format!("P6\n{} {}\n255\n", p.width, p.height).into_bytes();
            ppm.extend_from_slice(&rgb);
            let base = std::path::Path::new(name)
                .file_name()
                .unwrap()
                .to_string_lossy();
            std::fs::write(format!("{dir}/boxes-{base}"), ppm)?;
        }
    }
    for batch in [1, refs.len()] {
        let t = Instant::now();
        for _ in 0..runs {
            for chunk in refs.chunks(batch) {
                d.detect(chunk)?;
            }
        }
        let per = t.elapsed().as_secs_f64() * 1e3 / (runs * refs.len()) as f64;
        println!("batch {batch}: {per:.1} ms a picture");
    }
    Ok(())
}
