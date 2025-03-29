/**
 *  This file is part of Masstuffy. Masstuffy is free software:
 *  you can redistribute it and/or modify it under the terms of 
 *  the GNU Affero General Public License as published by
 *  the Free Software Foundation, either version 3 of the License,
 *  or (at your option) any later version.
 * 
 *  Masstuffy is distributed in the hope that it will be useful,
 *  but WITHOUT ANY WARRANTY; without even the implied warranty of
 *  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
 * 
 *  See the GNU General Public License for more details.
 *  You should have received a copy of the GNU General Public License
 *  along with Masstuffy. If not, see <https://www.gnu.org/licenses/>. 
 * 
 *  Copyright (C) 2025 5IGI0 / Ethan L. C. Lorenzetti
**/

use std::{error::Error, fs};

use clap::Parser;
use log::{debug, error, info};
use masstuffy::config::Config;
use uuid::Uuid;

#[derive(Parser)]
struct Args {
    path: Option<String>
}

pub fn main(argv: Vec<String>) -> Result<i32, Box<dyn Error>> {
    let args = Args::parse_from(&argv[1..]);

    let path = args.path.unwrap_or(".".to_string());

    info!("target directory: {}", &path);

    if fs::read_dir(&path)?.next().is_some() {
        error!("this directory is not empty");
        return Ok(1);
    }

    info!("initialisation...");

    debug!("generating config...");
    let mut config = Config::default();
    config.secret_key = Some(Uuid::new_v4().to_string());

    debug!("writing config...");
    fs::write(
        format!("{}/config.json", &path),
        serde_json::to_string_pretty(&config).ok().unwrap())
        .expect("unable to write config.json");

    debug!("creating directories...");
    fs::create_dir(format!("{}/data", &path))
        .expect("unable to create data dir");
    fs::create_dir(format!("{}/data/buffer", &path))
        .expect("unable to create buffer dir");
    fs::create_dir(format!("{}/data/dict", &path))
        .expect("unable to create dict dir");
    fs::create_dir(format!("{}/data/dict/zstd", &path))
        .expect("unable to create zstd dict dir");
    fs::create_dir(format!("{}/data/repository", &path))
        .expect("unable to create records dir");

    info!("done!");
    Ok(0)
}