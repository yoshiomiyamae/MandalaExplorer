# Contributing

## The shape of a change

`main` takes pull requests only, and a pull request merges once CI is green.
There is no review requirement, so a change of your own is yours to merge; the
rule exists so that nothing reaches `main` without having been built and tested
first, including the dependency updates that arrive on their own.

```
git switch -c what-it-does
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
git push -u origin what-it-does
gh pr create
```

Those three commands are exactly what CI runs, in that order, so running them
first turns a twenty-minute round trip into a local one. `main` also refuses
force pushes and deletion, and that rule has no exception for anyone.

## Cutting a release

Bump `version` in the workspace `Cargo.toml`, land that through a pull request,
then tag it:

```
git tag v0.2.0
git push origin v0.2.0
```

The tag builds the release, runs the tests, packages the MSIX, checks that the
packaged executable is byte-for-byte the one just built, and publishes a GitHub
release carrying the MSIX and a portable exe. The workflow refuses a tag that
disagrees with `Cargo.toml`, so the two cannot drift.

Submitting to the Store is still done by hand in Partner Center. The automated
route -- the msstore CLI and the action wrapping it -- is documented as
supporting free products only, and this one is priced per market.

## What the tests are for

Anything with a decision in it gets a test: grid geometry, the playback
scheduler, the slot planner, sort ordering, cache keys, pixel conversion. The
Media Foundation tests go further and encode a short clip before decoding it
back, because colour order and row orientation fail silently and look almost
right — the kind of fault a screenshot will not catch.

If you are fixing a bug, the useful order is to write the failing test first
and watch it fail. A test written afterwards proves the code does what it does,
not what it should.

## House style

Comments and documentation are in English. Comments carry the reasoning that
the code cannot: why a threshold is the value it is, what was measured, what
was tried and did not work. Restating the line below is worse than silence.

Measurements go in as numbers. "Faster" ages badly; "94% of a core to 20%" can
be checked later and argued with.

## Layout

| Crate | |
| --- | --- |
| `mandala-core` | Scanning, grid geometry, scheduling. No UI, no OS calls, no platform types. |
| `mandala-media` | The decoding seam: a `MediaBackend` trait and its Media Foundation implementation. |
| `mandala-app` | The egui front end, thumbnail workers, decoder pool, and the interface's wording. |

The boundaries are worth keeping. `mandala-core` stays testable without a
window or a GPU, and `mandala-media` is where a second backend would go.
