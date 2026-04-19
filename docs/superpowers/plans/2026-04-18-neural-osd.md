# Neural OSD Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Train a neural network to predict which LDPC info bits are wrong after BP failure, reducing OSD trials from 125K to ~200 while improving sensitivity.

**Architecture:** Two phases: (1) Python training pipeline generates data, trains a 4-layer CNN, and exports weights as Rust const arrays. (2) Rust integration adds the forward pass, BP trajectory collection, and neural-guided OSD ordering. The model is ~20K parameters embedded directly in the binary.

**Tech Stack:** Python (PyTorch, numpy), Rust (pure — no ML framework dependencies)

**Spec:** `docs/superpowers/specs/2026-04-18-neural-osd-design.md`

---

## Phase 1: Python Training Pipeline

### Task 1: Training data generator

**Files:**
- Create: `training/neural_osd/generate_data.py`
- Create: `training/neural_osd/requirements.txt`
- Create: `training/neural_osd/README.md`

- [ ] **Step 1: Create requirements.txt**

```
torch>=2.0
numpy>=1.24
```

- [ ] **Step 2: Create the LDPC parity check matrix in Python**

The FT8 LDPC(174,91) parity check matrix is defined in `pancetta-ft8/src/ldpc.rs`. Read the `LDPC_GENERATOR` constant there — it's a packed representation of the 83×174 matrix. The generator needs this matrix to:
1. Encode random 91-bit payloads into 174-bit codewords
2. Run BP decoding (sum-product) on noisy received words

In `generate_data.py`, hardcode the parity check matrix. The matrix is sparse — each row has 3-7 nonzero entries. Read the Rust source to extract the exact indices. The format in Rust is `LDPC_GENERATOR: [[u8; 22]; 83]` where each row is a packed byte array (174 bits packed into 22 bytes).

```python
#!/usr/bin/env python3
"""Generate training data for Neural OSD model.

Creates simulated FT8 LDPC decoding failures with per-iteration
LLR trajectories and known error patterns.
"""
import numpy as np
import torch
import struct
import os

# FT8 LDPC parameters
N_CODEWORD = 174  # codeword length
K_INFO = 91       # information bits
N_PARITY = 83     # parity checks
BP_ITERATIONS = 25

# Parity check matrix H (83 x 174)
# Each row lists the column indices of the nonzero entries.
# Extract from pancetta-ft8/src/ldpc.rs LDPC_GENERATOR constant.
# This is the Nm array (variable nodes connected to each check node).
# Read the actual values from the Rust source.
def load_parity_check_matrix():
    """Load the FT8 LDPC parity check matrix.
    
    Read from pancetta-ft8/src/ldpc.rs — the LDPC_GENERATOR constant
    contains the 83x174 matrix in packed byte form (22 bytes per row).
    Parse it to get the sparse representation.
    """
    # The generator matrix in pancetta is stored as packed bytes.
    # Each of the 83 rows is 22 bytes = 176 bits (174 used).
    # Read the actual constant from the Rust source file.
    rust_src = os.path.join(
        os.path.dirname(__file__), '..', '..', 
        'pancetta-ft8', 'src', 'ldpc.rs'
    )
    
    H = np.zeros((N_PARITY, N_CODEWORD), dtype=np.float32)
    
    # Parse the Rust source to extract LDPC_GENERATOR
    # Look for the array literal and extract byte values
    with open(rust_src) as f:
        content = f.read()
    
    # Find the LDPC_GENERATOR constant
    # It's formatted as: pub const LDPC_GENERATOR: [[u8; 22]; 83] = [...]
    # Extract the 83 rows of 22 bytes each
    import re
    # Find the array content between the outermost brackets
    match = re.search(r'LDPC_GENERATOR:\s*\[\[u8;\s*22\];\s*83\]\s*=\s*\[([\s\S]*?)\];', content)
    if match is None:
        raise RuntimeError("Could not find LDPC_GENERATOR in ldpc.rs")
    
    array_text = match.group(1)
    # Parse each row: [0x.., 0x.., ...]
    row_pattern = re.compile(r'\[([\s\S]*?)\]')
    rows = row_pattern.findall(array_text)
    
    for row_idx, row_text in enumerate(rows[:N_PARITY]):
        # Parse hex bytes
        bytes_list = re.findall(r'0x([0-9a-fA-F]+)', row_text)
        for byte_idx, hex_val in enumerate(bytes_list[:22]):
            byte_val = int(hex_val, 16)
            for bit in range(8):
                col = byte_idx * 8 + bit
                if col < N_CODEWORD:
                    if (byte_val >> (7 - bit)) & 1:
                        H[row_idx, col] = 1.0
    
    return H


def encode_ldpc(info_bits, H):
    """Encode information bits using systematic LDPC.
    
    For FT8's systematic code, the first 91 bits are info bits.
    Parity bits are computed as p = H_info * info_bits (mod 2).
    """
    # FT8 LDPC is systematic: codeword = [info | parity]
    # p = H[:,0:91] @ info mod 2
    parity = (H[:, :K_INFO] @ info_bits) % 2
    codeword = np.concatenate([info_bits, parity])
    return codeword.astype(np.float32)


def bp_decode_with_trajectory(llrs, H, max_iter=BP_ITERATIONS):
    """Sum-product BP decoding, returning per-iteration LLR trajectory.
    
    Args:
        llrs: channel LLRs (174,)
        H: parity check matrix (83, 174)
        max_iter: number of BP iterations
    
    Returns:
        trajectory: (max_iter, 174) LLR values at each iteration
        converged: bool
        final_llrs: (174,) final output LLRs
    """
    n_checks, n_vars = H.shape
    trajectory = np.zeros((max_iter, n_vars), dtype=np.float32)
    
    # Build sparse connectivity
    # For each check node, list of connected variable nodes
    check_to_vars = []
    for c in range(n_checks):
        check_to_vars.append(np.where(H[c] > 0)[0].tolist())
    
    # For each variable node, list of connected check nodes
    var_to_checks = []
    for v in range(n_vars):
        var_to_checks.append(np.where(H[:, v] > 0)[0].tolist())
    
    # Messages: v2c[c][idx] and c2v[c][idx]
    # Use dicts indexed by (check, var)
    v2c = {}
    c2v = {}
    
    # Initialize v2c with channel LLRs
    for c in range(n_checks):
        for v in check_to_vars[c]:
            v2c[(c, v)] = llrs[v]
            c2v[(c, v)] = 0.0
    
    output_llrs = llrs.copy()
    
    for iteration in range(max_iter):
        # Check node update (sum-product)
        for c in range(n_checks):
            vars_c = check_to_vars[c]
            for v in vars_c:
                product = 1.0
                for u in vars_c:
                    if u != v:
                        x = np.clip(v2c[(c, u)] / 2.0, -10, 10)
                        product *= np.tanh(x)
                product = np.clip(product, -0.9999999, 0.9999999)
                c2v[(c, v)] = 2.0 * np.arctanh(product)
        
        # Variable node update
        for v in range(n_vars):
            total = llrs[v]
            for c in var_to_checks[v]:
                total += c2v[(c, v)]
            output_llrs[v] = total
            
            for c in var_to_checks[v]:
                v2c[(c, v)] = total - c2v[(c, v)]
        
        trajectory[iteration] = output_llrs.copy()
        
        # Check syndrome
        hard = (output_llrs < 0).astype(np.float32)
        syndrome = (H @ hard) % 2
        if np.sum(syndrome) == 0:
            # Fill remaining iterations with final LLRs
            for remaining in range(iteration + 1, max_iter):
                trajectory[remaining] = output_llrs.copy()
            return trajectory, True, output_llrs
    
    return trajectory, False, output_llrs


def generate_sample(H, snr_db):
    """Generate one training sample.
    
    Returns:
        trajectory: (25, 174) or None if BP converged
        error_pattern: (91,) binary — which info bits are wrong
    """
    # Random info bits
    info_bits = np.random.randint(0, 2, K_INFO).astype(np.float32)
    
    # Encode
    codeword = encode_ldpc(info_bits, H)
    
    # BPSK modulate: 0 -> +1, 1 -> -1
    modulated = 1.0 - 2.0 * codeword
    
    # Add AWGN noise
    # SNR_dB = 10*log10(Eb/N0), Eb/N0 = 10^(SNR/10)
    # For BPSK: sigma^2 = 1/(2*R*Eb/N0) where R = K/N
    rate = K_INFO / N_CODEWORD
    eb_n0 = 10.0 ** (snr_db / 10.0)
    sigma = np.sqrt(1.0 / (2.0 * rate * eb_n0))
    noise = np.random.randn(N_CODEWORD).astype(np.float32) * sigma
    received = modulated + noise
    
    # Channel LLRs: 2*y/sigma^2
    channel_llrs = (2.0 * received / (sigma ** 2)).astype(np.float32)
    
    # BP decode with trajectory
    trajectory, converged, final_llrs = bp_decode_with_trajectory(channel_llrs, H)
    
    if converged:
        return None  # Don't need this sample — BP succeeded
    
    # Error pattern: which info bits are wrong in BP's hard decision?
    hard_decision = (final_llrs[:K_INFO] < 0).astype(np.float32)
    error_pattern = (hard_decision != info_bits).astype(np.float32)
    
    return trajectory, error_pattern


def main():
    import argparse
    parser = argparse.ArgumentParser(description='Generate Neural OSD training data')
    parser.add_argument('--n-train', type=int, default=100_000)
    parser.add_argument('--n-val', type=int, default=10_000)
    parser.add_argument('--snr-min', type=float, default=-28.0)
    parser.add_argument('--snr-max', type=float, default=-18.0)
    parser.add_argument('--output-dir', type=str, default='data')
    parser.add_argument('--seed', type=int, default=42)
    args = parser.parse_args()
    
    np.random.seed(args.seed)
    os.makedirs(args.output_dir, exist_ok=True)
    
    print("Loading parity check matrix...")
    H = load_parity_check_matrix()
    print(f"H shape: {H.shape}, nonzeros: {int(H.sum())}")
    
    for split, n_target in [('train', args.n_train), ('val', args.n_val)]:
        print(f"\nGenerating {split} set ({n_target} samples)...")
        trajectories = []
        errors = []
        n_attempts = 0
        
        while len(trajectories) < n_target:
            snr = np.random.uniform(args.snr_min, args.snr_max)
            result = generate_sample(H, snr)
            n_attempts += 1
            
            if result is not None:
                traj, err = result
                trajectories.append(traj)
                errors.append(err)
                
                if len(trajectories) % 10000 == 0:
                    print(f"  {len(trajectories)}/{n_target} "
                          f"(attempts: {n_attempts}, "
                          f"fail rate: {len(trajectories)/n_attempts:.1%})")
        
        X = np.stack(trajectories)  # (N, 25, 174)
        Y = np.stack(errors)        # (N, 91)
        
        np.save(os.path.join(args.output_dir, f'{split}_X.npy'), X)
        np.save(os.path.join(args.output_dir, f'{split}_Y.npy'), Y)
        print(f"  Saved {split}: X={X.shape}, Y={Y.shape}")
        print(f"  Avg errors per sample: {Y.sum(axis=1).mean():.1f}")


if __name__ == '__main__':
    main()
```

- [ ] **Step 3: Create README.md**

```markdown
# Neural OSD Training Pipeline

Trains a CNN to predict which LDPC info bits are wrong after BP failure,
enabling smart OSD trial ordering that reduces trials from 125K to ~200.

## Setup

```bash
cd training/neural_osd
pip install -r requirements.txt
```

## Pipeline

### 1. Generate training data
```bash
python generate_data.py --n-train 100000 --n-val 10000 --output-dir data
```
Takes ~30-60 minutes. Generates ~2GB of data.

### 2. Train the model
```bash
python train.py --data-dir data --epochs 50 --output model.pt
```
Takes ~10 minutes on CPU.

### 3. Export weights to Rust
```bash
python export_weights.py --model model.pt --output ../../pancetta-ft8/src/neural_osd_weights.rs
```
Generates a Rust source file with const weight arrays (~80KB).

## Model Architecture

4-layer CNN (Decoding Information Aggregation):
- Input: 25 BP iterations × 174 codeword bits
- Conv1D(25→32, k=3) + ReLU
- Conv1D(32→16, k=3) + ReLU
- Conv1D(16→1, k=1)
- Linear(174→91) + sigmoid
- Output: 91 probabilities (which info bits are wrong)

Based on: "Boosting OSD of Short LDPC Codes with Neural Networks"
(Li et al., IEEE Communications Letters, 2024, arxiv 2404.14165)
```

- [ ] **Step 4: Verify data generation**

```bash
cd training/neural_osd
pip install -r requirements.txt
python generate_data.py --n-train 1000 --n-val 100 --output-dir data_test
```

Expected: `data_test/train_X.npy` (1000, 25, 174) and `data_test/train_Y.npy` (1000, 91).

- [ ] **Step 5: Commit**

```bash
git add training/neural_osd/
git commit -m "feat: neural OSD training data generator for FT8 LDPC(174,91)"
```

---

### Task 2: Train the DIA model

**Files:**
- Create: `training/neural_osd/train.py`

- [ ] **Step 1: Create train.py**

```python
#!/usr/bin/env python3
"""Train the DIA (Decoding Information Aggregation) model for Neural OSD."""
import numpy as np
import torch
import torch.nn as nn
import torch.optim as optim
from torch.utils.data import TensorDataset, DataLoader
import os


class DIAModel(nn.Module):
    """Decoding Information Aggregation model.
    
    Takes BP iteration trajectory (25 x 174) and predicts
    which of the 91 info bits are wrong (binary classification).
    """
    def __init__(self):
        super().__init__()
        # Conv1D operates on the "channel" dimension (25 BP iterations)
        # treating the 174 codeword positions as the sequence length
        self.conv1 = nn.Conv1d(25, 32, kernel_size=3, padding=1)
        self.conv2 = nn.Conv1d(32, 16, kernel_size=3, padding=1)
        self.conv3 = nn.Conv1d(16, 1, kernel_size=1)
        self.linear = nn.Linear(174, 91)
        self.relu = nn.ReLU()
    
    def forward(self, x):
        # x: (batch, 25, 174)
        h = self.relu(self.conv1(x))   # (batch, 32, 174)
        h = self.relu(self.conv2(h))   # (batch, 16, 174)
        h = self.conv3(h).squeeze(1)   # (batch, 174)
        h = torch.sigmoid(self.linear(h))  # (batch, 91)
        return h


def train(args):
    print("Loading data...")
    train_X = np.load(os.path.join(args.data_dir, 'train_X.npy'))
    train_Y = np.load(os.path.join(args.data_dir, 'train_Y.npy'))
    val_X = np.load(os.path.join(args.data_dir, 'val_X.npy'))
    val_Y = np.load(os.path.join(args.data_dir, 'val_Y.npy'))
    
    print(f"Train: {train_X.shape}, Val: {val_X.shape}")
    print(f"Avg errors/sample (train): {train_Y.sum(axis=1).mean():.2f}")
    
    train_ds = TensorDataset(
        torch.from_numpy(train_X),
        torch.from_numpy(train_Y),
    )
    val_ds = TensorDataset(
        torch.from_numpy(val_X),
        torch.from_numpy(val_Y),
    )
    
    train_loader = DataLoader(train_ds, batch_size=args.batch_size, shuffle=True)
    val_loader = DataLoader(val_ds, batch_size=args.batch_size)
    
    model = DIAModel()
    print(f"Model parameters: {sum(p.numel() for p in model.parameters()):,}")
    
    criterion = nn.BCELoss()
    optimizer = optim.Adam(model.parameters(), lr=args.lr, weight_decay=1e-4)
    scheduler = optim.lr_scheduler.ReduceLROnPlateau(optimizer, patience=5, factor=0.5)
    
    best_val_loss = float('inf')
    patience_counter = 0
    
    for epoch in range(args.epochs):
        # Train
        model.train()
        train_loss = 0.0
        for X_batch, Y_batch in train_loader:
            optimizer.zero_grad()
            pred = model(X_batch)
            loss = criterion(pred, Y_batch)
            loss.backward()
            optimizer.step()
            train_loss += loss.item() * X_batch.size(0)
        train_loss /= len(train_ds)
        
        # Validate
        model.eval()
        val_loss = 0.0
        correct_bits = 0
        total_bits = 0
        with torch.no_grad():
            for X_batch, Y_batch in val_loader:
                pred = model(X_batch)
                loss = criterion(pred, Y_batch)
                val_loss += loss.item() * X_batch.size(0)
                
                # Bit-level accuracy
                pred_binary = (pred > 0.5).float()
                correct_bits += (pred_binary == Y_batch).sum().item()
                total_bits += Y_batch.numel()
        
        val_loss /= len(val_ds)
        bit_accuracy = correct_bits / total_bits
        scheduler.step(val_loss)
        
        print(f"Epoch {epoch+1:3d}: train_loss={train_loss:.4f} "
              f"val_loss={val_loss:.4f} bit_acc={bit_accuracy:.4f} "
              f"lr={optimizer.param_groups[0]['lr']:.2e}")
        
        if val_loss < best_val_loss:
            best_val_loss = val_loss
            torch.save(model.state_dict(), args.output)
            patience_counter = 0
            print(f"  -> Saved best model (val_loss={val_loss:.4f})")
        else:
            patience_counter += 1
            if patience_counter >= args.patience:
                print(f"Early stopping at epoch {epoch+1}")
                break
    
    print(f"\nBest validation loss: {best_val_loss:.4f}")
    print(f"Model saved to: {args.output}")


if __name__ == '__main__':
    import argparse
    parser = argparse.ArgumentParser()
    parser.add_argument('--data-dir', type=str, default='data')
    parser.add_argument('--output', type=str, default='model.pt')
    parser.add_argument('--epochs', type=int, default=50)
    parser.add_argument('--batch-size', type=int, default=256)
    parser.add_argument('--lr', type=float, default=1e-3)
    parser.add_argument('--patience', type=int, default=10)
    args = parser.parse_args()
    train(args)
```

- [ ] **Step 2: Generate full training data and train**

```bash
cd training/neural_osd
python generate_data.py --n-train 100000 --n-val 10000 --output-dir data
python train.py --data-dir data --epochs 50 --output model.pt
```

Expected: model.pt saved, validation bit accuracy >80%.

- [ ] **Step 3: Commit**

```bash
git add training/neural_osd/train.py
git commit -m "feat: DIA model training script for Neural OSD"
```

---

### Task 3: Export weights to Rust

**Files:**
- Create: `training/neural_osd/export_weights.py`

- [ ] **Step 1: Create export_weights.py**

```python
#!/usr/bin/env python3
"""Export trained DIA model weights to a Rust source file."""
import torch
import numpy as np
import argparse
import os


def format_array(name, values, per_line=8):
    """Format a float array as a Rust const."""
    lines = [f"pub const {name}: &[f32] = &["]
    for i in range(0, len(values), per_line):
        chunk = values[i:i+per_line]
        line = "    " + ", ".join(f"{v:.8e}" for v in chunk) + ","
        lines.append(line)
    lines.append("];")
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--model', type=str, default='model.pt')
    parser.add_argument('--output', type=str, 
                        default='../../pancetta-ft8/src/neural_osd_weights.rs')
    args = parser.parse_args()
    
    # Load model
    from train import DIAModel
    model = DIAModel()
    model.load_state_dict(torch.load(args.model, map_location='cpu', weights_only=True))
    model.eval()
    
    # Extract weights
    weights = {}
    for name, param in model.named_parameters():
        weights[name] = param.detach().numpy().flatten().tolist()
    
    # Generate Rust source
    lines = [
        "// Auto-generated by training/neural_osd/export_weights.py",
        "// DO NOT EDIT — regenerate with:",
        "//   cd training/neural_osd && python export_weights.py",
        "",
        "#![allow(clippy::excessive_precision)]",
        "",
    ]
    
    # Conv1: weight shape (32, 25, 3), bias shape (32,)
    lines.append(format_array("CONV1_WEIGHT", weights['conv1.weight']))
    lines.append("")
    lines.append(format_array("CONV1_BIAS", weights['conv1.bias']))
    lines.append("")
    
    # Conv2: weight shape (16, 32, 3), bias shape (16,)
    lines.append(format_array("CONV2_WEIGHT", weights['conv2.weight']))
    lines.append("")
    lines.append(format_array("CONV2_BIAS", weights['conv2.bias']))
    lines.append("")
    
    # Conv3: weight shape (1, 16, 1), bias shape (1,)
    lines.append(format_array("CONV3_WEIGHT", weights['conv3.weight']))
    lines.append("")
    lines.append(format_array("CONV3_BIAS", weights['conv3.bias']))
    lines.append("")
    
    # Linear: weight shape (91, 174), bias shape (91,)
    lines.append(format_array("LINEAR_WEIGHT", weights['linear.weight']))
    lines.append("")
    lines.append(format_array("LINEAR_BIAS", weights['linear.bias']))
    
    rust_code = "\n".join(lines) + "\n"
    
    os.makedirs(os.path.dirname(args.output), exist_ok=True)
    with open(args.output, 'w') as f:
        f.write(rust_code)
    
    total_params = sum(len(v) for v in weights.values())
    file_size = len(rust_code)
    print(f"Exported {total_params:,} parameters to {args.output}")
    print(f"File size: {file_size:,} bytes ({file_size/1024:.1f} KB)")


if __name__ == '__main__':
    main()
```

- [ ] **Step 2: Run export**

```bash
cd training/neural_osd
python export_weights.py --model model.pt --output ../../pancetta-ft8/src/neural_osd_weights.rs
```

Expected: `pancetta-ft8/src/neural_osd_weights.rs` created with const arrays (~80KB).

- [ ] **Step 3: Verify the generated file compiles**

```bash
touch pancetta-ft8/src/neural_osd_weights.rs
# Add to lib.rs temporarily to check compilation:
# mod neural_osd_weights;
cargo build -p pancetta-ft8 2>&1 | tail -5
```

- [ ] **Step 4: Commit**

```bash
git add training/neural_osd/export_weights.py pancetta-ft8/src/neural_osd_weights.rs
git commit -m "feat: export DIA model weights to Rust const arrays"
```

---

## Phase 2: Rust Integration

### Task 4: Neural OSD forward pass in Rust

**Files:**
- Create: `pancetta-ft8/src/neural_osd.rs`
- Modify: `pancetta-ft8/src/lib.rs`

- [ ] **Step 1: Create neural_osd.rs with forward pass**

```rust
//! Neural OSD: CNN-guided bit-flip ordering for OSD decoding.
//!
//! Uses a trained DIA (Decoding Information Aggregation) model to predict
//! which LDPC info bits are most likely wrong after BP failure. The predicted
//! probabilities replace |LLR|-based ordering in OSD, reducing trials from
//! ~125K to ~200.

mod neural_osd_weights;

use neural_osd_weights::*;

const N_CODEWORD: usize = 174;
const K_INFO: usize = 91;
const BP_ITERS: usize = 25;
const CONV1_OUT: usize = 32;
const CONV2_OUT: usize = 16;

/// Predict which info bits are most likely wrong after BP failure.
///
/// Input: LLR trajectory from 25 BP iterations (trajectory[iter][bit]).
/// Output: 91 probabilities — higher means more likely wrong.
pub fn predict_error_bits(trajectory: &[[f32; N_CODEWORD]; BP_ITERS]) -> [f32; K_INFO] {
    // Layer 1: Conv1D(25→32, kernel=3, padding=1) + ReLU
    let mut h1 = [[0.0f32; N_CODEWORD]; CONV1_OUT];
    for out_ch in 0..CONV1_OUT {
        for pos in 0..N_CODEWORD {
            let mut sum = CONV1_BIAS[out_ch];
            for in_ch in 0..BP_ITERS {
                for k in 0..3usize {
                    let p = pos as isize + k as isize - 1;
                    if p >= 0 && (p as usize) < N_CODEWORD {
                        sum += trajectory[in_ch][p as usize]
                            * CONV1_WEIGHT[out_ch * BP_ITERS * 3 + in_ch * 3 + k];
                    }
                }
            }
            h1[out_ch][pos] = sum.max(0.0); // ReLU
        }
    }

    // Layer 2: Conv1D(32→16, kernel=3, padding=1) + ReLU
    let mut h2 = [[0.0f32; N_CODEWORD]; CONV2_OUT];
    for out_ch in 0..CONV2_OUT {
        for pos in 0..N_CODEWORD {
            let mut sum = CONV2_BIAS[out_ch];
            for in_ch in 0..CONV1_OUT {
                for k in 0..3usize {
                    let p = pos as isize + k as isize - 1;
                    if p >= 0 && (p as usize) < N_CODEWORD {
                        sum += h1[in_ch][p as usize]
                            * CONV2_WEIGHT[out_ch * CONV1_OUT * 3 + in_ch * 3 + k];
                    }
                }
            }
            h2[out_ch][pos] = sum.max(0.0); // ReLU
        }
    }

    // Layer 3: Conv1D(16→1, kernel=1) → squeeze
    let mut h3 = [0.0f32; N_CODEWORD];
    for pos in 0..N_CODEWORD {
        let mut sum = CONV3_BIAS[0];
        for in_ch in 0..CONV2_OUT {
            sum += h2[in_ch][pos] * CONV3_WEIGHT[in_ch];
        }
        h3[pos] = sum;
    }

    // Layer 4: Linear(174→91) + sigmoid
    let mut output = [0.0f32; K_INFO];
    for i in 0..K_INFO {
        let mut sum = LINEAR_BIAS[i];
        for j in 0..N_CODEWORD {
            sum += h3[j] * LINEAR_WEIGHT[i * N_CODEWORD + j];
        }
        output[i] = 1.0 / (1.0 + (-sum).exp()); // sigmoid
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_predict_error_bits_runs() {
        // Verify the forward pass doesn't crash with random input
        let mut trajectory = [[0.0f32; N_CODEWORD]; BP_ITERS];
        for iter in 0..BP_ITERS {
            for bit in 0..N_CODEWORD {
                trajectory[iter][bit] = (iter as f32 - 12.0) * 0.1;
            }
        }
        let probs = predict_error_bits(&trajectory);
        
        // Output should be 91 probabilities in [0, 1]
        assert_eq!(probs.len(), K_INFO);
        for &p in &probs {
            assert!(p >= 0.0 && p <= 1.0, "Probability {} out of range", p);
        }
    }

    #[test]
    fn test_predict_deterministic() {
        let trajectory = [[1.0f32; N_CODEWORD]; BP_ITERS];
        let p1 = predict_error_bits(&trajectory);
        let p2 = predict_error_bits(&trajectory);
        assert_eq!(p1, p2, "Forward pass should be deterministic");
    }
}
```

- [ ] **Step 2: Add to lib.rs**

In `pancetta-ft8/src/lib.rs`, add:
```rust
pub mod neural_osd;
```

- [ ] **Step 3: Build and test**

```bash
touch pancetta-ft8/src/neural_osd.rs pancetta-ft8/src/lib.rs
cargo test -p pancetta-ft8 --lib -- neural_osd 2>&1 | tail -10
```

Expected: 2 tests pass (forward pass runs, output in valid range).

- [ ] **Step 4: Commit**

```bash
git add pancetta-ft8/src/neural_osd.rs pancetta-ft8/src/lib.rs
git commit -m "feat: neural OSD forward pass — pure Rust CNN inference"
```

---

### Task 5: BP trajectory collection

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs` (LdpcDecoder)

- [ ] **Step 1: Add trajectory-collecting BP variant**

Find `LdpcDecoder` in `decoder.rs`. Add a method alongside `belief_propagation`:

```rust
/// Belief propagation with per-iteration LLR trajectory collection.
/// Returns (final_llrs, Some(trajectory)) when BP fails to converge.
/// Returns (final_llrs, None) when BP converges (no trajectory needed).
fn belief_propagation_with_trajectory(
    &self,
    channel_llrs: &[f32],
) -> Ft8Result<(Vec<f32>, Option<[[f32; 174]; 25]>)> {
    // Identical to belief_propagation, except:
    // 1. Allocate trajectory array at the start
    // 2. At the end of each iteration, snapshot output_llrs into trajectory[iter]
    // 3. If BP converges, return (llrs, None) — discard trajectory
    // 4. If BP doesn't converge, return (llrs, Some(trajectory))
    
    // ... copy the entire belief_propagation body ...
    // Add after each iteration's variable node update:
    //   trajectory[iteration] = output_llrs;
    // And at the early return on convergence:
    //   return Ok((output_llrs.to_vec(), None));
}
```

Read the existing `belief_propagation` method and create the trajectory variant. The key additions are:
1. `let mut trajectory = [[0.0f32; 174]; 25];` at the start
2. `trajectory[_iteration] = output_llrs;` after each iteration
3. Return `None` trajectory on convergence, `Some(trajectory)` on failure

- [ ] **Step 2: Build and test**

```bash
touch pancetta-ft8/src/decoder.rs
cargo test -p pancetta-ft8 --lib 2>&1 | tail -5
```

- [ ] **Step 3: Commit**

```bash
git add pancetta-ft8/src/decoder.rs
git commit -m "feat: BP trajectory collection for neural OSD input"
```

---

### Task 6: Wire neural OSD into decode pipeline

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs` (decode_soft)
- Modify: `pancetta-ft8/src/osd.rs` (accept external ordering)

- [ ] **Step 1: Add neural ordering parameter to OSD**

In `pancetta-ft8/src/osd.rs`, modify `OsdDecoder::decode` to accept an optional neural ordering:

```rust
pub fn decode(
    &self,
    llrs: &[f32; 174],
    neural_ordering: Option<&[f32; 91]>,
) -> Option<BitVec> {
    // Step 1-2: Sort bits by reliability
    // If neural_ordering is Some, sort info bits by predicted error probability
    // (descending — most likely wrong first) instead of |LLR| (ascending).
    
    // ... existing code to build sorted_indices ...
    // Replace the sort key:
    if let Some(probs) = neural_ordering {
        // Sort by neural-predicted error probability (highest first)
        indices[..K_INFO].sort_by(|&a, &b| {
            probs[b].partial_cmp(&probs[a]).unwrap_or(std::cmp::Ordering::Equal)
        });
    } else {
        // Original: sort by |LLR| (smallest first = least reliable)
        // ... existing sort code ...
    }
    
    // Steps 3-7 remain unchanged: Gaussian elimination, hard decision,
    // perturbation trials, CRC check
}
```

Update all call sites of `osd.decode()` to pass `None` for the neural ordering parameter (backward compatible).

- [ ] **Step 2: Wire neural OSD into decode_soft**

In `decoder.rs`, modify `decode_soft` in `LdpcDecoder`:

```rust
pub fn decode_soft(&self, llrs: &[f32]) -> Ft8Result<BitVec> {
    // Use trajectory-collecting BP
    let (decoded_llrs, trajectory) = self.belief_propagation_with_trajectory(llrs)?;
    
    let bp_converged = {
        let arr: &[f32; 174] = decoded_llrs[..174].try_into().unwrap();
        self.check_syndrome_fast(arr)
    };
    
    if bp_converged {
        return self.llrs_to_bits(&decoded_llrs);
    }
    
    // BP failed — try OSD with neural ordering if trajectory available
    if let Some(ref osd) = self.osd {
        let llr_arr: &[f32; 174] = decoded_llrs[..174].try_into().unwrap();
        let parity_errors = self.count_parity_errors(llr_arr);
        
        // With neural ordering, we can afford a more generous gate
        const MAX_PARITY_ERRORS_FOR_OSD: usize = 5;
        
        if parity_errors <= MAX_PARITY_ERRORS_FOR_OSD {
            // Compute neural ordering if trajectory is available
            let neural_ordering = trajectory.as_ref().map(|traj| {
                crate::neural_osd::predict_error_bits(traj)
            });
            
            if let Some(codeword) = osd.decode(
                llr_arr,
                neural_ordering.as_ref(),
            ) {
                return Ok(codeword);
            }
        }
    }
    
    self.llrs_to_bits(&decoded_llrs)
}
```

- [ ] **Step 3: Build and run all tests**

```bash
touch pancetta-ft8/src/decoder.rs pancetta-ft8/src/osd.rs
cargo test -p pancetta-ft8 --lib 2>&1 | tail -5
cargo test -p pancetta -- --test-threads=1 2>&1 | grep "test result" | head -5
```

- [ ] **Step 4: Run cross-validation benchmark**

```bash
cargo test -p pancetta-ft8 --test wav_decode_tests -- test_cross_validate --nocapture 2>&1 | grep -E "^[a-z].*ours=|Overall"
```

Expected: sensitivity improvement (more decodes from the wider OSD gate + smarter ordering).

- [ ] **Step 5: Run speed benchmark**

```bash
time cargo test -p pancetta-ft8 --release --test wav_decode_tests -- test_cross_validate 2>&1 | grep "finished in"
```

Expected: comparable or faster than brute-force OSD (fewer trials despite wider gate).

- [ ] **Step 6: Commit**

```bash
git add pancetta-ft8/src/decoder.rs pancetta-ft8/src/osd.rs
git commit -m "feat: wire neural OSD into decode pipeline — smarter trial ordering

Neural-predicted bit-flip probabilities replace |LLR|-magnitude
ordering in OSD. Parity error gate widened from ≤3 to ≤5 since
neural ordering targets the right bits in ~200 trials."
```

---

## Execution Notes

- **Phase 1 (Tasks 1-3)** requires Python with PyTorch. Training takes ~30-60 minutes for data generation and ~10 minutes for model training on CPU.
- **Phase 2 (Tasks 4-6)** is pure Rust. Task 4 is independent. Task 5 and 6 modify decoder.rs sequentially.
- The training pipeline reads the LDPC parity matrix from `pancetta-ft8/src/ldpc.rs` — if that file's format changes, `generate_data.py` needs updating.
- The exported weights file `neural_osd_weights.rs` is auto-generated and should not be manually edited. It's ~80KB of Rust source.
- **If training data quality is poor** (model accuracy <70%), increase training samples to 500K or adjust SNR range.
- **Key metric:** the cross-validation benchmark should show improvement from the wider OSD gate (≤5 parity errors instead of ≤3) without an increase in false positives.
