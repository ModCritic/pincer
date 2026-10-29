//! Write the golden trajectories shared with `pincer-cursor-motion`:
//!
//! ```bash
//! cargo run -p pincer-cursor-motion --features serde --example export_golden \
//!   > crates/pincer-cursor-motion/fixtures/golden.json
//! ```

#[path = "../tests/common/golden_cases.rs"]
mod golden_cases;

fn main() {
    println!(
        "{}",
        serde_json::to_string(&golden_cases::golden()).unwrap()
    );
}
