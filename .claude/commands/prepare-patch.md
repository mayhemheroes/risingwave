# Prepare Patch Command

## Description
Prepare the repository for patching by creating a single-stage Dockerfile and extracting build steps to a shell script.

## Task
This command combines two key steps:

### 1. Create Dockerfile.rlenv (Single-Stage Build)
Analyze the existing Dockerfile and create `Dockerfile.rlenv` with the following requirements:

- **Single Stage**: Combine all stages into one stage based on an appropriate base image
- **Preserve Dependencies**: Include all package installations from all stages
- **Maintain Build Tools**: Ensure compilers, build systems, and development tools are available
- **Runtime Requirements**: Include both build-time and runtime dependencies
- **Environment Variables**: Preserve necessary ENV declarations
- **Working Directory**: Set WORKDIR appropriately for the build
- **Source Location**: Copy source code to `/rlenv/source/<project-name>` in the image
- **Entry Point**: Maintain or create appropriate ENTRYPOINT/CMD

### 2. Create Build Script
Extract the application build process from the Dockerfile into `rlenv/mayhem/data/scripts/build.sh`:

- **Working Directory**: Start with `cd /rlenv/source/<project-name>`
- **Extract Build Commands**: Identify RUN commands that build the application
- **Preserve Environment**: Include necessary environment variable exports
- **Error Handling**: Add proper error handling with `set -euo pipefail`
- **Build Process**: Build from the source at `/rlenv/source/<project-name>`
- **Output Artifacts**: Copy build products to expected locations if needed

## Guidelines

### For Dockerfile.rlenv:
- Start with the most appropriate base image (usually from the initial stage)
- Combine all RUN commands that install packages
- Remove intermediate cleanup that might remove needed build tools
- Add any missing development tools (gcc, make, cmake, etc.)
- Copy source code directly to `/rlenv/source/<project-name>` in the image
- Build the application from `/rlenv/source/<project-name>`
- If build artifacts need to be at specific locations (e.g., `/out/`), copy them there
- Ensure the image can both build and run the application
- Copy the rlenv directory from build context to `/rlenv` in the image (final step)

### For build.sh:
- Start with `cd /rlenv/source/<project-name>` to work from the source directory
- Focus on commands that compile/build the application (not install dependencies)
- Preserve the sequence of build steps from the original Dockerfile
- Handle any file copying or preprocessing steps
- Include any test compilation or validation steps
- If build artifacts need to be at specific locations, copy them there after building
- Make the script executable and robust
- IMPORTANT: You must not include commands that reach out to the network
- IMPORTANT: You must not include commands that install packages
- IMPORTANT: You must not run the build script on the host machine, it is only to be run from within the playground docker image

## Critical Dockerfile.rlenv Requirements

When creating `Dockerfile.rlenv`, you MUST:

1. **Install dependencies**: Include all package installations and dependencies from the original Dockerfile

2. **Single source location**: The source code should be copied directly to `/rlenv/source/<project-name>` in the image

3. **Build from standardized location**: All build commands should work from `/rlenv/source/<project-name>`

4. **Handle build artifacts**: If the original Dockerfile expects artifacts at specific locations (e.g., `/out/`), copy them there after building

## Output
1. Create `Dockerfile.rlenv` in the repository root that produces a single-stage image with all build capabilities
2. Update `rlenv/mayhem/data/scripts/build.sh` with the actual build commands extracted from the Dockerfile

There is no need to build or test the image as part of this command, that will be done separately.
```
