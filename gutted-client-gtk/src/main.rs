//! gutted-client-gtk: Pure-Rust browser client entrypoint (zero C dependencies, Linux & Redox relibc compatible).

fn main() -> gutted_client::Result<()> {
    gutted_client::run_client()
}
