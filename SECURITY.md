# Security

## Reporting a vulnerability

Mail **info@famoce-succellion.com**. Please do not open a public issue for
something exploitable; a reply should come within a week.

Include what you did, what happened, and the Windows version — a file the app
mishandles is worth attaching if you can share it.

## What the attack surface actually is

Worth being straight about, because it narrows what is plausible: the app makes
no network connections of any kind, runs no server, accepts no input from
anywhere but the local filesystem, and does not elevate. It reads media files
the person running it points it at, and writes a thumbnail cache and its
settings under its own application data.

So the interesting cases are malformed media: a file crafted to crash or
mislead the decoder. Decoding is Windows Media Foundation's, reached through
`windows-rs`; a flaw in Media Foundation itself belongs to Microsoft, but how
this app drives it — the buffers it maps, the sizes it trusts, the frames it
uploads to the GPU — is ours, and a report about that is welcome.

The MSIX package declares `runFullTrust`, which every Desktop Bridge package
must. See [PRIVACY.md](PRIVACY.md) for what the app stores and where.

## Supported versions

The latest release. There are no maintained older branches.
