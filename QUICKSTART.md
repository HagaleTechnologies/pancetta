# Pancetta Quick Start Guide

**Get up and running with Pancetta FT8 operations in 5 minutes!**

## Prerequisites Checklist

- [ ] Modern computer with audio input/output
- [ ] Amateur radio license and call sign
- [ ] Time-synchronized system (NTP enabled)
- [ ] Basic terminal/command line familiarity

## Step 1: Install Pancetta (2 minutes)

### Option A: Package Manager (Recommended)

**macOS/Linux (Homebrew):**
```bash
brew install pancetta-team/pancetta/pancetta
```

**Windows (Chocolatey):**
```powershell
choco install pancetta
```

**Linux (Cargo):**
```bash
cargo install pancetta
```

### Option B: Download Binary

1. Visit [GitHub Releases](https://github.com/pancetta-team/pancetta/releases)
2. Download for your platform
3. Extract and place in your PATH

## Step 2: Initial Setup (1 minute)

### Quick Audio Test

```bash
# Test your audio system
pancetta --test-latency
```

**Expected output:**
```
✅ Audio system initialized successfully
✅ Latency: 0.89ms (Target: <1ms achieved)
```

### Set Your Call Sign

```bash
# Set your amateur radio call sign
pancetta --set-callsign W1ABC

# Set your grid square
pancetta --set-grid FN42
```

## Step 3: First Run (30 seconds)

### Launch Pancetta

```bash
# Start Pancetta with default settings
pancetta
```

### Main Interface Overview

```
┌─────────────────────── Pancetta v0.1.0 ─────────────────────────┐
│ Status: Running | Audio: OK | FT8: Monitoring | Latency: 0.89ms │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  ┌─── Waterfall ──────────────┐  ┌─── Decodes ────────────────┐ │
│  │    [Frequency Display]     │  │ 071500 CQ W1ABC FN42      │ │
│  │                            │  │ 071515 W1ABC K1XYZ FN31   │ │
│  │  [Scrolling Waterfall]     │  │ 071530 K1XYZ W1ABC R-08   │ │
│  └────────────────────────────┘  └───────────────────────────┘ │
│                                                                 │
│  Audio: ████████░░ -12dB       CPU: 3.2%  Memory: 45MB        │
└─────────────────────────────────────────────────────────────────┘
```

**Key indicators:**
- **Green status bar**: System running normally
- **Audio levels**: Should show activity with signals
- **Low latency**: <1ms for optimal performance

## Step 4: Basic Operation (1 minute)

### Essential Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `Space` | Start/Stop audio processing |
| `D` | Toggle FT8 decoding |
| `Tab` | Switch between panels |
| `F1` | Help |
| `Ctrl+C` | Exit |

### Monitor FT8 Activity

1. **Tune your radio** to an FT8 frequency:
   - **20m**: 14.074 MHz
   - **40m**: 7.074 MHz
   - **15m**: 21.074 MHz

2. **Watch the waterfall** for FT8 signals (parallel horizontal lines)

3. **Check decode window** for decoded messages

### Verify Time Synchronization

```bash
# Check system time (should be within ±1 second of UTC)
date -u

# On Linux/macOS, sync if needed:
sudo ntpdate -s pool.ntp.org
```

## Step 5: Making Your First Contact (1 minute)

### Listen First

- Watch for stations calling CQ: `CQ W1ABC FN42`
- Note signal strength and timing
- FT8 transmissions are exactly 12.64 seconds long

### Call CQ

1. Press `C` to call CQ
2. Your message: `CQ W1ABC FN42` (automatically formatted)
3. Wait for responses in the decode window

### Respond to CQ

1. Click on a CQ in the decode window, or
2. Manually enter: `W1ABC K1XYZ FN31`
3. Follow the standard FT8 exchange sequence

### Standard FT8 Exchange

```
Station A (calling CQ)     Station B (responding)
─────────────────────────  ───────────────────────
CQ W1ABC FN42             →
                          ← W1ABC K1XYZ FN31
K1XYZ W1ABC R-08          →
                          ← W1ABC K1XYZ RR73
K1XYZ W1ABC 73            →
```

## Quick Troubleshooting

### Problem: No audio detected
**Solution:**
```bash
# List audio devices
pancetta --list-audio-devices

# Set specific device
pancetta --audio-device "USB Audio Interface"
```

### Problem: High latency warning
**Solution:**
```bash
# Reduce buffer size
pancetta --buffer-size 64

# Check for background apps using audio
```

### Problem: No FT8 decodes
**Check:**
- [ ] Radio tuned to FT8 frequency
- [ ] Audio levels showing signal
- [ ] System time synchronized
- [ ] Radio in USB mode (upper sideband)

### Problem: Time sync error
**Solution:**
```bash
# Linux/macOS
sudo timedatectl set-ntp true

# Windows
# Run Windows Time service
net start w32time
w32tm /resync
```

## Next Steps

### Learn More
- Read the [User Manual](USER_MANUAL.md) for detailed features
- Check [Installation Guide](INSTALL.md) for optimization tips
- Join our community at https://github.com/pancetta-team/pancetta/discussions

### Advanced Features
- Contest logging and scoring
- Multi-band monitoring
- Remote operation
- Plugin system

### Get Help
- Press `F1` in Pancetta for context help
- Visit GitHub Issues for support
- Join Matrix chat: #pancetta:matrix.org

## FT8 Frequency Reference

| Band | Frequency | Notes |
|------|-----------|-------|
| 160m | 1.840 MHz | Night time only |
| 80m | 3.573 MHz | Good for regional |
| 40m | 7.074 MHz | Excellent day/night |
| 20m | 14.074 MHz | Best DX band |
| 17m | 18.100 MHz | Good DX conditions |
| 15m | 21.074 MHz | Daytime DX |
| 12m | 24.915 MHz | Daytime only |
| 10m | 28.074 MHz | Solar cycle dependent |
| 6m | 50.313 MHz | Sporadic-E openings |

## Success Checklist

After 5 minutes, you should have:
- [ ] Pancetta installed and running
- [ ] Audio latency <1ms
- [ ] Call sign configured
- [ ] FT8 signals visible in waterfall
- [ ] Messages decoding in decode window
- [ ] System time synchronized
- [ ] Made first FT8 contact (optional)

**Congratulations! You're now ready for FT8 operations with Pancetta.**

---

*Need help? Check the [User Manual](USER_MANUAL.md) or open an issue on [GitHub](https://github.com/pancetta-team/pancetta/issues)*