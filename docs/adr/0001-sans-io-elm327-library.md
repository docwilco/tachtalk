# Sans-io ELM327 library; the invented `1` repeat is retired

The ELM327 wire protocol was implemented five times across the workspace
(firmware proxy, firmware dongle client, test firmware, mock server,
benchmark client), with only a thin AT-command slice shared. Decision
(issue #6, design grill confirmed 2026-09-29): `tachtalk-elm327-lib` grows
into the whole protocol as two sans-io session machines — `ServerSession`
(server side) and `ClientSession` (client side; named `DongleClient` until
2026-10-13, renamed because the client side also speaks to the TachTalk proxy
and the mock server, not only to dongles) — fed bytes and returning
actions, with sockets, timeouts, and business rules staying in each
consumer's adapter. Chosen over per-consumer loops because the duplication
had already diverged (normalization differed between firmware and mock) and
because sans-io makes the protocol host-testable, where the firmware itself
cannot be.

One consequence deserves its own record: the bare-`1` repeat command,
introduced by aca34f3 on both proxy and dongle sides, is not ELM327 — the
datasheet's only repeat mechanism is a single bare carriage return, which
the proxy at the time silently ignored. The library implements the
datasheet's CR repeat and `1` is deleted everywhere; its only known speaker
was the in-repo benchmark client. If a repeat regression is ever suspected
against third-party client apps, this swap is the place to look.
