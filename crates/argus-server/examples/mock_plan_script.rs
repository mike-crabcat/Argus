//! Prints the scripted model conversation used to demo plan import with
//! the mock provider (see `argus_server::plan_agent::demo`):
//!
//!     cargo run -p argus-server --example mock_plan_script > /tmp/x/mock.json
//!
//! then, in a test config only:
//!
//!     [agent]
//!     provider = "mock"
//!     model = "mock"
//!     mock_script = "/tmp/x/mock.json"
//!
//! and start argus with ARGUS_TEST_MOCK_AGENT=1.

fn main() {
    let level = argus_plan::Plan::default().levels[0].id;
    println!(
        "{}",
        serde_json::to_string_pretty(&argus_server::plan_agent::demo::mock_script(level)).unwrap()
    );
}
