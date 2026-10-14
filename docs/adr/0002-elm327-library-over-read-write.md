# The ELM327 library works over std::io::Read + Write, not sans-io

Status: accepted. Supersedes the sans-io part of ADR 0001; 0001's retirement
of the invented `1` repeat stands.

ADR 0001 made `tachtalk-elm327-lib` sans-io: session machines fed bytes and
returning actions, with each consumer's adapter owning the socket. Building
the server side and starting the client side showed the cost (issue #30,
design confirmed 2026-10-14). The client side is strictly half-duplex (write
a command, read until the `>` prompt, sometimes write once more), and
sans-io turned that sequence into a protocol every adapter re-implemented.
On the server side, `feed` processed a whole chunk before the adapter
answered anything, which made a later AT line change the settings an
earlier OBD line was answered with (fixed in 941c924, as a rule adapters had
to remember). Every consumer — the firmware proxy and dongle task, the mock
server, the benchmark client — is blocking `std` code on its own thread, so
the portability sans-io buys was not being used.

Decision: both sessions are generic over `S: Read + Write` and own the
stream they are given. The server side is pull-style (`next_command()`), and
each command is answered through itself, so one line is answered before the
next is read by construction. The client side's queries are ordinary
blocking calls. The library returns `std::io::Result` and may sleep and read
the clock. Host tests stay socket-free through an in-memory fake stream, and
the firmware records captures with a stream wrapper instead of the session
reporting its traffic.

Considered and rejected: sans-io for the server and Read + Write for the
client only (two shapes in one library), and keeping the sans-io machine as
a private core under a Read + Write layer (two layers where the logic is
small enough to rewrite, guarded by the unchanged characterization bytes).
