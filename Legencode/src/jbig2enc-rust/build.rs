use std::error::Error;
use vergen::{BuildBuilder, Emitter};

fn main() -> Result<(), Box<dyn Error>> {
    // Set the JBIG2ENC_VERSION environment variable for the build
    println!("cargo:rustc-env=JBIG2ENC_VERSION=0.29");

    // Configure and build the build instructions
    let build = BuildBuilder::default().build_timestamp(true).build()?;

    // Create emitter and add instructions
    Emitter::default().add_instructions(&build)?.emit()?;

    Ok(())
}
