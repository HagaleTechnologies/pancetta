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

        model.eval()
        val_loss = 0.0
        correct_bits = 0
        total_bits = 0
        with torch.no_grad():
            for X_batch, Y_batch in val_loader:
                pred = model(X_batch)
                loss = criterion(pred, Y_batch)
                val_loss += loss.item() * X_batch.size(0)
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
