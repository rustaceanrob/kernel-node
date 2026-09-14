# Design

## Initial block download

IBD is done from several peers at once, selected from the DNS seed nodes. The
peers share a queue of blocks to download, so each block is fetched once and a
slow peer does not hold up the sync. If a connection fails, the blocks it had
yet to deliver return to the queue and a new peer is selected. A direct
connection can also be selected from the command line, which limits the sync to
that single peer. See `--help` for this.
