//! Example demonstrating thread-safe Mode usage with custom variants

use pancetta_core::{ModeValue, StandardMode};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn main() {
    println!("Thread-Safe Mode Example\n");

    // Create various mode types
    let standard_mode = ModeValue::standard(StandardMode::FT8);
    let custom_mode = ModeValue::custom("OLIVIA-16/500");
    let experimental_mode = ModeValue::custom("EXPERIMENTAL-QAM64");

    println!("Created modes:");
    println!("  Standard: {}", standard_mode);
    println!("  Custom: {}", custom_mode);
    println!("  Experimental: {}", experimental_mode);

    // Demonstrate thread-safe sharing
    println!("\nSharing modes across threads:");

    let modes = vec![standard_mode, custom_mode, experimental_mode];
    let shared_modes = Arc::new(modes);

    let mut handles = vec![];

    for i in 0..3 {
        let modes_clone = Arc::clone(&shared_modes);
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(i * 100));

            println!("Thread {}: Processing {} modes", i, modes_clone.len());
            for (idx, mode) in modes_clone.iter().enumerate() {
                let mode_type = if mode.is_standard() {
                    "standard"
                } else {
                    "custom"
                };

                let properties = format!(
                    "digital={}, voice={}, cw={}",
                    mode.is_digital(),
                    mode.is_voice(),
                    mode.is_cw()
                );

                println!(
                    "  Thread {} - Mode[{}]: {} ({}) - {}",
                    i, idx, mode, mode_type, properties
                );

                if let Some(bandwidth) = mode.default_bandwidth() {
                    println!("    Bandwidth: {} Hz", bandwidth);
                }
            }
        });
        handles.push(handle);
    }

    // Wait for all threads to complete
    for handle in handles {
        handle.join().unwrap();
    }

    // Demonstrate parsing and serialization
    println!("\nParsing and Serialization:");

    let parsed: ModeValue = "VARA-HF".parse().unwrap();
    println!("  Parsed 'VARA-HF': {:?}", parsed);

    let json = serde_json::to_string(&parsed).unwrap();
    println!("  JSON serialized: {}", json);

    let deserialized: ModeValue = serde_json::from_str(&json).unwrap();
    println!("  Deserialized: {}", deserialized);

    // Demonstrate migration from old Mode
    println!("\nMigration from old Mode enum:");
    use pancetta_core::Mode;

    let old_mode = Mode::PSK31;
    let migrated: ModeValue = old_mode.into();
    println!("  Old Mode::PSK31 -> {}", migrated);

    // Demonstrate memory efficiency
    println!("\nMemory efficiency test:");
    let original = ModeValue::custom("TEST-MODE");
    let mut clones = vec![];

    for i in 0..1000 {
        clones.push(original.clone());
    }

    println!("  Created 1000 clones of custom mode");
    println!("  All clones share the same underlying data (Arc)");
    println!("  First clone == last clone: {}", clones[0] == clones[999]);
}
