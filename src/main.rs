mod edot;
mod location;
mod terminal;

type Result<T, E = anyhow::Error> = anyhow::Result<T, E>;

fn main() -> Result<()> {
    env_logger::init();
    edot::run(edot::new()?)?;
    Ok(())
}
