use clap::Parser;
use pgrx_bindgen::build;
use pgrx_pg_config::PgConfig;
use std::{fs::File, io::Write, path::PathBuf};

#[derive(Parser, Debug)]
#[clap(version, about, author)]
struct Args {
    #[clap(long, env = "TARGET", default_value = target_triple::TARGET)]
    target: String,
    #[clap(long("pg_config"), env = "PG_CONFIG")]
    pg_config: PathBuf,
    #[clap(long, short)]
    output: PathBuf,
    #[clap(long, env = "PGRX_BINDGEN_NO_DETECT_INCLUDES", default_value = "false")]
    pgrx_bindgen_no_detect_includes: bool,
}

fn guess_os(target: &str) -> eyre::Result<&str> {
    if target.contains("-macos") {
        Ok("macos")
    } else {
        // safe since only the listed items are handled
        Ok("")
    }
}

fn guess_env(target: &str) -> eyre::Result<&str> {
    if target.ends_with("-msvc") {
        Ok("msvc")
    } else {
        // safe since only the listed items are handled
        Ok("")
    }
}

fn main() -> eyre::Result<()> {
    use zip::write::SimpleFileOptions;
    let args = Args::parse();
    unsafe {
        // set the environment variable in case of bindgen reads it
        std::env::set_var("TARGET", &args.target);
    }
    let pg_config = PgConfig::from_pg_config_path(args.pg_config);
    let (binding, binding_oids, binding_cshim) = build::generate_bindings(
        guess_os(&args.target)?,
        guess_env(&args.target)?,
        &pg_config,
        args.pgrx_bindgen_no_detect_includes,
    )?;
    let mut builder = zip::write::ZipWriter::new(File::create(args.output)?);
    builder.start_file("binding.rs", SimpleFileOptions::default())?;
    builder.write_all(binding.as_bytes())?;
    builder.start_file("binding_oids.rs", SimpleFileOptions::default())?;
    builder.write_all(binding_oids.as_bytes())?;
    builder.start_file("binding_cshim.c", SimpleFileOptions::default())?;
    builder.write_all(binding_cshim.as_bytes())?;
    builder.finish()?;
    Ok(())
}
