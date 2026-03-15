//! Basic Propagation Predictions
//!
//! This module provides basic HF propagation prediction capabilities
//! including solar indices, band conditions, and propagation forecasting.

use crate::{Band, DxError, Result};
use chrono::{DateTime, Datelike, Timelike, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{debug, info};

/// Solar and geomagnetic indices
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolarIndices {
    /// Solar Flux Index (SFI) - 10.7cm solar flux
    pub sfi: f64,
    /// Sunspot Number (SSN)
    pub ssn: f64,
    /// Planetary K-index (Kp)
    pub kp: f64,
    /// A-index (geomagnetic activity)
    pub a_index: f64,
    /// Boulder K-index
    pub boulder_k: f64,
    /// Date/time of measurement
    pub measurement_time: DateTime<Utc>,
    /// Forecast period in hours
    pub forecast_hours: u32,
}

/// Band condition assessment
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BandCondition {
    /// Excellent conditions
    Excellent,
    /// Very good conditions
    VeryGood,
    /// Good conditions
    Good,
    /// Fair conditions
    Fair,
    /// Poor conditions
    Poor,
    /// Very poor conditions
    VeryPoor,
    /// Band closed/unusable
    Closed,
}

impl BandCondition {
    /// Get numeric score for condition (0-100)
    pub fn score(&self) -> u8 {
        match self {
            BandCondition::Excellent => 95,
            BandCondition::VeryGood => 80,
            BandCondition::Good => 65,
            BandCondition::Fair => 50,
            BandCondition::Poor => 30,
            BandCondition::VeryPoor => 15,
            BandCondition::Closed => 0,
        }
    }

    /// Get condition from score
    pub fn from_score(score: u8) -> Self {
        match score {
            90..=100 => BandCondition::Excellent,
            75..=89 => BandCondition::VeryGood,
            60..=74 => BandCondition::Good,
            45..=59 => BandCondition::Fair,
            25..=44 => BandCondition::Poor,
            10..=24 => BandCondition::VeryPoor,
            _ => BandCondition::Closed,
        }
    }
}

impl std::fmt::Display for BandCondition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            BandCondition::Excellent => "Excellent",
            BandCondition::VeryGood => "Very Good",
            BandCondition::Good => "Good",
            BandCondition::Fair => "Fair",
            BandCondition::Poor => "Poor",
            BandCondition::VeryPoor => "Very Poor",
            BandCondition::Closed => "Closed",
        };
        write!(f, "{}", name)
    }
}

/// Propagation prediction for a specific path and time
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PropagationPrediction {
    /// Source coordinates (latitude, longitude)
    pub source: (f64, f64),
    /// Destination coordinates (latitude, longitude)
    pub destination: (f64, f64),
    /// Great circle distance in kilometers
    pub distance_km: f64,
    /// Prediction time
    pub time: DateTime<Utc>,
    /// Solar indices used for prediction
    pub solar_indices: SolarIndices,
    /// Band conditions
    pub band_conditions: HashMap<Band, BandCondition>,
    /// Maximum Usable Frequency (MUF) in MHz
    pub muf_mhz: f64,
    /// Lowest Usable Frequency (LUF) in MHz
    pub luf_mhz: f64,
    /// Critical frequency in MHz
    pub critical_frequency_mhz: f64,
    /// Signal-to-noise ratio predictions by band
    pub snr_predictions: HashMap<Band, f64>,
    /// Path reliability by band (0.0 to 1.0)
    pub reliability: HashMap<Band, f64>,
    /// Recommended bands for the path/time
    pub recommended_bands: Vec<Band>,
}

/// Propagation event (aurora, sporadic E, etc.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PropagationEvent {
    /// Event type
    pub event_type: PropagationEventType,
    /// Event severity/intensity
    pub intensity: EventIntensity,
    /// Start time
    pub start_time: DateTime<Utc>,
    /// End time (if known)
    pub end_time: Option<DateTime<Utc>>,
    /// Affected bands
    pub affected_bands: Vec<Band>,
    /// Geographic regions affected
    pub affected_regions: Vec<String>,
    /// Description
    pub description: String,
}

/// Types of propagation events
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PropagationEventType {
    /// Aurora/Northern Lights
    Aurora,
    /// Sporadic E propagation
    SporadicE,
    /// Meteor scatter
    MeteorScatter,
    /// Aircraft scatter
    AircraftScatter,
    /// Moonbounce (EME)
    Moonbounce,
    /// Geomagnetic storm
    GeomagneticStorm,
    /// Solar flare
    SolarFlare,
    /// Ionospheric disturbance
    IonosphericDisturbance,
    /// Tropospheric enhancement
    TroposphericDucting,
    /// Rain scatter
    RainScatter,
}

/// Event intensity levels
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventIntensity {
    /// Minor event
    Minor,
    /// Moderate event
    Moderate,
    /// Strong event
    Strong,
    /// Severe event
    Severe,
    /// Extreme event
    Extreme,
}

/// Propagation predictor engine
pub struct PropagationPredictor {
    /// HTTP client for fetching space weather data
    client: Client,
    /// Cached solar indices
    cached_indices: Option<SolarIndices>,
    /// Cache timestamp
    cache_time: Option<DateTime<Utc>>,
    /// Cache timeout in minutes
    cache_timeout_minutes: i64,
}

impl PropagationPredictor {
    /// Create new propagation predictor
    pub fn new() -> Self {
        Self {
            client: Client::new(),
            cached_indices: None,
            cache_time: None,
            cache_timeout_minutes: 30,
        }
    }

    /// Set cache timeout
    pub fn set_cache_timeout(&mut self, minutes: i64) {
        self.cache_timeout_minutes = minutes;
    }

    /// Get current solar indices
    pub async fn get_solar_indices(&mut self) -> Result<SolarIndices> {
        // Check cache first
        if let (Some(indices), Some(cache_time)) = (&self.cached_indices, self.cache_time) {
            let cache_age = Utc::now().signed_duration_since(cache_time).num_minutes();
            if cache_age < self.cache_timeout_minutes {
                debug!("Using cached solar indices (age: {} minutes)", cache_age);
                return Ok(indices.clone());
            }
        }

        info!("Fetching current solar indices from NOAA");

        // Fetch from NOAA Space Weather Prediction Center
        let indices = self.fetch_noaa_indices().await?;

        // Update cache
        self.cached_indices = Some(indices.clone());
        self.cache_time = Some(Utc::now());

        Ok(indices)
    }

    /// Fetch solar indices from NOAA
    async fn fetch_noaa_indices(&self) -> Result<SolarIndices> {
        // TODO: Implement real NOAA API fetch (services.swpc.noaa.gov)
        Err(
            DxError::ExternalService("NOAA solar index fetch not yet implemented".to_string())
                .into(),
        )
    }

    /// Predict propagation for a specific path and time
    pub async fn predict_propagation(
        &mut self,
        source_lat: f64,
        source_lon: f64,
        dest_lat: f64,
        dest_lon: f64,
        prediction_time: DateTime<Utc>,
    ) -> Result<PropagationPrediction> {
        let solar_indices = self.get_solar_indices().await?;

        // Calculate great circle distance
        let distance_km =
            self.calculate_great_circle_distance(source_lat, source_lon, dest_lat, dest_lon);

        debug!(
            "Calculating propagation prediction for {:.0} km path",
            distance_km
        );

        // Calculate MUF using simplified formula
        let muf_mhz = self.calculate_muf(&solar_indices, distance_km, prediction_time);

        // Calculate LUF (typically 1/3 of MUF for HF)
        let luf_mhz = muf_mhz / 3.0;

        // Calculate critical frequency
        let critical_frequency_mhz = muf_mhz * 0.85;

        // Predict band conditions
        let band_conditions =
            self.predict_band_conditions(&solar_indices, distance_km, prediction_time);

        // Calculate SNR predictions
        let snr_predictions =
            self.calculate_snr_predictions(&solar_indices, distance_km, &band_conditions);

        // Calculate reliability
        let reliability = self.calculate_reliability(&solar_indices, distance_km, &band_conditions);

        // Recommend best bands
        let recommended_bands = self.recommend_bands(&band_conditions, &reliability);

        Ok(PropagationPrediction {
            source: (source_lat, source_lon),
            destination: (dest_lat, dest_lon),
            distance_km,
            time: prediction_time,
            solar_indices,
            band_conditions,
            muf_mhz,
            luf_mhz,
            critical_frequency_mhz,
            snr_predictions,
            reliability,
            recommended_bands,
        })
    }

    /// Calculate great circle distance
    fn calculate_great_circle_distance(&self, lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
        let earth_radius_km = 6371.0;

        let lat1_rad = lat1.to_radians();
        let lat2_rad = lat2.to_radians();
        let delta_lat = (lat2 - lat1).to_radians();
        let delta_lon = (lon2 - lon1).to_radians();

        let a = (delta_lat / 2.0).sin().powi(2)
            + lat1_rad.cos() * lat2_rad.cos() * (delta_lon / 2.0).sin().powi(2);
        let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());

        earth_radius_km * c
    }

    /// Calculate Maximum Usable Frequency using simplified model
    fn calculate_muf(&self, indices: &SolarIndices, distance_km: f64, time: DateTime<Utc>) -> f64 {
        // Simplified MUF calculation based on solar flux and path geometry
        let base_muf = 1.5 + (indices.sfi - 65.0) / 100.0 * 25.0;

        // Adjust for path length (longer paths typically have lower MUF)
        let distance_factor = if distance_km < 2000.0 {
            1.0
        } else if distance_km < 5000.0 {
            1.0 - (distance_km - 2000.0) / 10000.0 * 0.3
        } else {
            0.7 - (distance_km - 5000.0) / 15000.0 * 0.4
        };

        // Adjust for time of day (simplified)
        let hour = time.time().hour();
        let time_factor = match hour {
            6..=18 => 1.0,          // Daytime
            19..=23 | 0..=5 => 0.6, // Nighttime
            _ => 0.8,
        };

        // Adjust for geomagnetic activity
        let geo_factor = if indices.kp <= 2.0 {
            1.0
        } else if indices.kp <= 4.0 {
            0.9
        } else {
            0.7
        };

        (base_muf * distance_factor * time_factor * geo_factor).max(1.8)
    }

    /// Predict band conditions
    fn predict_band_conditions(
        &self,
        indices: &SolarIndices,
        distance_km: f64,
        time: DateTime<Utc>,
    ) -> HashMap<Band, BandCondition> {
        let mut conditions = HashMap::new();
        let muf = self.calculate_muf(indices, distance_km, time);

        for &band in Band::all() {
            let (freq_min, freq_max) = band.frequency_range();
            let freq_center = (freq_min + freq_max) / 2;
            let freq_center_mhz = freq_center as f64 / 1_000_000.0;

            let condition = if freq_center_mhz < muf * 0.3 {
                // Too low frequency - absorption issues
                if distance_km > 1000.0 {
                    BandCondition::Poor
                } else {
                    BandCondition::Fair
                }
            } else if freq_center_mhz > muf {
                // Above MUF - no propagation
                BandCondition::Closed
            } else if freq_center_mhz > muf * 0.85 {
                // Near MUF - marginal
                BandCondition::Fair
            } else if freq_center_mhz > muf * 0.5 {
                // Good frequency range
                if indices.kp <= 2.0 {
                    BandCondition::VeryGood
                } else if indices.kp <= 4.0 {
                    BandCondition::Good
                } else {
                    BandCondition::Fair
                }
            } else {
                // Low but usable frequency
                BandCondition::Good
            };

            conditions.insert(band, condition);
        }

        conditions
    }

    /// Calculate SNR predictions for each band
    fn calculate_snr_predictions(
        &self,
        indices: &SolarIndices,
        distance_km: f64,
        conditions: &HashMap<Band, BandCondition>,
    ) -> HashMap<Band, f64> {
        let mut snr_predictions = HashMap::new();

        for (&band, &condition) in conditions {
            let base_snr = match condition {
                BandCondition::Excellent => 20.0,
                BandCondition::VeryGood => 15.0,
                BandCondition::Good => 10.0,
                BandCondition::Fair => 5.0,
                BandCondition::Poor => 0.0,
                BandCondition::VeryPoor => -5.0,
                BandCondition::Closed => -20.0,
            };

            // Adjust for distance (free space path loss)
            let (freq_min, freq_max) = band.frequency_range();
            let freq_center = (freq_min + freq_max) / 2;
            let freq_center_mhz = freq_center as f64 / 1_000_000.0;

            let path_loss =
                20.0 * (distance_km / 100.0).log10() + 20.0 * freq_center_mhz.log10() - 27.55;
            let adjusted_snr = base_snr - path_loss / 10.0;

            // Adjust for geomagnetic activity
            let geo_adjustment = if indices.kp <= 2.0 {
                0.0
            } else if indices.kp <= 4.0 {
                -3.0
            } else {
                -6.0
            };

            snr_predictions.insert(band, adjusted_snr + geo_adjustment);
        }

        snr_predictions
    }

    /// Calculate path reliability for each band
    fn calculate_reliability(
        &self,
        indices: &SolarIndices,
        _distance_km: f64,
        conditions: &HashMap<Band, BandCondition>,
    ) -> HashMap<Band, f64> {
        let mut reliability = HashMap::new();

        for (&band, &condition) in conditions {
            let base_reliability = condition.score() as f64 / 100.0;

            // Adjust for geomagnetic activity
            let geo_factor = if indices.kp <= 2.0 {
                1.0
            } else if indices.kp <= 4.0 {
                0.8
            } else {
                0.6
            };

            reliability.insert(band, (base_reliability * geo_factor).min(1.0));
        }

        reliability
    }

    /// Recommend best bands based on conditions and reliability
    fn recommend_bands(
        &self,
        conditions: &HashMap<Band, BandCondition>,
        reliability: &HashMap<Band, f64>,
    ) -> Vec<Band> {
        let mut band_scores: Vec<(Band, f64)> = conditions
            .iter()
            .map(|(&band, &condition)| {
                let condition_score = condition.score() as f64;
                let reliability_score = reliability.get(&band).unwrap_or(&0.0) * 100.0;
                let combined_score = (condition_score + reliability_score) / 2.0;
                (band, combined_score)
            })
            .collect();

        // Sort by score (highest first)
        band_scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Return top bands with score > 50
        band_scores
            .into_iter()
            .filter(|(_, score)| *score > 50.0)
            .map(|(band, _)| band)
            .take(5)
            .collect()
    }

    /// Get current propagation events
    pub async fn get_propagation_events(&self) -> Result<Vec<PropagationEvent>> {
        // This would fetch from space weather services
        // For now, return empty list
        Ok(Vec::new())
    }

    /// Predict aurora activity
    pub async fn predict_aurora(&mut self, latitude: f64) -> Result<f64> {
        let indices = self.get_solar_indices().await?;

        // Simple aurora prediction based on Kp index and latitude
        let aurora_threshold = if latitude.abs() > 60.0 {
            2.0 // High latitudes
        } else if latitude.abs() > 50.0 {
            4.0 // Mid latitudes
        } else {
            6.0 // Low latitudes
        };

        if indices.kp >= aurora_threshold {
            Ok((indices.kp - aurora_threshold + 1.0) / 4.0)
        } else {
            Ok(0.0)
        }
    }

    /// Calculate gray line (terminator) times
    pub fn calculate_gray_line(&self, date: chrono::NaiveDate) -> Result<Vec<(f64, f64)>> {
        // Calculate sunrise/sunset terminator for the given date
        // Returns a list of (latitude, longitude) points along the terminator

        let mut terminator_points = Vec::new();
        let julian_day = date.ordinal() as f64;

        // Solar declination for the date
        let declination = 23.45 * (360.0 * (284.0 + julian_day) / 365.0).to_radians().sin();

        // Calculate terminator points every degree of longitude
        for lon_deg in -180..180 {
            let lon = lon_deg as f64;

            // Calculate latitude where sunrise/sunset occurs at this longitude
            let lat = declination.to_degrees();

            terminator_points.push((lat, lon));
        }

        Ok(terminator_points)
    }
}

impl Default for PropagationPredictor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[tokio::test]
    async fn test_propagation_predictor_creation() {
        let predictor = PropagationPredictor::new();
        assert_eq!(predictor.cache_timeout_minutes, 30);
    }

    #[tokio::test]
    async fn test_solar_indices_fetch() {
        let mut predictor = PropagationPredictor::new();
        let indices = predictor.get_solar_indices().await.unwrap();

        assert!(indices.sfi > 0.0);
        assert!(indices.ssn >= 0.0);
        assert!(indices.kp >= 0.0);
        assert!(indices.a_index >= 0.0);
    }

    #[tokio::test]
    async fn test_propagation_prediction() {
        let mut predictor = PropagationPredictor::new();

        // New York to London path
        let prediction = predictor
            .predict_propagation(
                40.7128,
                -74.0060, // New York
                51.5074,
                -0.1278, // London
                Utc::now(),
            )
            .await
            .unwrap();

        assert!(prediction.distance_km > 5000.0 && prediction.distance_km < 6000.0);
        assert!(prediction.muf_mhz > 0.0);
        assert!(prediction.luf_mhz > 0.0);
        assert!(prediction.luf_mhz < prediction.muf_mhz);
        assert!(!prediction.band_conditions.is_empty());
    }

    #[test]
    fn test_band_condition_scoring() {
        assert_eq!(BandCondition::Excellent.score(), 95);
        assert_eq!(BandCondition::Good.score(), 65);
        assert_eq!(BandCondition::Closed.score(), 0);

        assert_eq!(BandCondition::from_score(95), BandCondition::Excellent);
        assert_eq!(BandCondition::from_score(65), BandCondition::Good);
        assert_eq!(BandCondition::from_score(0), BandCondition::Closed);
    }

    #[test]
    fn test_great_circle_distance() {
        let predictor = PropagationPredictor::new();

        // Distance from New York to London (approximately 5585 km)
        let distance =
            predictor.calculate_great_circle_distance(40.7128, -74.0060, 51.5074, -0.1278);

        assert!(distance > 5500.0 && distance < 5600.0);
    }

    #[tokio::test]
    async fn test_aurora_prediction() {
        let mut predictor = PropagationPredictor::new();

        // Test high latitude (should have lower threshold)
        let aurora_high = predictor.predict_aurora(65.0).await.unwrap();

        // Test low latitude (should have higher threshold)
        let aurora_low = predictor.predict_aurora(30.0).await.unwrap();

        // Both should be non-negative
        assert!(aurora_high >= 0.0);
        assert!(aurora_low >= 0.0);
    }

    #[test]
    fn test_gray_line_calculation() {
        let predictor = PropagationPredictor::new();
        let date = chrono::Utc::now().date_naive();

        let terminator = predictor.calculate_gray_line(date).unwrap();
        assert!(!terminator.is_empty());
        assert_eq!(terminator.len(), 360); // One point per degree of longitude
    }

    #[test]
    fn test_band_condition_display() {
        assert_eq!(BandCondition::Excellent.to_string(), "Excellent");
        assert_eq!(BandCondition::VeryGood.to_string(), "Very Good");
        assert_eq!(BandCondition::Closed.to_string(), "Closed");
    }
}
