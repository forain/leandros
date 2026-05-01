# LeandrOS Development Guidelines

## Git Practices
- Never mention Claude in commit messages or authorship
- Keep commit history clean and professional

## Build System
- Use `./scripts/build-all.sh` to build the complete system
- Use `./scripts/run-qemu.sh` to run and test
- **ALWAYS build release targets only** - debug builds crash early due to symbols and desync issues
- Never use debug builds for testing

## Cross-Platform Testing
- **MANDATORY**: Test both architectures in QEMU with `run-qemu.sh` after every change
- This ensures new development maintains cross-platform compatibility
- Both x86_64 and aarch64 targets must work

## Quick Workflow
```bash
# Build everything
./scripts/build-all.sh

# Test both architectures
./scripts/run-qemu.sh aarch64
./scripts/run-qemu.sh x86_64
```