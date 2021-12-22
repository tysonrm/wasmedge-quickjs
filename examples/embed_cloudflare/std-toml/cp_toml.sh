rustup target add wasm32-unknown-unknown
cp examples/embed_cloudflare/std-toml/Cargo.toml Cargo.toml
cargo build --target wasm32-unknown-unknown --release
