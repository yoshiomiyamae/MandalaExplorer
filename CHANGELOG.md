# Changelog

## 0.4.0 (unreleased)

### Added

- Browse ZIP archives as read-only folders and preview their images without
  extracting the entire archive. Video playback inside ZIPs is not supported.
- Tabs: Ctrl-double-click a folder or ZIP to open it in a new tab, and use the
  tab strip to switch or close tabs. Only the active tab plays video and audio.
- Restore the previous scroll position when returning to a folder. Each tab
  keeps its own scroll positions and filter during the current session.
- Remember thumbnail size, playback count, sort order and other view settings
  separately for each directory, including folders inside ZIPs.
- Select files with Ctrl/Shift-click or Ctrl+A. Use the native Windows file
  context menu, Ctrl+C/X/V for clipboard operations, and F2 to rename a file.
  Text fields retain their editing shortcuts; ZIP contents remain read-only.
- Sort folders by their immediate file count, or by recursive file-size totals
  under the existing Size key. ZIP totals use uncompressed sizes without
  extraction. Folder statistics are collected in the background and refreshed
  on navigation or F5. Size traversal skips symlinks and Windows reparse points.

### Improved

- Reuse cached ZIP cover thumbnails before scanning archive contents.
- Prefetch off-screen thumbnails after visible requests, retaining deferred
  requests in FIFO order and promoting them when they become visible.
- Adapt background thumbnail concurrency to CPU pressure and foreground work,
  reserving capacity for visible thumbnails.

### Fixed

- Bound pending playback frames to the latest frame per slot, preventing an
  undrained frame queue from growing indefinitely during shutdown.
- Signal playback, audio and thumbnail workers before tearing down the app,
  without waiting indefinitely for workers blocked in external I/O.

### Store submission

- Submit the unsigned `MandalaExplorer-0.4.0.msix` built by the `v0.4.0` release
  workflow. The Store package version is `0.4.0.0`.
- Update release notes and the privacy policy. File operations now modify
  selected files only when requested and use the Windows clipboard and Shell.
- Do not attach a portable executable; installation remains through the
  [Microsoft Store](https://apps.microsoft.com/detail/9P93GF7J2R94).
- Before tagging, smoke-test native clipboard interoperability with File
  Explorer, the context menu and rename dialog, tabs and scroll restoration,
  ZIP browsing, and shutdown during an active thumbnail workload.
