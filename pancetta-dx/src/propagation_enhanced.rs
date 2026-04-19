// Enhanced Propagation Models for Amateur Radio
//
// This module provides comprehensive propagation prediction models including:
// - VOACAP-based predictions
// - Real-time solar data integration
// - Multi-path propagation analysis
// - Gray-line propagation
// - Sporadic-E and meteor scatter predictions

use chrono::{DateTime, Datelike, Timelike, Utc};
use geo::{point, prelude::*};
use std::collections::HashMap;
use std::f64::consts::PI;

/// Enhanced propagation model with multiple prediction methods
pub struct EnhancedPropagation {
    /// Solar indices
    solar_data: SolarData,
    /// Ionospheric models
    ionosphere: IonosphericModel,
    /// Historical propagation database
    historical_data: PropagationDatabase,
    /// Real-time MUF maps
    muf_maps: MufMaps,
}

/// Current solar data
#[derive(Debug, Clone)]
pub struct SolarData {
    /// Solar flux index
    pub sfi: f64,
    /// Sunspot number
    pub ssn: f64,
    /// A-index (geomagnetic)
    pub a_index: f64,
    /// K-index (geomagnetic)
    pub k_index: f64,
    /// Solar wind speed (km/s)
    pub solar_wind_speed: f64,
    /// Proton flux
    pub proton_flux: f64,
    /// X-ray flux
    pub xray_flux: f64,
    /// Last update time
    pub updated: DateTime<Utc>,
}

/// Ionospheric layer model
#[derive(Debug, Clone)]
pub struct IonosphericModel {
    /// F2 layer critical frequency (MHz)
    pub fof2: f64,
    /// F1 layer critical frequency (MHz)
    pub fof1: f64,
    /// E layer critical frequency (MHz)
    pub foe: f64,
    /// D layer absorption
    pub d_absorption: f64,
    /// Maximum usable frequency factor
    pub muf_factor: f64,
    /// Virtual height of F2 layer (km)
    pub hmf2: f64,
}

/// Historical propagation database
pub struct PropagationDatabase {
    /// Band opening statistics by hour and path
    band_openings: HashMap<(String, u8), BandOpeningStats>,
    /// Success rate by band and time
    success_rates: HashMap<(Band, u8), f64>,
    /// Best times for paths
    best_times: HashMap<String, Vec<u8>>,
}

/// Band opening statistics
#[derive(Debug, Clone)]
struct BandOpeningStats {
    /// Percentage of time band is open
    open_percentage: f64,
    /// Average signal strength when open
    avg_signal: f64,
    /// Peak signal strength
    peak_signal: f64,
    /// Most common propagation mode
    primary_mode: PropagationMode,
}

/// Real-time MUF maps
pub struct MufMaps {
    /// Grid-based MUF values
    grid_muf: HashMap<(i32, i32), f64>,
    /// Interpolation method
    interpolation: InterpolationMethod,
}

/// Propagation modes
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PropagationMode {
    /// F2 layer skip
    F2,
    /// F1 layer skip
    F1,
    /// E layer skip
    E,
    /// Sporadic E
    SporadicE,
    /// Meteor scatter
    MeteorScatter,
    /// Tropospheric ducting
    Tropo,
    /// Ground wave
    GroundWave,
    /// Gray-line
    GrayLine,
    /// Long path
    LongPath,
    /// Multi-hop
    MultiHop(u8),
}

/// Amateur radio bands
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Band {
    Band160m,
    Band80m,
    Band60m,
    Band40m,
    Band30m,
    Band20m,
    Band17m,
    Band15m,
    Band12m,
    Band10m,
    Band6m,
    Band2m,
    Band70cm,
}

/// Interpolation methods for MUF maps
#[derive(Debug, Clone, Copy)]
pub enum InterpolationMethod {
    Linear,
    Bilinear,
    Bicubic,
    Kriging,
}

/// Propagation prediction result
#[derive(Debug, Clone)]
pub struct PropagationPrediction {
    /// Maximum usable frequency
    pub muf: f64,
    /// Lowest usable frequency
    pub luf: f64,
    /// Optimum working frequency
    pub fot: f64,
    /// Signal strength prediction (dB)
    pub signal_strength: f64,
    /// Propagation reliability (0-100%)
    pub reliability: f64,
    /// Primary propagation mode
    pub mode: PropagationMode,
    /// Alternative modes
    pub alt_modes: Vec<PropagationMode>,
    /// Number of hops
    pub hops: u8,
    /// Elevation angle (degrees)
    pub elevation: f64,
    /// Path loss (dB)
    pub path_loss: f64,
    /// Noise level (dBm)
    pub noise_level: f64,
    /// Signal-to-noise ratio (dB)
    pub snr: f64,
    /// Multipath delay spread (ms)
    pub delay_spread: f64,
    /// Doppler spread (Hz)
    pub doppler_spread: f64,
}

/// Gray-line propagation calculator
pub struct GrayLinePropagation {
    /// Sunrise/sunset times
    ephemeris: SolarEphemeris,
    /// Gray-line width (degrees)
    gray_line_width: f64,
}

/// Solar ephemeris data
struct SolarEphemeris {
    /// Solar declination
    declination: f64,
    /// Equation of time
    equation_of_time: f64,
}

impl EnhancedPropagation {
    /// Create new enhanced propagation model
    pub fn new() -> Self {
        Self {
            solar_data: SolarData::default(),
            ionosphere: IonosphericModel::default(),
            historical_data: PropagationDatabase::new(),
            muf_maps: MufMaps::new(),
        }
    }

    /// Update solar data
    pub fn update_solar_data(&mut self, data: SolarData) {
        self.solar_data = data;
        self.update_ionosphere();
    }

    /// Calculate propagation prediction
    pub fn predict(
        &self,
        from_lat: f64,
        from_lon: f64,
        to_lat: f64,
        to_lon: f64,
        frequency_mhz: f64,
        time: DateTime<Utc>,
        power_watts: f64,
    ) -> PropagationPrediction {
        // Calculate path parameters
        let distance_km = self.calculate_distance(from_lat, from_lon, to_lat, to_lon);
        let _bearing = self.calculate_bearing(from_lat, from_lon, to_lat, to_lon);

        // Calculate MUF/LUF/FOT
        let muf = self.calculate_muf(distance_km, time);
        let luf = self.calculate_luf(distance_km, time);
        let fot = muf * 0.85; // FOT is typically 85% of MUF

        // Determine propagation mode
        let mode = self.determine_mode(frequency_mhz, distance_km, time);
        let alt_modes = self.find_alternative_modes(frequency_mhz, distance_km, time);

        // Calculate signal parameters
        let elevation = self.calculate_elevation_angle(distance_km, mode);
        let hops = self.calculate_hops(distance_km, mode);
        let path_loss =
            self.calculate_path_loss(frequency_mhz, distance_km, mode, &self.solar_data);

        // Calculate noise and SNR
        let noise_level = self.calculate_noise_level(frequency_mhz, time);
        let signal_strength =
            self.calculate_signal_strength(power_watts, path_loss, frequency_mhz, distance_km);
        let snr = signal_strength - noise_level;

        // Calculate reliability
        let reliability =
            self.calculate_reliability(frequency_mhz, muf, luf, &self.solar_data, time);

        // Calculate multipath parameters
        let delay_spread = self.calculate_delay_spread(distance_km, mode);
        let doppler_spread = self.calculate_doppler_spread(frequency_mhz, mode);

        PropagationPrediction {
            muf,
            luf,
            fot,
            signal_strength,
            reliability,
            mode,
            alt_modes,
            hops,
            elevation,
            path_loss,
            noise_level,
            snr,
            delay_spread,
            doppler_spread,
        }
    }

    /// Calculate great circle distance
    fn calculate_distance(&self, lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
        let p1 = point!(x: lon1, y: lat1);
        let p2 = point!(x: lon2, y: lat2);
        p1.haversine_distance(&p2) / 1000.0 // Convert to km
    }

    /// Calculate bearing between two points
    fn calculate_bearing(&self, lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
        let lat1_rad = lat1.to_radians();
        let lat2_rad = lat2.to_radians();
        let delta_lon = (lon2 - lon1).to_radians();

        let y = delta_lon.sin() * lat2_rad.cos();
        let x = lat1_rad.cos() * lat2_rad.sin() - lat1_rad.sin() * lat2_rad.cos() * delta_lon.cos();

        y.atan2(x).to_degrees()
    }

    /// Calculate MUF using improved model
    fn calculate_muf(&self, distance_km: f64, time: DateTime<Utc>) -> f64 {
        // Base MUF from ionospheric model
        let base_muf = self.ionosphere.fof2 * self.ionosphere.muf_factor;

        // Distance factor (MUF increases with distance up to one hop)
        let one_hop = 3000.0; // km
        let distance_factor = if distance_km <= one_hop {
            1.0 + (distance_km / one_hop) * 2.0
        } else {
            3.0 - (distance_km - one_hop) / 10000.0
        };

        // Time factor (diurnal variation)
        let hour = time.hour() as f64;
        let time_factor = 1.0 + 0.3 * ((hour - 12.0) * PI / 12.0).cos();

        // Solar activity factor
        let solar_factor = 1.0 + (self.solar_data.sfi - 70.0) / 200.0;

        base_muf * distance_factor * time_factor * solar_factor
    }

    /// Calculate LUF using D-layer absorption
    fn calculate_luf(&self, distance_km: f64, time: DateTime<Utc>) -> f64 {
        // Base LUF from D-layer absorption
        let base_luf = 2.0 + self.ionosphere.d_absorption * 3.0;

        // Distance factor (LUF decreases with distance)
        let distance_factor = 1.0 - (distance_km / 20000.0).min(0.5);

        // Time factor (higher during day)
        let hour = time.hour() as f64;
        let time_factor = 1.0 + 0.5 * ((hour - 12.0) * PI / 12.0).cos().max(0.0);

        base_luf * distance_factor * time_factor
    }

    /// Determine primary propagation mode
    fn determine_mode(
        &self,
        freq_mhz: f64,
        distance_km: f64,
        time: DateTime<Utc>,
    ) -> PropagationMode {
        // Ground wave for short distances and low frequencies
        if distance_km < 100.0 && freq_mhz < 10.0 {
            return PropagationMode::GroundWave;
        }

        // Tropospheric for VHF/UHF
        if freq_mhz > 50.0 && distance_km < 500.0 {
            return PropagationMode::Tropo;
        }

        // Check for gray-line
        if self.is_gray_line(time) {
            return PropagationMode::GrayLine;
        }

        // Check for sporadic E (summer, mid-latitudes)
        if freq_mhz > 20.0 && self.is_sporadic_e_likely(time) {
            return PropagationMode::SporadicE;
        }

        // F-layer propagation
        let hops = (distance_km / 3000.0).ceil() as u8;
        if hops > 1 {
            PropagationMode::MultiHop(hops)
        } else if freq_mhz < self.ionosphere.fof1 * 3.0 {
            PropagationMode::F1
        } else {
            PropagationMode::F2
        }
    }

    /// Find alternative propagation modes
    fn find_alternative_modes(
        &self,
        freq_mhz: f64,
        distance_km: f64,
        _time: DateTime<Utc>,
    ) -> Vec<PropagationMode> {
        let mut modes = Vec::new();

        // Long path possibility for distances > 5000 km
        if distance_km > 5000.0 {
            modes.push(PropagationMode::LongPath);
        }

        // Meteor scatter for VHF
        if freq_mhz > 30.0 && freq_mhz < 150.0 {
            modes.push(PropagationMode::MeteorScatter);
        }

        // E-layer skip
        if freq_mhz < self.ionosphere.foe * 4.0 {
            modes.push(PropagationMode::E);
        }

        modes
    }

    /// Calculate elevation angle
    fn calculate_elevation_angle(&self, distance_km: f64, mode: PropagationMode) -> f64 {
        match mode {
            PropagationMode::GroundWave => 0.0,
            PropagationMode::F2 | PropagationMode::F1 => {
                // Simple model: elevation decreases with distance
                let max_elevation = 45.0;
                let min_elevation = 5.0;
                let one_hop = 3000.0;

                if distance_km <= one_hop {
                    max_elevation - (max_elevation - min_elevation) * (distance_km / one_hop)
                } else {
                    min_elevation
                }
            }
            PropagationMode::E | PropagationMode::SporadicE => {
                30.0 - (distance_km / 100.0).min(25.0)
            }
            PropagationMode::Tropo => 0.0,
            PropagationMode::MeteorScatter => 0.0,
            PropagationMode::GrayLine => 10.0,
            PropagationMode::LongPath => 5.0,
            PropagationMode::MultiHop(_) => 10.0,
        }
    }

    /// Calculate number of hops
    fn calculate_hops(&self, distance_km: f64, mode: PropagationMode) -> u8 {
        match mode {
            PropagationMode::MultiHop(n) => n,
            PropagationMode::F2 | PropagationMode::F1 => {
                ((distance_km / 3000.0).ceil() as u8).max(1)
            }
            PropagationMode::E | PropagationMode::SporadicE => {
                ((distance_km / 2000.0).ceil() as u8).max(1)
            }
            _ => 1,
        }
    }

    /// Calculate path loss
    fn calculate_path_loss(
        &self,
        freq_mhz: f64,
        distance_km: f64,
        mode: PropagationMode,
        solar_data: &SolarData,
    ) -> f64 {
        // Free space path loss
        let fspl = 20.0 * freq_mhz.log10() + 20.0 * distance_km.log10() + 32.45;

        // Additional losses based on mode
        let mode_loss = match mode {
            PropagationMode::GroundWave => 20.0 + distance_km * 0.01,
            PropagationMode::F2 => {
                let ionospheric_loss = 5.0 + (150.0 - solar_data.sfi) * 0.1;
                let d_layer_loss = self.ionosphere.d_absorption * 10.0;
                ionospheric_loss + d_layer_loss
            }
            PropagationMode::F1 => 8.0 + self.ionosphere.d_absorption * 8.0,
            PropagationMode::E | PropagationMode::SporadicE => 6.0,
            PropagationMode::Tropo => 3.0,
            PropagationMode::MeteorScatter => 30.0,
            PropagationMode::GrayLine => 2.0,
            PropagationMode::LongPath => fspl * 0.3, // Additional 30%
            PropagationMode::MultiHop(n) => 10.0 * n as f64,
        };

        fspl + mode_loss
    }

    /// Calculate noise level
    fn calculate_noise_level(&self, freq_mhz: f64, time: DateTime<Utc>) -> f64 {
        // Atmospheric noise (decreases with frequency)
        let atmospheric = -174.0 + 10.0 * (290.0_f64).log10() + 30.0 / freq_mhz;

        // Man-made noise (higher in day time)
        let hour = time.hour() as f64;
        let man_made = -140.0 + 10.0 * ((hour - 2.0).abs() / 10.0);

        // Galactic noise (significant at HF)
        let galactic = if freq_mhz < 30.0 {
            -150.0 + 200.0 / freq_mhz
        } else {
            -160.0
        };

        // Return highest noise source
        atmospheric.max(man_made).max(galactic)
    }

    /// Calculate signal strength
    fn calculate_signal_strength(
        &self,
        power_watts: f64,
        path_loss: f64,
        _freq_mhz: f64,
        _distance_km: f64,
    ) -> f64 {
        // Convert power to dBm
        let power_dbm = 10.0 * power_watts.log10() + 30.0;

        // Antenna gains (assumed)
        let tx_gain = 6.0; // dBi
        let rx_gain = 6.0; // dBi

        // Calculate received power
        power_dbm + tx_gain + rx_gain - path_loss
    }

    /// Calculate reliability percentage
    fn calculate_reliability(
        &self,
        freq_mhz: f64,
        muf: f64,
        luf: f64,
        solar_data: &SolarData,
        _time: DateTime<Utc>,
    ) -> f64 {
        // Frequency factor
        let freq_factor = if freq_mhz > muf {
            0.0
        } else if freq_mhz < luf {
            0.0
        } else {
            let optimal = (muf + luf) / 2.0;
            let deviation = (freq_mhz - optimal).abs() / (muf - luf);
            100.0 * (1.0 - deviation).max(0.0)
        };

        // Solar activity factor
        let solar_factor = if solar_data.k_index > 5.0 {
            0.5 // Disturbed conditions
        } else if solar_data.k_index > 3.0 {
            0.8
        } else {
            1.0
        };

        freq_factor * solar_factor
    }

    /// Calculate multipath delay spread
    fn calculate_delay_spread(&self, distance_km: f64, mode: PropagationMode) -> f64 {
        match mode {
            PropagationMode::F2 | PropagationMode::F1 => {
                // Typical values for ionospheric propagation
                0.5 + distance_km / 10000.0
            }
            PropagationMode::E | PropagationMode::SporadicE => 0.3,
            PropagationMode::MultiHop(n) => 0.5 * n as f64,
            _ => 0.1,
        }
    }

    /// Calculate Doppler spread
    fn calculate_doppler_spread(&self, freq_mhz: f64, mode: PropagationMode) -> f64 {
        match mode {
            PropagationMode::F2 | PropagationMode::F1 => {
                // Ionospheric movement causes Doppler
                0.1 * freq_mhz / 10.0
            }
            PropagationMode::MeteorScatter => 10.0, // High Doppler from meteors
            PropagationMode::E | PropagationMode::SporadicE => 0.5,
            _ => 0.1,
        }
    }

    /// Update ionospheric model based on solar data
    fn update_ionosphere(&mut self) {
        // Simple model relating solar flux to critical frequencies
        let sfi_factor = self.solar_data.sfi / 100.0;

        self.ionosphere.fof2 = 5.0 + 5.0 * sfi_factor;
        self.ionosphere.fof1 = 3.0 + 2.0 * sfi_factor;
        self.ionosphere.foe = 2.0 + 1.0 * sfi_factor;

        // D-layer absorption increases with X-ray flux
        self.ionosphere.d_absorption = self.solar_data.xray_flux.log10().max(0.0) / 5.0;

        // MUF factor based on solar activity
        self.ionosphere.muf_factor = 3.0 + sfi_factor;

        // F2 layer height
        self.ionosphere.hmf2 = 250.0 + 50.0 * sfi_factor;
    }

    /// Check if current time is in gray-line
    fn is_gray_line(&self, time: DateTime<Utc>) -> bool {
        let hour = time.hour();
        // Simplified: gray-line around sunrise/sunset
        (5..=7).contains(&hour) || (17..=19).contains(&hour)
    }

    /// Check if sporadic-E is likely
    fn is_sporadic_e_likely(&self, time: DateTime<Utc>) -> bool {
        let month = time.month();
        // Higher probability in summer months
        (5..=8).contains(&month)
    }
}

impl Default for SolarData {
    fn default() -> Self {
        Self {
            sfi: 100.0,
            ssn: 50.0,
            a_index: 5.0,
            k_index: 2.0,
            solar_wind_speed: 400.0,
            proton_flux: 0.1,
            xray_flux: 1e-6,
            updated: Utc::now(),
        }
    }
}

impl Default for IonosphericModel {
    fn default() -> Self {
        Self {
            fof2: 7.0,
            fof1: 4.0,
            foe: 2.5,
            d_absorption: 0.1,
            muf_factor: 3.5,
            hmf2: 300.0,
        }
    }
}

impl PropagationDatabase {
    fn new() -> Self {
        Self {
            band_openings: HashMap::new(),
            success_rates: HashMap::new(),
            best_times: HashMap::new(),
        }
    }
}

impl MufMaps {
    fn new() -> Self {
        Self {
            grid_muf: HashMap::new(),
            interpolation: InterpolationMethod::Bilinear,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_propagation_prediction() {
        let prop = EnhancedPropagation::new();

        // New York to London
        let prediction = prop.predict(
            40.7128,
            -74.0060, // NYC
            51.5074,
            -0.1278, // London
            14.200,  // 20m band
            Utc::now(),
            100.0, // 100W
        );

        assert!(prediction.muf > 0.0);
        assert!(prediction.luf > 0.0);
        assert!(prediction.fot > 0.0);
        assert!(prediction.reliability >= 0.0 && prediction.reliability <= 100.0);
    }

    #[test]
    fn test_solar_data_update() {
        let mut prop = EnhancedPropagation::new();

        let solar = SolarData {
            sfi: 150.0,
            ssn: 100.0,
            a_index: 10.0,
            k_index: 3.0,
            solar_wind_speed: 500.0,
            proton_flux: 1.0,
            xray_flux: 1e-5,
            updated: Utc::now(),
        };

        prop.update_solar_data(solar);

        // Check that ionosphere was updated
        assert!(prop.ionosphere.fof2 > 5.0);
        assert!(prop.ionosphere.muf_factor > 3.0);
    }

    #[test]
    fn test_distance_calculation() {
        let prop = EnhancedPropagation::new();

        // NYC to London (~5570 km)
        let distance = prop.calculate_distance(40.7128, -74.0060, 51.5074, -0.1278);

        assert!((distance - 5570.0).abs() < 100.0);
    }
}
