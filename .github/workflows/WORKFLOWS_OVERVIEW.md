# GitHub Actions Workflows Overview

This document provides a quick overview of all available CI/CD workflows in this repository.

**Note:** All workflows use **manual triggers only** (`workflow_dispatch`). There are no automatic triggers from push or pull request events. Builds are **macOS-only** (Apple Silicon, `aarch64-apple-darwin`) and **unsigned** — no code-signing or notarization secrets are required.

## Workflow Files

### 1. **build-macos.yml** - macOS Standalone Builds
**Purpose:** Build a DMG for Apple Silicon (M1/M2/M3 and up)

**Key Features:**
- Unsigned build (ad-hoc signing, no Apple Developer Certificate)
- Builds the `llama-helper` sidecar with Metal support
- Caches pnpm store, Rust target dir, and FFmpeg binary
- Uploads `.dmg` and `.app` bundle as workflow artifacts (30-day retention)

**Triggers:**
- Manual dispatch only (`build-type`: debug/release, `upload-artifacts`: yes/no)

**Use When:**
- You want a DMG to download from the Actions tab

**Outputs (artifacts):**
- `meetily_0.x.x_aarch64.dmg`
- `meetily.app`

---

### 2. **build.yml** - Reusable Build Workflow
**Purpose:** Shared macOS build used by other workflows

**Key Features:**
- Reusable workflow (`workflow_call`), not directly triggered
- Builds `llama-helper` + Tauri app (unsigned)
- Uploads artifacts directly to a GitHub Release when `release-id` is passed
- Used by `release.yml`

---

### 3. **release.yml** - Production Release
**Purpose:** Create an official release with the macOS DMG

**Key Features:**
- Creates a GitHub Release (draft)
- Version tags from `tauri.conf.json`
- Uploads release assets via the reusable `build.yml` workflow
- **Auto-increment versioning**: If tag exists, auto-increments (e.g., `0.1.1` -> `0.1.1.1` -> `0.1.1.2`, up to `.100`)

**Triggers:**
- Manual dispatch only

**Use When:**
- Ready to publish a new version

**Outputs:**
- GitHub Release (draft) with `meetily_0.x.x_aarch64.dmg`

**Version Behavior:**
- If `v0.1.1` tag doesn't exist: creates `v0.1.1`
- If `v0.1.1` exists: creates `v0.1.1.1`
- Maximum: `v0.1.1.100` (then update `tauri.conf.json`)

---

### 4. **pr-main-check.yml** - Validation Check
**Purpose:** Quick validation of version and configuration

**Key Features:**
- No builds triggered
- Validates version format
- Shows current branch info
- Provides next steps guidance

**Triggers:**
- Manual dispatch only

---

## How to Run Workflows

1. **Go to Actions tab** in GitHub repository
2. **Select workflow** from left sidebar
3. **Click "Run workflow"** button
4. **Select branch** to run against
5. **Configure options** (build type, artifacts, etc.)
6. **Click "Run workflow"** to start
7. **Monitor progress** in the Actions tab

---

## Quick Decision Guide

### "I need a DMG to test or distribute..."
- **Use `build-macos.yml`** (manual dispatch)
- Download the artifact from the run

### "I'm ready to release..."
- **Use `release.yml`** (manual dispatch)
- Creates a draft GitHub Release with the DMG
- Review, then publish when ready

---

## Workflow Dependencies

```
build.yml (reusable)
    |-- release.yml (calls build.yml)

Standalone (don't use build.yml):
    |-- build-macos.yml
    |-- pr-main-check.yml (validation only)
```

---

## Comparison Matrix

| Workflow | Platform | Signing | Retention | Use Case |
|----------|----------|---------|-----------|----------|
| `build-macos.yml` | macOS | None | 30 days | Get a DMG |
| `build.yml` | macOS | None | - | Reusable (internal) |
| `release.yml` | macOS | None | Permanent | Official release |
| `pr-main-check.yml` | - | - | - | Validation only |

---

## Required Secrets

**None.** Builds are unsigned. The Tauri updater key (`TAURI_SIGNING_PRIVATE_KEY`) from the upstream repository is not required: `createUpdaterArtifacts` is disabled in `tauri.conf.json`, so no `.sig` files are produced and automatic updates are not available in these builds.

**User note:** Unsigned apps require a Gatekeeper bypass on first launch (right-click the app → Open → Open).
