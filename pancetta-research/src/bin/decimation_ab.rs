//! PAN-320 decimation A/B CLI.
//!
//! ```text
//! cargo run --release -p pancetta-research --bin decimation-ab \
//!   -- --manifest /data/pan320/captures.json --output /tmp/pan320-result.json
//! Wrote decimation comparison to /tmp/pan320-result.json
//! ```
//!
//! Exit codes: 0 on a successful comparison (the report is written
//! atomically, replacing the output only then); 2 when the manifest cannot
//! be read (`decimation-ab: cannot read capture manifest`) or any input is
//! invalid (`decimation-ab: invalid capture input`, with the offending
//! path and underlying reason on the following stderr line). A failed run
//! never creates or replaces the output report. No progress display.

use pancetta_research::decimation_ab;

fn usage() -> ! {
    eprintln!("usage: decimation-ab --manifest <captures.json> --output <result.json>");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut manifest: Option<&str> = None;
    let mut output: Option<&str> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--manifest" => {
                manifest = args.get(i + 1).map(String::as_str);
                i += 2;
            }
            "--output" => {
                output = args.get(i + 1).map(String::as_str);
                i += 2;
            }
            _ => usage(),
        }
    }
    let (Some(manifest), Some(output)) = (manifest, output) else {
        usage();
    };

    // Missing manifest: the dedicated error, before anything else.
    let manifest_text = match std::fs::read_to_string(manifest) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("decimation-ab: cannot read capture manifest");
            eprintln!("{manifest}: {e}");
            std::process::exit(2);
        }
    };

    let run = || -> anyhow::Result<()> {
        let parsed: decimation_ab::CaptureManifest = serde_json::from_str(&manifest_text)
            .map_err(|e| anyhow::anyhow!("manifest {manifest} is not valid JSON: {e}"))?;
        decimation_ab::validate_manifest(&parsed)
            .map_err(|e| anyhow::anyhow!("manifest {manifest} failed validation: {e}"))?;
        let manifest_path = std::path::Path::new(manifest);
        let dir = manifest_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        let mut captures = Vec::with_capacity(parsed.captures.len());
        for entry in &parsed.captures {
            captures.push(
                decimation_ab::load_capture(dir, entry)
                    .map_err(|e| anyhow::anyhow!("{}: {e}", entry.path))?,
            );
        }
        let mut arm = decimation_ab::Ft8DecoderArm::new();
        let mut report = decimation_ab::compare_captures(&captures, &mut arm, false);
        report.manifest = Some(parsed);
        decimation_ab::write_report_atomic(&report, std::path::Path::new(output))
    };

    match run() {
        Ok(()) => {
            println!("Wrote decimation comparison to {output}");
        }
        Err(e) => {
            eprintln!("decimation-ab: invalid capture input");
            // The offending path and underlying reason on the following line.
            eprintln!("{e:#}");
            std::process::exit(2);
        }
    }
}
