// SPDX-License-Identifier: AGPL-3.0-only

fn main() {
    // `sqlx::migrate!` embeds the directory at compile time; without this a new
    // migration file would not trigger a rebuild.
    println!("cargo:rerun-if-changed=migrations");
}
