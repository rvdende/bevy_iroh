# Examples, natively and in a browser. `just` lists these; `just voice` hosts a room and
# prints a ticket, `just voice <ticket>` joins one. Web recipes build with the `web` profile
# and serve on http://localhost:8000; open `/?join=<ticket>` there.

set positional-arguments

default:
    @just --list

# The hello world: a cube each, driven with the arrow keys. `just cube <ticket>` joins.
cube *ticket:
    cargo run --example cube -- "$@"

# Voice: spheres with a bar over each head, mute and device pickers top right.
voice *ticket:
    cargo run --example voice -- "$@"

# Voice, a camera and a screen: everyone's picture over their sphere, a shared screen above.
webcam *ticket:
    cargo run -- "$@"

# A headless peer that keeps a room alive with an orbiting cube, for browser testing.
host *ticket:
    cargo run --example host -- "$@"

# The same three in a browser tab: `just web voice [port]`.
web example="cube" port="8000":
    ./scripts/web.sh {{example}} {{port}}

web-cube port="8000":
    ./scripts/web.sh cube {{port}}

web-voice port="8000":
    ./scripts/web.sh voice {{port}}

web-webcam port="8000":
    ./scripts/web.sh webcam {{port}}

# Everything that is checked before a commit.
check:
    cargo fmt --all -- --check
    cargo clippy --all-targets -- -D warnings
    cargo test
    cargo check --target wasm32-unknown-unknown --no-default-features --features web-demo --examples

# `just release 0.5.0`, on a clean main: the checks, the version in Cargo.toml, a commit, the
# tag `v0.5.0` and a push. The publish workflow takes it from there.
# Cut a release: checks, version bump, commit, tag and push; publish.yml does the rest.
release version:
    #!/usr/bin/env sh
    set -eu
    [ -z "$(git status --porcelain)" ] || { echo "commit or stash first" >&2; exit 1; }
    just check
    sed -i 's/^version = ".*"/version = "{{version}}"/' Cargo.toml
    cargo update --workspace --offline
    git commit -am "release: bevy_iroh {{version}}"
    git tag "v{{version}}"
    git push origin HEAD "v{{version}}"

# The browser transport test, in a headless browser (geckodriver or chromedriver on PATH).
test-wasm:
    cargo test --profile wasm-test --target wasm32-unknown-unknown --features wasm --test wasm
