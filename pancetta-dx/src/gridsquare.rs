//! Maidenhead Grid Square Operations
//!
//! This module provides comprehensive support for Maidenhead Locator System
//! including conversion to/from coordinates, distance calculations, and validation.

use crate::{DxError, Result};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Maidenhead grid square precision levels
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GridPrecision {
    /// 2-character field (20° × 10°)
    Field = 2,
    /// 4-character square (2° × 1°)
    Square = 4,
    /// 6-character subsquare (5' × 2.5')
    Subsquare = 6,
    /// 8-character extended square (1.25' × 0.625')
    ExtendedSquare = 8,
    /// 10-character sub-extended square (0.125' × 0.0625')
    SubExtendedSquare = 10,
}

impl GridPrecision {
    /// Get all precision levels
    pub fn all() -> &'static [GridPrecision] {
        &[
            GridPrecision::Field,
            GridPrecision::Square,
            GridPrecision::Subsquare,
            GridPrecision::ExtendedSquare,
            GridPrecision::SubExtendedSquare,
        ]
    }
    
    /// Get approximate resolution in degrees
    pub fn resolution_degrees(&self) -> (f64, f64) {
        match self {
            GridPrecision::Field => (20.0, 10.0),
            GridPrecision::Square => (2.0, 1.0),
            GridPrecision::Subsquare => (5.0 / 60.0, 2.5 / 60.0),
            GridPrecision::ExtendedSquare => (1.25 / 60.0, 0.625 / 60.0),
            GridPrecision::SubExtendedSquare => (0.125 / 60.0, 0.0625 / 60.0),
        }
    }
    
    /// Get approximate resolution in kilometers at equator
    pub fn resolution_km(&self) -> (f64, f64) {
        let (lon_deg, lat_deg) = self.resolution_degrees();
        (lon_deg * 111.32, lat_deg * 110.54) // Approximate km per degree
    }
}

/// Maidenhead grid square
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GridSquare {
    /// Grid square string (e.g., "FN31pr")
    grid: String,
    /// Precision level
    precision: GridPrecision,
}

impl GridSquare {
    /// Create new grid square from string
    pub fn new(grid: &str) -> Result<Self> {
        let grid = grid.to_uppercase();
        let precision = match grid.len() {
            2 => GridPrecision::Field,
            4 => GridPrecision::Square,
            6 => GridPrecision::Subsquare,
            8 => GridPrecision::ExtendedSquare,
            10 => GridPrecision::SubExtendedSquare,
            _ => return Err(DxError::InvalidGridSquare(format!("Invalid grid square length: {}", grid.len()))),
        };
        
        let mut grid_square = Self { grid, precision };
        grid_square.validate()?;
        
        Ok(grid_square)
    }
    
    /// Create grid square from coordinates
    pub fn from_coordinates(latitude: f64, longitude: f64, precision: GridPrecision) -> Result<Self> {
        let grid = coordinates_to_grid(latitude, longitude, precision)?;
        Ok(Self { grid, precision })
    }
    
    /// Get grid square string
    pub fn grid(&self) -> &str {
        &self.grid
    }
    
    /// Get precision level
    pub fn precision(&self) -> GridPrecision {
        self.precision
    }
    
    /// Convert to coordinates (center of grid square)
    pub fn to_coordinates(&self) -> Result<(f64, f64)> {
        grid_to_coordinates(&self.grid)
    }
    
    /// Get grid square bounds (southwest and northeast corners)
    pub fn bounds(&self) -> Result<((f64, f64), (f64, f64))> {
        let (center_lat, center_lon) = self.to_coordinates()?;
        let (lon_res, lat_res) = self.precision.resolution_degrees();
        
        let sw_lat = center_lat - lat_res / 2.0;
        let sw_lon = center_lon - lon_res / 2.0;
        let ne_lat = center_lat + lat_res / 2.0;
        let ne_lon = center_lon + lon_res / 2.0;
        
        Ok(((sw_lat, sw_lon), (ne_lat, ne_lon)))
    }
    
    /// Calculate distance to another grid square
    pub fn distance_to(&self, other: &GridSquare) -> Result<f64> {
        let (lat1, lon1) = self.to_coordinates()?;
        let (lat2, lon2) = other.to_coordinates()?;
        
        Ok(calculate_distance(lat1, lon1, lat2, lon2))
    }
    
    /// Calculate bearing to another grid square
    pub fn bearing_to(&self, other: &GridSquare) -> Result<f64> {
        let (lat1, lon1) = self.to_coordinates()?;
        let (lat2, lon2) = other.to_coordinates()?;
        
        Ok(calculate_bearing(lat1, lon1, lat2, lon2))
    }
    
    /// Extend precision of grid square
    pub fn extend_precision(&self, new_precision: GridPrecision) -> Result<Self> {
        if (new_precision as usize) <= (self.precision as usize) {
            return Err(DxError::InvalidGridSquare(
                "Cannot extend to lower precision".to_string()
            ));
        }
        
        let (lat, lon) = self.to_coordinates()?;
        Self::from_coordinates(lat, lon, new_precision)
    }
    
    /// Reduce precision of grid square
    pub fn reduce_precision(&self, new_precision: GridPrecision) -> Result<Self> {
        if (new_precision as usize) >= (self.precision as usize) {
            return Err(DxError::InvalidGridSquare(
                "Cannot reduce to higher precision".to_string()
            ));
        }
        
        let new_len = new_precision as usize;
        let truncated_grid = &self.grid[..new_len];
        
        Ok(Self {
            grid: truncated_grid.to_string(),
            precision: new_precision,
        })
    }
    
    /// Get adjacent grid squares
    pub fn adjacent_squares(&self) -> Result<Vec<GridSquare>> {
        let (center_lat, center_lon) = self.to_coordinates()?;
        let (lon_res, lat_res) = self.precision.resolution_degrees();
        
        let mut adjacent = Vec::new();
        
        // 8 surrounding squares
        for lat_offset in [-1, 0, 1] {
            for lon_offset in [-1, 0, 1] {
                if lat_offset == 0 && lon_offset == 0 {
                    continue; // Skip center square
                }
                
                let adj_lat = center_lat + lat_offset as f64 * lat_res;
                let adj_lon = center_lon + lon_offset as f64 * lon_res;
                
                // Wrap longitude
                let adj_lon = if adj_lon > 180.0 {
                    adj_lon - 360.0
                } else if adj_lon <= -180.0 {
                    adj_lon + 360.0
                } else {
                    adj_lon
                };
                
                // Skip if latitude is out of bounds
                if adj_lat < -90.0 || adj_lat > 90.0 {
                    continue;
                }
                
                if let Ok(grid) = Self::from_coordinates(adj_lat, adj_lon, self.precision) {
                    adjacent.push(grid);
                }
            }
        }
        
        Ok(adjacent)
    }
    
    /// Check if this grid square contains given coordinates
    pub fn contains(&self, latitude: f64, longitude: f64) -> Result<bool> {
        let ((sw_lat, sw_lon), (ne_lat, ne_lon)) = self.bounds()?;
        
        Ok(latitude >= sw_lat && latitude < ne_lat && 
           longitude >= sw_lon && longitude < ne_lon)
    }
    
    /// Validate grid square format
    fn validate(&self) -> Result<()> {
        let chars: Vec<char> = self.grid.chars().collect();
        
        if chars.is_empty() || chars.len() > 10 || chars.len() % 2 != 0 {
            return Err(DxError::InvalidGridSquare(
                "Grid square must have 2, 4, 6, 8, or 10 characters".to_string()
            ));
        }
        
        // Validate each pair of characters
        for (i, chunk) in chars.chunks(2).enumerate() {
            match i {
                0 => {
                    // First pair: letters A-R
                    if !chunk[0].is_ascii_alphabetic() || !chunk[1].is_ascii_alphabetic() ||
                       chunk[0] < 'A' || chunk[0] > 'R' ||
                       chunk[1] < 'A' || chunk[1] > 'R' {
                        return Err(DxError::InvalidGridSquare(
                            "First pair must be letters A-R".to_string()
                        ));
                    }
                },
                1 => {
                    // Second pair: digits 0-9
                    if !chunk[0].is_ascii_digit() || !chunk[1].is_ascii_digit() {
                        return Err(DxError::InvalidGridSquare(
                            "Second pair must be digits 0-9".to_string()
                        ));
                    }
                },
                2 => {
                    // Third pair: letters A-X
                    if !chunk[0].is_ascii_alphabetic() || !chunk[1].is_ascii_alphabetic() ||
                       chunk[0] < 'A' || chunk[0] > 'X' ||
                       chunk[1] < 'A' || chunk[1] > 'X' {
                        return Err(DxError::InvalidGridSquare(
                            "Third pair must be letters A-X".to_string()
                        ));
                    }
                },
                3 => {
                    // Fourth pair: digits 0-9
                    if !chunk[0].is_ascii_digit() || !chunk[1].is_ascii_digit() {
                        return Err(DxError::InvalidGridSquare(
                            "Fourth pair must be digits 0-9".to_string()
                        ));
                    }
                },
                4 => {
                    // Fifth pair: letters A-X
                    if !chunk[0].is_ascii_alphabetic() || !chunk[1].is_ascii_alphabetic() ||
                       chunk[0] < 'A' || chunk[0] > 'X' ||
                       chunk[1] < 'A' || chunk[1] > 'X' {
                        return Err(DxError::InvalidGridSquare(
                            "Fifth pair must be letters A-X".to_string()
                        ));
                    }
                },
                _ => {
                    return Err(DxError::InvalidGridSquare(
                        "Too many character pairs".to_string()
                    ));
                }
            }
        }
        
        Ok(())
    }
}

impl fmt::Display for GridSquare {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.grid)
    }
}

impl std::str::FromStr for GridSquare {
    type Err = DxError;
    
    fn from_str(s: &str) -> Result<Self> {
        Self::new(s)
    }
}

/// Convert coordinates to Maidenhead grid square
pub fn coordinates_to_grid(latitude: f64, longitude: f64, precision: GridPrecision) -> Result<String> {
    if latitude < -90.0 || latitude > 90.0 {
        return Err(DxError::InvalidGridSquare(format!("Invalid latitude: {}", latitude)));
    }
    if longitude < -180.0 || longitude > 180.0 {
        return Err(DxError::InvalidGridSquare(format!("Invalid longitude: {}", longitude)));
    }
    
    let mut grid = String::new();
    
    // Normalize coordinates
    let mut lon = longitude + 180.0; // 0-360
    let mut lat = latitude + 90.0;   // 0-180
    
    // Field (first pair - letters A-R)
    if precision as usize >= 2 {
        let lon_field = (lon / 20.0) as usize;
        let lat_field = (lat / 10.0) as usize;
        
        grid.push(char::from(b'A' + lon_field as u8));
        grid.push(char::from(b'A' + lat_field as u8));
        
        lon %= 20.0;
        lat %= 10.0;
    }
    
    // Square (second pair - digits 0-9)
    if precision as usize >= 4 {
        let lon_square = (lon / 2.0) as usize;
        let lat_square = (lat / 1.0) as usize;
        
        grid.push(char::from(b'0' + lon_square as u8));
        grid.push(char::from(b'0' + lat_square as u8));
        
        lon %= 2.0;
        lat %= 1.0;
    }
    
    // Subsquare (third pair - letters a-x, converted to A-X)
    if precision as usize >= 6 {
        let lon_subsquare = (lon / (2.0 / 24.0)) as usize;
        let lat_subsquare = (lat / (1.0 / 24.0)) as usize;
        
        grid.push(char::from(b'A' + lon_subsquare as u8));
        grid.push(char::from(b'A' + lat_subsquare as u8));
        
        lon %= 2.0 / 24.0;
        lat %= 1.0 / 24.0;
    }
    
    // Extended square (fourth pair - digits 0-9)
    if precision as usize >= 8 {
        let lon_ext = (lon / (2.0 / 240.0)) as usize;
        let lat_ext = (lat / (1.0 / 240.0)) as usize;
        
        grid.push(char::from(b'0' + lon_ext as u8));
        grid.push(char::from(b'0' + lat_ext as u8));
        
        lon %= 2.0 / 240.0;
        lat %= 1.0 / 240.0;
    }
    
    // Sub-extended square (fifth pair - letters A-X)
    if precision as usize >= 10 {
        let lon_subext = (lon / (2.0 / 5760.0)) as usize;
        let lat_subext = (lat / (1.0 / 5760.0)) as usize;
        
        grid.push(char::from(b'A' + lon_subext as u8));
        grid.push(char::from(b'A' + lat_subext as u8));
    }
    
    Ok(grid)
}

/// Convert Maidenhead grid square to coordinates (center of square)
pub fn grid_to_coordinates(grid: &str) -> Result<(f64, f64)> {
    let grid = grid.to_uppercase();
    let chars: Vec<char> = grid.chars().collect();
    
    if chars.is_empty() || chars.len() > 10 || chars.len() % 2 != 0 {
        return Err(DxError::InvalidGridSquare(
            "Invalid grid square length".to_string()
        ));
    }
    
    let mut longitude = 0.0;
    let mut latitude = 0.0;
    
    // Process each pair
    for (i, chunk) in chars.chunks(2).enumerate() {
        match i {
            0 => {
                // Field (A-R)
                let lon_field = (chunk[0] as u8 - b'A') as f64;
                let lat_field = (chunk[1] as u8 - b'A') as f64;
                longitude += lon_field * 20.0;
                latitude += lat_field * 10.0;
            },
            1 => {
                // Square (0-9)
                let lon_square = (chunk[0] as u8 - b'0') as f64;
                let lat_square = (chunk[1] as u8 - b'0') as f64;
                longitude += lon_square * 2.0;
                latitude += lat_square * 1.0;
            },
            2 => {
                // Subsquare (A-X)
                let lon_subsquare = (chunk[0] as u8 - b'A') as f64;
                let lat_subsquare = (chunk[1] as u8 - b'A') as f64;
                longitude += lon_subsquare * (2.0 / 24.0);
                latitude += lat_subsquare * (1.0 / 24.0);
            },
            3 => {
                // Extended square (0-9)
                let lon_ext = (chunk[0] as u8 - b'0') as f64;
                let lat_ext = (chunk[1] as u8 - b'0') as f64;
                longitude += lon_ext * (2.0 / 240.0);
                latitude += lat_ext * (1.0 / 240.0);
            },
            4 => {
                // Sub-extended square (A-X)
                let lon_subext = (chunk[0] as u8 - b'A') as f64;
                let lat_subext = (chunk[1] as u8 - b'A') as f64;
                longitude += lon_subext * (2.0 / 5760.0);
                latitude += lat_subext * (1.0 / 5760.0);
            },
            _ => {}
        }
    }
    
    // Add half of the resolution to get center coordinates
    let precision = match chars.len() {
        2 => GridPrecision::Field,
        4 => GridPrecision::Square,
        6 => GridPrecision::Subsquare,
        8 => GridPrecision::ExtendedSquare,
        10 => GridPrecision::SubExtendedSquare,
        _ => return Err(DxError::InvalidGridSquare("Invalid length".to_string())),
    };
    
    let (lon_res, lat_res) = precision.resolution_degrees();
    longitude += lon_res / 2.0;
    latitude += lat_res / 2.0;
    
    // Convert back to standard coordinates
    longitude -= 180.0;
    latitude -= 90.0;
    
    Ok((latitude, longitude))
}

/// Calculate distance between two grid squares
pub fn grid_distance(grid1: &str, grid2: &str) -> Result<f64> {
    let (lat1, lon1) = grid_to_coordinates(grid1)?;
    let (lat2, lon2) = grid_to_coordinates(grid2)?;
    
    Ok(calculate_distance(lat1, lon1, lat2, lon2))
}

/// Calculate bearing between two grid squares
pub fn grid_bearing(grid1: &str, grid2: &str) -> Result<f64> {
    let (lat1, lon1) = grid_to_coordinates(grid1)?;
    let (lat2, lon2) = grid_to_coordinates(grid2)?;
    
    Ok(calculate_bearing(lat1, lon1, lat2, lon2))
}

/// Calculate great circle distance between coordinates (km)
fn calculate_distance(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let earth_radius_km = 6371.0;
    
    let lat1_rad = lat1.to_radians();
    let lat2_rad = lat2.to_radians();
    let delta_lat = (lat2 - lat1).to_radians();
    let delta_lon = (lon2 - lon1).to_radians();
    
    let a = (delta_lat / 2.0).sin().powi(2) + 
            lat1_rad.cos() * lat2_rad.cos() * (delta_lon / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());
    
    earth_radius_km * c
}

/// Calculate bearing between coordinates (degrees)
fn calculate_bearing(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let lat1_rad = lat1.to_radians();
    let lat2_rad = lat2.to_radians();
    let delta_lon = (lon2 - lon1).to_radians();
    
    let y = delta_lon.sin() * lat2_rad.cos();
    let x = lat1_rad.cos() * lat2_rad.sin() - 
            lat1_rad.sin() * lat2_rad.cos() * delta_lon.cos();
    
    let bearing_rad = y.atan2(x);
    (bearing_rad.to_degrees() + 360.0) % 360.0
}

/// Parse multiple grid squares from text
pub fn parse_grids_from_text(text: &str) -> Vec<GridSquare> {
    let mut grids = Vec::new();
    
    // Look for potential grid square patterns
    let words: Vec<&str> = text.split_whitespace().collect();
    
    for word in words {
        // Remove common punctuation
        let clean_word = word.trim_matches(|c: char| !c.is_alphanumeric());
        
        // Try to parse as grid square
        if let Ok(grid) = GridSquare::new(clean_word) {
            grids.push(grid);
        }
    }
    
    grids
}

/// Validate grid square format
pub fn validate_grid(grid: &str) -> Result<()> {
    GridSquare::new(grid)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;
    
    #[test]
    fn test_grid_creation() {
        let grid = GridSquare::new("FN31pr").unwrap();
        assert_eq!(grid.grid(), "FN31PR");
        assert_eq!(grid.precision(), GridPrecision::Subsquare);
    }
    
    #[test]
    fn test_invalid_grids() {
        assert!(GridSquare::new("ZZ99zz").is_err()); // Invalid field
        assert!(GridSquare::new("AA").is_ok());      // Valid field
        assert!(GridSquare::new("AAAA").is_err());   // Invalid digits
        assert!(GridSquare::new("AA00").is_ok());    // Valid square
    }
    
    #[test]
    fn test_coordinates_conversion() {
        let (lat, lon) = grid_to_coordinates("FN31pr").unwrap();
        assert_relative_eq!(lat, 41.895833, epsilon = 0.1);
        assert_relative_eq!(lon, -72.958333, epsilon = 0.1);
        
        let grid = coordinates_to_grid(lat, lon, GridPrecision::Subsquare).unwrap();
        assert_eq!(grid, "FN31PR");
    }
    
    #[test]
    fn test_distance_calculation() {
        let distance = grid_distance("FN31pr", "EN90cv").unwrap();
        assert!(distance > 1000.0); // Should be over 1000 km
        
        let grid1 = GridSquare::new("FN31pr").unwrap();
        let grid2 = GridSquare::new("EN90cv").unwrap();
        let distance2 = grid1.distance_to(&grid2).unwrap();
        
        assert_relative_eq!(distance, distance2, epsilon = 0.1);
    }
    
    #[test]
    fn test_bearing_calculation() {
        let bearing = grid_bearing("FN31pr", "EN90cv").unwrap();
        assert!(bearing >= 0.0 && bearing < 360.0);
        
        let grid1 = GridSquare::new("FN31pr").unwrap();
        let grid2 = GridSquare::new("EN90cv").unwrap();
        let bearing2 = grid1.bearing_to(&grid2).unwrap();
        
        assert_relative_eq!(bearing, bearing2, epsilon = 0.1);
    }
    
    #[test]
    fn test_grid_bounds() {
        let grid = GridSquare::new("FN31").unwrap();
        let ((sw_lat, sw_lon), (ne_lat, ne_lon)) = grid.bounds().unwrap();
        
        assert!(sw_lat < ne_lat);
        assert!(sw_lon < ne_lon);
        assert_relative_eq!(ne_lat - sw_lat, 1.0, epsilon = 0.01); // 1 degree latitude
        assert_relative_eq!(ne_lon - sw_lon, 2.0, epsilon = 0.01); // 2 degrees longitude
    }
    
    #[test]
    fn test_contains() {
        let grid = GridSquare::new("FN31").unwrap();
        let (center_lat, center_lon) = grid.to_coordinates().unwrap();
        
        assert!(grid.contains(center_lat, center_lon).unwrap());
        
        // Test point outside
        assert!(!grid.contains(center_lat + 2.0, center_lon).unwrap());
    }
    
    #[test]
    fn test_adjacent_squares() {
        let grid = GridSquare::new("FN31").unwrap();
        let adjacent = grid.adjacent_squares().unwrap();
        
        assert_eq!(adjacent.len(), 8); // 8 surrounding squares
        
        // All adjacent squares should be different
        for (i, grid1) in adjacent.iter().enumerate() {
            for (j, grid2) in adjacent.iter().enumerate() {
                if i != j {
                    assert_ne!(grid1, grid2);
                }
            }
        }
    }
    
    #[test]
    fn test_precision_conversion() {
        let grid = GridSquare::new("FN31pr12").unwrap();
        assert_eq!(grid.precision(), GridPrecision::ExtendedSquare);
        
        let reduced = grid.reduce_precision(GridPrecision::Square).unwrap();
        assert_eq!(reduced.grid(), "FN31");
        assert_eq!(reduced.precision(), GridPrecision::Square);
        
        let extended = reduced.extend_precision(GridPrecision::Subsquare).unwrap();
        assert_eq!(extended.precision(), GridPrecision::Subsquare);
        assert_eq!(extended.grid().len(), 6);
    }
    
    #[test]
    fn test_grid_precision_resolution() {
        let field_res = GridPrecision::Field.resolution_degrees();
        assert_eq!(field_res, (20.0, 10.0));
        
        let square_res = GridPrecision::Square.resolution_degrees();
        assert_eq!(square_res, (2.0, 1.0));
        
        let subsquare_res = GridPrecision::Subsquare.resolution_degrees();
        assert_relative_eq!(subsquare_res.0, 5.0 / 60.0, epsilon = 0.001);
        assert_relative_eq!(subsquare_res.1, 2.5 / 60.0, epsilon = 0.001);
    }
    
    #[test]
    fn test_parse_grids_from_text() {
        let text = "QRT FN31pr 73 GL";
        let grids = parse_grids_from_text(text);
        
        assert_eq!(grids.len(), 1);
        assert_eq!(grids[0].grid(), "FN31PR");
    }
    
    #[test]
    fn test_grid_display() {
        let grid = GridSquare::new("fn31pr").unwrap();
        assert_eq!(format!("{}", grid), "FN31PR");
    }
    
    #[test]
    fn test_coordinate_edge_cases() {
        // Test coordinates at edges
        let grid_north = coordinates_to_grid(89.0, 0.0, GridPrecision::Square).unwrap();
        assert!(grid_north.starts_with("JN"));
        
        let grid_south = coordinates_to_grid(-89.0, 0.0, GridPrecision::Square).unwrap();
        assert!(grid_south.starts_with("JA"));
        
        let grid_west = coordinates_to_grid(0.0, -179.0, GridPrecision::Square).unwrap();
        assert!(grid_west.starts_with("AK"));
        
        let grid_east = coordinates_to_grid(0.0, 179.0, GridPrecision::Square).unwrap();
        assert!(grid_east.starts_with("RK"));
    }
}