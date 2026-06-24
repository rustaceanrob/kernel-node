# Using the Wallet

`kernel-node` offers an experimental [silent payments](https://bitcoinops.org/en/topics/silent-payments/) wallet. This is a simple walkthrough to make a wallet and receive a payment. For build instructions, see [`build.md`](../build.md).

## Generating keys

A silent payment address is comprised of two elliptic curve keypairs, known as a "scan key" and "spend key." To generate these two keys and optionally save them to a file, one may use the CLI. Note that a running instance of `kernel-node` is _not_ required for this step.

Generate keys in memory and print them to the console:
```
cargo run --bin cli wallet generate-keys
```

Generate keys to an output file:
```
cargo run --bin cli wallet generate-keys --out keys.bin
```

Recover scan PrivateKey and spend PublicKey from file:
```
cargo run --bin cli wallet print-keys-from-keys-file keys.bin
```

## Starting the daemon

Using the keys file that was generated in the previous step, you can start `kernel-node` and scan for payments with these keys. Note that `--sp-keys-file` requires a fully qualified path for use with the `daemon` option. For example (`/home/me/some_directory/keys.bin`).

Start the node in daemon and import keys:
```
cargo run --bin node --release -- --network signet --sp-keys-file=/path/to/keys.bin --daemon=true
```

This will create a `wallet.bin` file in your data directory.

## Receive a payment

To print the silent payment address to console:
```
cargo run --bin cli wallet receive
```

## Showing the wallet balance

Once a payment has confirmed in a block, you may print your balance:
```
cargo run --bin cli wallet balance
```
