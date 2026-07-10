use std::{env, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let proto_root = manifest_dir.join("weather");
    let proto_file = proto_root.join("schema/v1/weather.proto");

    println!("cargo:rerun-if-changed={}", proto_file.display());

    let mut config = prost_build::Config::new();
    config.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);
    config.type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]");
    config.field_attribute(
        ".weather.schema.v1.AppConfig.daemon",
        "#[serde(default, skip_serializing)]",
    );
    config.field_attribute(
        ".weather.schema.v1.IpcConfig.transport",
        "#[serde(default, skip_serializing)]",
    );
    config.field_attribute(
        ".weather.schema.v1.DbConfig.lock_path",
        "#[serde(default, skip_serializing)]",
    );
    config.compile_protos(&[proto_file], &[proto_root])?;
    Ok(())
}
