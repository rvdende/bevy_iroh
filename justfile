# Examples, natively and in a browser. `just` lists these; `just voice` hosts a room and
# prints a ticket, `just voice <ticket>` joins one. Web recipes build with the `web` profile
# and serve on http://localhost:8000; open `/?join=<ticket>` there.

set positional-arguments

# On Linux the webcam example wants the `v4l2` feature; elsewhere it falls back to no camera.
v4l2 := if os() == "linux" { ",v4l2" } else { "" }

default:
    @just --list

# The hello world: a cube each, driven with the arrow keys. `just cube <ticket>` joins.
cube *ticket:
    cargo run --example cube -- "$@"

# Voice: spheres with a bar over each head, mute and device pickers top right.
voice *ticket:
    cargo run --example voice --features ui,webrtc -- "$@"

# Voice and a camera: the same, with everyone's picture over their sphere.
webcam *ticket:
    cargo run --example webcam --features ui,webrtc{{v4l2}} -- "$@"

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
    cargo clippy --features ui,webrtc{{v4l2}} --all-targets -- -D warnings
    cargo test --features ui,webrtc{{v4l2}}
    cargo check --target wasm32-unknown-unknown --features ui,wasm,webrtc --examples

# The browser transport test, in a headless browser (geckodriver or chromedriver on PATH).
test-wasm:
    cargo test --profile wasm-test --target wasm32-unknown-unknown --features wasm --test wasm
