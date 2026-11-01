//! Lista as câmeras (sem ligar nenhuma): cargo run --release -p polley-media --example cameras
fn main() {
    for (i, name) in polley_media::camera::list().iter().enumerate() {
        println!("[{i}] {name}");
        for (w, h, fps, kind) in polley_media::camera::formats(i) {
            println!("      {w}x{h} {fps} fps {kind}");
        }
    }
}
