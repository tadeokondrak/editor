#![allow(dead_code)]

mod edot;
mod id_vec;
mod location;
mod terminal;

type Result<T = (), E = anyhow::Error> = anyhow::Result<T, E>;
type Error = anyhow::Error;

fn main() -> Result<()> {
    env_logger::init();
    edot::run(edot::new()?)?;
    Ok(())
}
