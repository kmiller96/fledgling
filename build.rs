fn main() {
    // dir-test expands its globs at compile time. Watch the directory so adding
    // or removing a scenario regenerates the test list without a Rust edit.
    println!("cargo:rerun-if-changed=tests/scenarios");
}
