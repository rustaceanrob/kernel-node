@0xc2e20cc9503cf68f;

interface Server {
    echo @0 (msg :Text) -> (reply :Text);
    shutdown @1 () -> ();
}
