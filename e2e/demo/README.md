Public census and metadata files of the demo elections on the DAVINCI Gnosis deployment, one
directory per election; the second wave's are under `wave2/`.
`e2e/tests/demo.rs` writes them in its prepare phase from voter keys it keeps outside the
repository, and its run phase points each election's census URI and metadata at these files at a
fixed commit on raw.githubusercontent.com, where the sequencers download the censuses and check
their roots.
