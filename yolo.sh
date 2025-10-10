#!/bin/bash
set -euo pipefail

# RLENV Yolo Script
# This script invokes Claude Code with yolo-mode flags and runs the prepare-patch command
# to automatically create a single-stage Dockerfile and extract build commands

# Check if claude command is available
if ! command -v claude &> /dev/null; then
    echo "Error: Claude Code is not installed or not in PATH"
    echo "Please install Claude Code from https://claude.ai/code"
    exit 1
fi

# Check if we're in a prepared patch repository
if [ ! -f "CLAUDE.md" ] || [ ! -d ".claude" ] || [ ! -d "rlenv" ]; then
    echo "Error: This doesn't appear to be a prepared patch repository"
    echo "Make sure you're in the root of a repository prepared with 'rlenv prepare-patch'"
    exit 1
fi

# Run Claude Code with yolo-mode flags and the prepare-patch command
claude --dangerously-skip-permissions -p "/prepare-patch" --verbose --output-format stream-json | jq

echo ""
echo "RLENV: Yolo mode completed!"
echo "Next steps:"
echo "  1. Review the generated Dockerfile.rlenv"
echo "  2. Review the generated rlenv/mayhem/data/scripts/build.sh"
echo "  3. Test your changes:"
echo "     rlenv patch"
echo "     docker run --rm IMAGE /rlenv/mayhem/data/scripts/replay.sh /data/testsuite/crashing/[hash]"
echo "     docker run --rm IMAGE /rlenv/mayhem/data/scripts/build.sh"
