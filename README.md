# ztail

> This branch (`cursor/legacy-crystal-1065`) preserves the original Crystal
> implementation. The Rust rewrite with incremental inflate lives on
> `cursor/rust-ztail-1065` (and will become `main` after that PR merges).

Like the linux `tail` command but for gzip compressed files

# Usage

## Tail a file

By default the functionality of `-f` is emulated

```
./ztail ~/test.log.gz
```

## Stop following a file:

`Ctrl+C`

# Build

* Install Crystal
* Checkout this repo
* Build with: `crystal build --static --release ztail.cr`
* Or with docker/podman `docker run --rm -it -v $(pwd):/workspace -w /workspace crystallang/crystal:latest-alpine crystal build --static --release ztail.cr`

# Known issues

* High CPU usage as it needs to re-read the gzip file in every iteration (this is because plain gzip do not support random access)
