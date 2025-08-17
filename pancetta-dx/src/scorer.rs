//! Rarity Scoring Algorithm
//!
//! This module provides sophisticated algorithms for calculating the rarity
//! and desirability of DX entities, bands, and modes for prioritization.

use crate::{tracker::DxTracker, Band, Mode, DxError, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use statrs::statistics::Statistics;
use std::collections::HashMap;
use tracing::info;

/// Rarity scoring configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoringConfig {
    /// Weight for entity rarity (0.0 to 1.0)
    pub entity_weight: f64,
    /// Weight for band rarity (0.0 to 1.0)
    pub band_weight: f64,
    /// Weight for mode rarity (0.0 to 1.0)
    pub mode_weight: f64,
    /// Weight for recency (0.0 to 1.0)
    pub recency_weight: f64,
    /// Weight for propagation favorability (0.0 to 1.0)
    pub propagation_weight: f64,
    /// Days to consider for recency calculation
    pub recency_days: i64,
    /// Minimum activity threshold for scoring
    pub min_activity_threshold: u32,
    /// Apply logarithmic scaling to QSO counts
    pub logarithmic_scaling: bool,
}

impl Default for ScoringConfig {
    fn default() -> Self {
        Self {
            entity_weight: 0.4,
            band_weight: 0.2,
            mode_weight: 0.15,
            recency_weight: 0.15,
            propagation_weight: 0.1,
            recency_days: 365,
            min_activity_threshold: 10,
            logarithmic_scaling: true,
        }
    }
}

/// Statistical data for rarity calculations
#[derive(Debug, Clone)]
struct RarityStats {
    /// Total QSOs for this entity
    total_qsos: u32,
    /// QSOs in the last year
    recent_qsos: u32,
    /// QSOs per band
    band_qsos: HashMap<Band, u32>,
    /// QSOs per mode
    mode_qsos: HashMap<Mode, u32>,
    /// Last QSO date
    last_qso: Option<DateTime<Utc>>,
    /// Average QSOs per day (recent period)
    activity_rate: f64,
}

/// Calculated rarity metrics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RarityMetrics {
    /// Overall rarity score (0.0 to 1.0, higher = rarer)
    pub overall_score: f64,
    /// Entity rarity component
    pub entity_score: f64,
    /// Band rarity component
    pub band_score: f64,
    /// Mode rarity component
    pub mode_score: f64,
    /// Recency component (higher = less recent activity)
    pub recency_score: f64,
    /// Propagation favorability (higher = better conditions)
    pub propagation_score: f64,
    /// Activity level classification
    pub activity_level: ActivityLevel,
    /// Percentile ranking among all entities
    pub percentile_rank: f64,
}

/// Activity level classification
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivityLevel {
    /// Very rare (top 5%)
    VeryRare,
    /// Rare (top 25%)
    Rare,
    /// Uncommon (top 50%)
    Uncommon,
    /// Common (bottom 50%)
    Common,
    /// Very common (bottom 25%)
    VeryCommon,
    /// No recent activity
    Inactive,
}

/// Rarity scorer engine
pub struct RarityScorer {
    config: ScoringConfig,
    entity_stats: HashMap<u16, RarityStats>,
    global_stats: GlobalStats,
    last_update: DateTime<Utc>,
}

/// Global statistics for normalization
#[derive(Debug, Clone)]
struct GlobalStats {
    /// Total entities with activity
    active_entities: u32,
    /// Distribution of QSO counts per entity
    qso_distribution: Vec<u32>,
    /// Mean QSOs per entity
    mean_qsos: f64,
    /// Standard deviation of QSOs per entity
    stddev_qsos: f64,
    /// Median QSOs per entity
    median_qsos: f64,
    /// 95th percentile QSO count
    p95_qsos: f64,
}

impl RarityScorer {
    /// Create new rarity scorer
    pub async fn new(tracker: &DxTracker) -> Result<Self> {
        let mut scorer = Self {
            config: ScoringConfig::default(),
            entity_stats: HashMap::new(),
            global_stats: GlobalStats {
                active_entities: 0,
                qso_distribution: Vec::new(),
                mean_qsos: 0.0,
                stddev_qsos: 0.0,
                median_qsos: 0.0,
                p95_qsos: 0.0,
            },
            last_update: Utc::now(),
        };
        
        scorer.update_statistics(tracker).await?;
        Ok(scorer)
    }
    
    /// Create scorer with custom configuration
    pub async fn new_with_config(tracker: &DxTracker, config: ScoringConfig) -> Result<Self> {
        let mut scorer = Self {
            config,
            entity_stats: HashMap::new(),
            global_stats: GlobalStats {
                active_entities: 0,
                qso_distribution: Vec::new(),
                mean_qsos: 0.0,
                stddev_qsos: 0.0,
                median_qsos: 0.0,
                p95_qsos: 0.0,
            },
            last_update: Utc::now(),
        };
        
        scorer.update_statistics(tracker).await?;
        Ok(scorer)
    }
    
    /// Update scoring configuration
    pub fn update_config(&mut self, config: ScoringConfig) {
        self.config = config;
    }
    
    /// Calculate rarity score for an entity/band/mode combination
    pub async fn calculate_rarity_score(
        &self,
        entity_code: u16,
        band: Option<Band>,
        mode: Option<&Mode>,
    ) -> Result<f64> {
        let metrics = self.calculate_rarity_metrics(entity_code, band, mode).await?;
        Ok(metrics.overall_score)
    }
    
    /// Calculate detailed rarity metrics
    pub async fn calculate_rarity_metrics(
        &self,
        entity_code: u16,
        band: Option<Band>,
        mode: Option<&Mode>,
    ) -> Result<RarityMetrics> {
        let entity_stats = self.entity_stats.get(&entity_code)
            .ok_or_else(|| DxError::DxccNotFound(format!("No statistics for entity {}", entity_code)))?;
        
        // Calculate individual component scores
        let entity_score = self.calculate_entity_rarity_score(entity_stats);
        let band_score = self.calculate_band_rarity_score(entity_stats, band);
        let mode_score = self.calculate_mode_rarity_score(entity_stats, mode);
        let recency_score = self.calculate_recency_score(entity_stats);
        let propagation_score = self.calculate_propagation_score(entity_code, band).await;
        
        // Combine scores using configured weights
        let overall_score = 
            entity_score * self.config.entity_weight +
            band_score * self.config.band_weight +
            mode_score * self.config.mode_weight +
            recency_score * self.config.recency_weight +
            propagation_score * self.config.propagation_weight;
        
        // Determine activity level
        let activity_level = self.classify_activity_level(entity_stats);
        
        // Calculate percentile rank
        let percentile_rank = self.calculate_percentile_rank(entity_stats.total_qsos);
        
        Ok(RarityMetrics {
            overall_score,
            entity_score,
            band_score,
            mode_score,
            recency_score,
            propagation_score,
            activity_level,
            percentile_rank,
        })
    }
    
    /// Update statistics from QSO database
    pub async fn update_statistics(&mut self, tracker: &DxTracker) -> Result<()> {
        info!("Updating rarity scoring statistics");
        
        self.entity_stats.clear();
        let cutoff_date = Utc::now() - Duration::days(self.config.recency_days);
        
        // Get QSO statistics per entity
        let qso_stats = tracker.get_qso_statistics_by_entity().await?;
        let recent_qso_stats = tracker.get_qso_statistics_by_entity_since(cutoff_date).await?;
        
        // Build entity statistics
        for (entity_code, total_qsos) in qso_stats {
            let recent_qsos = recent_qso_stats.get(&entity_code).copied().unwrap_or(0);
            let band_qsos = tracker.get_qso_statistics_by_band(entity_code).await?;
            let mode_qsos = tracker.get_qso_statistics_by_mode(entity_code).await?;
            let last_qso = tracker.get_last_qso_date(entity_code).await?;
            
            let activity_rate = if self.config.recency_days > 0 {
                recent_qsos as f64 / self.config.recency_days as f64
            } else {
                0.0
            };
            
            let stats = RarityStats {
                total_qsos,
                recent_qsos,
                band_qsos,
                mode_qsos,
                last_qso,
                activity_rate,
            };
            
            self.entity_stats.insert(entity_code, stats);
        }
        
        // Calculate global statistics for normalization
        self.update_global_statistics();
        
        self.last_update = Utc::now();
        info!("Updated statistics for {} entities", self.entity_stats.len());
        
        Ok(())
    }
    
    /// Update global statistics for normalization
    fn update_global_statistics(&mut self) {
        let qso_counts: Vec<u32> = self.entity_stats.values()
            .map(|stats| stats.total_qsos)
            .collect();
        
        if qso_counts.is_empty() {
            return;
        }
        
        let qso_counts_f64: Vec<f64> = qso_counts.iter().map(|&x| x as f64).collect();
        
        let mean_qsos = Self::calculate_mean(&qso_counts_f64);
        let stddev_qsos = Self::calculate_std_dev(&qso_counts_f64, mean_qsos);
        
        self.global_stats = GlobalStats {
            active_entities: qso_counts.len() as u32,
            qso_distribution: qso_counts.clone(),
            mean_qsos,
            stddev_qsos,
            median_qsos: Self::calculate_median(&qso_counts_f64),
            p95_qsos: Self::calculate_percentile(&qso_counts_f64, 95.0),
        };
    }
    
    /// Calculate entity rarity score based on total activity
    fn calculate_entity_rarity_score(&self, stats: &RarityStats) -> f64 {
        if self.global_stats.active_entities == 0 {
            return 0.5; // Default neutral score
        }
        
        let qso_count = if self.config.logarithmic_scaling {
            (stats.total_qsos as f64 + 1.0).ln()
        } else {
            stats.total_qsos as f64
        };
        
        let reference_count = if self.config.logarithmic_scaling {
            (self.global_stats.mean_qsos + 1.0).ln()
        } else {
            self.global_stats.mean_qsos
        };
        
        // Invert score so that lower activity = higher rarity
        let normalized = 1.0 - (qso_count / (reference_count * 2.0)).min(1.0);
        normalized.max(0.0)
    }
    
    /// Calculate band-specific rarity score
    fn calculate_band_rarity_score(&self, stats: &RarityStats, band: Option<Band>) -> f64 {
        let Some(band) = band else {
            return 0.5; // Neutral score if no band specified
        };
        
        let band_qsos = stats.band_qsos.get(&band).copied().unwrap_or(0) as f64;
        let total_qsos = stats.total_qsos as f64;
        
        if total_qsos == 0.0 {
            return 1.0; // Maximum rarity if no QSOs
        }
        
        // Calculate band activity ratio
        let band_ratio = band_qsos / total_qsos;
        
        // Invert ratio for rarity score (less activity on band = higher rarity)
        1.0 - band_ratio
    }
    
    /// Calculate mode-specific rarity score
    fn calculate_mode_rarity_score(&self, stats: &RarityStats, mode: Option<&Mode>) -> f64 {
        let Some(mode) = mode else {
            return 0.5; // Neutral score if no mode specified
        };
        
        let mode_qsos = stats.mode_qsos.get(mode).copied().unwrap_or(0) as f64;
        let total_qsos = stats.total_qsos as f64;
        
        if total_qsos == 0.0 {
            return 1.0; // Maximum rarity if no QSOs
        }
        
        // Calculate mode activity ratio
        let mode_ratio = mode_qsos / total_qsos;
        
        // Invert ratio for rarity score (less activity on mode = higher rarity)
        1.0 - mode_ratio
    }
    
    /// Calculate recency score (higher = less recent activity)
    fn calculate_recency_score(&self, stats: &RarityStats) -> f64 {
        let Some(last_qso) = stats.last_qso else {
            return 1.0; // Maximum score if never worked
        };
        
        let days_since = Utc::now().signed_duration_since(last_qso).num_days();
        let max_days = self.config.recency_days;
        
        if days_since <= 0 {
            return 0.0; // Recent activity
        }
        
        if days_since >= max_days {
            return 1.0; // Very old or no activity
        }
        
        // Linear scaling based on days since last QSO
        days_since as f64 / max_days as f64
    }
    
    /// Calculate propagation score (placeholder - would integrate with propagation prediction)
    async fn calculate_propagation_score(&self, _entity_code: u16, _band: Option<Band>) -> f64 {
        // This would integrate with actual propagation prediction
        // For now, return neutral score
        0.5
    }
    
    /// Classify activity level based on statistics
    fn classify_activity_level(&self, stats: &RarityStats) -> ActivityLevel {
        if stats.total_qsos == 0 {
            return ActivityLevel::Inactive;
        }
        
        let percentile = self.calculate_percentile_rank(stats.total_qsos);
        
        match percentile {
            p if p >= 95.0 => ActivityLevel::VeryCommon,
            p if p >= 75.0 => ActivityLevel::Common,
            p if p >= 50.0 => ActivityLevel::Uncommon,
            p if p >= 25.0 => ActivityLevel::Rare,
            _ => ActivityLevel::VeryRare,
        }
    }
    
    /// Calculate percentile rank for QSO count
    fn calculate_percentile_rank(&self, qso_count: u32) -> f64 {
        if self.global_stats.qso_distribution.is_empty() {
            return 50.0;
        }
        
        let lower_count = self.global_stats.qso_distribution.iter()
            .filter(|&&count| count < qso_count)
            .count();
        
        let equal_count = self.global_stats.qso_distribution.iter()
            .filter(|&&count| count == qso_count)
            .count();
        
        let total_count = self.global_stats.qso_distribution.len();
        
        if total_count == 0 {
            return 50.0;
        }
        
        // Calculate percentile using midrank method
        ((lower_count as f64 + equal_count as f64 / 2.0) / total_count as f64) * 100.0
    }
    
    /// Get top rarest entities
    pub fn get_rarest_entities(&self, limit: usize) -> Vec<(u16, f64)> {
        let mut entities: Vec<(u16, f64)> = self.entity_stats.iter()
            .map(|(&entity_code, stats)| {
                let score = self.calculate_entity_rarity_score(stats);
                (entity_code, score)
            })
            .collect();
        
        entities.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        entities.truncate(limit);
        entities
    }
    
    /// Get entities needing specific band/mode combinations
    pub async fn get_needed_combinations(
        &self,
        entity_code: u16,
        bands: &[Band],
        modes: &[Mode],
    ) -> Result<Vec<(Band, Mode, f64)>> {
        let mut combinations = Vec::new();
        
        for &band in bands {
            for mode in modes {
                let score = self.calculate_rarity_score(entity_code, Some(band), Some(mode)).await?;
                combinations.push((band, mode.clone(), score));
            }
        }
        
        // Sort by rarity score (highest first)
        combinations.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        
        Ok(combinations)
    }
    
    /// Get scoring statistics summary
    pub fn get_statistics_summary(&self) -> HashMap<String, f64> {
        let mut summary = HashMap::new();
        
        summary.insert("total_entities".to_string(), self.entity_stats.len() as f64);
        summary.insert("mean_qsos_per_entity".to_string(), self.global_stats.mean_qsos);
        summary.insert("median_qsos_per_entity".to_string(), self.global_stats.median_qsos);
        summary.insert("stddev_qsos_per_entity".to_string(), self.global_stats.stddev_qsos);
        summary.insert("p95_qsos_per_entity".to_string(), self.global_stats.p95_qsos);
        
        let active_entities = self.entity_stats.values()
            .filter(|stats| stats.recent_qsos > 0)
            .count();
        summary.insert("recently_active_entities".to_string(), active_entities as f64);
        
        summary
    }
    
    /// Calculate median value from a vector of f64
    fn calculate_median(values: &[f64]) -> f64 {
        if values.is_empty() {
            return 0.0;
        }
        
        let mut sorted = values.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        
        let mid = sorted.len() / 2;
        if sorted.len() % 2 == 0 {
            (sorted[mid - 1] + sorted[mid]) / 2.0
        } else {
            sorted[mid]
        }
    }
    
    /// Calculate percentile value from a vector of f64
    fn calculate_percentile(values: &[f64], percentile: f64) -> f64 {
        if values.is_empty() {
            return 0.0;
        }
        
        let mut sorted = values.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        
        let index = (percentile / 100.0 * (sorted.len() - 1) as f64).round() as usize;
        sorted[index.min(sorted.len() - 1)]
    }
    
    /// Calculate mean value from a vector of f64
    fn calculate_mean(values: &[f64]) -> f64 {
        if values.is_empty() {
            return 0.0;
        }
        values.iter().sum::<f64>() / values.len() as f64
    }
    
    /// Calculate standard deviation from a vector of f64 and its mean
    fn calculate_std_dev(values: &[f64], mean: f64) -> f64 {
        if values.len() <= 1 {
            return 0.0;
        }
        
        let variance: f64 = values.iter()
            .map(|&x| (x - mean).powi(2))
            .sum::<f64>() / (values.len() - 1) as f64;
        
        variance.sqrt()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;
    
    async fn create_test_tracker() -> DxTracker {
        let temp_file = NamedTempFile::new().unwrap();
        DxTracker::new(temp_file.path().to_str().unwrap()).await.unwrap()
    }
    
    #[tokio::test]
    async fn test_scorer_creation() {
        let tracker = create_test_tracker().await;
        let scorer = RarityScorer::new(&tracker).await.unwrap();
        assert!(scorer.entity_stats.is_empty());
    }
    
    #[tokio::test]
    async fn test_custom_config() {
        let tracker = create_test_tracker().await;
        let config = ScoringConfig {
            entity_weight: 0.5,
            band_weight: 0.3,
            ..Default::default()
        };
        let scorer = RarityScorer::new_with_config(&tracker, config).await.unwrap();
        assert_eq!(scorer.config.entity_weight, 0.5);
        assert_eq!(scorer.config.band_weight, 0.3);
    }
    
    #[test]
    fn test_activity_level_classification() {
        let scorer = RarityScorer {
            config: ScoringConfig::default(),
            entity_stats: HashMap::new(),
            global_stats: GlobalStats {
                active_entities: 100,
                qso_distribution: (1..=100).collect(),
                mean_qsos: 50.0,
                stddev_qsos: 25.0,
                median_qsos: 50.0,
                p95_qsos: 95.0,
            },
            last_update: Utc::now(),
        };
        
        let stats = RarityStats {
            total_qsos: 0,
            recent_qsos: 0,
            band_qsos: HashMap::new(),
            mode_qsos: HashMap::new(),
            last_qso: None,
            activity_rate: 0.0,
        };
        
        assert_eq!(scorer.classify_activity_level(&stats), ActivityLevel::Inactive);
        
        let stats = RarityStats {
            total_qsos: 1,
            recent_qsos: 1,
            band_qsos: HashMap::new(),
            mode_qsos: HashMap::new(),
            last_qso: Some(Utc::now()),
            activity_rate: 1.0,
        };
        
        assert_eq!(scorer.classify_activity_level(&stats), ActivityLevel::VeryRare);
    }
    
    #[test]
    fn test_percentile_calculation() {
        let scorer = RarityScorer {
            config: ScoringConfig::default(),
            entity_stats: HashMap::new(),
            global_stats: GlobalStats {
                active_entities: 10,
                qso_distribution: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
                mean_qsos: 5.5,
                stddev_qsos: 3.0,
                median_qsos: 5.5,
                p95_qsos: 9.5,
            },
            last_update: Utc::now(),
        };
        
        assert!((scorer.calculate_percentile_rank(1) - 5.0).abs() < 1.0);
        assert!((scorer.calculate_percentile_rank(5) - 45.0).abs() < 5.0);
        assert!((scorer.calculate_percentile_rank(10) - 95.0).abs() < 5.0);
    }
}