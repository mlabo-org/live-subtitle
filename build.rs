fn main() {
    // ScreenCaptureKit's Swift bridge needs the OS-bundled Swift runtime.
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
}
