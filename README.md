# Mandala Explorer

A file browser for folders full of pictures and video.

**[Install it from the Microsoft Store](https://apps.microsoft.com/detail/9P93GF7J2R94)**
— free in Japan, ¥120 elsewhere.

Ordinary explorers treat a photo the same way they treat a spreadsheet: a tiny
icon and a filename. Mandala Explorer goes the other way. Thumbnails are as large as you
want them, and videos play inline in their tiles, several at once, so a folder
can be read at a glance instead of opened one file at a time.

Windows only. Decoding goes through Media Foundation, so there is no runtime to
install and hardware decoding is used where the machine offers it.

## Running it

```
cargo run --release -p mandala-app -- "C:\some\folder"
```

With no argument it opens your Pictures folder.

The binary worth keeping is `target\release\mandala.exe`. A debug build
deliberately keeps a console attached so its logging stays visible, so opening
`target\debug\mandala.exe` from Explorer brings a terminal window with it. The
release build has no console, and runs the app code optimised rather than at
`opt-level = 1`.

## Controls

| Action | |
| --- | --- |
| Open a folder or file | double click |
| Open a folder or ZIP in a new tab | Ctrl + double click |
| Open the current folder in a new tab | the + button in the tab strip |
| Switch or close a tab | click its name or its × button |
| Go up | the arrow button, or Backspace |
| Read the folder again | the circular arrow, or F5 |
| Jump to a path | type it in the path box, Enter |
| Thumbnail size | the size slider, or Ctrl and the wheel |
| Scrub a video | point at its tile, drag the strip along the bottom |
| Reorder | pick a key next to Sort, click the arrow to reverse |
| How many videos play at once | the "at once" slider |
| Play only what you point at | untick Autoplay |
| Hear the tile you point at | tick Sound |
| Narrow to some of the names | type in the filter box |
| Keep a folder to come back to | the star, next to the path |
| Go back to one | the Bookmarks button |
| Select several files | Ctrl + click; Shift + click for a range; Ctrl + A for all shown |
| Windows file menu | right click a file or folder |
| Copy / cut / paste files | Ctrl + C / Ctrl + X / Ctrl + V |
| Rename the selected file | F2 |

Plain wheel scrolls, so resizing needs Ctrl held. Tile size, playback count,
sort order, autoplay, sound and filename visibility are remembered separately
for each directory, including folders inside ZIPs, across visits and restarts.
New folders start with the settings you had before upgrading to per-directory
views (the standard defaults on a fresh installation). Bookmarks and window
geometry remain shared across directories. The filter is not saved: an
application that starts up hiding most of a
folder, for a reason written down in a settings file, is one nobody can work
out from the outside. It clears when you open another folder, for the same
reason.

Each tab remembers its own scroll position for every folder visited during
the session, so going up returns to where you left the parent folder. Switching
tabs also restores the tab's filter. Tabs and scroll positions are not saved
across restarts. Only the active tab decodes videos and plays audio.

File operations use the Windows Shell, including its clipboard formats, so
copy and paste work with File Explorer. Text fields keep their normal editing
shortcuts. ZIP contents remain read-only: native file operations and external
context menus are unavailable there. The native menu uses the classic Shell
menu (the Windows 11 "Show more options" style), including installed extensions.

"Files in folder" sorts directories by the number of immediate files, not
subdirectories or descendants. ZIP folders follow the same rule. Counts are
read on a background worker and applied as a batch; unknown or unreadable
counts sort last in both directions. Counts are kept for the current listing
and refreshed on navigation or F5. Files remain below directories.

Size sorting also compares folders using recursive file-size totals. It is
calculated only while sorting by size, on the same background worker, and
cached for the current listing until navigation or F5. Symlinks and Windows
reparse points (including junctions) are not followed. Unreadable or unfinished
totals sort last in either direction. ZIP folders use uncompressed sizes from
their directory metadata without extracting files; ZIP files encountered
inside an ordinary directory contribute their on-disk file size instead.

Reloading keeps the filter and the scroll position, and does not throw the
thumbnails away: the disk cache is keyed on size and modification time, so a
file that did not change comes back identical and one that did comes back
different a moment later. Blanking the grid first would arrive at the same
place, slower and with a flash.

ZIP files open like read-only folders, including their subfolders. Images get
thumbnails and folder mosaics, with the same sorting, filtering and bookmarks
as ordinary folders. You can also enter `C:\photos.zip\holiday` in the path box.
Only images are extracted, on demand, into temporary files removed after decoding.
Videos and other files are listed but are not extracted or played. Opening ZIP
members in another application, encrypted archives and nested ZIPs are not
supported. Temporary image data is limited to 256 MiB per image and 512 MiB in
total; generated thumbnails use the existing thumbnail cache.
ZIP cover thumbnails are looked up using the archive's path, size and
modification time, plus the thumbnail tier, before scanning the ZIP's contents.
An unchanged archive can therefore reuse its cover without extracting images.

Thumbnail requests cover the viewport and two extra rows above and below it.
Once those requests finish and scrolling settles, low-priority work
prefetches the other files and folder covers in the current filtered listing,
starting just beyond the viewport. Prefetch stores disk thumbnails without
uploading off-screen GPU textures. Requests scrolled off screen move to the end
of a FIFO background queue and run after visible work. Every 500 ms the app
samples Windows CPU utilisation and adjusts background parallelism: below 60%
it adds one slot, at 85% or above it halves the limit (down to zero), and between
those thresholds it holds steady or resumes with one slot after a pause. These
are initial policy thresholds, not hardware-calibrated values. At least one
thumbnail worker is reserved for visible work; background parallelism can reach
seven on the largest eight-worker pool. Visible requests immediately pause new
background starts, while running jobs finish normally. If CPU readings are
unavailable, the fallback is one background worker. GPU utilisation and storage
latency are not measured by this policy.
Returning to a tile promotes its queued request; an already running background
decode can deliver its result directly to that visible tile. Duplicate requests
are merged, and there is no request-count limit that drops older jobs. The
queue retains unique requests for the current folder and thumbnail tier.
Navigation and reload cancel the old queue; changing thumbnail tiers cancels
obsolete sizes. Sorting preserves deferred work. A job already decoding is
allowed to finish. Prefetch periodically sweeps the disk
cache using the existing 2 GB limit. Folder covers inspect only the usual
bounded selection of children, not an entire recursive directory tree.

Video workers retain only the latest unconsumed frame per playback slot, so a
paused or closing UI cannot accumulate a backlog of decoded images. Closing
disconnects playback, audio and thumbnail work before dropping the UI's
resources. It does not join threads blocked in Windows media or network I/O;
those threads stop at their next cancellation boundary, or when the process exits.

Filtering takes several words, and every one of them has to appear somewhere in
the name, in any order -- `beach 2026` finds `2026-08-beach-trip.jpg`. Case is
ignored, and the extension is part of the name, so `mp4` is also how you see
only the video. The count on the right says how many of the folder you are
looking at, because a bare number while a filter is on reads as the rest having
gone missing.

Sorting works on name, type, size, modification time, or running length, each
either way round. Folders stay at the top whichever key and direction is
chosen -- a folder is somewhere to go, not something to compare against the
files beside it, and reversing "largest first" should not bury the way back out
at the bottom of the grid.

## Packaging

```
.\packaginguild-msix.ps1 -Version 0.1.0.0
```

Builds `target\msix\MandalaExplorer.msix`, unsigned, which is what the Store
wants -- it signs submissions itself. The icons are drawn by
`packaging/make_assets.py` from one description rather than checked in, so all
77 of them stay consistent and the repository holds no binaries.

Add `-Sign` to sign with a self-signed certificate and install it locally
first. Windows will not install it until that certificate is trusted, which
needs an elevated shell:

```
Export-Certificate -Cert Cert:\CurrentUser\My\<thumbprint> -FilePath test.cer
Import-Certificate -FilePath test.cer -CertStoreLocation Cert:\LocalMachine\TrustedPeople
Add-AppxPackage target\msix\MandalaExplorer.msix
```

The script prints the thumbprint when it creates the certificate. The
certificate's subject has to match the manifest's `Publisher` exactly or the
install fails with an error that does not say so, which is why the script reads
it out of the manifest rather than repeating it.

## How it is put together

| Crate | |
| --- | --- |
| `mandala-core` | Directory scanning, grid geometry, playback scheduling. No UI, no OS calls. |
| `mandala-media` | The decoding seam: a `MediaBackend` trait plus its Media Foundation implementation. |
| `mandala-app` | The egui front end, the thumbnail workers, and the decoder pool. |

Two decisions shape everything else:

**Only what is on screen exists.** Tile positions are computed from an index, so
a folder of fifty thousand files costs the same as one with fifty. Textures
outside a wide margin around the viewport are dropped.

**Nothing starts decoding mid-flick.** Opening a decoder for a tile that is
about to scroll off wastes the open and floods Media Foundation with sources it
will never read. Playback only starts once the view has held still briefly --
which measurably cut thread count and memory during scrolling, and reads better
too, since tiles stop flickering in and out of playback as you move.

**Decoding belongs on the GPU, and so does everything after it.** A Source
Reader with no D3D11 device manager attached silently picks a software decoder,
which costs a whole CPU core for a dozen small tiles. Handing it a shared device
moved that onto the GPU's decode block, and took twelve simultaneous videos from
94% of a core to 20%.

That was not the end of it. Asking Media Foundation for a finished frame *in
system memory* -- any format, scaled or not -- makes the pipeline give the
hardware decoder up again, because a frame in system memory is not something a
hardware decoder can produce. The reader will still oblige, quietly, on the
processor. On a 3840x2160 60fps HEVC clip that cost ten cores for 32 frames a
second per stream. Taking the decoder's own texture instead, and doing the
scale and the colour conversion with a Direct3D video processor before reading
back only the finished tile, gives 131 frames a second per stream for three
tenths of one core: eight times the frames for a thirty-seventh of the
processor. Three such clips play together in 0.18 of a core.

What crosses the bus is the tile, a megabyte, rather than the 4K frame it came
from. The older path is still there for machines with no usable device -- a
remote desktop session, a stripped-down VM -- where slow and correct beats
nothing.

**Playback is a budget, not a free-for-all.** Every playing video costs a
decoder, a thread, and GPU decode capacity, so a fixed number of slots is handed
out to the tiles that most deserve them: whatever you are pointing at first,
then whatever is nearest the middle of the screen. Tiles that keep their slot
across a scroll keep playing without restarting, and video is decoded no larger
than the tile can actually show.

**Length is learned once and kept.** A running time means opening the file, so
it is cached beside the thumbnail -- under a key that deliberately excludes the
size tier, since how long a video runs for has nothing to do with the size it
was last drawn at. Anything already thumbnailed knows its length for free, and
sorting by length only probes the files it still has to ask about, a few per
frame so one enormous folder cannot flood the queue.

**Non-Latin names are readable.** egui carries Latin coverage only, so system
fonts are loaded at startup as fallbacks -- Japanese, plus Korean and Simplified
Chinese where present. Each one carries a measured baseline correction, because
faces disagree about where the baseline sits and the mismatch shows up as the
extension sliding below the name it belongs to. Labels are laid out and elided
by egui rather than trimmed by character count, since a full-width character is
about twice the width of a Latin one.

**Folders show what is inside them.** A folder icon says nothing a filename
does not, so a folder's tile is built from the first few pictures below it: one
fills the square, two take a side each, three put one down the left and stack
the others, four take a corner each. Every one is cropped to its cell rather
than squashed into it. A library sorted into dated folders has nothing but more
folders at the top, so one level down is opened to make up a shortfall --
lazily, and bounded, so a folder that already fills its tile reads nothing extra
and a folder of a thousand folders is not a thousand directory reads. The tile
is keyed on the files it was built from, so a photograph added to a folder
changes the folder.

**HEIC arrives through Windows.** The `image` crate decodes what it decodes;
anything it refuses is handed to WIC, which is how a phone's photographs work
here without libheif and a C toolchain arriving with them, and it brings camera
raw and the rest of Windows' codecs along for free. This one depends on the
machine rather than on us: the pictures inside a `.heic` are coded with HEVC, so
it wants the HEVC Video Extension as well as the HEIF one, and that is a paid
download. A file that cannot be decoded still appears in the grid, just without
a preview -- hiding photographs because they might not draw would be worse.

**Sound follows the picture, and only one tile of it.** A grid where every
playing tile is also audible is not a feature anyone wants twice, so the one
under the pointer is the one you hear, and only if Sound is ticked -- a browser
that starts making noise because the pointer crossed it would not stay open.

The picture leads. A tile is already partway through its clip by the time
anyone points at it, it loops on its own schedule, and its seek bar can be
dragged, so the sound is told where the picture is and moves to meet it. Only
once they are more than four tenths of a second apart, though: a clip that
cannot be decoded at full speed plays slowly, and chasing that exactly would
mean re-seeking several times a second, which is audible as damage rather than
as synchronisation.

Output is WASAPI in shared mode, so nothing else on the machine is silenced.
Shared mode means the device dictates the format, which is handed to Media
Foundation as it stands -- resampling and channel mixing are the part of audio
most easily got subtly wrong, and Windows already has a resampler that is not
subtly wrong.

**The cache is capped.** Thumbnails live in
`%LOCALAPPDATA%\mandala\thumbnails`, sharded by hash so no directory holds a
hundred thousand entries, and keyed on path, size, modification time and size
tier -- so a re-encoded video cannot keep a stale thumbnail. The cap is 2 GB,
enforced on startup by evicting whatever has gone longest unused. Cache hits
refresh that timestamp at most once a day, which is enough to tell a live
thumbnail from a dead one without a metadata write per hit.

## Tests

```
cargo test
```

The grid, the scheduler, the slot planner and the pixel conversions are covered
by unit tests. `mandala-media` additionally encodes a short H.264 clip with
Media Foundation and decodes it back, which is what pins down colour order and
row orientation -- the two things that fail silently and look almost right.
Those tests also cover seeking, and assert that opening many distinct files in
parallel does not leak threads, which is the failure mode a browser of large
folders would otherwise hit only in front of a user.

## Licence

MIT. See [LICENSE](LICENSE).

Security reports go to the address in [SECURITY.md](SECURITY.md); what the app
stores, and what it does not, is in [PRIVACY.md](PRIVACY.md).
