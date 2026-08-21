#!/bin/bash
# Generate ZODER_CLI_SURFACE.txt from the live Clap command tree
# This script is used by CI to ensure CLI documentation stays synchronized with the actual CLI

set -euo pipefail
cd "$(dirname "$0")/.."

echo "Generating ZODER_CLI_SURFACE.txt from zoder CLI..."

# Find the zoder binary - either built locally or find it in target/ directory
echo "Building zoder to ensure we have the latest CLI..."
if ! cargo build --bin zoder --release --locked 2>/dev/null; then
    echo "Building unlocked due to potential Cargo.lock drift..."
    cargo build --bin zoder --release
fi

ZODER_BIN="./target/release/zoder"
if [ ! -f "$ZODER_BIN" ]; then
    echo "ERROR: zoder binary not found at $ZODER_BIN"
    exit 1
fi

# Generate the help output
echo "Generating CLI surface help output..."
$ZODER_BIN --help > /tmp/zoder-help.txt 2>/dev/null || $ZODER_BIN help > /tmp/zoder-help.txt 2>&1

# Get detailed subcommand help
echo "Generating detailed subcommand help..."

# Main commands and subcommands
{
    echo "=== MAIN COMMANDS ==="
    $ZODER_BIN --help | sed -n '/^Commands:/,/^Options:/p' | head -n -1

    echo -e "\n=== GLOBAL OPTIONS ==="
    $ZODER_BIN --help | sed -n '/^Options:/,/^Commands:/p' | head -n -1
} > /tmp/zoder-cli-surface-temp.txt

# Add completions subcommand help
echo -e "\n=== COMPLETIONS ===" >> /tmp/zoder-cli-surface-temp.txt
$ZODER_BIN completions --help 2>/dev/null || echo "completions command not available" >> /tmp/zoder-cli-surface-temp.txt

# Generate the final file
if [ -f "./scripts/ZODER_CLI_SURFACE.header.txt" ]; then
    cat ./scripts/ZODER_CLI_SURFACE.header.txt > ZODER_CLI_SURFACE.txt
fi

cat /tmp/zoder-cli-surface-temp.txt >> ZODER_CLI_SURFACE.txt

# Clean up
echo "CLI surface generated at ./ZODER_CLI_SURFACE.txt"
rm -f /tmp/zoder-help.txt /tmp/zoder-cli-surface-temp.txt
