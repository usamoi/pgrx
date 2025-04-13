fn main() -> Result<(), Box<dyn std::error::Error>> {
    dbg!(std::env::vars());
    let path = std::env::var("DEP_POSTGRES_PG_CONFIG")?;
    println!("cargo:PG_CONFIG={}", path);
    Ok(())
}
