# rsclip

After changes to rsclip, build both Windows installers from the project root with `pnpm dlx @tauri-apps/cli@2 build --bundles nsis,msi`. Verify both generated installers and include their paths in the handoff. If packaging fails, report the specific blocker.
