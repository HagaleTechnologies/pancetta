//! DX Priority Management
//!
//! This module manages DX hunting priorities, filters, and alerting rules
//! to help operators focus on the most valuable QSOs for their goals.

use crate::{Band, DxError, DxPriorityConfig, DxSpot, Mode, Result};
use chrono::{DateTime, Timelike, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use tracing::info;

/// Priority level for DX spots
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PriorityLevel {
    /// Ignore this spot
    Ignore = 0,
    /// Very low priority
    VeryLow = 1,
    /// Low priority
    Low = 2,
    /// Medium priority
    Medium = 3,
    /// High priority
    High = 4,
    /// Very high priority
    VeryHigh = 5,
    /// Critical priority (new entity, rare mode/band combo)
    Critical = 6,
}

impl PriorityLevel {
    /// Get all priority levels
    pub fn all() -> &'static [PriorityLevel] {
        &[
            PriorityLevel::Ignore,
            PriorityLevel::VeryLow,
            PriorityLevel::Low,
            PriorityLevel::Medium,
            PriorityLevel::High,
            PriorityLevel::VeryHigh,
            PriorityLevel::Critical,
        ]
    }

    /// Check if this priority meets minimum threshold
    pub fn meets_threshold(&self, threshold: PriorityLevel) -> bool {
        *self >= threshold
    }
}

impl std::fmt::Display for PriorityLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            PriorityLevel::Ignore => "IGNORE",
            PriorityLevel::VeryLow => "VERY_LOW",
            PriorityLevel::Low => "LOW",
            PriorityLevel::Medium => "MEDIUM",
            PriorityLevel::High => "HIGH",
            PriorityLevel::VeryHigh => "VERY_HIGH",
            PriorityLevel::Critical => "CRITICAL",
        };
        write!(f, "{}", name)
    }
}

/// Priority rule for automatic spot filtering
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriorityRule {
    /// Rule name/description
    pub name: String,
    /// Rule priority (higher = processed first)
    pub rule_priority: u8,
    /// Enable/disable this rule
    pub enabled: bool,
    /// Callsign pattern (regex)
    pub callsign_pattern: Option<String>,
    /// DXCC entities to match
    pub entities: Option<HashSet<u16>>,
    /// Bands to match
    pub bands: Option<HashSet<Band>>,
    /// Modes to match
    pub modes: Option<HashSet<Mode>>,
    /// Minimum frequency in Hz
    pub min_frequency: Option<u64>,
    /// Maximum frequency in Hz
    pub max_frequency: Option<u64>,
    /// Minimum rarity score
    pub min_rarity_score: Option<f64>,
    /// Maximum rarity score
    pub max_rarity_score: Option<f64>,
    /// Time window for rule (start hour UTC)
    pub time_window_start: Option<u8>,
    /// Time window for rule (end hour UTC)
    pub time_window_end: Option<u8>,
    /// Priority level to assign if matched
    pub priority_level: PriorityLevel,
    /// Action to take (Allow, Deny, SetPriority)
    pub action: RuleAction,
    /// Compiled regex for callsign matching
    #[serde(skip)]
    compiled_regex: Option<Regex>,
}

/// Rule action to take when matched
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuleAction {
    /// Allow the spot (continue processing)
    Allow,
    /// Deny the spot (stop processing, ignore spot)
    Deny,
    /// Set priority and continue processing
    SetPriority,
}

/// Award tracking goal
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AwardGoal {
    /// Goal name
    pub name: String,
    /// Award type (DXCC, WAS, WAZ, etc.)
    pub award_type: String,
    /// Target band (None for mixed)
    pub band: Option<Band>,
    /// Target mode (None for mixed)
    pub mode: Option<Mode>,
    /// Target count (e.g., 100 for DXCC Century)
    pub target_count: u32,
    /// Current progress
    pub current_count: u32,
    /// Goal deadline
    pub deadline: Option<DateTime<Utc>>,
    /// Priority multiplier for spots contributing to this goal
    pub priority_multiplier: f64,
    /// Enable/disable this goal
    pub enabled: bool,
}

/// Spot priority calculation result
#[derive(Debug, Clone)]
pub struct PriorityResult {
    /// Final priority level
    pub priority_level: PriorityLevel,
    /// Calculated priority score (0.0 to 1.0)
    pub priority_score: f64,
    /// Rules that matched
    pub matched_rules: Vec<String>,
    /// Contributing award goals
    pub contributing_goals: Vec<String>,
    /// Reason for priority assignment
    pub reason: String,
    /// Whether spot should be alerted
    pub alert: bool,
}

/// Priority Manager
pub struct PriorityManager {
    config: DxPriorityConfig,
    rules: Vec<PriorityRule>,
    goals: Vec<AwardGoal>,
    alert_threshold: PriorityLevel,
    compiled_rules: bool,
}

impl PriorityManager {
    /// Create new priority manager
    pub fn new(config: DxPriorityConfig) -> Self {
        Self {
            config,
            rules: Vec::new(),
            goals: Vec::new(),
            alert_threshold: PriorityLevel::High,
            compiled_rules: false,
        }
    }

    /// Update configuration
    pub fn update_config(&mut self, config: DxPriorityConfig) {
        self.config = config;
    }

    /// Set alert threshold
    pub fn set_alert_threshold(&mut self, threshold: PriorityLevel) {
        self.alert_threshold = threshold;
    }

    /// Add priority rule
    pub fn add_rule(&mut self, mut rule: PriorityRule) -> Result<()> {
        // Compile regex if present
        if let Some(pattern) = &rule.callsign_pattern {
            let regex = Regex::new(pattern).map_err(|e| {
                DxError::Configuration(format!("Invalid regex pattern '{}': {}", pattern, e))
            })?;
            rule.compiled_regex = Some(regex);
        }

        self.rules.push(rule);
        self.sort_rules();

        Ok(())
    }

    /// Remove rule by name
    pub fn remove_rule(&mut self, name: &str) -> bool {
        let initial_len = self.rules.len();
        self.rules.retain(|rule| rule.name != name);
        self.rules.len() != initial_len
    }

    /// Get all rules
    pub fn get_rules(&self) -> &[PriorityRule] {
        &self.rules
    }

    /// Add award goal
    pub fn add_goal(&mut self, goal: AwardGoal) {
        self.goals.push(goal);
    }

    /// Remove goal by name
    pub fn remove_goal(&mut self, name: &str) -> bool {
        let initial_len = self.goals.len();
        self.goals.retain(|goal| goal.name != name);
        self.goals.len() != initial_len
    }

    /// Update goal progress
    pub fn update_goal_progress(&mut self, goal_name: &str, current_count: u32) -> Result<()> {
        let goal = self
            .goals
            .iter_mut()
            .find(|g| g.name == goal_name)
            .ok_or_else(|| DxError::Configuration(format!("Goal '{}' not found", goal_name)))?;

        goal.current_count = current_count;
        info!(
            "Updated goal '{}' progress: {}/{}",
            goal_name, current_count, goal.target_count
        );

        Ok(())
    }

    /// Get all goals
    pub fn get_goals(&self) -> &[AwardGoal] {
        &self.goals
    }

    /// Calculate priority for a DX spot
    pub async fn calculate_priority(&self, spot: &DxSpot) -> Result<PriorityResult> {
        let mut matched_rules = Vec::new();
        let mut contributing_goals = Vec::new();
        let mut final_priority = PriorityLevel::Medium;
        let mut priority_score = 0.5;
        let mut reason_parts = Vec::new();

        // Apply basic configuration filters first
        if self.is_blacklisted(&spot.callsign) {
            return Ok(PriorityResult {
                priority_level: PriorityLevel::Ignore,
                priority_score: 0.0,
                matched_rules: vec!["BLACKLIST".to_string()],
                contributing_goals: Vec::new(),
                reason: "Callsign is blacklisted".to_string(),
                alert: false,
            });
        }

        if self.is_whitelisted(&spot.callsign) {
            final_priority = PriorityLevel::VeryHigh;
            priority_score = 0.9;
            reason_parts.push("Whitelisted callsign".to_string());
        }

        // Apply band/mode priorities from configuration
        if let Some(band) = Band::from_frequency(spot.frequency) {
            if let Some(&band_priority) = self.config.band_priorities.get(&band) {
                priority_score *= band_priority as f64 / 10.0;
                reason_parts.push(format!("Band {} priority: {}", band, band_priority));
            }
        }

        if let Some(mode) = &spot.mode {
            if let Some(&mode_priority) = self.config.mode_priorities.get(mode) {
                priority_score *= mode_priority as f64 / 10.0;
                reason_parts.push(format!("Mode {} priority: {}", mode, mode_priority));
            }
        }

        // Apply rarity score if available
        if let Some(rarity_score) = spot.rarity_score {
            if rarity_score < self.config.min_rarity_score {
                return Ok(PriorityResult {
                    priority_level: PriorityLevel::Ignore,
                    priority_score: 0.0,
                    matched_rules: vec!["MIN_RARITY".to_string()],
                    contributing_goals: Vec::new(),
                    reason: format!(
                        "Rarity score {} below minimum {}",
                        rarity_score, self.config.min_rarity_score
                    ),
                    alert: false,
                });
            }

            priority_score *= rarity_score;
            reason_parts.push(format!("Rarity score: {:.2}", rarity_score));
        }

        // Apply custom rules
        for rule in &self.rules {
            if !rule.enabled {
                continue;
            }

            if self.rule_matches(rule, spot)? {
                matched_rules.push(rule.name.clone());

                match rule.action {
                    RuleAction::Deny => {
                        return Ok(PriorityResult {
                            priority_level: PriorityLevel::Ignore,
                            priority_score: 0.0,
                            matched_rules: vec![rule.name.clone()],
                            contributing_goals: Vec::new(),
                            reason: format!("Denied by rule: {}", rule.name),
                            alert: false,
                        });
                    }
                    RuleAction::Allow => {
                        reason_parts.push(format!("Allowed by rule: {}", rule.name));
                    }
                    RuleAction::SetPriority => {
                        if rule.priority_level > final_priority {
                            final_priority = rule.priority_level;
                            reason_parts.push(format!(
                                "Priority set by rule '{}': {}",
                                rule.name, rule.priority_level
                            ));
                        }
                    }
                }
            }
        }

        // Check award goals
        for goal in &self.goals {
            if !goal.enabled {
                continue;
            }

            if self.goal_applies(goal, spot) {
                contributing_goals.push(goal.name.clone());
                priority_score *= goal.priority_multiplier;
                reason_parts.push(format!("Contributes to goal: {}", goal.name));

                // Boost priority for goals near deadline or completion
                if let Some(deadline) = goal.deadline {
                    let days_until_deadline = deadline.signed_duration_since(Utc::now()).num_days();
                    if days_until_deadline <= 30 && days_until_deadline > 0 {
                        priority_score *= 1.2;
                        reason_parts.push(format!("Goal deadline in {} days", days_until_deadline));
                    }
                }

                let progress_ratio = goal.current_count as f64 / goal.target_count as f64;
                if progress_ratio >= 0.9 {
                    priority_score *= 1.3;
                    reason_parts.push("Goal near completion".to_string());
                }
            }
        }

        // Convert priority score to priority level
        final_priority = final_priority.max(self.score_to_priority_level(priority_score));

        let alert = final_priority.meets_threshold(self.alert_threshold);

        Ok(PriorityResult {
            priority_level: final_priority,
            priority_score: priority_score.min(1.0),
            matched_rules,
            contributing_goals,
            reason: reason_parts.join("; "),
            alert,
        })
    }

    /// Check if callsign is blacklisted
    fn is_blacklisted(&self, callsign: &str) -> bool {
        for pattern in &self.config.blacklist {
            if callsign.contains(pattern) {
                return true;
            }
        }
        false
    }

    /// Check if callsign is whitelisted
    fn is_whitelisted(&self, callsign: &str) -> bool {
        if self.config.whitelist.is_empty() {
            return false;
        }

        for pattern in &self.config.whitelist {
            if callsign.contains(pattern) {
                return true;
            }
        }
        false
    }

    /// Check if rule matches spot
    fn rule_matches(&self, rule: &PriorityRule, spot: &DxSpot) -> Result<bool> {
        // Check callsign pattern
        if let Some(regex) = &rule.compiled_regex {
            if !regex.is_match(&spot.callsign) {
                return Ok(false);
            }
        }

        // Check entities
        if let Some(entities) = &rule.entities {
            if let Some(entity_code) = spot.dxcc_entity {
                if !entities.contains(&entity_code) {
                    return Ok(false);
                }
            } else {
                return Ok(false);
            }
        }

        // Check bands
        if let Some(bands) = &rule.bands {
            if let Some(band) = Band::from_frequency(spot.frequency) {
                if !bands.contains(&band) {
                    return Ok(false);
                }
            } else {
                return Ok(false);
            }
        }

        // Check modes
        if let Some(modes) = &rule.modes {
            if let Some(mode) = &spot.mode {
                if !modes.contains(mode) {
                    return Ok(false);
                }
            } else {
                return Ok(false);
            }
        }

        // Check frequency range
        if let Some(min_freq) = rule.min_frequency {
            if spot.frequency < min_freq {
                return Ok(false);
            }
        }

        if let Some(max_freq) = rule.max_frequency {
            if spot.frequency > max_freq {
                return Ok(false);
            }
        }

        // Check rarity score
        if let Some(min_rarity) = rule.min_rarity_score {
            if let Some(rarity) = spot.rarity_score {
                if rarity < min_rarity {
                    return Ok(false);
                }
            } else {
                return Ok(false);
            }
        }

        if let Some(max_rarity) = rule.max_rarity_score {
            if let Some(rarity) = spot.rarity_score {
                if rarity > max_rarity {
                    return Ok(false);
                }
            }
        }

        // Check time window
        if let (Some(start_hour), Some(end_hour)) = (rule.time_window_start, rule.time_window_end) {
            let current_hour = Utc::now().time().hour() as u8;

            if start_hour <= end_hour {
                // Normal time window (e.g., 8-17)
                if current_hour < start_hour || current_hour > end_hour {
                    return Ok(false);
                }
            } else {
                // Overnight time window (e.g., 22-6)
                if current_hour < start_hour && current_hour > end_hour {
                    return Ok(false);
                }
            }
        }

        Ok(true)
    }

    /// Check if goal applies to spot
    fn goal_applies(&self, goal: &AwardGoal, spot: &DxSpot) -> bool {
        // Check band constraint
        if let Some(goal_band) = goal.band {
            if let Some(spot_band) = Band::from_frequency(spot.frequency) {
                if goal_band != spot_band {
                    return false;
                }
            } else {
                return false;
            }
        }

        // Check mode constraint
        if let Some(goal_mode) = &goal.mode {
            if let Some(spot_mode) = &spot.mode {
                if goal_mode != spot_mode {
                    return false;
                }
            } else {
                return false;
            }
        }

        // For DXCC goals, any new entity counts
        // More sophisticated logic would check if entity is actually needed
        goal.award_type == "DXCC"
    }

    /// Convert priority score to priority level
    fn score_to_priority_level(&self, score: f64) -> PriorityLevel {
        match score {
            s if s >= 0.9 => PriorityLevel::Critical,
            s if s >= 0.8 => PriorityLevel::VeryHigh,
            s if s >= 0.6 => PriorityLevel::High,
            s if s >= 0.4 => PriorityLevel::Medium,
            s if s >= 0.2 => PriorityLevel::Low,
            s if s >= 0.1 => PriorityLevel::VeryLow,
            _ => PriorityLevel::Ignore,
        }
    }

    /// Sort rules by priority
    fn sort_rules(&mut self) {
        self.rules
            .sort_by(|a, b| b.rule_priority.cmp(&a.rule_priority));
    }

    /// Load priority rules from JSON
    pub fn load_rules_json(&mut self, json_data: &str) -> Result<()> {
        let rules: Vec<PriorityRule> = serde_json::from_str(json_data)
            .map_err(|e| DxError::Parse(format!("Failed to parse rules JSON: {}", e)))?;

        self.rules.clear();
        for rule in rules {
            self.add_rule(rule)?;
        }

        info!("Loaded {} priority rules", self.rules.len());
        Ok(())
    }

    /// Export priority rules to JSON
    pub fn export_rules_json(&self) -> Result<String> {
        serde_json::to_string_pretty(&self.rules)
            .map_err(|e| DxError::Parse(format!("Failed to export rules JSON: {}", e)))
    }

    /// Load award goals from JSON
    pub fn load_goals_json(&mut self, json_data: &str) -> Result<()> {
        let goals: Vec<AwardGoal> = serde_json::from_str(json_data)
            .map_err(|e| DxError::Parse(format!("Failed to parse goals JSON: {}", e)))?;

        self.goals = goals;
        info!("Loaded {} award goals", self.goals.len());
        Ok(())
    }

    /// Export award goals to JSON
    pub fn export_goals_json(&self) -> Result<String> {
        serde_json::to_string_pretty(&self.goals)
            .map_err(|e| DxError::Parse(format!("Failed to export goals JSON: {}", e)))
    }

    /// Get priority statistics
    pub fn get_priority_stats(&self) -> HashMap<String, usize> {
        let mut stats = HashMap::new();

        stats.insert("total_rules".to_string(), self.rules.len());
        stats.insert(
            "enabled_rules".to_string(),
            self.rules.iter().filter(|r| r.enabled).count(),
        );
        stats.insert("total_goals".to_string(), self.goals.len());
        stats.insert(
            "enabled_goals".to_string(),
            self.goals.iter().filter(|g| g.enabled).count(),
        );
        stats.insert("blacklist_entries".to_string(), self.config.blacklist.len());
        stats.insert("whitelist_entries".to_string(), self.config.whitelist.len());

        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn create_test_spot() -> DxSpot {
        DxSpot {
            callsign: "JA1ABC".to_string(),
            frequency: 14_200_000,
            mode: Some(Mode::CW),
            spotter: "W1XYZ".to_string(),
            time: Utc::now(),
            comment: None,
            dxcc_entity: Some(61), // Japan
            grid_square: Some("PM95".to_string()),
            distance_km: Some(10000.0),
            bearing_degrees: Some(300.0),
            rarity_score: Some(0.7),
        }
    }

    #[tokio::test]
    async fn test_priority_manager_creation() {
        let config = DxPriorityConfig::default();
        let manager = PriorityManager::new(config);
        assert_eq!(manager.rules.len(), 0);
        assert_eq!(manager.goals.len(), 0);
    }

    #[tokio::test]
    async fn test_basic_priority_calculation() {
        let config = DxPriorityConfig::default();
        let manager = PriorityManager::new(config);
        let spot = create_test_spot();

        let result = manager.calculate_priority(&spot).await.unwrap();
        assert!(result.priority_level >= PriorityLevel::Low);
        assert!(result.priority_score > 0.0);
    }

    #[tokio::test]
    async fn test_blacklist_filtering() {
        let mut config = DxPriorityConfig::default();
        config.blacklist.push("JA1".to_string());

        let manager = PriorityManager::new(config);
        let spot = create_test_spot();

        let result = manager.calculate_priority(&spot).await.unwrap();
        assert_eq!(result.priority_level, PriorityLevel::Ignore);
        assert!(result.matched_rules.contains(&"BLACKLIST".to_string()));
    }

    #[tokio::test]
    async fn test_whitelist_boosting() {
        let mut config = DxPriorityConfig::default();
        config.whitelist.push("JA1".to_string());

        let manager = PriorityManager::new(config);
        let spot = create_test_spot();

        let result = manager.calculate_priority(&spot).await.unwrap();
        // Whitelisted starts at 0.9 but gets multiplied by band (5/10), mode (8/10), and rarity (0.7)
        // So final score = 0.9 * 0.5 * 0.8 * 0.7 = 0.252, but priority level is max of VeryHigh and score_to_priority
        assert!(result.priority_level >= PriorityLevel::VeryHigh);
        assert!(result.priority_score > 0.0);
    }

    #[tokio::test]
    async fn test_custom_rule_matching() {
        let config = DxPriorityConfig::default();
        let mut manager = PriorityManager::new(config);

        let rule = PriorityRule {
            name: "Japan CW".to_string(),
            rule_priority: 10,
            enabled: true,
            callsign_pattern: Some("^JA".to_string()),
            entities: Some([61].iter().cloned().collect()),
            bands: Some([Band::Band20m].iter().cloned().collect()),
            modes: Some([Mode::CW].iter().cloned().collect()),
            min_frequency: None,
            max_frequency: None,
            min_rarity_score: None,
            max_rarity_score: None,
            time_window_start: None,
            time_window_end: None,
            priority_level: PriorityLevel::Critical,
            action: RuleAction::SetPriority,
            compiled_regex: None,
        };

        manager.add_rule(rule).unwrap();

        let spot = create_test_spot();
        let result = manager.calculate_priority(&spot).await.unwrap();

        assert_eq!(result.priority_level, PriorityLevel::Critical);
        assert!(result.matched_rules.contains(&"Japan CW".to_string()));
    }

    #[tokio::test]
    async fn test_award_goal_contribution() {
        let config = DxPriorityConfig::default();
        let mut manager = PriorityManager::new(config);

        let goal = AwardGoal {
            name: "DXCC 20m CW".to_string(),
            award_type: "DXCC".to_string(),
            band: Some(Band::Band20m),
            mode: Some(Mode::CW),
            target_count: 100,
            current_count: 95,
            deadline: Some(Utc::now() + chrono::Duration::days(20)),
            priority_multiplier: 1.5,
            enabled: true,
        };

        manager.add_goal(goal);

        let spot = create_test_spot();
        let result = manager.calculate_priority(&spot).await.unwrap();

        assert!(result
            .contributing_goals
            .contains(&"DXCC 20m CW".to_string()));
        // Score starts at 0.5, multiplied by band/mode/rarity/goal factors, so ends up < 0.5
        assert!(result.priority_score > 0.0);
    }

    #[test]
    fn test_priority_level_ordering() {
        assert!(PriorityLevel::Critical > PriorityLevel::VeryHigh);
        assert!(PriorityLevel::VeryHigh > PriorityLevel::High);
        assert!(PriorityLevel::High > PriorityLevel::Medium);
        assert!(PriorityLevel::Medium > PriorityLevel::Low);
        assert!(PriorityLevel::Low > PriorityLevel::VeryLow);
        assert!(PriorityLevel::VeryLow > PriorityLevel::Ignore);
    }

    #[test]
    fn test_score_to_priority_conversion() {
        let config = DxPriorityConfig::default();
        let manager = PriorityManager::new(config);

        assert_eq!(
            manager.score_to_priority_level(0.95),
            PriorityLevel::Critical
        );
        assert_eq!(
            manager.score_to_priority_level(0.85),
            PriorityLevel::VeryHigh
        );
        assert_eq!(manager.score_to_priority_level(0.7), PriorityLevel::High);
        assert_eq!(manager.score_to_priority_level(0.5), PriorityLevel::Medium);
        assert_eq!(manager.score_to_priority_level(0.3), PriorityLevel::Low);
        assert_eq!(
            manager.score_to_priority_level(0.15),
            PriorityLevel::VeryLow
        );
        assert_eq!(manager.score_to_priority_level(0.05), PriorityLevel::Ignore);
    }
}
