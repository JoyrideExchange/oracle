#!/bin/bash
# Deploy script for Joyride Oracle Service
# Run this on your EC2 instance

set -e

echo "=== Deploying Joyride Oracle ==="

# Build the release binary
echo "Building release binary..."
cargo build --release

# Stop existing service if running
if systemctl is-active --quiet joyride-oracle; then
    echo "Stopping existing service..."
    sudo systemctl stop joyride-oracle
fi

# Copy binary to /usr/local/bin
echo "Installing binary..."
sudo cp target/release/joyride-oracle /usr/local/bin/
sudo chmod +x /usr/local/bin/joyride-oracle

# Install systemd service if not exists
if [ ! -f /etc/systemd/system/joyride-oracle.service ]; then
    echo "Installing systemd service..."
    sudo cp joyride-oracle.service /etc/systemd/system/
    sudo systemctl daemon-reload
    sudo systemctl enable joyride-oracle
fi

# Start the service
echo "Starting service..."
sudo systemctl start joyride-oracle

# Show status
echo ""
echo "=== Deployment complete ==="
sudo systemctl status joyride-oracle --no-pager

echo ""
echo "View logs with: journalctl -u joyride-oracle -f"
