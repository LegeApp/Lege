use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=proto/onnx.proto");

    protobuf_codegen::Codegen::new()
        .pure()
        .cargo_out_dir("onnx")
        .include("proto")
        .input("proto/onnx.proto")
        .run()
        .expect("generate ONNX protobuf bindings");

    // `include!()` cannot contain inner attributes emitted by rust-protobuf.
    // The containing module owns the equivalent lint allowances instead.
    let generated =
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set")).join("onnx/onnx.rs");
    let source = fs::read_to_string(&generated).expect("read generated ONNX bindings");
    let source = source
        .lines()
        .filter(|line| !line.starts_with("#!"))
        .map(|line| match line.strip_prefix("//!") {
            Some(rest) => format!("//{rest}"),
            None => line.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(generated, source).expect("normalize generated ONNX bindings");
}
