# Pancetta User Guide

Welcome to the Pancetta FT8 amateur radio application! This guide will help you get started and make the most of Pancetta's features.

## Table of Contents

1. [Quick Start](#quick-start)
2. [Understanding FT8](#understanding-ft8)
3. [User Interface](#user-interface)
4. [Operating Procedures](#operating-procedures)
5. [Making QSOs](#making-qsos)
6. [Advanced Features](#advanced-features)
7. [Tips and Tricks](#tips-and-tricks)
8. [Common Operations](#common-operations)

## Quick Start

### First Run

1. **Start Pancetta**:
   ```bash
   ./pancetta
   ```

2. **Check Audio Setup**:
   - Pancetta will automatically detect your audio devices
   - Watch the signal meter to ensure audio is being received
   - Adjust your radio's audio output to keep levels in the green zone

3. **Set Your Station Info**:
   - Edit `~/.config/pancetta/config.toml`:
   ```toml
   [qso]
   my_callsign = "YOUR_CALL"
   my_grid = "EM00aa"
   my_name = "Your Name"
   ```

4. **Tune to FT8 Frequency**:
   - 20m: 14.074 MHz
   - 40m: 7.074 MHz
   - 80m: 3.573 MHz

5. **Start Decoding**:
   - Messages will appear automatically every 12.64 seconds
   - FT8 cycles start at :00 and :15 seconds

### Basic Operation Checklist

- [ ] Computer time synchronized (within 2 seconds)
- [ ] Radio on FT8 frequency
- [ ] Audio levels set correctly
- [ ] Mode set to USB
- [ ] Passband filter 200-3000 Hz

## Understanding FT8

### FT8 Protocol Basics

FT8 is a digital mode designed for weak signal communication:

- **Cycle Time**: 12.64 seconds
- **Bandwidth**: ~50 Hz per signal
- **Message Length**: 13 characters
- **SNR Range**: -24 to +20 dB

### Timing

FT8 requires accurate time synchronization:

```
:00 - :12.6  Even period (TX or RX)
:15 - :27.6  Odd period (TX or RX)
:30 - :42.6  Even period (TX or RX)
:45 - :57.6  Odd period (TX or RX)
```

### Message Types

1. **CQ Call**: `CQ W1ABC EM00`
2. **Reply**: `W1ABC K2XYZ -10`
3. **Report**: `K2XYZ W1ABC R-15`
4. **Roger Report**: `W1ABC K2XYZ RRR`
5. **73**: `K2XYZ W1ABC 73`

## User Interface

### Terminal UI Layout

```
┌─────────────────────────────────────────────────┐
│                  Pancetta v1.0                  │
├─────────────────────────────────────────────────┤
│ Frequency: 14.074.000 MHz  Mode: USB  PWR: 25W │
├─────────────────────────────────────────────────┤
│                                                 │
│              [Waterfall Display]                │
│                                                 │
├─────────────────────────────────────────────────┤
│ Time   Freq  SNR  Message                      │
│ 12:15  1234  -10  CQ DX W1ABC EM00            │
│ 12:15  1567   +5  K2XYZ W3DEF R-15            │
│ 12:15  2100  -18  W4GHI K5JKL 73              │
├─────────────────────────────────────────────────┤
│ Status: Receiving | Audio: OK | Decode: 3 msgs │
└─────────────────────────────────────────────────┘
```

### Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `q` | Quit application |
| `Space` | Pause/Resume decoding |
| `c` | Clear message list |
| `↑/↓` | Scroll messages |
| `PgUp/PgDn` | Page through messages |
| `f` | Toggle frequency display |
| `w` | Toggle waterfall |
| `s` | Save messages to file |
| `h` | Show help |

### Status Indicators

- **Green**: Normal operation
- **Yellow**: Warning (check audio levels)
- **Red**: Error (check configuration)

## Operating Procedures

### Calling CQ

1. **Find Clear Frequency**:
   - Look for unused space in waterfall
   - Avoid QRMing existing QSOs

2. **Enable TX** (when implemented):
   ```
   TX: CQ W1ABC EM00
   ```

3. **Wait for Responses**:
   - Watch for your callsign in decoded messages
   - Replies appear 1-2 cycles later

### Answering CQ

1. **Double-click CQ message** (or note frequency)
2. **Reply with signal report**:
   ```
   W1ABC K2XYZ -10
   ```

3. **Complete Exchange**:
   - Send report
   - Confirm with RRR
   - End with 73

### Standard QSO Flow

```
Station 1        Station 2
---------        ---------
CQ W1ABC EM00    
                 W1ABC K2XYZ FN20
K2XYZ W1ABC -10  
                 W1ABC K2XYZ R-15
K2XYZ W1ABC RRR  
                 W1ABC K2XYZ 73
K2XYZ W1ABC 73   
```

## Making QSOs

### Before Transmitting

1. **Listen First**:
   - Monitor for 2-3 minutes
   - Understand band conditions
   - Note active stations

2. **Check Propagation**:
   - Good times: sunrise/sunset
   - Check solar indices
   - Use PSKReporter to see where you're heard

3. **Set Power Appropriately**:
   - Start with 25W
   - Increase only if needed
   - QRP: 5W, QRO: >100W

### During QSO

1. **Be Patient**:
   - Allow 2-3 cycles for response
   - Weak signals may take longer

2. **Handle QRM**:
   - Move frequency if needed
   - Use narrower decode window

3. **Log Contact**:
   - Pancetta auto-logs completed QSOs
   - Verify callsign and grid

### After QSO

1. **QSL Options**:
   - Upload to LoTW
   - Send to eQSL
   - Paper QSL if requested

2. **Update Logbook**:
   - Export ADIF file
   - Import to logging software

## Advanced Features

### Weak Signal Decoding

Enable deep search for challenging conditions:

```bash
# Maximum sensitivity
./pancetta --decode-depth 3 --sensitivity 0.9
```

Configuration:
```toml
[ft8]
decode_depth = 3
deep_search = true
sensitivity = 0.9
```

### Multi-Band Monitoring

Run multiple instances on different bands:

```bash
# Terminal 1 - 20m
./pancetta --freq 14074000

# Terminal 2 - 40m  
./pancetta --freq 7074000
```

### Remote Operation

Access Pancetta over SSH:

```bash
# On remote machine
./pancetta --headless > pancetta.log &

# Monitor log
tail -f pancetta.log
```

### Contest Mode

Optimize for high-rate operation:

```toml
[ft8]
decode_depth = 1       # Faster decoding
max_decodes = 100      # More simultaneous
sensitivity = 0.3      # Strong signals only

[runtime]
worker_threads = 8     # Maximum performance
```

### DXpedition Support

Special features for DX operations:

- Directed CQ: `CQ NA W1ABC`
- Split operation support
- High-rate message handling

## Tips and Tricks

### Improving Decode Rate

1. **Optimize Audio Levels**:
   - Keep input around -20 to -10 dB
   - Avoid clipping (red indicators)
   - Use AGC if available

2. **Reduce Noise**:
   ```toml
   [dsp]
   noise_reduction = true
   noise_reduction_strength = 0.7
   ```

3. **Antenna Considerations**:
   - Use resonant antenna
   - Minimize local noise
   - Consider receive antennas

### Working DX

1. **Best Times**:
   - Gray line propagation
   - Check DX clusters
   - Monitor beacons

2. **Calling Strategy**:
   - Call slightly off frequency
   - Use directed CQ when appropriate
   - Be persistent but courteous

3. **Split Operation**:
   - Listen on DX frequency
   - Transmit up 1-2 kHz

### QRP Operation

Running low power (5W or less):

```toml
[ft8]
decode_depth = 3       # Maximum sensitivity
sensitivity = 0.95     # Decode everything

[dsp]
noise_reduction_strength = 0.9  # Strong NR
```

Tips:
- Choose optimal times
- Use efficient antennas
- Be patient

## Common Operations

### Exporting Logs

Export to ADIF format:

```bash
# Export all QSOs
./pancetta --export-adif ~/my_log.adi

# Export date range
./pancetta --export-adif ~/contest.adi --from 2024-01-01 --to 2024-01-31
```

### Backup Configuration

```bash
# Backup config and logs
tar -czf pancetta_backup.tar.gz \
  ~/.config/pancetta \
  ~/.local/share/pancetta
```

### Performance Monitoring

Check system performance:

```bash
# Enable metrics
PANCETTA_ENABLE_METRICS=true ./pancetta

# View metrics (different terminal)
curl http://localhost:9090/metrics
```

### Troubleshooting Decodes

No decodes? Check:

1. **Time Sync**:
   ```bash
   # Check system time
   timedatectl status
   ```

2. **Audio Path**:
   ```bash
   # Test with stub audio
   PANCETTA_STUB_AUDIO=1 ./pancetta
   ```

3. **Debug Mode**:
   ```bash
   RUST_LOG=debug ./pancetta
   ```

## Frequency Guide

### HF Bands

| Band | Frequency (MHz) | Best Times |
|------|----------------|------------|
| 160m | 1.840 | Night |
| 80m | 3.573 | Night/Morning |
| 40m | 7.074 | Night/Morning |
| 30m | 10.136 | Variable |
| 20m | 14.074 | Day |
| 17m | 18.100 | Day |
| 15m | 21.074 | Day |
| 12m | 24.915 | Day |
| 10m | 28.074 | Day/Openings |

### VHF/UHF

| Band | Frequency (MHz) | Notes |
|------|----------------|-------|
| 6m | 50.313 | Sporadic E |
| 2m | 144.174 | Local/Tropo |
| 70cm | 432.174 | Local |

## Best Practices

### Operating Ethics

1. **Listen before transmitting**
2. **Don't QRM ongoing QSOs**
3. **Keep power to minimum needed**
4. **Complete QSOs properly**
5. **Be courteous to all operators**

### Station Optimization

1. **Computer**:
   - Dedicated machine preferred
   - Minimize other applications
   - Disable power saving

2. **Audio Interface**:
   - Use isolation transformer
   - Set levels carefully
   - Monitor for RFI

3. **Antenna System**:
   - Resonant antenna best
   - Good ground system
   - Choke RFI at feed point

### Emergency Communications

FT8 can be valuable for EmComm:

- Works at very low signal levels
- Handles poor conditions
- Automated operation possible
- Low bandwidth requirements

Configure for EmComm:
```toml
[ft8]
decode_depth = 3          # Maximum reliability
max_decodes = 20          # Focus on strong signals

[logging]
auto_log = true           # Automatic record keeping
```

## Getting Help

### Resources

- **Pancetta Discord**: Real-time help
- **GitHub Issues**: Bug reports
- **Documentation**: `/docs` folder
- **FT8 Groups**: Operating practice

### Common Issues

See [Troubleshooting Guide](TROUBLESHOOTING.md) for:
- Audio problems
- No decodes
- High CPU usage
- Configuration issues

### Learning More

- **WSJT-X Documentation**: Protocol details
- **PSKReporter**: See where you're heard
- **DXWatch**: DX spotting
- **Ham Radio Forums**: Community help

---

**Enjoy using Pancetta for your FT8 operations!**

73 and good DX!

*Remember: Amateur radio is about experimentation, learning, and helping others. Have fun and be courteous on the air!*