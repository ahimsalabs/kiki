use std::{env, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);

    let file_descriptors = protox::compile(["jj_interface.proto"], ["."])?;

    // Write the binary descriptor set for tonic-reflection / include_file_descriptor_set!().
    // compile_fds() doesn't honor file_descriptor_set_path(), so we write it ourselves.
    std::fs::write(
        out_dir.join("grpc_descriptor.bin"),
        prost::Message::encode_to_vec(&file_descriptors),
    )?;

    tonic_prost_build::configure()
        .build_server(true)
        .compile_fds(file_descriptors)?;
    Ok(())
}
