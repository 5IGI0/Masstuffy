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

use std::{collections::HashMap, fs, rc};
use std::cell::RefCell;

use anyhow::{anyhow, bail, Result};
use collections::{load_collection, Collection};
use log::{debug, error, info};

use crate::warc::cdx::CDXFileReader;
use crate::{config::Config, warc::WarcRecord};

mod collections;

pub struct FileSystem {
    path: String,
    config: Config,
    collections: rc::Rc<RefCell<HashMap<String, Collection>>>
}

pub fn init() -> Result<FileSystem> {
    let mut ret: FileSystem = FileSystem{
        path: std::env::var("MASSTUFFY_WORKDIR").unwrap_or("./".to_string()),
        config: Config::default(),
        collections: rc::Rc::new(RefCell::new(HashMap::new()))};

    info!("filesystem initialisation...");
    debug!("workdir: {}", ret.path);

    debug!("reading config...");
    ret.config = serde_json::from_slice(
        &fs::read(format!("{}/config.json", &ret.path))?)?;
    debug!("config: {:?}", ret.config);
    debug!("validating config");
    
    if let Some(err) = ret.config.validate() {
        error!("invalid config: {}", &err);
        return Err(anyhow!(err));
    }

    debug!("loading collections...");
    
    for f in fs::read_dir(format!("{}/data/repository/", ret.path))? {
        let f = f?;
        // debug!("{}", f.path().to_str().unwrap());
        if f.file_name().to_string_lossy().ends_with(".json") {
            debug!("found {}", f.file_name().to_string_lossy());
            let coll_ret = load_collection(f.path().to_str().unwrap());

            if let Ok(collection) = coll_ret {
                ret.collections.borrow_mut()
                    .insert(collection.get_slug(), collection);
            }
        }
    }

    Ok(ret)
}

impl FileSystem {
    pub fn has_collection(&self, slug: &String) -> bool {
        self.collections.borrow_mut().get(slug).is_some()
    }

    pub fn create_collection(&mut self, slug: String) -> anyhow::Result<bool>{
        if self.has_collection(&slug) {
            return Ok(false);
        }

        let coll = collections::create_collection(&format!("{}/data/repository", self.path), &slug)?;

        self.collections.borrow_mut().insert(slug, coll);

        Ok(true)
    }

    pub fn add_warc(&mut self, slug: &String, record: &WarcRecord) -> anyhow::Result<()> {
        if let Some(c) = self.collections.borrow_mut().get_mut(slug) {
            if let Err(x) = c.add_warc(record) {
                bail!("unable to write warc: {}", x);
            } else {
                return Ok(());
            }
        } else {
            bail!("no such collection");
        }
    }

    pub fn get_collection_list(&self) -> Vec<String>{
        self.collections.borrow().keys().cloned().collect()
    }

    pub fn get_collection_cdx_iter(&self, collection_name: &str) -> anyhow::Result<CDXFileReader>{
        if let Some(col) = self.collections.borrow().get(collection_name) {
            Ok(col.iter_cdx()?)
        } else {
            Err(anyhow::Error::msg("no such collection"))
        }
    }
}