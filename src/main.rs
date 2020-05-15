#![allow(dead_code)]

mod edot;
mod id_vec;
mod location;
mod terminal;

type Result<T = (), E = anyhow::Error> = anyhow::Result<T, E>;
type Error = anyhow::Error;

use crate::edot::Edot;
use fehler::throws;

#[throws]
fn main() {
    env_logger::init();
    Edot::new()?.run()?;
}
