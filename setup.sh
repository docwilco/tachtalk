#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")" && pwd)"

echo "Installing git hooks..."
ln -sf "../../.githooks/pre-commit" "$root/.git/hooks/pre-commit"
echo "Done. Pre-commit hook installed."
