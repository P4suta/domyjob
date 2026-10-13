set -eu
umask 077
base="${XDG_CACHE_HOME:-$HOME/.cache}/domyjob/bootstrap"
mkdir -p "$base"
base="$(cd "$base" && pwd -P)"
work="$base/@TICKET@"
mkdir "$work"
trap 'result=$?; if ! cd "$HOME"; then echo "domyjob: bootstrap cwd recovery failed" >&2; fi; if ! rm -rf -- "$work"; then echo "domyjob: bootstrap cleanup failed" >&2; if [ "$result" -eq 0 ]; then result=1; fi; fi; exit "$result"' EXIT
archive="$work/source.tar"
dd ibs=1 obs=65536 count=@SOURCE_BYTES@ of="$archive"
[ "$(wc -c < "$archive")" -eq @SOURCE_BYTES@ ] || { echo "domyjob: truncated bootstrap source" >&2; exit 1; }
tar -xmf "$archive" -C "$work"
cd "$work"
mkdir -p "$work/target"
printf "%s\n" "@CACHE_TAG@" > "$work/target/CACHEDIR.TAG"
export CARGO_TARGET_DIR="$work/target" MISE_TRUSTED_CONFIG_PATHS="$work"
bash -lc 'work="$1"; cd "$work"; exec mise x -- cargo install --jobs @BUILD_JOBS@ --debug --locked --path crates/domyjob --bin domyjob --root "$work/install" --force' -- "$work" < /dev/null >&2
"$work/install/bin/domyjob" publish-node
