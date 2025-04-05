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

use std::{collections::HashMap, sync::Arc};

use std::path::PathBuf;
use log::{debug, error, info, warn};
use tokio::sync::RwLock;

struct ZstdDict {
    path: PathBuf,
    cache: Option<Arc<Vec<u8>>>
}

pub struct DictStore {
    store_location: String,
    zstd_dicts: RwLock<HashMap<u32, RwLock<ZstdDict>>>
}

impl DictStore {
    pub async fn from_dir(path: String) -> anyhow::Result<DictStore> {
        let store = DictStore {
            store_location: path,
            zstd_dicts: RwLock::new(HashMap::new())
        };

        store.reload().await;

        Ok(store)
    }

    pub async fn reload(&self) {
        let mut zstd_dicts = self.zstd_dicts.write().await;
        debug!("(re)loading zstd dictionaries...");
        // TODO: use fs
        if let Ok(dir) = std::fs::read_dir(format!("{}/zstd/", self.store_location)) {
            for f in dir {
                if let Err(e) = f {
                    warn!("failed to fetch new files: {}", e);
                    break;
                }

                let f = f.ok();
                if f.is_none() {
                    break;
                }

                let f = f.unwrap();

                debug!("file {} found", f.file_name().to_string_lossy());
                let path = f.path();
                let filename = f.file_name().to_string_lossy().to_string();
                let parts: Vec<&str> = filename.split(".").collect();

                if parts.len() != 3 {
                    warn!("invalid dictionary filename: '{}', ignored.", filename);
                    continue;
                }

                let id = parts[1].parse::<u32>();
                if let Err(e) = id {
                    warn!("invalid dictionary id: '{}' ({})", parts[1], e);
                    continue;
                }
                let id = id.unwrap();

                if let Some(dict) = zstd_dicts.get(&id) {
                    let dict = dict.read().await;
                    if dict.path != path {
                        error!("duplicate dictionaries with same id ({})", id);
                        panic!("duplicate dictionaries with same id ({})", id);
                    } else {
                        continue;
                    }
                }

                zstd_dicts.insert(
                    id,
                    RwLock::new(ZstdDict{
                        path: path,
                        cache: None}));
                info!("dictionary {} ({}) found", id, parts[0]);
            }
        }
    }

    pub async fn get_zstd_dict(&self, id: u32) -> Option<Arc<Vec<u8>>> {
        let dict = self.zstd_dicts.read().await;

        if let Some(x) = dict.get(&id) {
            let dict = x.read().await;
            if let Some(d) = &dict.cache {
                Some(d.clone())
            } else {
                drop(dict);
                let mut dict = x.write().await;
                if let Some(d) = &dict.cache {
                    Some(d.clone())
                } else {
                    let ret = tokio::fs::read(&dict.path).await;
                    if let Err(x) = ret {
                        warn!("unable to load dictionary: {}", x);
                        None
                    } else {
                        dict.cache = Some(Arc::new(ret.unwrap()));
                        Some(dict.cache.as_ref().unwrap().clone())
                    }
                }
            }
        } else {
            None
        }
    }

    pub async fn has_zstd_dict(&self, id: u32) -> bool {
        let dicts = self.zstd_dicts.read().await;
        dicts.get(&id).is_some()
    }
}