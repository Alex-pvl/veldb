fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/veldb.proto");
    tonic_prost_build::compile_protos("proto/veldb.proto")?;
    Ok(())
}
