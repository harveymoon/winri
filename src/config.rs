use std::fs;

use koto::{
    CompileArgs, Koto, KotoSettings,
    runtime::{KValue, KotoVmSettings},
    serde::from_koto_value,
};

/// Winri configuration

#[derive(serde::Deserialize, serde::Serialize, Debug, Clone)]
pub struct Root {
    pub tiler: Tiler,
}

#[derive(serde::Deserialize, serde::Serialize, Debug, Clone)]
pub struct Tiler {
    padding: f32,
}

#[derive(Debug)]
pub enum LoadError {
    NotFound,
    ConfigScript(String),
    BadScriptExport(String),
    Validation(String),
}

pub fn load() -> Result<Root, LoadError> {
    let config_script = fs::read_to_string("config/main.koto").map_err(|_| LoadError::NotFound)?;

    let mut koto = Koto::default();

    let config_script_return = koto
        .compile_and_run(CompileArgs::new(&config_script).script_path("config/main.koto"))
        .map_err(|e| LoadError::ConfigScript(format!("Error in config script: {e}")))?;

    let config: Root = from_koto_value(koto.exports().clone())
        .map_err(|e| LoadError::BadScriptExport(format!("Error converting config exports: {e}")))?;

    Ok(config)
}
