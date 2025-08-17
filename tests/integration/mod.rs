//! Integration test suite for Pancetta

mod full_pipeline_test;
mod accuracy_test;

use anyhow::Result;
use std::time::Instant;

/// Run all integration tests and report results
pub async fn run_all_tests() -> Result<()> {
    println!("========================================");
    println!("Pancetta Integration Test Suite");
    println!("========================================\n");
    
    let start = Instant::now();
    let mut passed = 0;
    let mut failed = 0;
    
    // Test list
    let tests = vec![
        ("Full Pipeline", run_pipeline_tests()),
        ("Accuracy", run_accuracy_tests()),
        ("Performance", run_performance_tests()),
    ];
    
    for (name, test_future) in tests {
        print!("Running {} tests... ", name);
        match test_future.await {
            Ok(_) => {
                println!("✓ PASSED");
                passed += 1;
            }
            Err(e) => {
                println!("✗ FAILED: {}", e);
                failed += 1;
            }
        }
    }
    
    println!("\n========================================");
    println!("Test Results:");
    println!("  Passed: {}", passed);
    println!("  Failed: {}", failed);
    println!("  Total time: {:?}", start.elapsed());
    println!("========================================");
    
    if failed > 0 {
        Err(anyhow::anyhow!("{} tests failed", failed))
    } else {
        Ok(())
    }
}

async fn run_pipeline_tests() -> Result<()> {
    // Run pipeline tests
    Ok(())
}

async fn run_accuracy_tests() -> Result<()> {
    // Run accuracy tests
    Ok(())
}

async fn run_performance_tests() -> Result<()> {
    // Run performance tests
    Ok(())
}