//! What an ideal camera of the demo house would see of the floor, as a
//! PPM picture: rooms, walls, the lawn and a metre grid, in perspective.
//! For demos of calibration (no camera footage involved):
//!
//!     cargo run -p argus-plan --example render_view -- CAMERA OUT.ppm [WIDTH HEIGHT]

use argus_plan::calib::{picture_map, pinhole, project, to_picture};
use argus_plan::fixture::demo_house;
use argus_plan::geom::contains;
use argus_plan::outline::wall_pieces;
use argus_plan::rooms::room_polygon;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (name, out) = (&args[0], &args[1]);
    let w: usize = args.get(2).map_or(896, |v| v.parse().unwrap());
    let h: usize = args.get(3).map_or(672, |v| v.parse().unwrap());
    let plan = demo_house();
    let l = &plan.levels[0];
    let cam = l
        .cameras
        .iter()
        .find(|c| &c.camera == name)
        .expect("no such camera");
    let hm = pinhole(cam, w as f64 / h as f64).unwrap();
    let inv = picture_map(&hm).unwrap();
    let walls: Vec<Vec<_>> = l
        .walls
        .iter()
        .flat_map(|wl| wall_pieces(l, wl))
        .map(|q| q.to_vec())
        .collect();
    let rooms: Vec<(usize, Vec<_>)> = l
        .rooms
        .iter()
        .enumerate()
        .filter_map(|(i, r)| room_polygon(l, r).map(|p| (i, p)))
        .collect();
    let tones = [
        [196, 170, 140],
        [180, 182, 186],
        [205, 190, 160],
        [170, 160, 150],
        [190, 196, 200],
    ];
    let mut px = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            let f = [(x as f64 + 0.5) / w as f64, (y as f64 + 0.5) / h as f64];
            let c: [u8; 3] = match project(&hm, f) {
                // On the floor, in front of the camera (not its mirror image
                // behind it), and not too far.
                Some(p) if to_picture(&inv, p).is_some() && p.dist(cam.at) < 3.0 * cam.range => {
                    let mut c = if walls.iter().any(|q| contains(q, p)) {
                        [90, 88, 92]
                    } else if let Some((i, _)) = rooms.iter().find(|(_, poly)| contains(poly, p)) {
                        tones[i % tones.len()]
                    } else {
                        [96, 140, 80]
                    };
                    // A metre grid on the floor, and haze with distance.
                    let (gx, gy) = (
                        (p.x / 1000.0).rem_euclid(1.0),
                        (p.y / 1000.0).rem_euclid(1.0),
                    );
                    if gx < 0.02 || gy < 0.02 {
                        c = c.map(|v| (v as f64 * 0.82) as u8);
                    }
                    let haze = (p.dist(cam.at) / (3.0 * cam.range)).min(1.0);
                    c.map(|v| (v as f64 * (1.0 - 0.5 * haze) + 200.0 * 0.5 * haze) as u8)
                }
                _ => {
                    let t = y as f64 / h as f64;
                    [(150.0 + 60.0 * t) as u8, (180.0 + 40.0 * t) as u8, 225]
                }
            };
            px.extend_from_slice(&c);
        }
    }
    let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
    ppm.extend_from_slice(&px);
    std::fs::write(out, ppm).unwrap();
}
