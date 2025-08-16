# Pancetta Requirements Specification

## 1. Executive Summary

This document defines the functional and non-functional requirements for Pancetta, a modern ham radio digital mode terminal. Requirements are based on user research with 25+ amateur radio operators and analysis of existing solutions.

## 2. User Personas

### Primary Persona: "DX Hunter Dave"
- **Demographics**: 45-65 years old, Extra class license
- **Experience**: 10+ years in ham radio, comfortable with computers
- **Goals**: Work new DXCC entities, achieve DXCC Honor Roll
- **Pain Points**: WSJT-X UI is dated, poor DX prioritization
- **Needs**: Intelligent DX spotting, clean modern interface

### Secondary Persona: "Emergency Communicator Emily"
- **Demographics**: 30-50 years old, General/Extra license
- **Experience**: ARES/RACES member, field operations
- **Goals**: Reliable emergency communications
- **Pain Points**: Complex setup in field, poor battery efficiency
- **Needs**: Simple operation, low resource usage, reliability

### Tertiary Persona: "Newcomer Nick"
- **Demographics**: 25-40 years old, Technician/General license
- **Experience**: New to digital modes (<1 year)
- **Goals**: Make first FT8 contacts, learn digital modes
- **Pain Points**: Overwhelming interfaces, complex configuration
- **Needs**: Guided setup, clear documentation, simple UI

## 3. Functional Requirements

### 3.1 Core Digital Mode Operations

#### FR-001: FT8 Decoding
- **Description**: System shall decode FT8 signals in real-time
- **Acceptance Criteria**:
  - Decode accuracy ≥ 95% at SNR -20dB
  - Decode latency < 100ms after 12.64s window
  - Support simultaneous decode of 50+ signals
- **Priority**: P0 (Critical)
- **Verification**: Automated test with reference signals

#### FR-002: FT8 Encoding
- **Description**: System shall encode and transmit FT8 messages
- **Acceptance Criteria**:
  - Generate valid FT8 audio within 50ms
  - Support all standard message types
  - Maintain frequency accuracy ± 1 Hz
- **Priority**: P0 (Critical)
- **Verification**: Decode with reference decoder

#### FR-003: QSO State Machine
- **Description**: System shall manage QSO progression automatically
- **Acceptance Criteria**:
  - Handle CQ → Grid → Report → RR73 sequence
  - Support manual intervention at any state
  - Timeout incomplete QSOs after 10 minutes
- **Priority**: P0 (Critical)
- **Verification**: End-to-end QSO test scenarios

### 3.2 Station Management

#### FR-010: Rig Control
- **Description**: System shall control transceiver via CAT
- **Acceptance Criteria**:
  - Support top 20 transceivers via hamlib
  - Read/set frequency within 100ms
  - Control PTT with < 10ms latency
- **Priority**: P0 (Critical)
- **Verification**: Hardware-in-loop testing

#### FR-011: Audio Device Management
- **Description**: System shall handle audio I/O reliably
- **Acceptance Criteria**:
  - Enumerate all system audio devices
  - Handle device disconnection gracefully
  - Support 48kHz and 8kHz sample rates
- **Priority**: P0 (Critical)
- **Verification**: Audio loopback tests

### 3.3 DX Features

#### FR-020: DX Entity Detection
- **Description**: System shall identify DXCC entities from callsigns
- **Acceptance Criteria**:
  - Correctly identify 95% of standard callsigns
  - Handle special event and portable calls
  - Update time < 10ms per callsign
- **Priority**: P1 (High)
- **Verification**: Test against callsign database

#### FR-021: Rarity Scoring
- **Description**: System shall score stations by rarity
- **Acceptance Criteria**:
  - Consider entity rarity, distance, band/mode
  - Update scores within 100ms of decode
  - Sort list by score in real-time
- **Priority**: P1 (High)
- **Verification**: Scoring algorithm tests

### 3.4 Logging

#### FR-030: ADIF Export
- **Description**: System shall export logs in ADIF 3.0 format
- **Acceptance Criteria**:
  - Include all required ADIF fields
  - Support batch export
  - Import into LoTW successfully
- **Priority**: P1 (High)
- **Verification**: ADIF validator tool

## 4. Non-Functional Requirements

### 4.1 Performance

#### NFR-001: Audio Latency
- **Requirement**: End-to-end audio latency < 50ms
- **Rationale**: Required for real-time operation
- **Verification**: Latency measurement tests

#### NFR-002: CPU Usage
- **Requirement**: CPU usage < 20% on i5-6500 or equivalent
- **Rationale**: Must run alongside other ham software
- **Verification**: Performance profiling

#### NFR-003: Memory Usage
- **Requirement**: RAM usage < 200MB steady-state
- **Rationale**: Support older computers and Raspberry Pi
- **Verification**: Memory profiling

### 4.2 Reliability

#### NFR-010: Uptime
- **Requirement**: 99.9% uptime (8.76 hours downtime/year)
- **Rationale**: Critical for unattended operation
- **Verification**: 30-day stability test

#### NFR-011: Data Integrity
- **Requirement**: Zero data loss on unexpected shutdown
- **Rationale**: Preserve QSO logs
- **Verification**: Kill testing

### 4.3 Usability

#### NFR-020: Time to First QSO
- **Requirement**: New user completes QSO within 15 minutes
- **Rationale**: Reduce barrier to entry
- **Verification**: User testing with newcomers

#### NFR-021: Accessibility
- **Requirement**: WCAG 2.1 AA compliance for TUI
- **Rationale**: Support vision-impaired operators
- **Verification**: Screen reader testing

### 4.4 Security

#### NFR-030: No Network Listeners
- **Requirement**: No inbound network connections
- **Rationale**: Security best practice
- **Verification**: Port scanning

#### NFR-031: Input Validation
- **Requirement**: Validate all user input
- **Rationale**: Prevent injection attacks
- **Verification**: Fuzz testing

## 5. Regulatory Requirements

### 5.1 FCC Part 97 Compliance

#### REG-001: Station Identification
- **Requirement**: Transmit callsign every 10 minutes
- **Reference**: FCC §97.119
- **Verification**: Transmission log audit

#### REG-002: Spurious Emissions
- **Requirement**: Maintain signal within 2.8 kHz bandwidth
- **Reference**: FCC §97.307
- **Verification**: Spectrum analysis

#### REG-003: Power Limits
- **Requirement**: Enforce band-specific power limits
- **Reference**: FCC §97.313
- **Verification**: Power output measurement

### 5.2 International Compliance

#### REG-010: IARU Band Plan
- **Requirement**: Operate only in designated digital segments
- **Reference**: IARU Region 1/2/3 band plans
- **Verification**: Frequency validation

## 6. Constraints

### 6.1 Technical Constraints
- Must integrate with existing hamlib library
- Must support FT8 protocol as defined by WSJT-X
- Must run on Raspberry Pi 4 (2GB RAM)

### 6.2 Business Constraints
- Open source (MIT license)
- No commercial dependencies
- Single developer maintainable

## 7. Acceptance Criteria

### 7.1 MVP Acceptance (Phase 1)
- [ ] Complete 10 FT8 QSOs successfully
- [ ] Decode accuracy ≥ 95% verified
- [ ] 24-hour stability test passed
- [ ] Works on Linux and macOS
- [ ] Documentation complete

### 7.2 Release Acceptance (v1.0)
- [ ] All P0 requirements implemented
- [ ] 80% of P1 requirements implemented
- [ ] Zero critical bugs
- [ ] Performance targets met
- [ ] Beta user approval (10+ users)

## 8. Requirements Traceability Matrix

| Requirement | User Need | Architecture Component | Test Case |
|------------|-----------|----------------------|-----------|
| FR-001 | Decode signals | Digital Mode Engine | TC-001 |
| FR-002 | Transmit | Digital Mode Engine | TC-002 |
| FR-003 | Easy QSOs | QSO Manager | TC-003 |
| FR-010 | Rig control | Rig Control Service | TC-010 |
| FR-020 | Work DX | DX Hunter Engine | TC-020 |
| FR-030 | Keep logs | Logging Service | TC-030 |

## 9. Change Management

### Version History
- v1.0 (2024-01-15): Initial requirements based on user research
- v1.1 (TBD): Updates based on beta feedback

### Change Process
1. Requirements changes require user justification
2. Impact analysis on architecture and timeline
3. Stakeholder approval required
4. Update traceability matrix

## 10. Appendices

### A. User Interview Summary
- 25 operators interviewed
- Key finding: Simplicity more important than features
- Main competitor pain point: Complex configuration

### B. Competitive Analysis
- WSJT-X: Feature-complete but dated UI
- JS8Call: Good UI but different protocol
- GridTracker: Excellent mapping but separate app

### C. Glossary
- **DXCC**: DX Century Club award program
- **CAT**: Computer Aided Transceiver control
- **SNR**: Signal-to-Noise Ratio
- **ADIF**: Amateur Data Interchange Format