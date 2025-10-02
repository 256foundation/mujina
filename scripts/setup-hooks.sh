#!/bin/bash
# Setup development environment hooks

echo "Setting up Git hooks..."

# Configure git to use the .githooks directory
git config core.hooksPath .githooks

echo "Git hooks configured to use .githooks/"
echo ""
echo "Hooks installed:"
for hook in .githooks/*; do
    if [ -f "$hook" ]; then
        echo "  - $(basename $hook)"
    fi
done
