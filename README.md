# ishtaria-server

Authoritative world server of Ishtaria: one planet, its simulation, persistence and the federation endpoint that links worlds through portals.

**Status:** M0 – configuration and identity bootstrap. See the [roadmap](https://vitexsoftware.github.io/ishtaria-docs/roadmap.html).

## Installation

Debian / Ubuntu (x86-64) only:

```sh
echo "deb http://repo.vitexsoftware.com $(lsb_release -sc) main" | sudo tee /etc/apt/sources.list.d/vitexsoftware.list
sudo wget -O /etc/apt/trusted.gpg.d/vitexsoftware.gpg http://repo.vitexsoftware.com/keyring.gpg
sudo apt update
sudo apt install ishtaria-server ishtaria-content
```

```sh
sudoedit /etc/ishtaria/server.toml        # server_name is permanent – choose carefully
sudo systemctl enable --now ishtaria-server
```

## Building

```sh
cargo build --release
dpkg-buildpackage -us -uc -b
```

To develop against a local checkout of `ishtaria-core`, add to `.cargo/config.toml` (not committed):

```toml
[patch."https://github.com/VitexSoftware/ishtaria-core"]
ishtaria-core = { path = "../ishtaria-core" }
```

License: AGPL-3.0-only – anyone may run a world; modified servers offered over the network must publish their source.

## Part of Ishtaria

Ishtaria is an open-source, persistent, federated virtual planet of Earth size.
Documentation: https://vitexsoftware.github.io/ishtaria-docs/ · All repositories: https://github.com/VitexSoftware?q=ishtaria
