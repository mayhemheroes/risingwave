# RLENV Patch Playground

## Run Information
- Mayhem Run: mayhemheroes/risingwave/fuzz-parse-sql/131
- Git Commit: 0e8532d12f11b22c5e6105cf276703df23d83ed7
- Original Image: ghcr.io/mayhemheroes/risingwave:main
- CI URL: https://github.com:443/mayhemheroes/risingwave/actions/runs/3361928584

## Build Configuration
- Dockerfile: mayhem/mayhem.Dockerfile
- Build Context: .
- Build Command: `docker build -f mayhem/mayhem.Dockerfile .`

## Test Data
Test data will be downloaded and embedded during the `rlenv patch` command.
After running `rlenv patch`, the `rlenv/` directory will contain:
- `rlenv/mayhem/data/testsuite/crashing/` - Crashing test cases
- `rlenv/mayhem/data/testsuite/testsuite/` - Non-crashing test cases
- `rlenv/mayhem/data/testsuite/metadata/` - Test case metadata
- `rlenv/mayhem/data/testsuite/defects/` - Organized by defect type
- `rlenv/mayhem/data/scripts/replay.sh` - Script to replay test cases
- `rlenv/mayhem/data/scripts/build.sh` - Script to rebuild the application
- `rlenv/source/risingwave/` - Source code (build from this location)
- `rlenv/problem/` - Problem metadata (prompt, id, type)
- `rlenv/mayhem/` - Mayhem metadata (url, meta)

## Tasks for Patching Playground

1. **Analyze the Dockerfile**: Understand the current multi-stage build structure
2. **Create `Dockerfile.rlenv`**: Single-stage build with all tools and dependencies
3. **Extract build steps**: Move build commands to `rlenv/mayhem/data/scripts/build.sh` that builds from `/rlenv/source/risingwave/`
4. **Preserve dependencies**: Ensure all build and runtime dependencies are included
5. **Test the setup**: Verify the image builds and can replay test cases

## Available Commands
The `.claude/commands/` directory contains specialized commands to help with this task:
- `prepare-patch` - Create single-stage Dockerfile and extract build commands to shell script

## Testing Your Changes
After modifying the Dockerfile:
```bash
# Build the image
docker build -t my-patch-env -f Dockerfile.rlenv .

# Test with a crashing test case
docker run --rm -v ./rlenv:/rlenv my-patch-env /rlenv/mayhem/data/scripts/replay.sh /rlenv/mayhem/data/testsuite/crashing/[hash]

# Test the build script inside the container
docker run --rm -v ./rlenv:/rlenv my-patch-env /rlenv/mayhem/data/scripts/build.sh
```

## Building the Final Patch Playground Image
After modifying Dockerfile.rlenv and testing:
```bash
# From within the repository directory:
rlenv patch
# This will download the testsuite and build the image

# Or from another directory:
rlenv patch /path/to/prepared/repo

# With custom tag:
rlenv patch --tag my-custom-tag
```

## Notes
- The original Dockerfile should be preserved as-is
- All modifications should go into `Dockerfile.rlenv`
- Source code should be at `/rlenv/source/risingwave/` in the container
- Build script should work from `/rlenv/source/risingwave/`
- Ensure the final image contains all necessary build tools for patching

## Tips

Go libfuzzer "reflect imported and not used" Issue

Problem:
- compile_native_go_fuzzer (from go-118-fuzz-build) generates intermediate Go files during compilation
- These generated files sometimes include "reflect" import that isn't actually used in the code
- Go compiler treats unused imports as errors, causing build failures with: "reflect" imported and not used

Root Cause:
- The go-118-fuzz-build tool transforms native Go 1.18+ fuzz tests into libFuzzer-compatible binaries
- During code generation, it may add imports that aren't always needed depending on the specific fuzz target

Fix Pattern:
1. Run compile_native_go_fuzzer once (generates files with potential unused imports)
2. Remove unused reflect import: sed -i '/^[[:space:]]*"reflect"$/d' path/to/generated_fuzz.go
3. Run compile_native_go_fuzzer again (compiles cleanly)

Example:
# First compilation - generates bgp_test.go_fuzz.go with unused reflect import
compile_native_go_fuzzer $PWD/pkg/packet/bgp FuzzParseBGPMessage fuzz_parse_bgp_message

# Fix: Remove unused reflect import from generated file
if [ -f pkg/packet/bgp/bgp_test.go_fuzz.go ]; then
  sed -i '/^[[:space:]]*"reflect"$/d' pkg/packet/bgp/bgp_test.go_fuzz.go
fi

# Second compilation - now succeeds without import error
compile_native_go_fuzzer $PWD/pkg/packet/bgp FuzzParseBGPMessage fuzz_parse_bgp_message

When This Occurs:
- OSS-Fuzz projects using compile_native_go_fuzzer
- Go projects with complex fuzz targets that trigger code generation edge cases
- Particularly common with go-118-fuzz-build versions that have this known issue
