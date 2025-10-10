#!/bin/bash
# RLENV Validation Script
# This script validates the patch playground by:
# 1. Running replay.sh on a sample file
# 2. Running build.sh to rebuild the application
# 3. Validating that the target executable modification time was updated
# 4. Running replay.sh again and checking that the result is the same

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPLAY_SCRIPT="$SCRIPT_DIR/replay.sh"
BUILD_SCRIPT="$SCRIPT_DIR/build.sh"
METADATA_DIR="$(dirname "$SCRIPT_DIR")"

# Check that required scripts exist
if [ ! -f "$REPLAY_SCRIPT" ]; then
    echo "Error: replay.sh not found at $REPLAY_SCRIPT"
    exit 1
fi

if [ ! -f "$BUILD_SCRIPT" ]; then
    echo "Error: build.sh not found at $BUILD_SCRIPT"
    exit 1
fi

# Read target executable from metadata if available
TARGET_EXEC=""
METADATA_FILE="$METADATA_DIR/../metadata.json"
if command -v jq >/dev/null 2>&1 && [ -f "$METADATA_FILE" ]; then
    TARGET_EXEC=$(jq -r '.target_executable // empty' "$METADATA_FILE" 2>/dev/null)
    if [ -n "$TARGET_EXEC" ] && [ "$TARGET_EXEC" != "null" ]; then
        echo "Target executable from metadata: $TARGET_EXEC"
        if [ ! -f "$TARGET_EXEC" ]; then
            echo "Warning: Target executable not found at $TARGET_EXEC"
            TARGET_EXEC=""
        fi
    else
        echo "No target executable found in metadata"
        TARGET_EXEC=""
    fi
else
    echo "Metadata file not found or jq not available: $METADATA_FILE"
fi

# If no target executable from metadata, try to find common executables
if [ -z "$TARGET_EXEC" ]; then
    echo "Attempting to find target executable automatically..."

    # Look for executables in common locations
    for potential in "/usr/local/bin/"* "/usr/bin/"* "/bin/"* "/opt/"*"/bin/"* "/app/"* "./main" "./app" "./server" "./client"; do
        if [ -x "$potential" ] && [ -f "$potential" ]; then
            # Skip common system binaries
            basename_exec=$(basename "$potential")
            if [[ ! "$basename_exec" =~ ^(sh|bash|cat|ls|grep|awk|sed|find|tar|gzip|curl|wget)$ ]]; then
                TARGET_EXEC="$potential"
                echo "Found potential target executable: $TARGET_EXEC"
                break
            fi
        fi
    done
fi

# Find a sample test case from our non-crashing directory
TESTSUITE_DIR="$(dirname "$SCRIPT_DIR")/testsuite"
SAMPLE_FILE=""

if [ -d "$TESTSUITE_DIR/testsuite" ] && [ "$(find "$TESTSUITE_DIR/testsuite" -type f -o -type l | head -1)" ]; then
    SAMPLE_FILE="$(find "$TESTSUITE_DIR/testsuite" -type f -o -type l | head -1)"
    echo "Using non-crashing test case: $(basename "$SAMPLE_FILE")"
else
    echo "No test cases found in $TESTSUITE_DIR, creating dummy testcase as fallback"
    DUMMY_FILE="/tmp/dummy_testcase"
    # Create a dummy testcase with 256 'A' characters
    printf 'A%.0s' {1..256} > "$DUMMY_FILE"
    SAMPLE_FILE="$DUMMY_FILE"
    echo "Using dummy testcase: $SAMPLE_FILE"
fi

# Step 1: Run replay.sh on the sample file and capture the exit code
echo "Step 1 - Running initial replay..."
"$REPLAY_SCRIPT" "$SAMPLE_FILE"
FIRST_REPLAY_EXIT_CODE=$?
echo "Initial replay exit code: $FIRST_REPLAY_EXIT_CODE"

# Step 2: Capture target executable modification time before build
BEFORE_MTIME=""
if [ -n "$TARGET_EXEC" ] && [ -f "$TARGET_EXEC" ]; then
    # Use stat command (cross-platform compatible)
    if stat -c %Y "$TARGET_EXEC" >/dev/null 2>&1; then
        BEFORE_MTIME=$(stat -c %Y "$TARGET_EXEC")
    elif stat -f %m "$TARGET_EXEC" >/dev/null 2>&1; then
        BEFORE_MTIME=$(stat -f %m "$TARGET_EXEC")
    fi
    echo "Target executable modification time before build: $BEFORE_MTIME"
fi

# Step 3: Run build.sh
echo "Step 3 - Running build script..."
if ! "$BUILD_SCRIPT"; then
    echo "ERROR: Build script failed"
    echo "Validation FAILED - build script returned non-zero exit code"
    exit 1
fi
echo "Build script completed successfully"

# Step 4: Check if target executable was modified
if [ -n "$TARGET_EXEC" ] && [ -f "$TARGET_EXEC" ]; then
    if [ -n "$BEFORE_MTIME" ]; then
        AFTER_MTIME=""
        if stat -c %Y "$TARGET_EXEC" >/dev/null 2>&1; then
            AFTER_MTIME=$(stat -c %Y "$TARGET_EXEC")
        elif stat -f %m "$TARGET_EXEC" >/dev/null 2>&1; then
            AFTER_MTIME=$(stat -f %m "$TARGET_EXEC")
        fi
        echo "Target executable modification time after build: $AFTER_MTIME"

        if [ "$BEFORE_MTIME" = "$AFTER_MTIME" ]; then
            echo "WARNING: Target executable was not modified by build process"
            echo "  This may indicate the build script is not working correctly"
        else
            echo "SUCCESS: Target executable was modified by build process"
        fi
    else
        echo "INFO: Could not capture modification time before build"
    fi
else
    echo "INFO: No target executable found for modification time validation"
    echo "  Validation will rely on build script exit code and replay consistency"
fi

# Step 5: Run replay.sh again and verify the exit code is the same
echo "Step 5 - Running second replay..."
"$REPLAY_SCRIPT" "$SAMPLE_FILE"
SECOND_REPLAY_EXIT_CODE=$?
echo "Second replay exit code: $SECOND_REPLAY_EXIT_CODE"

# Step 6: Compare exit codes
if [ "$FIRST_REPLAY_EXIT_CODE" -eq "$SECOND_REPLAY_EXIT_CODE" ]; then
    echo "Validation PASSED - replay exit codes match ($FIRST_REPLAY_EXIT_CODE)"
    exit 0
else
    echo "Validation FAILED - replay exit codes differ"
    echo "  First replay: $FIRST_REPLAY_EXIT_CODE"
    echo "  Second replay: $SECOND_REPLAY_EXIT_CODE"
    exit 1
fi
