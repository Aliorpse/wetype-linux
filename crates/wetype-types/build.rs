fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/wetype/ipc/ipc.proto");
    prost_build::Config::new().compile_protos(&["proto/wetype/ipc/ipc.proto"], &["proto"])?;
    Ok(())
}
