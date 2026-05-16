#!/usr/bin/env sh
# Copy tracked hook scripts into .git/hooks/. Run once after cloning.
set -e
cd "$(dirname "$0")/.."
install -m 755 scripts/pre-commit .git/hooks/pre-commit
echo "installed: .git/hooks/pre-commit"
