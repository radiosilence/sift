# Changelog

## Unreleased

- `Importer::compare` lines a folder up against one release track by track: pairs with title and length differences, and release tracks with no file and files with no track, for deciding a review.
- `Importer::tracks` returns a folder's files and their tags as an import reads them.
- `import_as_is` takes `Edits`, album-wide and per-file corrections applied before the coherence check, so an edit cannot file what the check would refuse. `Outcome::Imported::release` stays `None` for these.
- `sift import --as-is` (`-A`, as beets' `--noautotag`) and `Importer::import_as_is` file an album by the files' own tags, for releases MusicBrainz does not have. They refuse, saying why, unless the tags describe one album: an album and artist every file agrees on, and a title and distinct track number per file, taken from the file name where the tag is missing. Several artists and no album artist make a Various Artists compilation.
- `Outcome::Imported::release` is optional; it is `None` for an import as-is.
- An album artist of `VA`, `V.A.` or `Various` is searched as "Various Artists", which is how MusicBrainz credits every compilation; the abbreviation found nothing.
- Track lengths may differ by 2% of the track (full penalty at 6%) where that exceeds beets' flat 10 s (30 s). Long sides differ by twenty seconds between a trimmed digital release and a vinyl edit, which the flat allowance read as different recordings; tracks under about eight minutes are judged as before.
- The import log breaks the best match's distance into album, artist, title and length, so a review shows what kept the match from being applied.
- A folder holding an album more than once (FLAC beside WAV, `Track (1).flac` beside `Track.flac`) is imported from one copy of each track, FLAC first; the spares are left in place and logged. Matching every copy counted the spares as extra tracks, and filing them split the album across format-named folders.
- MusicBrainz pacing adapts: refusals widen the gap between requests and successes narrow it again, and when the service's global budget (`X-RateLimit-*`) is nearly spent, requests wait for it to reset. MusicBrainz refuses every client when that budget runs out, so a fixed rate alone cannot avoid it.
- 429, 5xx, timeouts and refused connections are retried, matching beets; previously only 503 was.
- Search hits with as many tracks as the folder are looked up first. Popular albums have a dozen pressings that score alike, and the right one could fall past the lookup limit.
- `ImportError::is_transient` tells a caller whether trying later could succeed, so a service can defer an import rather than fail it.
- First version: MusicBrainz matching, tag writing, Cover Art Archive art, beets config compatibility, fb2k-style path templates, `sift import` and `sift match`.
