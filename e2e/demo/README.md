Public census and metadata files of the demo elections, one directory per election; the second
and third waves' are under `wave2/` and `wave3/`. The demo driver (`e2e/tests/demo.rs`) writes
them in its `prepare` phase from voter keys it keeps outside the repository, and its `run` phase
points each election's census and metadata URIs at these files at a fixed commit, where the
sequencers download the censuses and check their roots. See [docs/demo.md](../../docs/demo.md).
