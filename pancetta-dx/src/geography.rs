//! Geographic Calculations
//!
//! This module provides distance and bearing calculations between geographic
//! coordinates using great circle calculations and other geographic utilities.

use crate::{DxError, Result};
use geo::{Coord, Point};
use geographiclib_rs::{Geodesic, InverseGeodesic, DirectGeodesic};
use serde::{Deserialize, Serialize};
use chrono::Datelike;

/// Geographic coordinate
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Coordinate {
    /// Latitude in decimal degrees (-90 to +90)
    pub latitude: f64,
    /// Longitude in decimal degrees (-180 to +180)
    pub longitude: f64,
}

impl Coordinate {
    /// Create new coordinate
    pub fn new(latitude: f64, longitude: f64) -> Result<Self> {
        if !(-90.0..=90.0).contains(&latitude) {
            return Err(DxError::Geography(format!("Invalid latitude: {}", latitude)));
        }
        if !(-180.0..=180.0).contains(&longitude) {
            return Err(DxError::Geography(format!("Invalid longitude: {}", longitude)));
        }
        
        Ok(Self { latitude, longitude })
    }
    
    /// Convert to radians
    pub fn to_radians(&self) -> (f64, f64) {
        (self.latitude.to_radians(), self.longitude.to_radians())
    }
    
    /// Convert to geo::Point
    pub fn to_point(&self) -> Point<f64> {
        Point::new(self.longitude, self.latitude)
    }
    
    /// Convert to geo::Coord
    pub fn to_coord(&self) -> Coord<f64> {
        Coord { x: self.longitude, y: self.latitude }
    }
}

impl std::fmt::Display for Coordinate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.6}°, {:.6}°", self.latitude, self.longitude)
    }
}

/// Distance calculation result
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DistanceResult {
    /// Distance in kilometers
    pub distance_km: f64,
    /// Distance in miles
    pub distance_miles: f64,
    /// Distance in nautical miles
    pub distance_nm: f64,
    /// Forward azimuth (bearing from point 1 to point 2) in degrees
    pub forward_azimuth: f64,
    /// Reverse azimuth (bearing from point 2 to point 1) in degrees
    pub reverse_azimuth: f64,
}

/// Bearing calculation result
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BearingResult {
    /// True bearing in degrees (0-360)
    pub true_bearing: f64,
    /// Magnetic bearing in degrees (if magnetic declination provided)
    pub magnetic_bearing: Option<f64>,
    /// Cardinal direction (N, NE, E, SE, S, SW, W, NW)
    pub cardinal_direction: String,
    /// Detailed compass direction (e.g., "NNE", "ESE")
    pub compass_direction: String,
}

/// Geographic calculator
pub struct GeographyCalculator {
    /// Home station coordinates
    home_coordinate: Coordinate,
    /// Geodesic calculator for precise calculations
    geodesic: Geodesic,
}

impl GeographyCalculator {
    /// Create new geography calculator
    pub fn new(home_latitude: f64, home_longitude: f64) -> Self {
        let home_coordinate = Coordinate::new(home_latitude, home_longitude)
            .unwrap_or_else(|_| Coordinate { latitude: 0.0, longitude: 0.0 });
        
        Self {
            home_coordinate,
            geodesic: Geodesic::wgs84(),
        }
    }
    
    /// Update home coordinates
    pub fn set_home_coordinate(&mut self, latitude: f64, longitude: f64) -> Result<()> {
        self.home_coordinate = Coordinate::new(latitude, longitude)?;
        Ok(())
    }
    
    /// Get home coordinates
    pub fn home_coordinate(&self) -> Coordinate {
        self.home_coordinate
    }
    
    /// Calculate distance from home to target coordinates
    pub fn calculate_distance(&self, target_lat: f64, target_lon: f64) -> f64 {
        self.calculate_distance_between(
            self.home_coordinate.latitude,
            self.home_coordinate.longitude,
            target_lat,
            target_lon,
        ).distance_km
    }
    
    /// Calculate bearing from home to target coordinates
    pub fn calculate_bearing(&self, target_lat: f64, target_lon: f64) -> f64 {
        self.calculate_bearing_between(
            self.home_coordinate.latitude,
            self.home_coordinate.longitude,
            target_lat,
            target_lon,
        ).true_bearing
    }
    
    /// Calculate detailed distance between two points
    pub fn calculate_distance_between(
        &self,
        lat1: f64,
        lon1: f64,
        lat2: f64,
        lon2: f64,
    ) -> DistanceResult {
        let (distance_m, forward_azimuth, reverse_azimuth) = self.geodesic.inverse(lat1, lon1, lat2, lon2);
        
        let distance_km = distance_m / 1000.0;
        let distance_miles = distance_km * 0.621371;
        let distance_nm = distance_km * 0.539957;
        
        DistanceResult {
            distance_km,
            distance_miles,
            distance_nm,
            forward_azimuth: normalize_azimuth(forward_azimuth),
            reverse_azimuth: normalize_azimuth(reverse_azimuth),
        }
    }
    
    /// Calculate detailed bearing between two points
    pub fn calculate_bearing_between(
        &self,
        lat1: f64,
        lon1: f64,
        lat2: f64,
        lon2: f64,
    ) -> BearingResult {
        let (_, forward_azimuth, _) = self.geodesic.inverse(lat1, lon1, lat2, lon2);
        let true_bearing = normalize_azimuth(forward_azimuth);
        
        BearingResult {
            true_bearing,
            magnetic_bearing: None, // Would need magnetic declination calculation
            cardinal_direction: bearing_to_cardinal(true_bearing),
            compass_direction: bearing_to_compass(true_bearing),
        }
    }
    
    /// Calculate midpoint between two coordinates
    pub fn calculate_midpoint(&self, lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> Coordinate {
        // Convert to radians
        let lat1_rad = lat1.to_radians();
        let lon1_rad = lon1.to_radians();
        let lat2_rad = lat2.to_radians();
        let lon2_rad = lon2.to_radians();
        
        let d_lon = lon2_rad - lon1_rad;
        
        let bx = lat2_rad.cos() * d_lon.cos();
        let by = lat2_rad.cos() * d_lon.sin();
        
        let mid_lat = (lat1_rad.sin() + lat2_rad.sin()).atan2(
            ((lat1_rad.cos() + bx).powi(2) + by.powi(2)).sqrt()
        );
        let mid_lon = lon1_rad + by.atan2(lat1_rad.cos() + bx);
        
        Coordinate {
            latitude: mid_lat.to_degrees(),
            longitude: mid_lon.to_degrees(),
        }
    }
    
    /// Calculate destination point given starting point, bearing, and distance
    pub fn calculate_destination(
        &self,
        start_lat: f64,
        start_lon: f64,
        bearing_degrees: f64,
        distance_km: f64,
    ) -> Coordinate {
        // Use proper geodesic calculation with geographiclib-rs
        let distance_m = distance_km * 1000.0;
        let (end_lat, end_lon, _end_azimuth) = self.geodesic.direct(start_lat, start_lon, bearing_degrees, distance_m);
        
        Coordinate {
            latitude: end_lat,
            longitude: end_lon,
        }
    }
    
    /// Calculate cross track distance (distance from point to great circle path)
    pub fn calculate_cross_track_distance(
        &self,
        point_lat: f64,
        point_lon: f64,
        path_start_lat: f64,
        path_start_lon: f64,
        path_end_lat: f64,
        path_end_lon: f64,
    ) -> f64 {
        // Calculate distance from point to start of path
        let dist_to_start = self.calculate_distance_between(
            point_lat, point_lon,
            path_start_lat, path_start_lon,
        ).distance_km;
        
        // Calculate bearing from start to point
        let bearing_to_point = self.calculate_bearing_between(
            path_start_lat, path_start_lon,
            point_lat, point_lon,
        ).true_bearing;
        
        // Calculate bearing of the path
        let path_bearing = self.calculate_bearing_between(
            path_start_lat, path_start_lon,
            path_end_lat, path_end_lon,
        ).true_bearing;
        
        // Calculate cross track distance using spherical trigonometry
        let delta_bearing = (bearing_to_point - path_bearing).to_radians();
        let cross_track_distance = dist_to_start * delta_bearing.sin();
        
        cross_track_distance.abs()
    }
    
    /// Check if point is within a circular area
    pub fn is_within_radius(
        &self,
        point_lat: f64,
        point_lon: f64,
        center_lat: f64,
        center_lon: f64,
        radius_km: f64,
    ) -> bool {
        let distance = self.calculate_distance_between(
            point_lat, point_lon,
            center_lat, center_lon,
        ).distance_km;
        
        distance <= radius_km
    }
    
    /// Calculate area of a polygon defined by coordinates (in square kilometers)
    pub fn calculate_polygon_area(&self, coordinates: &[Coordinate]) -> f64 {
        if coordinates.len() < 3 {
            return 0.0;
        }
        
        // Use the shoelace formula adapted for spherical coordinates
        let mut area = 0.0;
        let earth_radius_km = 6371.0;
        
        for i in 0..coordinates.len() {
            let j = (i + 1) % coordinates.len();
            let lat1 = coordinates[i].latitude.to_radians();
            let lon1 = coordinates[i].longitude.to_radians();
            let lat2 = coordinates[j].latitude.to_radians();
            let lon2 = coordinates[j].longitude.to_radians();
            
            area += (lon2 - lon1) * (2.0 + lat1.sin() + lat2.sin());
        }
        
        area = area.abs() * earth_radius_km * earth_radius_km / 2.0;
        area
    }
    
    /// Find the closest point on a great circle path to a given point
    pub fn closest_point_on_path(
        &self,
        point_lat: f64,
        point_lon: f64,
        path_start_lat: f64,
        path_start_lon: f64,
        path_end_lat: f64,
        path_end_lon: f64,
    ) -> Coordinate {
        // Calculate the bearing of the path
        let path_bearing = self.calculate_bearing_between(
            path_start_lat, path_start_lon,
            path_end_lat, path_end_lon,
        ).true_bearing;
        
        // Calculate distance from start to point
        let dist_to_point = self.calculate_distance_between(
            path_start_lat, path_start_lon,
            point_lat, point_lon,
        );
        
        // Calculate bearing from start to point
        let bearing_to_point = self.calculate_bearing_between(
            path_start_lat, path_start_lon,
            point_lat, point_lon,
        ).true_bearing;
        
        // Calculate along-track distance using proper spherical trigonometry
        let delta_bearing = (bearing_to_point - path_bearing).to_radians();
        let along_track_distance = dist_to_point.distance_km * delta_bearing.cos();
        
        // Ensure we don't go beyond the path endpoints
        let path_distance = self.calculate_distance_between(
            path_start_lat, path_start_lon,
            path_end_lat, path_end_lon,
        ).distance_km;
        
        let clamped_distance = along_track_distance.max(0.0).min(path_distance);
        
        // Calculate the closest point using geodesic calculation
        self.calculate_destination(
            path_start_lat,
            path_start_lon,
            path_bearing,
            clamped_distance,
        )
    }
    
    /// Convert coordinate to MGRS (Military Grid Reference System)
    pub fn coordinate_to_mgrs(&self, lat: f64, lon: f64) -> Result<String> {
        // Basic MGRS implementation for amateur radio use
        mgrs_from_coordinate(lat, lon)
    }
    
    /// Parse MGRS coordinate to lat/lon
    pub fn mgrs_to_coordinate(&self, mgrs: &str) -> Result<Coordinate> {
        // Basic MGRS parsing for amateur radio use
        coordinate_from_mgrs(mgrs)
    }
}

/// Normalize azimuth to 0-360 degrees
fn normalize_azimuth(azimuth: f64) -> f64 {
    let mut normalized = azimuth % 360.0;
    if normalized < 0.0 {
        normalized += 360.0;
    }
    normalized
}

/// Convert bearing to cardinal direction
fn bearing_to_cardinal(bearing: f64) -> String {
    let directions = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
    let index = ((bearing + 22.5) / 45.0) as usize % 8;
    directions[index].to_string()
}

/// Convert bearing to detailed compass direction
fn bearing_to_compass(bearing: f64) -> String {
    let directions = [
        "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE",
        "S", "SSW", "SW", "WSW", "W", "WNW", "NW", "NNW"
    ];
    let index = ((bearing + 11.25) / 22.5) as usize % 16;
    directions[index].to_string()
}

/// Calculate magnetic declination for a given coordinate and date
pub fn calculate_magnetic_declination(
    latitude: f64,
    longitude: f64,
    date: chrono::DateTime<chrono::Utc>,
) -> Result<f64> {
    // Simplified magnetic declination calculation using WMM2020 coefficients
    // This is a basic approximation suitable for amateur radio use
    
    let lat_rad = latitude.to_radians();
    let lon_rad = longitude.to_radians();
    
    // Calculate decimal year from date
    let year = date.year() as f64;
    let day_of_year = date.ordinal() as f64;
    let days_in_year = if chrono::NaiveDate::from_ymd_opt(date.year(), 12, 31).is_some() { 365.0 } else { 366.0 };
    let decimal_year = year + (day_of_year - 1.0) / days_in_year;
    
    // WMM2020 simplified coefficients (epoch 2020.0)
    let epoch = 2020.0;
    let dt = decimal_year - epoch;
    
    // Main field coefficients (simplified for basic calculation)
    let g10 = -29404.8 + 6.7 * dt;  // nT
    let g11 = -1450.9 + 7.4 * dt;   // nT
    let h11 = 4652.5 + -25.9 * dt;  // nT
    
    // Calculate magnetic field components (simplified)
    let cos_lat = lat_rad.cos();
    let sin_lat = lat_rad.sin();
    let cos_lon = lon_rad.cos();
    let sin_lon = lon_rad.sin();
    
    // Earth radius and magnetic field calculations
    let a = 6371.2; // Earth radius in km
    let _r = a; // Simplified - assume surface level
    
    // Magnetic field components (simplified dipole approximation)
    let x = g10 * cos_lat + g11 * cos_lat * cos_lon;
    let y = g11 * cos_lat * sin_lon + h11 * cos_lat * sin_lon;
    let _z = -g10 * sin_lat - 2.0 * g11 * sin_lat * cos_lon;
    
    // Calculate declination
    let declination_rad = y.atan2(x);
    let declination_deg = declination_rad.to_degrees();
    
    Ok(declination_deg)
}

/// Calculate sunrise/sunset times for a given coordinate and date
pub fn calculate_sun_times(
    latitude: f64,
    longitude: f64,
    date: chrono::NaiveDate,
) -> Result<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> {
    // Use actual date for calculation instead of fixed julian day
    let day_of_year = date.ordinal() as f64;
    let lat_rad = latitude.to_radians();
    
    // Calculate solar declination using actual date
    let p = (day_of_year - 81.0) * 2.0 * std::f64::consts::PI / 365.0;
    let declination = 23.45_f64.to_radians() * p.sin();
    
    // Hour angle for sunrise/sunset (accounting for atmospheric refraction)
    let cos_hour_angle = -(0.01454_f64.to_radians().tan()) - lat_rad.tan() * declination.tan();
    
    // Check for polar day/night conditions
    if cos_hour_angle > 1.0 {
        // Polar night - sun never rises
        let midnight = date.and_hms_opt(0, 0, 0).unwrap().and_utc();
        return Ok((midnight, midnight));
    } else if cos_hour_angle < -1.0 {
        // Polar day - sun never sets
        let noon = date.and_hms_opt(12, 0, 0).unwrap().and_utc();
        return Ok((noon, noon));
    }
    
    let hour_angle = cos_hour_angle.acos();
    let hour_angle_deg = hour_angle.to_degrees();
    
    // Calculate equation of time for more accurate solar noon
    let b = 2.0 * std::f64::consts::PI * (day_of_year - 81.0) / 365.0;
    let equation_of_time = 9.87 * (2.0 * b).sin() - 7.53 * b.cos() - 1.5 * b.sin();
    
    // Calculate solar noon in local solar time
    let solar_noon = 12.0 - longitude / 15.0 - equation_of_time / 60.0;
    let sunrise_hour = solar_noon - hour_angle_deg / 15.0;
    let sunset_hour = solar_noon + hour_angle_deg / 15.0;
    
    // Convert to UTC DateTime
    let sunrise = if sunrise_hour >= 0.0 && sunrise_hour < 24.0 {
        let hour = sunrise_hour as u32;
        let minute = ((sunrise_hour % 1.0) * 60.0) as u32;
        let second = (((sunrise_hour % 1.0) * 60.0 % 1.0) * 60.0) as u32;
        date.and_hms_opt(hour, minute, second).unwrap_or_else(|| date.and_hms_opt(12, 0, 0).unwrap()).and_utc()
    } else {
        // Handle day boundary crossing
        let adjusted_hour = if sunrise_hour < 0.0 { sunrise_hour + 24.0 } else { sunrise_hour - 24.0 };
        let hour = adjusted_hour as u32;
        let minute = ((adjusted_hour % 1.0) * 60.0) as u32;
        let second = (((adjusted_hour % 1.0) * 60.0 % 1.0) * 60.0) as u32;
        let adjusted_date = if sunrise_hour < 0.0 { date - chrono::Duration::days(1) } else { date + chrono::Duration::days(1) };
        adjusted_date.and_hms_opt(hour, minute, second).unwrap_or_else(|| date.and_hms_opt(6, 0, 0).unwrap()).and_utc()
    };
    
    let sunset = if sunset_hour >= 0.0 && sunset_hour < 24.0 {
        let hour = sunset_hour as u32;
        let minute = ((sunset_hour % 1.0) * 60.0) as u32;
        let second = (((sunset_hour % 1.0) * 60.0 % 1.0) * 60.0) as u32;
        date.and_hms_opt(hour, minute, second).unwrap_or_else(|| date.and_hms_opt(18, 0, 0).unwrap()).and_utc()
    } else {
        // Handle day boundary crossing
        let adjusted_hour = if sunset_hour < 0.0 { sunset_hour + 24.0 } else { sunset_hour - 24.0 };
        let hour = adjusted_hour as u32;
        let minute = ((adjusted_hour % 1.0) * 60.0) as u32;
        let second = (((adjusted_hour % 1.0) * 60.0 % 1.0) * 60.0) as u32;
        let adjusted_date = if sunset_hour < 0.0 { date - chrono::Duration::days(1) } else { date + chrono::Duration::days(1) };
        adjusted_date.and_hms_opt(hour, minute, second).unwrap_or_else(|| date.and_hms_opt(18, 0, 0).unwrap()).and_utc()
    };
    
    Ok((sunrise, sunset))
}

/// Basic MGRS conversion functions for amateur radio use
/// MGRS grid zones (simplified implementation)
fn mgrs_from_coordinate(lat: f64, lon: f64) -> Result<String> {
    // Basic MGRS implementation - simplified for amateur radio use
    
    // UTM zone calculation
    let zone = ((lon + 180.0) / 6.0).floor() as i32 + 1;
    let zone = zone.max(1).min(60);
    
    // Zone letter (latitude bands)
    let zone_letter = match lat {
        lat if lat >= -80.0 && lat < -72.0 => 'C',
        lat if lat >= -72.0 && lat < -64.0 => 'D',
        lat if lat >= -64.0 && lat < -56.0 => 'E',
        lat if lat >= -56.0 && lat < -48.0 => 'F',
        lat if lat >= -48.0 && lat < -40.0 => 'G',
        lat if lat >= -40.0 && lat < -32.0 => 'H',
        lat if lat >= -32.0 && lat < -24.0 => 'J',
        lat if lat >= -24.0 && lat < -16.0 => 'K',
        lat if lat >= -16.0 && lat < -8.0 => 'L',
        lat if lat >= -8.0 && lat < 0.0 => 'M',
        lat if lat >= 0.0 && lat < 8.0 => 'N',
        lat if lat >= 8.0 && lat < 16.0 => 'P',
        lat if lat >= 16.0 && lat < 24.0 => 'Q',
        lat if lat >= 24.0 && lat < 32.0 => 'R',
        lat if lat >= 32.0 && lat < 40.0 => 'S',
        lat if lat >= 40.0 && lat < 48.0 => 'T',
        lat if lat >= 48.0 && lat < 56.0 => 'U',
        lat if lat >= 56.0 && lat < 64.0 => 'V',
        lat if lat >= 64.0 && lat < 72.0 => 'W',
        lat if lat >= 72.0 && lat <= 84.0 => 'X',
        _ => return Err(DxError::Geography("Latitude out of MGRS range".to_string())),
    };
    
    // Simplified grid square identification (100km squares)
    let e_grid = ((lon + 180.0) % 6.0 / 6.0 * 8.0) as usize;
    let n_grid = ((lat + 80.0) % 8.0 / 8.0 * 8.0) as usize;
    
    let e_letters = ['A', 'B', 'C', 'D', 'E', 'F', 'G', 'H'];
    let n_letters = ['A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'J', 'K', 'L', 'M', 'N', 'P', 'Q', 'R', 'S', 'T', 'U', 'V'];
    
    let grid_e = e_letters.get(e_grid).unwrap_or(&'A');
    let grid_n = n_letters.get(n_grid).unwrap_or(&'A');
    
    // Basic format: zone + zone_letter + grid_e + grid_n + coordinates
    // For simplicity, use 4-digit precision (10m resolution)
    let central_meridian = (zone as f64 - 1.0) * 6.0 - 180.0 + 3.0;
    let easting = ((lon - central_meridian) / 6.0 * 800000.0 + 500000.0) as u32;
    let northing = if lat >= 0.0 {
        (lat / 8.0 * 1000000.0) as u32
    } else {
        (10000000.0 + lat / 8.0 * 1000000.0) as u32
    };
    
    let e_coord = (easting % 100000) / 10;
    let n_coord = (northing % 100000) / 10;
    
    Ok(format!("{:02}{}{}{}{:04}{:04}", zone, zone_letter, grid_e, grid_n, e_coord, n_coord))
}

fn coordinate_from_mgrs(mgrs: &str) -> Result<Coordinate> {
    if mgrs.len() < 5 {
        return Err(DxError::Geography("Invalid MGRS format".to_string()));
    }
    
    // Parse zone number (first 2 characters)
    let zone: i32 = mgrs[0..2].parse()
        .map_err(|_| DxError::Geography("Invalid MGRS zone".to_string()))?;
    
    if zone < 1 || zone > 60 {
        return Err(DxError::Geography("MGRS zone out of range".to_string()));
    }
    
    // Parse zone letter
    let zone_letter = mgrs.chars().nth(2)
        .ok_or_else(|| DxError::Geography("Missing MGRS zone letter".to_string()))?;
    
    // Convert zone letter to approximate latitude
    let base_lat = match zone_letter {
        'C' => -76.0, 'D' => -68.0, 'E' => -60.0, 'F' => -52.0,
        'G' => -44.0, 'H' => -36.0, 'J' => -28.0, 'K' => -20.0,
        'L' => -12.0, 'M' => -4.0, 'N' => 4.0, 'P' => 12.0,
        'Q' => 20.0, 'R' => 28.0, 'S' => 36.0, 'T' => 44.0,
        'U' => 52.0, 'V' => 60.0, 'W' => 68.0, 'X' => 76.0,
        _ => return Err(DxError::Geography("Invalid MGRS zone letter".to_string())),
    };
    
    // Calculate central meridian
    let central_meridian = (zone as f64 - 1.0) * 6.0 - 180.0 + 3.0;
    
    // For simplified implementation, return center of zone
    // A full implementation would parse the grid square and coordinates
    let lat = base_lat;
    let lon = central_meridian;
    
    Coordinate::new(lat, lon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;
    
    #[test]
    fn test_coordinate_creation() {
        let coord = Coordinate::new(40.7128, -74.0060).unwrap();
        assert_eq!(coord.latitude, 40.7128);
        assert_eq!(coord.longitude, -74.0060);
        
        // Test invalid coordinates
        assert!(Coordinate::new(91.0, 0.0).is_err());
        assert!(Coordinate::new(0.0, 181.0).is_err());
    }
    
    #[test]
    fn test_distance_calculation() {
        let calc = GeographyCalculator::new(40.7128, -74.0060); // New York
        
        // Distance to London (approximately)
        let distance = calc.calculate_distance(51.5074, -0.1278);
        assert!(distance > 5500.0 && distance < 5600.0); // ~5550 km
    }
    
    #[test]
    fn test_bearing_calculation() {
        let calc = GeographyCalculator::new(40.7128, -74.0060); // New York
        
        // Bearing to London
        let bearing = calc.calculate_bearing(51.5074, -0.1278);
        assert!(bearing > 50.0 && bearing < 60.0); // Approximately northeast
    }
    
    #[test]
    fn test_detailed_distance_calculation() {
        let calc = GeographyCalculator::new(0.0, 0.0);
        
        let result = calc.calculate_distance_between(0.0, 0.0, 0.0, 1.0);
        assert!(result.distance_km > 110.0 && result.distance_km < 112.0); // ~111 km per degree at equator
        assert_relative_eq!(result.forward_azimuth, 90.0, epsilon = 1.0); // Due east
    }
    
    #[test]
    fn test_midpoint_calculation() {
        let calc = GeographyCalculator::new(0.0, 0.0);
        
        let midpoint = calc.calculate_midpoint(0.0, 0.0, 0.0, 2.0);
        assert_relative_eq!(midpoint.latitude, 0.0, epsilon = 0.01);
        assert_relative_eq!(midpoint.longitude, 1.0, epsilon = 0.01);
    }
    
    #[test]
    fn test_destination_calculation() {
        let calc = GeographyCalculator::new(0.0, 0.0);
        
        let dest = calc.calculate_destination(0.0, 0.0, 90.0, 111.0); // 111 km due east
        assert_relative_eq!(dest.latitude, 0.0, epsilon = 0.01);
        assert_relative_eq!(dest.longitude, 1.0, epsilon = 0.1); // Approximately 1 degree
    }
    
    #[test]
    fn test_within_radius() {
        let calc = GeographyCalculator::new(40.7128, -74.0060);
        
        // Point very close to home
        assert!(calc.is_within_radius(40.7130, -74.0062, 40.7128, -74.0060, 1.0));
        
        // Point far away
        assert!(!calc.is_within_radius(51.5074, -0.1278, 40.7128, -74.0060, 1000.0));
    }
    
    #[test]
    fn test_bearing_to_cardinal() {
        assert_eq!(bearing_to_cardinal(0.0), "N");
        assert_eq!(bearing_to_cardinal(45.0), "NE");
        assert_eq!(bearing_to_cardinal(90.0), "E");
        assert_eq!(bearing_to_cardinal(135.0), "SE");
        assert_eq!(bearing_to_cardinal(180.0), "S");
        assert_eq!(bearing_to_cardinal(225.0), "SW");
        assert_eq!(bearing_to_cardinal(270.0), "W");
        assert_eq!(bearing_to_cardinal(315.0), "NW");
    }
    
    #[test]
    fn test_bearing_to_compass() {
        assert_eq!(bearing_to_compass(0.0), "N");
        assert_eq!(bearing_to_compass(22.5), "NNE");
        assert_eq!(bearing_to_compass(45.0), "NE");
        assert_eq!(bearing_to_compass(67.5), "ENE");
        assert_eq!(bearing_to_compass(90.0), "E");
    }
    
    #[test]
    fn test_normalize_azimuth() {
        assert_relative_eq!(normalize_azimuth(0.0), 0.0, epsilon = 0.01);
        assert_relative_eq!(normalize_azimuth(360.0), 0.0, epsilon = 0.01);
        assert_relative_eq!(normalize_azimuth(-90.0), 270.0, epsilon = 0.01);
        assert_relative_eq!(normalize_azimuth(450.0), 90.0, epsilon = 0.01);
    }
    
    #[test]
    fn test_coordinate_display() {
        let coord = Coordinate::new(40.712800, -74.006000).unwrap();
        let display = format!("{}", coord);
        assert!(display.contains("40.712800"));
        assert!(display.contains("-74.006000"));
    }
}