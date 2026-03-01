#!/bin/bash
# Smart clean that preserves Git dependencies

echo "🧹 Cleaning build artifacts while preserving Git cache..."

# Standard cargo clean but preserve registry cache
cargo clean

# The Git cache is automatically preserved in ~/.cargo/git
# But if you want to be extra sure, you can backup and restore:

# CARGO_HOME=${CARGO_HOME:-~/.cargo}
# GIT_CACHE="$CARGO_HOME/git"
#
# if [ -d "$GIT_CACHE" ]; then
#     echo "✅ Git cache preserved at $GIT_CACHE"
#     echo "📦 Windows-rs cache size: $(du -sh "$GIT_CACHE" 2>/dev/null | cut -f1)"
# fi

echo "✅ Clean complete - Git dependencies preserved"
echo "💡 Next build will reuse cached Windows-rs repository"