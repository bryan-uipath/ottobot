// OTTO_COMMIT from the root crate's build script.
#[path = "../../build.rs"]
mod commit;

fn main() {
    commit::main();
    tauri_build::build()
}
