# Personal edition acceptance snapshot

Date: 2026-09-21. Scope: the Windows x64 personal edition and deliberately generated documents, not a whole-computer scan or large private corpus.

## Binary identity

- Desktop executable: 21,193,728 bytes, built 2026-09-21 05:59 (UTC+08:00).
- SHA-256: `F76CD9B4B480A9F2976FFCDC5760F496FC92FD43DDB9C6236632AA08C2FB7C2E`.
- CLI SHA-256: `83EC321E16C0565AFF3883BCF29E738DFD6E502275C71C40D70798CC0E0C638D`.
- Release embeds the frontend using `custom-protocol`; no development server is required.

## Observed on the final executable

- GUI folder selection, indexing, Chinese body-only search and preview; restart preserved selected roots and 71 synthetic index records.
- PDF, DOCX, XLSX, PPTX, TXT (including GBK Chinese), and Markdown search/preview.
- A 12,010 Unicode-character document, a keyword spanning the 8,000-character boundary with two emoji, complete highlighting, and navigation to a unique tail keyword.
- Newly created files appeared automatically; deletion updated counts; editing the currently read file refreshed the full text. Unrelated file changes preserved the reading position.
- Content/name mode distinction; explicit damaged-DOCX and unsupported-DOC status; Enter opened the selected text file in its associated application.
- Maximize, minimize and restore; multiple sequential query/file switches without observed stale-result overwrite.

Earlier GUI checks covered type/root filtering, literal punctuation with ASCII case-insensitivity, and 55 results across 50+5 pages. Backend synthetic acceptance covered reordered presentations with more than ten slides, workbook relationships and coordinates. These unchanged paths were reused as evidence rather than claimed as a second complete run.

## Automated verification boundaries

- Final targeted cache regressions: **3 passed, 0 failed**. One test bypasses the wrapper's explicit cache clear and verifies real update/deletion with a reader snapshot change.
- Final frontend type check: **0 errors, 0 warnings**.
- Final standalone release build: passed; one MSVC import-library creation message appeared as a linker warning.
- Retained earlier full-suite log: 202 library tests plus integration tests. Earlier focused office/acceptance logs: 6+9 passing tests; strict CLI checks: 10 passing checks in each of two runs. These are earlier results, not a claim that the complete suite ran again after the final cache fix.
- Public packaging changed documentation, workflow triggers and added a portable build helper; application implementation and released executable hashes remain the tested ones. The helper was syntax-checked, not validated by rebuilding on a clean second machine.

## Performance and limitations

Earlier strict CLI runs recorded synthetic index sizes of 34,236 and 34,541 bytes and process-start-inclusive query times of 300–310ms and 339–359ms. Corpus state differed between runs: these are observations, not an optimization comparison. Final small-corpus GUI queries displayed 3ms warm and 138ms after restart. No large-corpus throughput or compression improvement is claimed.

No OCR/legacy Office acceptance, installer/signing, cross-machine compatibility claim, prolonged watcher stress, or exhaustive high-concurrency GUI race testing. Preview is extracted text, not original layout. Raw local logs and machine-specific paths are intentionally excluded from this public archive.
