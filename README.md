# mandala

A file browser for folders full of pictures and video.

Ordinary explorers treat a photo the same way they treat a spreadsheet: a tiny
icon and a filename. mandala goes the other way. Thumbnails are as large as you
want them, and videos play inline in their tiles, several at once, so a folder
can be read at a glance instead of opened one file at a time.

Windows only. Decoding goes through Media Foundation, so there is no runtime to
install and hardware decoding is used where the machine offers it.

## Running it

```
cargo run --release -p mandala-app -- "C:\some\folder"
```

With no argument it opens your Pictures folder.

## Controls

| Action | |
| --- | --- |
| Open a folder or file | double click |
| Go up | the arrow button, or Backspace |
| Jump to a path | type it in the path box, Enter |
| Thumbnail size | the size slider, or Ctrl and the wheel |
| Scrub a video | point at its tile, drag the strip along the bottom |
| How many videos play at once | the "at once" slider |
| Play only what you point at | untick Autoplay |

Plain wheel scrolls, so resizing needs Ctrl held. Tile size and the playback
count are remembered between runs, along with the window geometry.

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

**Decoding belongs on the GPU.** A Source Reader with no D3D11 device manager
attached silently picks a software decoder, which costs a whole CPU core for a
dozen small tiles. Handing it a shared device moved that onto the GPU's decode
block, and asking the video processor for RGBA rather than BGRA removed a
channel swap that ran over every pixel of every frame. Together those took
twelve simultaneous videos from 94% of a core to 20%.

**Playback is a budget, not a free-for-all.** Every playing video costs a
decoder, a thread, and GPU decode capacity, so a fixed number of slots is handed
out to the tiles that most deserve them: whatever you are pointing at first,
then whatever is nearest the middle of the screen. Tiles that keep their slot
across a scroll keep playing without restarting, and video is decoded no larger
than the tile can actually show.

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
