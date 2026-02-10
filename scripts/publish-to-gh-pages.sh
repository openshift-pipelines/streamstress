#!/bin/bash
# Publish test results to gh-pages branch with full dashboard.
# This script runs AFTER streamstress completes in the Job pod.
#
# Supports two modes:
#   1. Single run: OUTPUT_DIR/results/results.json exists
#   2. Batch run:  OUTPUT_DIR/YYYY-MM-DD/results/results.json (date subdirs)
#
# Required env vars:
#   GITHUB_TOKEN      - Token with repo write access
#   GITHUB_REPOSITORY - org/repo format
#
# Optional env vars:
#   OUTPUT_DIR     - Results directory (default: /test-output)
#   RUN_LABEL      - Label for this run (default: "CI run")
#   DASHBOARD_DIR  - Dashboard assets location (default: /dashboard or ./dashboard)

set -euo pipefail

# Check required env vars
if [ -z "${GITHUB_TOKEN:-}" ] || [ -z "${GITHUB_REPOSITORY:-}" ]; then
    echo "Publish skipped: GITHUB_TOKEN or GITHUB_REPOSITORY not set"
    exit 0
fi

# Verify jq is available (prevents silent 0/0/0 data when missing)
if ! command -v jq &>/dev/null; then
    echo "ERROR: jq is required for publishing but not found in container image"
    echo "Rebuild the container image with jq installed (see Dockerfile.cli)"
    exit 1
fi

OUTPUT_DIR="${OUTPUT_DIR:-/test-output}"
RUN_LABEL="${RUN_LABEL:-CI run}"

# Find dashboard directory
if [ -n "${DASHBOARD_DIR:-}" ] && [ -d "$DASHBOARD_DIR" ]; then
    DASHBOARD_SRC="$DASHBOARD_DIR"
elif [ -d "/dashboard" ]; then
    DASHBOARD_SRC="/dashboard"
elif [ -d "./dashboard" ]; then
    DASHBOARD_SRC="./dashboard"
else
    DASHBOARD_SRC=""
fi

# Collect all results files to publish
RESULTS_FILES=()

if [ -f "$OUTPUT_DIR/results/results.json" ]; then
    # Single run mode
    RESULTS_FILES+=("$OUTPUT_DIR/results/results.json")
else
    # Batch mode: scan for date subdirectories (YYYY-MM-DD pattern)
    for date_dir in "$OUTPUT_DIR"/????-??-??; do
        if [ -d "$date_dir" ] && [ -f "$date_dir/results/results.json" ]; then
            RESULTS_FILES+=("$date_dir/results/results.json")
        fi
    done
fi

if [ ${#RESULTS_FILES[@]} -eq 0 ]; then
    echo "Publish skipped: No results files found in $OUTPUT_DIR"
    exit 0
fi

echo "=== Publishing ${#RESULTS_FILES[@]} result(s) to gh-pages ==="

REPO_URL="https://x-access-token:${GITHUB_TOKEN}@github.com/${GITHUB_REPOSITORY}.git"
WORK_DIR=$(mktemp -d)
trap "rm -rf $WORK_DIR" EXIT

cd "$WORK_DIR"

# Clone gh-pages or create orphan branch
echo "Cloning gh-pages branch..."
if ! git clone --branch gh-pages --single-branch --depth 1 "$REPO_URL" . 2>/dev/null; then
    echo "gh-pages branch doesn't exist, creating..."
    git init
    git checkout --orphan gh-pages
    git remote add origin "$REPO_URL"
fi

# Configure git
git config user.name "github-actions[bot]"
git config user.email "github-actions[bot]@users.noreply.github.com"

# Copy dashboard assets if available (only on first setup or if missing)
if [ -n "$DASHBOARD_SRC" ] && [ ! -f "js/app.js" ]; then
    echo "Copying dashboard assets from $DASHBOARD_SRC..."
    cp "$DASHBOARD_SRC/index.html" .
    cp -r "$DASHBOARD_SRC/css" .
    cp -r "$DASHBOARD_SRC/js" .
fi

# Create runs directory
mkdir -p runs

# Publish each results file
for RESULTS_FILE in "${RESULTS_FILES[@]}"; do
    # Extract date label from path if it's a batch date subdir
    PARENT_DIR=$(basename "$(dirname "$(dirname "$RESULTS_FILE")")")
    if [[ "$PARENT_DIR" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]]; then
        DATE_LABEL="$PARENT_DIR"
        CURRENT_LABEL="${RUN_LABEL} (${DATE_LABEL})"
    else
        DATE_LABEL=""
        CURRENT_LABEL="$RUN_LABEL"
    fi

    # Use date string as part of filename for batch runs, timestamp for single runs
    TIMESTAMP=$(date +%s)
    if [ -n "$DATE_LABEL" ]; then
        RUN_FILE="${DATE_LABEL}.json"
    else
        RUN_FILE="${TIMESTAMP}.json"
    fi

    # Skip if this date's results already published
    if [ -f "runs/${RUN_FILE}" ]; then
        echo "Skipping ${RUN_FILE} (already published)"
        continue
    fi

    cp "$RESULTS_FILE" "runs/${RUN_FILE}"

    # Also copy metadata.json if it exists (for as_of_date tracking)
    METADATA_FILE="$(dirname "$RESULTS_FILE")/metadata.json"
    if [ -f "$METADATA_FILE" ]; then
        cp "$METADATA_FILE" "runs/${RUN_FILE%.json}-metadata.json"
    fi

    # Extract metadata from results for manifest entry
    TOTAL=$(jq -r '.total // 0' "$RESULTS_FILE" 2>/dev/null || echo "0")
    PASSED=$(jq -r '.passed // 0' "$RESULTS_FILE" 2>/dev/null || echo "0")
    FAILED=$(jq -r '.failed // 0' "$RESULTS_FILE" 2>/dev/null || echo "0")
    RUN_TIMESTAMP=$(jq -r '.timestamp // empty' "$RESULTS_FILE" 2>/dev/null || date -Iseconds)

    # Use as_of_date from metadata if available
    if [ -f "$METADATA_FILE" ]; then
        AS_OF=$(jq -r '.as_of_date // empty' "$METADATA_FILE" 2>/dev/null || true)
        if [ -n "$AS_OF" ]; then
            RUN_TIMESTAMP="${AS_OF}T23:59:59Z"
        fi
    fi

    # Create or update manifest.json
    echo "Adding to manifest: $RUN_FILE ($TOTAL total, $PASSED passed, $FAILED failed)"
    if [ -f "runs/manifest.json" ]; then
        jq --arg id "run-${DATE_LABEL:-$TIMESTAMP}" \
           --arg date "$RUN_TIMESTAMP" \
           --arg label "$CURRENT_LABEL" \
           --argjson total "$TOTAL" \
           --argjson passed "$PASSED" \
           --argjson failed "$FAILED" \
           --arg file "$RUN_FILE" \
           '.runs += [{
             "id": $id,
             "date": $date,
             "timestamp": $date,
             "label": $label,
             "total": $total,
             "passed": $passed,
             "failed": $failed,
             "file": $file
           }]' runs/manifest.json > runs/manifest.json.tmp
        mv runs/manifest.json.tmp runs/manifest.json
    else
        cat > runs/manifest.json << EOF
{
  "runs": [
    {
      "id": "run-${DATE_LABEL:-$TIMESTAMP}",
      "date": "${RUN_TIMESTAMP}",
      "timestamp": "${RUN_TIMESTAMP}",
      "label": "${CURRENT_LABEL}",
      "total": ${TOTAL},
      "passed": ${PASSED},
      "failed": ${FAILED},
      "file": "${RUN_FILE}"
    }
  ]
}
EOF
    fi
done

# Commit and push
git add -A
if git commit -m "Add results: $RUN_LABEL (${#RESULTS_FILES[@]} runs)"; then
    echo "Pushing to gh-pages..."
    git push -u origin gh-pages

    # Extract org and repo for URL
    ORG=$(echo "$GITHUB_REPOSITORY" | cut -d'/' -f1)
    REPO=$(echo "$GITHUB_REPOSITORY" | cut -d'/' -f2)
    echo ""
    echo "Results published!"
    echo "   Dashboard: https://${ORG}.github.io/${REPO}/"
else
    echo "No changes to publish."
fi
