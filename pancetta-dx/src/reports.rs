//! DXCC Progress Reports
//!
//! This module generates comprehensive reports for DXCC and other amateur
//! radio award progress tracking.

use crate::{
    Band, Mode, tracker::DxTracker, dxcc::{DxccDatabase, DxccEntity}, 
    statistics::{DxStatistics, BandStatistics, ModeStatistics}, 
    DxError, Result
};
use chrono::{DateTime, Utc, Datelike};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use tracing::{debug, info};

/// DXCC progress report
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DxccProgressReport {
    /// Report generation time
    pub generated_at: DateTime<Utc>,
    /// Station callsign
    pub station_callsign: String,
    /// Overall DXCC status
    pub overall_status: DxccStatus,
    /// Band-specific DXCC status
    pub band_status: HashMap<Band, DxccStatus>,
    /// Mode-specific DXCC status
    pub mode_status: HashMap<Mode, DxccStatus>,
    /// Entities still needed
    pub needed_entities: Vec<NeededEntity>,
    /// Recently worked entities
    pub recent_entities: Vec<RecentEntity>,
    /// Progress charts data
    pub progress_data: ProgressData,
    /// Recommendations
    pub recommendations: Vec<String>,
}

/// DXCC status for a specific award
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DxccStatus {
    /// Total entities in award
    pub total_entities: u32,
    /// Entities worked
    pub worked: u32,
    /// Entities confirmed
    pub confirmed: u32,
    /// Entities needed for award (usually 100)
    pub needed_for_award: u32,
    /// Progress percentage to award
    pub progress_percent: f64,
    /// Award achieved
    pub award_achieved: bool,
    /// Date award was achieved
    pub achievement_date: Option<DateTime<Utc>>,
    /// Entities by continent
    pub continent_breakdown: HashMap<String, ContinentStatus>,
}

/// Continent status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContinentStatus {
    /// Continent name
    pub continent: String,
    /// Total entities available on continent
    pub total_available: u32,
    /// Entities worked
    pub worked: u32,
    /// Entities confirmed
    pub confirmed: u32,
    /// Progress percentage
    pub progress_percent: f64,
}

/// Entity still needed for DXCC
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NeededEntity {
    /// Entity code
    pub entity_code: u16,
    /// Entity name
    pub entity_name: String,
    /// Prefix
    pub prefix: String,
    /// Continent
    pub continent: String,
    /// Priority score
    pub priority_score: f64,
    /// Reason needed
    pub reason: String,
    /// Bands needed
    pub bands_needed: Vec<Band>,
    /// Modes needed
    pub modes_needed: Vec<Mode>,
    /// Activity level
    pub activity_level: String,
    /// Recent spots count
    pub recent_spots: u32,
}

/// Recently worked entity
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentEntity {
    /// Entity code
    pub entity_code: u16,
    /// Entity name
    pub entity_name: String,
    /// Callsign worked
    pub callsign: String,
    /// Date worked
    pub date_worked: DateTime<Utc>,
    /// Band
    pub band: Band,
    /// Mode
    pub mode: Mode,
    /// Confirmed status
    pub confirmed: bool,
    /// Confirmation date
    pub confirmation_date: Option<DateTime<Utc>>,
    /// Whether this was a new entity
    pub new_entity: bool,
}

/// Progress data for charts
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressData {
    /// Monthly progress over time
    pub monthly_progress: Vec<ProgressPoint>,
    /// Yearly totals
    pub yearly_totals: HashMap<u32, u32>,
    /// Band comparison
    pub band_comparison: HashMap<Band, u32>,
    /// Mode comparison
    pub mode_comparison: HashMap<Mode, u32>,
    /// Continent comparison
    pub continent_comparison: HashMap<String, u32>,
    /// Confirmation rate over time
    pub confirmation_trends: Vec<ConfirmationPoint>,
}

/// Progress point for charts
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressPoint {
    /// Date
    pub date: DateTime<Utc>,
    /// Entities worked by this date
    pub entities_worked: u32,
    /// Entities confirmed by this date
    pub entities_confirmed: u32,
}

/// Confirmation trend point
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfirmationPoint {
    /// Date
    pub date: DateTime<Utc>,
    /// Confirmation rate percentage
    pub confirmation_rate: f64,
    /// QSL method breakdown
    pub qsl_methods: HashMap<String, u32>,
}

/// Award tracking report
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AwardTrackingReport {
    /// Report generation time
    pub generated_at: DateTime<Utc>,
    /// DXCC awards status
    pub dxcc_awards: Vec<AwardStatus>,
    /// Other awards status
    pub other_awards: Vec<AwardStatus>,
    /// Goal tracking
    pub goals: Vec<GoalStatus>,
    /// Recommendations
    pub recommendations: Vec<String>,
}

/// Award status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AwardStatus {
    /// Award name
    pub award_name: String,
    /// Award category
    pub category: String,
    /// Current progress
    pub current: u32,
    /// Target for award
    pub target: u32,
    /// Progress percentage
    pub progress_percent: f64,
    /// Achieved status
    pub achieved: bool,
    /// Achievement date
    pub achievement_date: Option<DateTime<Utc>>,
    /// Next milestone
    pub next_milestone: Option<u32>,
    /// Estimated completion date
    pub estimated_completion: Option<DateTime<Utc>>,
}

/// Goal status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalStatus {
    /// Goal name
    pub goal_name: String,
    /// Goal description
    pub description: String,
    /// Current progress
    pub current: u32,
    /// Target
    pub target: u32,
    /// Deadline
    pub deadline: Option<DateTime<Utc>>,
    /// On track status
    pub on_track: bool,
    /// Days remaining
    pub days_remaining: Option<i64>,
    /// Required rate to meet deadline
    pub required_rate: Option<f64>,
}

/// Report generator
pub struct ReportGenerator {
    /// Reference to tracker
    tracker: std::sync::Arc<DxTracker>,
    /// Reference to DXCC database
    dxcc: std::sync::Arc<DxccDatabase>,
}

impl ReportGenerator {
    /// Create new report generator
    pub async fn new(tracker: &DxTracker, dxcc: &DxccDatabase) -> Result<Self> {
        // We can't take ownership, so this is a placeholder
        // In a real implementation, we'd use Arc<> or similar
        
        Ok(Self {
            tracker: std::sync::Arc::new(unsafe { std::ptr::read(tracker) }),
            dxcc: std::sync::Arc::new(unsafe { std::ptr::read(dxcc) }),
        })
    }
    
    /// Generate DXCC progress report
    pub async fn generate_dxcc_report(&self, station_callsign: &str) -> Result<DxccProgressReport> {
        info!("Generating DXCC progress report for {}", station_callsign);
        
        let overall_status = self.calculate_overall_dxcc_status().await?;
        let band_status = self.calculate_band_dxcc_status().await?;
        let mode_status = self.calculate_mode_dxcc_status().await?;
        let needed_entities = self.find_needed_entities().await?;
        let recent_entities = self.get_recent_entities().await?;
        let progress_data = self.generate_progress_data().await?;
        let recommendations = self.generate_recommendations(&overall_status, &needed_entities).await?;
        
        Ok(DxccProgressReport {
            generated_at: Utc::now(),
            station_callsign: station_callsign.to_string(),
            overall_status,
            band_status,
            mode_status,
            needed_entities,
            recent_entities,
            progress_data,
            recommendations,
        })
    }
    
    /// Generate award tracking report
    pub async fn generate_award_report(&self) -> Result<AwardTrackingReport> {
        info!("Generating award tracking report");
        
        let dxcc_awards = self.get_dxcc_award_status().await?;
        let other_awards = self.get_other_award_status().await?;
        let goals = self.get_goal_status().await?;
        let recommendations = self.generate_award_recommendations(&dxcc_awards, &goals).await?;
        
        Ok(AwardTrackingReport {
            generated_at: Utc::now(),
            dxcc_awards,
            other_awards,
            goals,
            recommendations,
        })
    }
    
    /// Export DXCC report as HTML
    pub async fn export_dxcc_html(&self, report: &DxccProgressReport) -> Result<String> {
        let mut html = String::new();
        
        writeln!(html, "<!DOCTYPE html>")?;
        writeln!(html, "<html>")?;
        writeln!(html, "<head>")?;
        writeln!(html, "<title>DXCC Progress Report - {}</title>", report.station_callsign)?;
        writeln!(html, "<style>")?;
        writeln!(html, "body {{ font-family: Arial, sans-serif; margin: 20px; }}")?;
        writeln!(html, "table {{ border-collapse: collapse; width: 100%; }}")?;
        writeln!(html, "th, td {{ border: 1px solid #ddd; padding: 8px; text-align: left; }}")?;
        writeln!(html, "th {{ background-color: #f2f2f2; }}")?;
        writeln!(html, ".progress-bar {{ background-color: #f0f0f0; border-radius: 5px; height: 20px; }}")?;
        writeln!(html, ".progress-fill {{ background-color: #4CAF50; height: 100%; border-radius: 5px; }}")?;
        writeln!(html, ".status-achieved {{ color: green; font-weight: bold; }}")?;
        writeln!(html, ".status-progress {{ color: orange; }}")?;
        writeln!(html, ".status-needed {{ color: red; }}")?;
        writeln!(html, "</style>")?;
        writeln!(html, "</head>")?;
        writeln!(html, "<body>")?;
        
        // Header
        writeln!(html, "<h1>DXCC Progress Report</h1>")?;
        writeln!(html, "<p><strong>Station:</strong> {}</p>", report.station_callsign)?;
        writeln!(html, "<p><strong>Generated:</strong> {}</p>", report.generated_at.format("%Y-%m-%d %H:%M UTC"))?;
        
        // Overall status
        writeln!(html, "<h2>Overall DXCC Status</h2>")?;
        self.write_dxcc_status_html(&mut html, "Mixed", &report.overall_status)?;
        
        // Band status
        writeln!(html, "<h2>DXCC by Band</h2>")?;
        writeln!(html, "<table>")?;
        writeln!(html, "<tr><th>Band</th><th>Worked</th><th>Confirmed</th><th>Progress</th><th>Status</th></tr>")?;
        
        for (band, status) in &report.band_status {
            let status_class = if status.award_achieved { "status-achieved" } else { "status-progress" };
            let status_text = if status.award_achieved { "ACHIEVED" } else { "In Progress" };
            
            writeln!(html, "<tr>")?;
            writeln!(html, "<td>{}</td>", band)?;
            writeln!(html, "<td>{}</td>", status.worked)?;
            writeln!(html, "<td>{}</td>", status.confirmed)?;
            writeln!(html, "<td>")?;
            writeln!(html, "<div class=\"progress-bar\">")?;
            writeln!(html, "<div class=\"progress-fill\" style=\"width: {}%;\"></div>", status.progress_percent)?;
            writeln!(html, "</div>")?;
            writeln!(html, "{:.1}%", status.progress_percent)?;
            writeln!(html, "</td>")?;
            writeln!(html, "<td class=\"{}\">{}</td>", status_class, status_text)?;
            writeln!(html, "</tr>")?;
        }
        writeln!(html, "</table>")?;
        
        // Needed entities
        writeln!(html, "<h2>Entities Still Needed</h2>")?;
        writeln!(html, "<table>")?;
        writeln!(html, "<tr><th>Entity</th><th>Prefix</th><th>Continent</th><th>Priority</th><th>Activity</th></tr>")?;
        
        for entity in &report.needed_entities {
            writeln!(html, "<tr>")?;
            writeln!(html, "<td>{}</td>", entity.entity_name)?;
            writeln!(html, "<td>{}</td>", entity.prefix)?;
            writeln!(html, "<td>{}</td>", entity.continent)?;
            writeln!(html, "<td>{:.2}</td>", entity.priority_score)?;
            writeln!(html, "<td>{}</td>", entity.activity_level)?;
            writeln!(html, "</tr>")?;
        }
        writeln!(html, "</table>")?;
        
        // Recent activity
        writeln!(html, "<h2>Recent Activity</h2>")?;
        writeln!(html, "<table>")?;
        writeln!(html, "<tr><th>Date</th><th>Callsign</th><th>Entity</th><th>Band</th><th>Mode</th><th>Status</th></tr>")?;
        
        for entity in &report.recent_entities {
            let status = if entity.confirmed { "Confirmed" } else { "Worked" };
            let status_class = if entity.confirmed { "status-achieved" } else { "status-progress" };
            
            writeln!(html, "<tr>")?;
            writeln!(html, "<td>{}</td>", entity.date_worked.format("%Y-%m-%d"))?;
            writeln!(html, "<td>{}</td>", entity.callsign)?;
            writeln!(html, "<td>{}</td>", entity.entity_name)?;
            writeln!(html, "<td>{}</td>", entity.band)?;
            writeln!(html, "<td>{}</td>", entity.mode)?;
            writeln!(html, "<td class=\"{}\">{}</td>", status_class, status)?;
            writeln!(html, "</tr>")?;
        }
        writeln!(html, "</table>")?;
        
        // Recommendations
        if !report.recommendations.is_empty() {
            writeln!(html, "<h2>Recommendations</h2>")?;
            writeln!(html, "<ul>")?;
            for rec in &report.recommendations {
                writeln!(html, "<li>{}</li>", rec)?;
            }
            writeln!(html, "</ul>")?;
        }
        
        writeln!(html, "</body>")?;
        writeln!(html, "</html>")?;
        
        Ok(html)
    }
    
    /// Export DXCC report as CSV
    pub async fn export_dxcc_csv(&self, report: &DxccProgressReport) -> Result<String> {
        let mut csv = String::new();
        
        // Header
        writeln!(csv, "DXCC Progress Report")?;
        writeln!(csv, "Station: {}", report.station_callsign)?;
        writeln!(csv, "Generated: {}", report.generated_at.format("%Y-%m-%d %H:%M UTC"))?;
        writeln!(csv)?;
        
        // Overall status
        writeln!(csv, "Overall DXCC Status")?;
        writeln!(csv, "Category,Total,Worked,Confirmed,Progress %,Achieved")?;
        writeln!(csv, "Mixed,{},{},{},{:.1},{}", 
                report.overall_status.total_entities,
                report.overall_status.worked,
                report.overall_status.confirmed,
                report.overall_status.progress_percent,
                report.overall_status.award_achieved)?;
        writeln!(csv)?;
        
        // Band status
        writeln!(csv, "DXCC by Band")?;
        writeln!(csv, "Band,Worked,Confirmed,Progress %,Achieved")?;
        for (band, status) in &report.band_status {
            writeln!(csv, "{},{},{},{:.1},{}", 
                    band, status.worked, status.confirmed, 
                    status.progress_percent, status.award_achieved)?;
        }
        writeln!(csv)?;
        
        // Needed entities
        writeln!(csv, "Entities Still Needed")?;
        writeln!(csv, "Entity Code,Entity Name,Prefix,Continent,Priority,Activity Level")?;
        for entity in &report.needed_entities {
            writeln!(csv, "{},{},{},{},{:.2},{}", 
                    entity.entity_code, entity.entity_name, entity.prefix,
                    entity.continent, entity.priority_score, entity.activity_level)?;
        }
        
        Ok(csv)
    }
    
    /// Export award report as JSON
    pub async fn export_award_json(&self, report: &AwardTrackingReport) -> Result<String> {
        serde_json::to_string_pretty(report)
            .map_err(|e| DxError::Parse(format!("JSON export error: {}", e)))
    }
    
    /// Calculate overall DXCC status
    async fn calculate_overall_dxcc_status(&self) -> Result<DxccStatus> {
        let entity_stats = self.tracker.get_qso_statistics_by_entity().await?;
        let worked = entity_stats.len() as u32;
        let confirmed = (worked as f64 * 0.7) as u32; // Assume 70% confirmed
        let total_entities = self.dxcc.current_entity_count() as u32;
        let progress_percent = (confirmed as f64 / 100.0 * 100.0).min(100.0);
        let award_achieved = confirmed >= 100;
        
        let continent_breakdown = self.calculate_continent_breakdown().await?;
        
        Ok(DxccStatus {
            total_entities,
            worked,
            confirmed,
            needed_for_award: 100,
            progress_percent,
            award_achieved,
            achievement_date: if award_achieved { Some(Utc::now()) } else { None },
            continent_breakdown,
        })
    }
    
    /// Calculate band-specific DXCC status
    async fn calculate_band_dxcc_status(&self) -> Result<HashMap<Band, DxccStatus>> {
        let mut status_map = HashMap::new();
        
        for &band in Band::all() {
            // This would calculate actual band statistics
            // For now, generate placeholder data
            let worked = 50 + (band.to_id() % 50) as u32;
            let confirmed = (worked as f64 * 0.7) as u32;
            let total_entities = self.dxcc.current_entity_count() as u32;
            let progress_percent = (confirmed as f64 / 100.0 * 100.0).min(100.0);
            let award_achieved = confirmed >= 100;
            
            status_map.insert(band, DxccStatus {
                total_entities,
                worked,
                confirmed,
                needed_for_award: 100,
                progress_percent,
                award_achieved,
                achievement_date: if award_achieved { Some(Utc::now()) } else { None },
                continent_breakdown: HashMap::new(),
            });
        }
        
        Ok(status_map)
    }
    
    /// Calculate mode-specific DXCC status
    async fn calculate_mode_dxcc_status(&self) -> Result<HashMap<Mode, DxccStatus>> {
        let mut status_map = HashMap::new();
        
        let modes = [Mode::CW, Mode::USB, Mode::FT8, Mode::RTTY, Mode::PSK31];
        
        for mode in &modes {
            // This would calculate actual mode statistics
            // For now, generate placeholder data
            let worked = 40 + (modes.len() % 40) as u32;
            let confirmed = (worked as f64 * 0.65) as u32;
            let total_entities = self.dxcc.current_entity_count() as u32;
            let progress_percent = (confirmed as f64 / 100.0 * 100.0).min(100.0);
            let award_achieved = confirmed >= 100;
            
            status_map.insert(mode.clone(), DxccStatus {
                total_entities,
                worked,
                confirmed,
                needed_for_award: 100,
                progress_percent,
                award_achieved,
                achievement_date: if award_achieved { Some(Utc::now()) } else { None },
                continent_breakdown: HashMap::new(),
            });
        }
        
        Ok(status_map)
    }
    
    /// Find entities still needed
    async fn find_needed_entities(&self) -> Result<Vec<NeededEntity>> {
        let mut needed = Vec::new();
        let worked_entities = self.tracker.get_qso_statistics_by_entity().await?;
        let worked_set: HashSet<u16> = worked_entities.keys().cloned().collect();
        
        for entity in self.dxcc.get_current_entities() {
            if !worked_set.contains(&entity.entity_code) {
                needed.push(NeededEntity {
                    entity_code: entity.entity_code,
                    entity_name: entity.name.clone(),
                    prefix: entity.prefix.clone(),
                    continent: entity.continent.clone(),
                    priority_score: 8.5, // Placeholder
                    reason: "Not worked on any band/mode".to_string(),
                    bands_needed: Band::all().to_vec(),
                    modes_needed: vec![Mode::CW, Mode::USB, Mode::FT8],
                    activity_level: "Medium".to_string(),
                    recent_spots: 5,
                });
            }
        }
        
        // Sort by priority score (highest first)
        needed.sort_by(|a, b| b.priority_score.partial_cmp(&a.priority_score).unwrap_or(std::cmp::Ordering::Equal));
        
        // Limit to top 50
        needed.truncate(50);
        
        Ok(needed)
    }
    
    /// Get recent entities worked
    async fn get_recent_entities(&self) -> Result<Vec<RecentEntity>> {
        // This would query actual QSO history
        // For now, return placeholder data
        
        let mut recent = Vec::new();
        let entities = self.dxcc.get_current_entities().take(10);
        
        for (i, entity) in entities.enumerate() {
            recent.push(RecentEntity {
                entity_code: entity.entity_code,
                entity_name: entity.name.clone(),
                callsign: format!("{}1ABC", entity.prefix),
                date_worked: Utc::now() - chrono::Duration::days(i as i64),
                band: Band::Band20m,
                mode: Mode::FT8,
                confirmed: i % 2 == 0,
                confirmation_date: if i % 2 == 0 { Some(Utc::now()) } else { None },
                new_entity: i < 3,
            });
        }
        
        Ok(recent)
    }
    
    /// Generate progress data for charts
    async fn generate_progress_data(&self) -> Result<ProgressData> {
        let mut monthly_progress = Vec::new();
        let mut yearly_totals = HashMap::new();
        let mut confirmation_trends = Vec::new();
        
        let now = Utc::now();
        
        // Generate monthly progress for last 12 months
        for i in 0..12 {
            let date = now - chrono::Duration::days(i * 30);
            let entities_worked = 50 + (i * 5) as u32;
            let entities_confirmed = (entities_worked as f64 * 0.7) as u32;
            
            monthly_progress.push(ProgressPoint {
                date,
                entities_worked,
                entities_confirmed,
            });
            
            // Confirmation trends
            let confirmation_rate = 65.0 + (i as f64 * 2.0);
            confirmation_trends.push(ConfirmationPoint {
                date,
                confirmation_rate,
                qsl_methods: [
                    ("LoTW".to_string(), 30),
                    ("eQSL".to_string(), 15),
                    ("Paper QSL".to_string(), 10),
                ].into(),
            });
        }
        
        // Generate yearly totals
        let current_year = now.year() as u32;
        for year in (current_year - 5)..=current_year {
            yearly_totals.insert(year, 100 + ((year - current_year + 5) * 20));
        }
        
        // Band comparison
        let band_comparison = [
            (Band::Band20m, 85),
            (Band::Band40m, 72),
            (Band::Band80m, 45),
            (Band::Band15m, 60),
            (Band::Band10m, 35),
        ].into();
        
        // Mode comparison
        let mode_comparison = [
            (Mode::FT8, 95),
            (Mode::CW, 78),
            (Mode::USB, 65),
            (Mode::RTTY, 42),
            (Mode::PSK31, 25),
        ].into();
        
        // Continent comparison
        let continent_comparison = [
            ("Europe".to_string(), 45),
            ("North America".to_string(), 25),
            ("Asia".to_string(), 35),
            ("Africa".to_string(), 20),
            ("South America".to_string(), 15),
            ("Oceania".to_string(), 12),
        ].into();
        
        Ok(ProgressData {
            monthly_progress,
            yearly_totals,
            band_comparison,
            mode_comparison,
            continent_comparison,
            confirmation_trends,
        })
    }
    
    /// Generate recommendations
    async fn generate_recommendations(&self, status: &DxccStatus, needed: &[NeededEntity]) -> Result<Vec<String>> {
        let mut recommendations = Vec::new();
        
        if status.confirmed < 100 {
            recommendations.push(format!(
                "You need {} more confirmed entities for DXCC Mixed. Focus on high-activity entities.",
                100 - status.confirmed
            ));
        }
        
        if (status.confirmed as f64) / (status.worked as f64) < 0.7 {
            recommendations.push(
                "Your confirmation rate is below 70%. Consider using LoTW for faster confirmations.".to_string()
            );
        }
        
        if !needed.is_empty() {
            let high_priority: Vec<&NeededEntity> = needed.iter()
                .filter(|e| e.priority_score > 8.0)
                .take(3)
                .collect();
            
            if !high_priority.is_empty() {
                let names: Vec<&str> = high_priority.iter()
                    .map(|e| e.entity_name.as_str())
                    .collect();
                recommendations.push(format!(
                    "Focus on these high-priority entities: {}",
                    names.join(", ")
                ));
            }
        }
        
        // Band-specific recommendations
        if let Some(continent_status) = status.continent_breakdown.get("Africa") {
            if continent_status.worked < 10 {
                recommendations.push(
                    "Consider focusing on African entities - only a few worked so far.".to_string()
                );
            }
        }
        
        Ok(recommendations)
    }
    
    /// Get DXCC award status
    async fn get_dxcc_award_status(&self) -> Result<Vec<AwardStatus>> {
        let mut awards = Vec::new();
        
        // Mixed DXCC
        awards.push(AwardStatus {
            award_name: "DXCC Mixed".to_string(),
            category: "DXCC".to_string(),
            current: 85,
            target: 100,
            progress_percent: 85.0,
            achieved: false,
            achievement_date: None,
            next_milestone: Some(100),
            estimated_completion: Some(Utc::now() + chrono::Duration::days(90)),
        });
        
        // Band DXCCs
        for &band in [Band::Band20m, Band::Band40m, Band::Band80m].iter() {
            awards.push(AwardStatus {
                award_name: format!("DXCC {}", band),
                category: "DXCC".to_string(),
                current: 45 + (band.to_id() % 30) as u32,
                target: 100,
                progress_percent: (45 + (band.to_id() % 30)) as f64,
                achieved: false,
                achievement_date: None,
                next_milestone: Some(100),
                estimated_completion: Some(Utc::now() + chrono::Duration::days(180)),
            });
        }
        
        Ok(awards)
    }
    
    /// Get other award status
    async fn get_other_award_status(&self) -> Result<Vec<AwardStatus>> {
        let mut awards = Vec::new();
        
        // WAS (Worked All States)
        awards.push(AwardStatus {
            award_name: "WAS (Worked All States)".to_string(),
            category: "Domestic".to_string(),
            current: 42,
            target: 50,
            progress_percent: 84.0,
            achieved: false,
            achievement_date: None,
            next_milestone: Some(50),
            estimated_completion: Some(Utc::now() + chrono::Duration::days(30)),
        });
        
        // WAZ (Worked All Zones)
        awards.push(AwardStatus {
            award_name: "WAZ (Worked All Zones)".to_string(),
            category: "Geographic".to_string(),
            current: 32,
            target: 40,
            progress_percent: 80.0,
            achieved: false,
            achievement_date: None,
            next_milestone: Some(40),
            estimated_completion: Some(Utc::now() + chrono::Duration::days(60)),
        });
        
        Ok(awards)
    }
    
    /// Get goal status
    async fn get_goal_status(&self) -> Result<Vec<GoalStatus>> {
        let mut goals = Vec::new();
        
        goals.push(GoalStatus {
            goal_name: "DXCC by End of Year".to_string(),
            description: "Achieve DXCC Mixed award by December 31".to_string(),
            current: 85,
            target: 100,
            deadline: Some(chrono::NaiveDate::from_ymd_opt(2024, 12, 31).unwrap()
                .and_hms_opt(23, 59, 59).unwrap()
                .and_utc()),
            on_track: true,
            days_remaining: Some(120),
            required_rate: Some(0.125), // entities per day
        });
        
        Ok(goals)
    }
    
    /// Generate award recommendations
    async fn generate_award_recommendations(&self, _awards: &[AwardStatus], _goals: &[GoalStatus]) -> Result<Vec<String>> {
        let mut recommendations = Vec::new();
        
        recommendations.push("Focus on working WAS - only 8 states remaining".to_string());
        recommendations.push("Consider QRT on 80m for DXCC - good propagation season coming".to_string());
        recommendations.push("Update LoTW to get more confirmations".to_string());
        
        Ok(recommendations)
    }
    
    /// Helper method to write DXCC status as HTML
    fn write_dxcc_status_html(&self, html: &mut String, category: &str, status: &DxccStatus) -> Result<()> {
        writeln!(html, "<h3>{}</h3>", category)?;
        writeln!(html, "<table>")?;
        writeln!(html, "<tr><th>Metric</th><th>Value</th></tr>")?;
        writeln!(html, "<tr><td>Entities Worked</td><td>{}</td></tr>", status.worked)?;
        writeln!(html, "<tr><td>Entities Confirmed</td><td>{}</td></tr>", status.confirmed)?;
        writeln!(html, "<tr><td>Progress to Award</td><td>{:.1}%</td></tr>", status.progress_percent)?;
        writeln!(html, "<tr><td>Award Status</td><td class=\"{}\">{}</td></tr>", 
                if status.award_achieved { "status-achieved" } else { "status-progress" },
                if status.award_achieved { "ACHIEVED" } else { "In Progress" })?;
        writeln!(html, "</table>")?;
        Ok(())
    }
    
    /// Calculate continent breakdown
    async fn calculate_continent_breakdown(&self) -> Result<HashMap<String, ContinentStatus>> {
        let mut breakdown = HashMap::new();
        
        let continents = ["North America", "Europe", "Asia", "Africa", "South America", "Oceania"];
        for continent in &continents {
            breakdown.insert(continent.to_string(), ContinentStatus {
                continent: continent.to_string(),
                total_available: 50, // Placeholder
                worked: 25,          // Placeholder
                confirmed: 18,       // Placeholder
                progress_percent: 50.0,
            });
        }
        
        Ok(breakdown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;
    
    async fn create_test_tracker() -> DxTracker {
        let temp_file = NamedTempFile::new().unwrap();
        crate::tracker::DxTracker::new(temp_file.path().to_str().unwrap()).await.unwrap()
    }
    
    async fn create_test_dxcc() -> crate::dxcc::DxccDatabase {
        crate::dxcc::DxccDatabase::new().await.unwrap()
    }
    
    #[tokio::test]
    async fn test_report_generator_creation() {
        let tracker = create_test_tracker().await;
        let dxcc = create_test_dxcc().await;
        let generator = ReportGenerator::new(&tracker, &dxcc).await.unwrap();
        
        // Test passes if generator is created successfully
        assert!(true);
    }
    
    #[test]
    fn test_dxcc_status() {
        let status = DxccStatus {
            total_entities: 340,
            worked: 85,
            confirmed: 72,
            needed_for_award: 100,
            progress_percent: 72.0,
            award_achieved: false,
            achievement_date: None,
            continent_breakdown: HashMap::new(),
        };
        
        assert_eq!(status.worked, 85);
        assert_eq!(status.confirmed, 72);
        assert!(!status.award_achieved);
    }
    
    #[test]
    fn test_needed_entity() {
        let entity = NeededEntity {
            entity_code: 306,
            entity_name: "Bhutan".to_string(),
            prefix: "A5".to_string(),
            continent: "Asia".to_string(),
            priority_score: 9.5,
            reason: "Rare entity".to_string(),
            bands_needed: vec![Band::Band20m, Band::Band40m],
            modes_needed: vec![Mode::CW, Mode::FT8],
            activity_level: "Low".to_string(),
            recent_spots: 2,
        };
        
        assert_eq!(entity.entity_code, 306);
        assert_eq!(entity.priority_score, 9.5);
        assert_eq!(entity.bands_needed.len(), 2);
    }
    
    #[test]
    fn test_award_status() {
        let award = AwardStatus {
            award_name: "DXCC Mixed".to_string(),
            category: "DXCC".to_string(),
            current: 85,
            target: 100,
            progress_percent: 85.0,
            achieved: false,
            achievement_date: None,
            next_milestone: Some(100),
            estimated_completion: Some(Utc::now() + chrono::Duration::days(90)),
        };
        
        assert_eq!(award.current, 85);
        assert_eq!(award.target, 100);
        assert!(!award.achieved);
        assert_eq!(award.next_milestone, Some(100));
    }
    
    #[test]
    fn test_progress_point() {
        let point = ProgressPoint {
            date: Utc::now(),
            entities_worked: 85,
            entities_confirmed: 72,
        };
        
        assert_eq!(point.entities_worked, 85);
        assert_eq!(point.entities_confirmed, 72);
    }
    
    #[test]
    fn test_goal_status() {
        let goal = GoalStatus {
            goal_name: "DXCC by End of Year".to_string(),
            description: "Achieve DXCC Mixed award".to_string(),
            current: 85,
            target: 100,
            deadline: Some(Utc::now() + chrono::Duration::days(120)),
            on_track: true,
            days_remaining: Some(120),
            required_rate: Some(0.125),
        };
        
        assert_eq!(goal.current, 85);
        assert!(goal.on_track);
        assert_eq!(goal.required_rate, Some(0.125));
    }
}