# Rune

This directory contains the application-facing Rune crate from [anomalyco/rune](https://github.com/anomalyco/rune/tree/dd672fc8257a1bfb55d2e8595f585fc411e709df/crates/rune), revision `dd672fc8257a1bfb55d2e8595f585fc411e709df`, under the upstream MIT license.

The local workspace resolves Rune's sibling crates at the same upstream revision. This copy includes the build inputs for the application library. It adds `WindowHandle` operations for compositor-driven movement and resizing, minimization, and access to frame state, logical content size, and Linux caption button layout through the existing optional GIO integration.
