//! Compiles the Qt side of the app (`cpp/`) and the bridge between it and
//! the Rust side (`src/ffi.rs`), against the system's Qt 6.

use cxx_qt_build::CxxQtBuilder;

fn main() {
    CxxQtBuilder::new()
        .qt_module("DBus")
        .qt_module("Gui")
        .qt_module("Network")
        .qt_module("Qml")
        .qt_module("Quick")
        .qt_module("WebEngineCore")
        .qt_module("WebEngineQuick")
        .file("src/ffi.rs")
        .include_dir("cpp")
        // A header: moc runs over it.
        .cpp_file("cpp/backend.h")
        .cpp_file("cpp/backend.cpp")
        .cpp_file("cpp/momentum.cpp")
        .cpp_file("cpp/pages.cpp")
        .cpp_file("cpp/shell.cpp")
        .build();
    for file in [
        "backend.h",
        "momentum.cpp",
        "momentum.h",
        "pages.cpp",
        "pages.h",
        "shell.cpp",
        "shell.h",
    ] {
        println!("cargo:rerun-if-changed=cpp/{file}");
    }
}
