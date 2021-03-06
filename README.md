# synapse
[![Build Status](https://travis-ci.org/Luminarys/synapse.svg?branch=master)](https://travis-ci.org/Luminarys/synapse)

Synapse is a flexible and fast BitTorrent daemon.

It currently supports most systems which implement epoll or kqueue, with a focus on 64-bit linux servers.

## About
* Event based RPC using websockets
* HTTP downloads and TLS for easy server usage
* Can be used via web client with minimal setup - see [receptor](https://web.synapse-bt.org)

## Compiling
Install dependencies:

- rustc >= 1.20
- cargo >= 0.18
- OpenSSL >= 1.0.2
- c-ares 1.13 | autotools + gmake(FreeBSD only) *

_\*Only required for synapse_

Synapse and sycli can be installed with:
```
cargo build --release --all
cargo install
cargo install --path ./sycli/
```

If you'd just like to install sycli:
```
cargo build --release -p sycli
cargo install --path ./sycli/
```

## Configuration
Synapse expects its configuration file to be present at `$XDG_CONFIG_DIR/synapse.toml`,
or `~/.config/synapse.toml`.
If it is not present or invalid, a default configuration will be used.
These defaults are given in `example_config.toml`.

## Development
Please see [this issue](https://github.com/Luminarys/synapse/issues/1) for details on development status.
If you're interested in developing a client for synapse, see `doc/RPC` for the current RPC spec.
if you'd like to contribute to synapse, see `doc/HACKING`.
