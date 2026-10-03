//! Writes seed corpora from real artifacts into `corpus/<target>/`.
use std::path::Path;

fn main() -> std::io::Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus");
    for (target, bytes) in aven_core::sync::fuzz::seeds() {
        let dir = root.join(target);
        std::fs::create_dir_all(&dir)?;
        let name = format!("seed-{:016x}", fnv(&bytes));
        std::fs::write(dir.join(name), bytes)?;
    }
    Ok(())
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}
