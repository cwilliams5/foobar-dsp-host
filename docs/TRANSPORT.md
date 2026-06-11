# Transport spec (pointer)

This worker speaks the shared worker-transport spec — framing, the `VERS`/`VOK ` version
handshake, `ERR `/`QUIT` semantics, and the crash-as-EOF process contract.

**The canonical spec lives in the sibling repo, beside its reference implementation
(`tagpipe`):**

> https://github.com/cwilliams5/winamp-vst2-dsp-host/blob/main/docs/TRANSPORT.md

It is deliberately **not** duplicated here — one spec, one home, no drift. What you need
locally:

- This worker's **`VOCAB_VERSION` is 1** (its command vocabulary is [`PROTOCOL.md`](PROTOCOL.md),
  which also restates the handshake table).
- The Rust client crate ([`foobar-dsp-host`](../crates/client)) implements the parent side via
  the `tagpipe` crate and enforces the v1 exact-match version policy automatically.
