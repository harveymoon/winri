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
    border_color: iced::Color,
    border_width: f32,
    border_radius: f32,
}

#[derive(Debug)]
pub enum LoadError {
    NotFound,
    WriteLib,
    ConfigScript(String),
    BadScriptExport(String),
    Validation(String),
}

pub fn load() -> Result<Root, LoadError> {
    let config_script = fs::read_to_string("config/main.koto").map_err(|_| LoadError::NotFound)?;
    // fs::write("config/winri.koto", CONFIG_MODULE).map_err(|_| LoadError::WriteLib)?;
    let mut koto = Koto::default();

    let config_script_return = koto
        .compile_and_run(CompileArgs::new(&config_script).script_path("config/main.koto"))
        .map_err(|e| LoadError::ConfigScript(format!("Error in config script: {e}")))?;

    let KValue::Map(config_map) = config_script_return else {
        return Err(LoadError::BadScriptExport(
            "Config script did not return an object".into(),
        ));
    };

    from_koto_value(config_map).map_err(|e| LoadError::BadScriptExport(e.to_string()))
}
