use anyhow::Result;
use ron::de::from_reader;
use serde::Deserialize;
use std::{fs::File, path::Path};

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub username: String,
    pub token: String,
    pub channel: String,
}

impl Config {
    pub fn from_path<P>(path: P) -> Result<Self>
    where
        P: AsRef<Path>,
    {
        Ok(from_reader(File::open(path)?)?)
    }
}
