//! PAN-385 AC14: config mapping for the band scheduler.

use super::*;
use pancetta_config::autonomous::{AutonomousConfig, BandHopEntry, BandPlanWindow};

fn entry(dial_frequency: u64, band_name: &str, priority: u32) -> BandHopEntry {
    BandHopEntry {
        dial_frequency,
        band_name: band_name.into(),
        priority,
    }
}

fn window(utc: &str, bands: &[&str]) -> BandPlanWindow {
    BandPlanWindow {
        utc: utc.into(),
        bands: bands.iter().map(|b| (*b).to_string()).collect(),
    }
}

#[test]
fn qso_band_hopping_config_maps_every_field() {
    let mut config = AutonomousConfig::default();
    config.band_hopping.enabled = true;
    config.band_hopping.hop_threshold = 7;
    config.band_hopping.bands = vec![entry(14_074_000, "20m", 1), entry(7_074_000, "40m", 2)];
    config.band_hopping.needed_activity = false;
    config.band_hopping.needed_idle_secs = 1_234;
    config.band_hopping.grey_line = false;
    config.band_plan = vec![
        window("22:00-06:00", &["40m", "80m"]),
        window("not a window", &["20m"]),
        window("06:00-10:00", &["20m", "nonsense"]),
        window("10:00-22:00", &["17m"]),
    ];

    let mapped = qso_band_hopping_config(&config);

    assert!(mapped.enabled);
    assert_eq!(mapped.hop_threshold, 7);
    assert_eq!(mapped.bands.len(), 2);
    assert_eq!(mapped.bands[0].dial_frequency, 14_074_000);
    assert_eq!(mapped.bands[0].band_name, "20m");
    assert_eq!(mapped.bands[0].priority, 1);
    assert_eq!(mapped.bands[1].dial_frequency, 7_074_000);
    assert_eq!(mapped.bands[1].band_name, "40m");
    assert_eq!(mapped.bands[1].priority, 2);
    assert!(!mapped.needed_activity);
    assert_eq!(mapped.needed_idle_secs, 1_234);
    assert!(!mapped.grey_line);
    // Unparseable windows (bad `utc`, unknown band) are skipped.
    assert_eq!(
        mapped.plan,
        vec![
            pancetta_qso::BandPlanEntry {
                window: "22:00-06:00".parse().unwrap(),
                bands: vec![Band::Band40m, Band::Band80m],
            },
            pancetta_qso::BandPlanEntry {
                window: "10:00-22:00".parse().unwrap(),
                bands: vec![Band::Band17m],
            },
        ]
    );

    // The defaults carry through too (true/true, not hard-coded).
    let defaults = AutonomousConfig::default();
    let mapped = qso_band_hopping_config(&defaults);
    assert_eq!(mapped.enabled, defaults.band_hopping.enabled);
    assert_eq!(mapped.hop_threshold, defaults.band_hopping.hop_threshold);
    assert_eq!(mapped.bands.len(), defaults.band_hopping.bands.len());
    assert_eq!(
        mapped.needed_activity,
        defaults.band_hopping.needed_activity
    );
    assert_eq!(
        mapped.needed_idle_secs,
        defaults.band_hopping.needed_idle_secs
    );
    assert_eq!(mapped.grey_line, defaults.band_hopping.grey_line);
    assert!(mapped.plan.is_empty());
}

#[test]
fn opportunity_candidate_bands_unions_plan_and_list() {
    let mut config = AutonomousConfig {
        band_plan: vec![
            window("22:00-06:00", &["40m", "80m"]),
            window("06:00-22:00", &["17m", "40m"]),
        ],
        ..Default::default()
    };
    config.band_hopping.bands = vec![
        entry(14_074_000, "20m", 1),
        entry(7_074_000, "40m", 2),
        entry(18_100_000, "17m", 3),
        entry(10_136_000, "30m", 4),
    ];

    assert_eq!(
        opportunity_candidate_bands(&config),
        vec![
            Band::Band40m,
            Band::Band80m,
            Band::Band17m,
            Band::Band20m,
            Band::Band30m,
        ]
    );

    config.band_plan.clear();
    config.band_hopping.bands = vec![entry(21_074_000, "15m", 1)];
    assert_eq!(opportunity_candidate_bands(&config), vec![Band::Band15m]);
}
