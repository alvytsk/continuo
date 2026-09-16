# M6 acceptance

`docs/superpowers/specs/2026-09-16-continuo-m6-feed-management-design.md` §8 lists the evidence: the automated suites, then one manual run against Radio-T.

## Automated

| Requirement | Tests |
| --- | --- |
| Prompt, confirm, refresh keys, pending, correlation, reconciliation, notice drawing | `tests/m5_browser.rs` (the tests added for M6; §8's close-and-reopen case is covered in-state by the "fresh browser" block of `a_matching_answer_shows_the_notice_and_re_reads_the_list`, rather than through a delayed-response server, since `apply` is a pure function and the assertion is the same without loopback timing) |
| Worker answers with the CLI's wording; failures are values; refresh-all reports every feed; browsing makes no request | `tests/m6_feed_management.rs` |
| Browsing, enqueueing and restoring make no request | `tests/m5_no_network.rs` (unchanged) |
| Gates | `cargo fmt --check`, `cargo clippy --locked --all-targets --all-features -- -D warnings`, `cargo test --locked` — all exit 0; `cargo test --locked` across 83 binaries: 1030 passed, 0 failed, 1 ignored |

## Manual, Ghostty, Radio-T

| Step | Result |
| --- | --- |
| `b`, Tab to Podcasts, `a`, paste the Radio-T feed URL, Enter: "Subscribing…" then "radio-t: subscribed, N episodes retained" and the feed appears with its count | not run — requires a human at the terminal |
| Enter on the feed, `r`: "Refreshing…" then "radio-t: updated/unchanged", episodes still listed | not run — requires a human at the terminal |
| `R` on the feed list with two feeds: both lines reported | not run — requires a human at the terminal |
| `d`, `n`: nothing happens; `d`, `y` inside the feed: back on the list, feed gone, "radio-t: unsubscribed" | not run — requires a human at the terminal |
| Resubscribe: the feed returns with a fresh count | not run — requires a human at the terminal |
| Typing `q`, `b`, space in the prompt inserts text and nothing else happens | not run — requires a human at the terminal |
