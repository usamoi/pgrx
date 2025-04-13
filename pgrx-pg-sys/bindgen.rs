// not a build.rs so that it doesn't inherit the git history of the build.rs

use eyre::eyre;
use eyre::ContextCompat;
use pgrx_bindgen::build::*;
use pgrx_pg_config::{PgConfig, Pgrx, SUPPORTED_VERSIONS};
use std::env::VarError;
use std::path::{Path, PathBuf};

fn main() -> eyre::Result<()> {
    emit_rerun_if_changed()?;

    // dump the environment for debugging if asked
    if env_tracked("PGRX_BUILD_VERBOSE").as_deref() == Ok("true") {
        for (k, v) in std::env::vars() {
            eprintln!("{k}={v}");
        }
    }

    let docs_rs = env_tracked("DOCS_RS").as_deref() == Ok("1");
    let cargo_feature_cshim = std::env::var("CARGO_FEATURE_CSHIM").is_ok();

    let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);
    let target_os = std::env::var("CARGO_CFG_TARGET_OS")?;
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV")?;
    let pgrx_bindgen_no_detect_includes = env_tracked("PGRX_BINDGEN_NO_DETECT_INCLUDES").is_ok();

    let pg_config = {
        let mut found = Vec::new();
        for pg_version in SUPPORTED_VERSIONS() {
            let major_version = pg_version.major;
            if std::env::var(format!("CARGO_FEATURE_PG{major_version}")).is_ok() {
                found.push(pg_version);
            }
        }
        let found_ver = match &found[..] {
            [ver] => ver,
            [] => {
                return Err(eyre!(
                    "Did not find `pg$VERSION` feature. `pgrx-pg-sys` requires one of {} to be set",
                    SUPPORTED_VERSIONS()
                        .iter()
                        .map(|pgver| format!("`pg{}`", pgver.major))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            }
            versions => {
                return Err(eyre!(
                    "Multiple `pg$VERSION` features found.\n`--no-default-features` may be required.\nFound: {}",
                    versions
                        .iter()
                        .map(|version| format!("pg{}", version.major))
                        .collect::<Vec<String>>()
                        .join(", ")
                ))
            }
        };

        let found_major = found_ver.major;
        if let Ok(pg_config) = PgConfig::from_env() {
            let major_version = pg_config.major_version()?;

            if major_version != found_major {
                panic!("Feature flag `pg{found_major}` does not match version from the environment-described PgConfig (`{major_version}`)")
            }
            pg_config
        } else {
            let pg_config = Pgrx::from_config()?.get(&format!("pg{}", found_ver.major))?;
            pg_config
        }
    };

    let (binding, binding_oids, binding_cshim) = if let Ok(path) = env_tracked("PGRX_BINDING") {
        println!("cargo:rerun-if-changed={path}");
        unzip(Path::new(&path))?
    } else if docs_rs {
        use std::path::MAIN_SEPARATOR as S;
        let path = format!(
            "{}{S}assets{S}pg{}.zip",
            std::env::var("CARGO_MANIFEST_DIR")?,
            pg_config.major_version()?
        );
        println!("cargo:rerun-if-changed={path}");
        unzip(Path::new(&path))?
    } else {
        generate_bindings(&target_os, &target_env, &pg_config, pgrx_bindgen_no_detect_includes)?
    };

    std::fs::write(out_dir.join("binding.rs"), &binding)?;
    std::fs::write(out_dir.join("binding_oids.rs"), &binding_oids)?;
    std::fs::write(out_dir.join("binding_cshim.c"), &binding_cshim)?;

    if !docs_rs && cargo_feature_cshim {
        build_cshim(&out_dir, &target_os, &target_env, &pg_config)?;
    }

    let lib_dir = pg_config.lib_dir()?;
    println!(
        "cargo:rustc-link-search={}",
        lib_dir.to_str().ok_or(eyre!("{lib_dir:?} is not valid UTF-8 string"))?
    );

    println!(
        "cargo:PG_CONFIG={}",
        pg_config.path().wrap_err("failed to get pg_config path")?.display()
    );

    Ok(())
}

fn emit_rerun_if_changed() -> eyre::Result<()> {
    // `pgrx-pg-config` doesn't emit one for this.
    println!("cargo:rerun-if-env-changed=PGRX_PG_CONFIG_PATH");
    println!("cargo:rerun-if-env-changed=PGRX_PG_CONFIG_AS_ENV");
    // Bindgen's behavior depends on these vars, but it doesn't emit them
    // directly because the output would cause issue with `bindgen-cli`. Do it
    // on bindgen's behalf.
    println!("cargo:rerun-if-env-changed=LLVM_CONFIG_PATH");
    println!("cargo:rerun-if-env-changed=LIBCLANG_PATH");
    println!("cargo:rerun-if-env-changed=LIBCLANG_STATIC_PATH");
    // Follows the logic bindgen uses here, more or less.
    // https://github.com/rust-lang/rust-bindgen/blob/e6dd2c636/bindgen/lib.rs#L2918
    println!("cargo:rerun-if-env-changed=BINDGEN_EXTRA_CLANG_ARGS");
    let target = std::env::var("TARGET")?;
    println!("cargo:rerun-if-env-changed=BINDGEN_EXTRA_CLANG_ARGS_{target}");
    println!("cargo:rerun-if-env-changed=BINDGEN_EXTRA_CLANG_ARGS_{}", target.replace('-', "_"));

    if let Ok(pgrx_config) = Pgrx::config_toml() {
        println!("cargo:rerun-if-changed={}", pgrx_config.display());
    }

    Ok(())
}

fn env_tracked(s: &str) -> Result<String, VarError> {
    println!("cargo:rerun-if-env-changed={s}");
    std::env::var(s)
}
