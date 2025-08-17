//! Basic QSO management example
//! 
//! This example demonstrates basic QSO operations including:
//! - Starting a CQ call
//! - Responding to CQ calls
//! - Processing incoming messages
//! - Managing QSO state transitions

use pancetta_qso::*;
use std::error::Error;
use tokio::time::{sleep, Duration};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Initialize logging
    tracing_subscriber::fmt::init();
    
    println!("=== Pancetta QSO Basic Example ===");
    println!("Library version: {}", VERSION);
    
    // Create a basic QSO system
    let system = QsoSystem::new("W1ABC".to_string(), Some("FN42".to_string())).await?;
    
    println!("✓ QSO system initialized for W1ABC at FN42");
    
    // Start a CQ call on 20m FT8
    let frequency = 14074000.0; // 20m FT8 frequency
    let qso_id = system.start_cq(frequency).await?;
    
    println!("✓ Started CQ call on {:.3} MHz (QSO ID: {})", frequency / 1_000_000.0, qso_id);
    
    // Check QSO status
    let progress = system.get_qso(qso_id).await?;
    println!("  Current state: {:?}", progress.state);
    
    // Simulate receiving a response to our CQ
    sleep(Duration::from_millis(100)).await;
    
    let response_message = MessageType::CqResponse {
        calling_station: "W1ABC".to_string(),
        responding_station: "K1DEF".to_string(),
        grid: Some("FN31".to_string()),
    };
    
    println!("📡 Simulating incoming message: K1DEF responding to CQ");
    
    system.process_message(
        response_message,
        "W1ABC K1DEF FN31".to_string(),
        frequency,
        Some(-15.0), // Signal strength
    ).await?;
    
    // Check updated QSO status
    sleep(Duration::from_millis(50)).await;
    let progress = system.get_qso(qso_id).await?;
    println!("  Updated state: {:?}", progress.state);
    
    // Simulate sending a signal report
    sleep(Duration::from_millis(100)).await;
    
    let report_message = MessageType::SignalReport {
        to_station: "K1DEF".to_string(),
        from_station: "W1ABC".to_string(),
        report: -12,
    };
    
    println!("📡 Simulating outgoing signal report: -12 dB");
    
    // In a real application, this would be sent to the radio/FT8 software
    println!("  Would send: {}", utils::generate_ft8_message(&report_message, "W1ABC")?);
    
    // Simulate receiving acknowledgment
    sleep(Duration::from_millis(100)).await;
    
    let ack_message = MessageType::ReportAck {
        to_station: "W1ABC".to_string(),
        from_station: "K1DEF".to_string(),
        report: -18,
    };
    
    println!("📡 Simulating incoming report acknowledgment: R-18");
    
    system.process_message(
        ack_message,
        "W1ABC K1DEF R-18".to_string(),
        frequency,
        Some(-18.0),
    ).await?;
    
    // Check QSO progression
    sleep(Duration::from_millis(50)).await;
    let progress = system.get_qso(qso_id).await?;
    println!("  QSO progressed to: {:?}", progress.state);
    
    // Simulate final confirmation
    sleep(Duration::from_millis(100)).await;
    
    let confirmation_message = MessageType::FinalConfirmation {
        to_station: "K1DEF".to_string(),
        from_station: "W1ABC".to_string(),
    };
    
    println!("📡 Simulating final confirmation: RR73");
    println!("  Would send: {}", utils::generate_ft8_message(&confirmation_message, "W1ABC")?);
    
    // Simulate receiving 73
    sleep(Duration::from_millis(100)).await;
    
    let seventy_three = MessageType::SeventyThree {
        to_station: "W1ABC".to_string(),
        from_station: "K1DEF".to_string(),
    };
    
    println!("📡 Simulating incoming 73");
    
    system.process_message(
        seventy_three,
        "W1ABC K1DEF 73".to_string(),
        frequency,
        Some(-15.0),
    ).await?;
    
    // Check final QSO state
    sleep(Duration::from_millis(50)).await;
    let final_progress = system.get_qso(qso_id).await?;
    println!("✓ QSO completed!");
    println!("  Final state: {:?}", final_progress.state);
    
    // Show QSO details
    let metadata = &final_progress.metadata;
    println!("\n=== QSO Summary ===");
    println!("QSO ID: {}", metadata.qso_id);
    println!("Our call: {}", metadata.our_callsign);
    println!("Their call: {}", metadata.their_callsign.as_deref().unwrap_or("UNKNOWN"));
    println!("Frequency: {:.3} MHz", metadata.frequency / 1_000_000.0);
    println!("Mode: {}", metadata.mode);
    println!("Our grid: {}", metadata.grids.ours.as_deref().unwrap_or("UNKNOWN"));
    println!("Their grid: {}", metadata.grids.theirs.as_deref().unwrap_or("UNKNOWN"));
    println!("Reports sent/received: {:+}/{:+} dB", 
             metadata.reports.sent.unwrap_or(0),
             metadata.reports.received.unwrap_or(0));
    println!("Start time: {}", metadata.start_time.format("%Y-%m-%d %H:%M:%S UTC"));
    
    if let Some(end_time) = metadata.end_time {
        let duration = (end_time - metadata.start_time).num_seconds();
        println!("End time: {}", end_time.format("%Y-%m-%d %H:%M:%S UTC"));
        println!("Duration: {} seconds", duration);
    }
    
    // Show message history
    println!("\n=== Message History ===");
    for (i, message) in final_progress.messages.iter().enumerate() {
        println!("{}: [{}] {} - {}", 
                 i + 1,
                 message.timestamp.format("%H:%M:%S"),
                 match message.direction {
                     MessageDirection::Sent => "TX",
                     MessageDirection::Received => "RX",
                 },
                 message.raw_text);
    }
    
    // Demonstrate utility functions
    println!("\n=== Utility Functions Demo ===");
    
    // Callsign validation
    let test_calls = ["W1ABC", "K1DEF", "VE3XYZ", "G0ABC", "JA1XYZ", "INVALID"];
    for call in &test_calls {
        let valid = utils::validate_callsign(call);
        println!("Callsign '{}': {}", call, if valid { "✓ Valid" } else { "✗ Invalid" });
    }
    
    // Grid square validation
    let test_grids = ["FN42", "FN42ab", "EM73", "INVALID", "ZZ99"];
    for grid in &test_grids {
        let valid = utils::validate_grid_square(grid);
        println!("Grid '{}': {}", grid, if valid { "✓ Valid" } else { "✗ Invalid" });
    }
    
    // Frequency to band conversion
    let test_freqs = [1840000.0, 3573000.0, 7074000.0, 14074000.0, 21074000.0, 28074000.0, 50313000.0];
    for freq in &test_freqs {
        let band = utils::frequency_to_band(*freq);
        println!("Frequency {:.3} MHz = {}", freq / 1_000_000.0, band);
    }
    
    // Signal report calculation
    let test_signals = [(-5.0, -20.0), (-15.0, -25.0), (-25.0, -30.0)];
    for (signal, noise) in &test_signals {
        let report = utils::calculate_signal_report(*signal, *noise);
        println!("Signal {:.1} dB, Noise {:.1} dB = {:+} dB report", signal, noise, report);
    }
    
    println!("\n✓ Example completed successfully!");
    
    Ok(())
}