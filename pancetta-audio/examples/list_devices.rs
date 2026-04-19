use cpal::traits::{DeviceTrait, HostTrait};

fn main() {
    let host = cpal::default_host();
    println!("Audio host: {:?}", host.id());
    println!();

    println!("=== All devices ===");
    if let Ok(devices) = host.devices() {
        for device in devices {
            let name = device.name().unwrap_or_else(|_| "???".to_string());
            let in_cfgs = device
                .supported_input_configs()
                .map(|c| c.count())
                .unwrap_or(0);
            let out_cfgs = device
                .supported_output_configs()
                .map(|c| c.count())
                .unwrap_or(0);
            println!(
                "  {:40} input_configs: {}  output_configs: {}",
                name, in_cfgs, out_cfgs
            );
        }
    }

    println!();
    println!("=== Default input device ===");
    match host.default_input_device() {
        Some(d) => println!("  {}", d.name().unwrap_or_else(|_| "???".to_string())),
        None => println!("  (none)"),
    }

    println!("=== Default output device ===");
    match host.default_output_device() {
        Some(d) => println!("  {}", d.name().unwrap_or_else(|_| "???".to_string())),
        None => println!("  (none)"),
    }
}
