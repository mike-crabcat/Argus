//! Prints the made-up demo house as a `plan.json` (version 1), for test
//! servers and screenshots:
//!
//!     cargo run -p argus-plan --example demo_plan > /tmp/scratch-root/plan.json
//!
//! Don't put it in a real server's storage root: it would replace that
//! plan.

fn main() {
    let v = argus_plan::Versioned {
        version: 1,
        plan: argus_plan::fixture::demo_house(),
    };
    println!("{}", serde_json::to_string_pretty(&v).unwrap());
}
