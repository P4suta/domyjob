# `domyjob`

Persistent jobs over SSH that survive client disconnects.

## Installation

```sh
git clone https://github.com/P4suta/domyjob.git
cd domyjob
mise x -- cargo install --locked --path crates/domyjob
```

## Usage

```sh
domyjob run linux --wait -- cargo test
```

## Documentation

[Read the documentation](docs/README.md).

## License

Licensed under the [Apache License, Version 2.0](LICENSE-APACHE) or the [Massachusetts Institute of Technology (MIT) license](LICENSE-MIT), at your option.
