# Native Desktop requirements

The Desktop window uses GPUI. The sealed `resources/runtime` directory contains the same-source CLI, Core and independent browser application. Opening Desktop in control-only mode does not initialize or start Node. Agent programs and their authentication remain separately configured on the Node that executes work.

- macOS builds target Apple Silicon. Signed releases require Developer ID and Apple notarization; an unsigned hourly preview is identified in its provenance receipt.
- Windows builds target x64 and install for the current account. They need neither administrator rights, WSL, Git Bash nor Developer Mode. The installer registers the `vibeface:` URL scheme for that account.
- Linux builds target x64 or ARM64 with glibc, an X11 or Wayland desktop session, and a working Vulkan graphics driver. AppImage and ZIP do not install system libraries or graphics drivers. `native-dependencies.json` records the actual linked library names and required glibc symbol versions of the packaged executable. A successful hosted-runner check does not prove every distribution or GPU works.

Linux deep-link handling also needs desktop integration: the package includes `vibeface.desktop` with the URL scheme declaration, but copying or launching an AppImage does not by itself prove the desktop registered that scheme. Use the desktop environment's AppImage integration or install the desktop entry through the normal user-controlled installation process.

The native client has its own identity. The previous browser-based Desktop key was non-exportable. Join through an invitation from a managing device or explicit recovery; do not expect a silent identity export. Existing Node data and old profiles stay intact. Unknown writes must be reconciled under their original identity.
