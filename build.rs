//! Generates the gRPC types and service from the versioned API contract,
//! with a vendored compiler so builds need nothing installed.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/hinagata/v1/hinagata.proto");
    println!("cargo:rerun-if-changed=proto/onnx/onnx_subset.proto");
    // SAFETY: build scripts run single-threaded before any other code here.
    unsafe {
        std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?);
    }
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]")
        .type_attribute(".", "#[serde(rename_all = \"snake_case\")]")
        // Omitted JSON fields take their protobuf defaults, as on the wire.
        .message_attribute(".", "#[serde(default)]")
        .compile_protos(
            &[
                "proto/hinagata/v1/hinagata.proto",
                "proto/onnx/onnx_subset.proto",
            ],
            &["proto"],
        )?;
    Ok(())
}
