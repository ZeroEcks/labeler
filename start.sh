#!/usr/bin/env bash

# Strict mode
set -eou pipefail

# Check we have the mandatory secrets
if [ -z "$STRIPE_SECRET_KEY" ]; then
    echo "Error: STRIPE_SECRET_KEY is not set, check POSTINSTALL.md."
fi

# Run the app
/app/labeler
