fn main() {
    use regex::Regex;

    // The OLD regex (before the change)
    let old = Regex::new(r"^([A-Z0-9/]+)\s+([A-Z0-9/]+)\s+R?([+-]?\d{1,2})$").unwrap();
    
    // The NEW regex (after the change)
    let new = Regex::new(r"^([A-Z0-9/]+)\s+([A-Z0-9/]+)\s+(R?[+-]?\d{1,2})$").unwrap();

    let test_cases = vec![
        ("K1DEF W1ABC -15", "Simple report"),
        ("K1DEF W1ABC R-12", "ReportAck"),
        ("W1ABC K2DEF R+5", "ReportAck positive"),
        ("K1DEF W1ABC R -12", "ReportAck with space (decoder output)"),
    ];

    println!("Testing signal report regex patterns:\n");

    for (test, desc) in &test_cases {
        println!("Test: '{}' ({})", test, desc);
        
        if let Some(caps) = old.captures(test) {
            let g3 = caps.get(3).map(|m| m.as_str()).unwrap_or("NONE");
            println!("  OLD regex group 3: '{}'", g3);
        } else {
            println!("  OLD regex: NO MATCH");
        }
        
        if let Some(caps) = new.captures(test) {
            let g3 = caps.get(3).map(|m| m.as_str()).unwrap_or("NONE");
            println!("  NEW regex group 3: '{}'", g3);
        } else {
            println!("  NEW regex: NO MATCH");
        }
        println!();
    }
}
