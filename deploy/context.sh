# Sourced by deploy/build.sh: a Docker build context of committed sources
# only, vlichen's HEAD exported with `git archive` (no working-tree
# changes, no target/). lichen and lichen-btc come by git, at the commits
# Cargo.lock pins.
#   make_context      sets $ctx: a directory holding vlichen/ (removed on
#                     exit), and $lichen_commit: vlichen's HEAD as `git
#                     describe` names it (its tag, if any)

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

make_context() {
    local deploy vlichen
    deploy="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    vlichen="$(cd "$deploy/.." && pwd)"
    if [ -n "$(git -C "$vlichen" status --porcelain -- crates Cargo.toml Cargo.lock deploy)" ]; then
        echo "note: uncommitted changes in vlichen are not in the image (it builds HEAD)" >&2
    fi
    ctx="$(mktemp -d)"
    trap 'rm -rf "$ctx"' EXIT
    mkdir -p "$ctx/vlichen"
    git -C "$vlichen" archive HEAD | tar -x -C "$ctx/vlichen"
    lichen_commit="$(git -C "$vlichen" describe --tags --always)"
    echo "== vlichen $lichen_commit"
}
