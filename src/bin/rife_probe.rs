//! Prints the tensor contract of a RIFE ONNX model.
//!
//! The vs-mlrt RIFE exports are not documented anywhere findable, and the
//! graph names in the file (`input`, `base_grid`, `multiplier`) suggest this
//! one wants more than an image pair. Rather than guess at the packing, ask
//! the runtime:
//!
//!     cargo run --release --features rife --bin rife_probe -- rife/rife_v4.6.onnx

#[cfg(not(feature = "rife"))]
fn main() {
    eprintln!("built without the `rife` feature; nothing to probe.");
    eprintln!("cargo run --release --features rife --bin rife_probe");
}

#[cfg(feature = "rife")]
fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "rife/rife_v4.6.onnx".to_string());
    println!("loading {path}");

    // commit_from_file is behind ort's `std` feature, which we do not enable.
    // Reading the bytes ourselves is equivalent and one less feature to carry.
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("could not read {path}: {e}");
            return;
        }
    };

    let session = match ort::session::Session::builder()
        .and_then(|mut b| b.commit_from_memory(&bytes))
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("could not load {path}: {e}");
            return;
        }
    };

    println!("\ninputs:");
    for i in session.inputs() {
        println!("  {i:?}");
    }
    println!("\noutputs:");
    for o in session.outputs() {
        println!("  {o:?}");
    }
}
