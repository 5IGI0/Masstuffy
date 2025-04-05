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

use std::collections::HashMap;
use std::sync::Arc;
use tokio::fs;
use tokio::sync::{Mutex, RwLock};

use anyhow::{anyhow, bail, Result};
use collections::{load_collection, Collection};
use log::{debug, error, info};

use crate::warc::cdx::{CDXFileReader, CDXRecord};
use crate::{config::Config, warc::WarcRecord};

mod collections;
mod dict_store;

pub struct FileSystem {
    path: String,
    config: Config,
    collection_uuids: Arc<Mutex<HashMap<String, Arc<RwLock<Collection>>>>>, // TODO: RWLock for hashmap
    collection_slugs: Arc<Mutex<HashMap<String, Arc<RwLock<Collection>>>>>, // TODO: RWLock for hashmap
    dictionary_store: Arc<dict_store::DictStore>
}

pub async fn init() -> Result<FileSystem> {
    let path = std::env::var("MASSTUFFY_WORKDIR").unwrap_or("./".to_string());
    info!("filesystem initialisation...");
    info!("workdir: {}", path);

    debug!("reading config...");
    let config: Config = serde_json::from_slice(
        &fs::read(format!("{}/config.json", &path)).await?)?;
    debug!("config: {:?}", config);

    info!("validating config");
    if let Some(err) = config.validate() {
        error!("invalid config: {}", &err);
        return Err(anyhow!(err));
    }

    info!("finding dictionaries");
    let dictionary_store = Arc::new(dict_store::DictStore::from_dir(format!("{}/data/dict/", path)).await?);

    info!("loading collections...");
    
    let mut collection_slugs: HashMap<String, Arc<RwLock<Collection>>> = HashMap::new();
    let mut collection_uuids: HashMap<String, Arc<RwLock<Collection>>> = HashMap::new();
    let mut dir_handle = fs::read_dir(format!("{}/data/repository/", path)).await?;
    while let Some(f) = dir_handle.next_entry().await? {
        if f.metadata().await?.is_dir() {
            debug!("found {}", f.file_name().to_string_lossy());
            let coll_ret = load_collection(f.path().to_str().unwrap(), dictionary_store.clone()).await;

            if let Ok(collection) = coll_ret {
                let slug = collection.get_slug();
                let uuid = collection.get_uuid();
                let collection = Arc::new(RwLock::new(collection));
                collection_slugs.insert(slug.clone(), Arc::clone(&collection)); // TODO: check duplicate
                collection_uuids.insert(uuid, Arc::clone(&collection));
            }
        }
    }

    Ok(FileSystem{
        path, config,
        collection_slugs: Arc::new(Mutex::new(collection_slugs)),
        collection_uuids: Arc::new(Mutex::new(collection_uuids)),
        dictionary_store: dictionary_store})
}

impl FileSystem {
    pub async fn has_collection_slug(&self, slug: &String) -> bool {
        self.collection_slugs.lock().await.get(slug).is_some()
    }

    pub async fn has_collection_uuid(&self, slug: &String) -> bool {
        self.collection_uuids.lock().await.get(slug).is_some()
    }

    pub async fn create_collection(&mut self, slug: String, dictionary: Option<(String, u32)>) -> anyhow::Result<bool> {
        // TODO: fix race condition
        if self.has_collection_slug(&slug).await {
            return Ok(false);
        }

        let coll = collections::create_collection(
            &format!("{}/data/repository", self.path),
            &slug,
            dictionary,
            self.dictionary_store.clone()).await?;
        
        let slug = coll.get_slug();
        let uuid = coll.get_uuid();
        let coll = Arc::new(RwLock::new(coll));
        self.collection_slugs.lock().await.insert(slug, Arc::clone(&coll));
        self.collection_uuids.lock().await.insert(uuid, Arc::clone(&coll));

        Ok(true)
    }

    pub async fn add_warc(&mut self, slug: &String, record: &WarcRecord) -> anyhow::Result<CDXRecord> {
        if let Some(c) = self.collection_slugs.lock().await.get_mut(slug) {
            let ret = c.write().await.add_warc(record).await;
            if let Err(x) = ret {
                bail!("unable to write warc: {}", x);
            } else {
                return Ok(ret.unwrap());
            }
        } else {
            bail!("no such collection");
        }
    }

    pub async fn get_collection_list(&self) -> Vec<String>{
        self.collection_slugs.lock().await.keys().cloned().collect()
    }

    pub async fn get_collection_cdx_iter(&self, collection_name: &str) -> anyhow::Result<CDXFileReader>{
        if let Some(col) = self.collection_slugs.lock().await.get(collection_name) {
            Ok(col.read().await.iter_cdx()?)
        } else {
            Err(anyhow::Error::msg("no such collection"))
        }
    }

    pub fn get_database_conn_string(&self) -> String {
        self.config.database.clone()
    }

    pub async fn get_record(&self, coll_uuid: &str, filename: &str, offset: i64) -> anyhow::Result<Option<WarcRecord>> {
        let colls = self.collection_uuids.lock().await;
        let coll = colls.get(coll_uuid);

        if let Some(coll) = coll {
            Ok(coll.write().await.get_record(filename, offset).await?) // TODO: no need for write access
        } else {
            Ok(None)
        }
    }

    pub fn get_listen_addr(&self) -> String {
        self.config.listen_addr.clone()
    }

    pub async fn get_buffer_path(&self, name: &str, create: bool) -> anyhow::Result<(String, bool)>{
        let path = format!("{}/data/buffer/{}/", self.path, name); //TODO: validate no traversal path

        let exists = fs::metadata(&path).await.is_ok();
        if create && !exists {
            fs::create_dir(&path).await?;
            Ok((path, false))
        } else {
            Ok((path, exists))
        }
    }

    pub async fn has_zstd_dict(&self, id: u32) -> bool {
        self.dictionary_store.has_zstd_dict(id).await
    }

    pub async fn add_zstd_dict(&mut self, slug: &str, dict: Vec<u8>) {
        // TODO: check dict_id doesn't exists
        tokio::fs::write(
            format!("{}/data/dict/zstd/{}.{}.zstdict",
            self.path, slug, u32::from_le_bytes(dict[4..8].try_into().expect("invalid zstd dictionary"))),
            dict).await.expect("unable to write zstd dictionary file");
        self.dictionary_store.reload().await;
    }

    pub async fn delete_collection(&mut self, slug: &str) -> anyhow::Result<()> {
        let mut colls = self.collection_slugs.lock().await;

        let col = colls.get(slug);

        if let Some(col) = col {
            let mut col = col.write().await;
            let uuid = col.get_uuid();
            col.delete().await?;
            drop(col);
            colls.remove(slug); // TODO: remove from database
            colls.remove(&uuid);
        }

        Ok(())
    }

    pub async fn get_coll_uuid(&self, coll_slug: &str) -> anyhow::Result<String> {
        if let Some(col) = self.collection_slugs.lock().await.get(coll_slug) {
            Ok(col.read().await.get_uuid())
        } else {
            Err(anyhow::format_err!("no such collection"))
        }
    }
}