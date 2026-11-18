//! Prints the made-up demo house as a `plan.json` (version 1), for test
//! servers and screenshots:
//!
//!     cargo run -p argus-plan --example demo_plan > /tmp/scratch-root/plan.json
//!
//! With `--calibrated`, every camera is calibrated as an ideal camera of
//! its placement would be (for positions on the map), except any named
//! after it: `--calibrated garage` leaves the garage to calibrate by hand.
//!
//! Don't put it in a real server's storage root: it would replace that
//! plan.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut plan = argus_plan::fixture::demo_house();
    if args.first().is_some_and(|a| a == "--calibrated") {
        argus_plan::fixture::calibrate_ideal(&mut plan).unwrap();
        for l in &mut plan.levels {
            for c in &mut l.cameras {
                if args[1..].contains(&c.camera) {
                    c.calibration = None;
                }
            }
        }
    }
    let v = argus_plan::Versioned { version: 1, plan };
    println!("{}", serde_json::to_string_pretty(&v).unwrap());
}
