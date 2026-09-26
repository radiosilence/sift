# Changelog

## Unreleased

- A folder holding an album more than once (FLAC beside WAV, `Track (1).flac` beside `Track.flac`) is imported from one copy of each track, FLAC first; the spares are left in place and logged. Matching every copy counted the spares as extra tracks, and filing them split the album across format-named folders.
- MusicBrainz pacing adapts: refusals widen the gap between requests and successes narrow it again, and when the service's global budget (`X-RateLimit-*`) is nearly spent, requests wait for it to reset. MusicBrainz refuses every client when that budget runs out, so a fixed rate alone cannot avoid it.
- 429, 5xx, timeouts and refused connections are retried, matching beets; previously only 503 was.
- Search hits with as many tracks as the folder are looked up first. Popular albums have a dozen pressings that score alike, and the right one could fall past the lookup limit.
- `ImportError::is_transient` tells a caller whether trying later could succeed, so a service can defer an import rather than fail it.
- First version: MusicBrainz matching, tag writing, Cover Art Archive art, beets config compatibility, fb2k-style path templates, `sift import` and `sift match`.
