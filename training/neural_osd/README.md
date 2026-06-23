# Neural OSD Training Data Generator

Generates training data for a CNN that predicts which LDPC info bits are wrong
after belief propagation (BP) failure in FT8 decoding.

## FT8 LDPC Code

- Code: (174, 91) — 91 info bits, 83 parity bits
- Input: LLR trajectory matrix [25 x 174] from BP iterations
- Target: error pattern [91] binary — which info bits are wrong

## Setup

```bash
cd training/neural_osd
python3 -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
```

## Generate Training Data

```bash
# Small test run
python generate_data.py --n-train 1000 --n-val 100 --output-dir data_test

# Full training set
python generate_data.py --n-train 100000 --n-val 10000 --output-dir data
```

## Output Format

- `train_X.npy` — shape (N, 25, 174), float32 LLR trajectories
- `train_Y.npy` — shape (N, 91), float32 error patterns (0/1)
- `val_X.npy` — validation trajectories
- `val_Y.npy` — validation error patterns

## Options

| Flag | Default | Description |
|------|---------|-------------|
| `--n-train` | 100000 | Number of training samples |
| `--n-val` | 10000 | Number of validation samples |
| `--output-dir` | `data` | Output directory |
| `--snr-low` | -28.0 | Low end of SNR range (dB) |
| `--snr-high` | -18.0 | High end of SNR range (dB) |
| `--seed` | 42 | Random seed |
