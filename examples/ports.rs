// WINDOWS-PORT W10: the jack crate is `cfg(not(windows))` in this tree, so
// this JACK diagnostic example compiles to an inert stub on Windows (the
// `cargo check --all-targets` compile guard builds every example).
#[cfg(windows)]
fn main() {
    eprintln!("ports: Linux-only example (JACK backend); no-op on Windows.");
}

#[cfg(not(windows))]
mod app {
    use jack::{Client, ClientOptions, PortFlags};

    pub fn main() {
        let (client, _status) =
            Client::new("hermes-probe", ClientOptions::NO_START_SERVER).unwrap();
        println!("== INPUTS (remote output ports) ==");
        for n in client.ports(None, Some("audio"), PortFlags::IS_OUTPUT) {
            if !n.starts_with("hermes-probe:") {
                println!("{n}");
            }
        }
        println!("== OUTPUTS (remote input ports) ==");
        for n in client.ports(None, Some("audio"), PortFlags::IS_INPUT) {
            if !n.starts_with("hermes-probe:") {
                println!("{n}");
            }
        }
    }
}

#[cfg(not(windows))]
fn main() {
    app::main();
}
