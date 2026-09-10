//! `cargo run` with nothing else is the webcam example: voice, a camera and a screen.
//! The example is the whole program; this is only the door.

#[path = "../examples/webcam.rs"]
mod webcam;

fn main() {
    webcam::main()
}
