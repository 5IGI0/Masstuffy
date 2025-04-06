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

use tokio::{fs::{self, OpenOptions}, io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader}, sync::RwLock};

use anyhow::Result;
use log::{debug, info};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use std::sync::Arc;
use async_compression::tokio::bufread::{ZstdDecoder, ZstdEncoder};

use crate::warc::{cdx::{CDXFileReader, CDXRecord}, WarcReader, WarcRecord};

use super::dict_store::DictStore;

#[derive(Serialize, Deserialize, Debug)]
struct CollectionManifest {
    uuid: String,
    slug: String,
    compression: Option<String>,
    compression_level: i32,
    dict_id: Option<u32>,
    split_threshold: u64
}

impl CollectionManifest {
    pub async fn validate(&self) -> anyhow::Result<()> {
        if let Some(comp) = &self.compression {
            if comp != "zstd" {
                anyhow::bail!("{}: compression '{}' is not supported", self.slug, comp);
            }
    
            if let None = self.dict_id {
                anyhow::bail!("{}: compression with no dictionary is not supported", self.slug);
            }
        }

        Ok(())
    }
}

pub struct Collection {
    path: String,
    manifest: CollectionManifest,
    dict_store: Arc<DictStore>,
    dict: RwLock<Option<Arc<Vec<u8>>>>
}

impl Collection {
    pub fn get_uuid(&self) -> String {
        self.manifest.uuid.clone()
    }

    pub fn get_slug(&self) -> String {
        self.manifest.slug.clone()
    }

    fn gen_warc_filename(&self, n: i32) -> String{
        format!(
            "records.{}{}.warc{}", n,
            if let Some(id) = &self.manifest.dict_id {
                format!(".{}", id)
            } else {"".to_string()},
            if let Some(alg) = &self.manifest.compression {
                format!(".{}", alg)
            } else {"".to_string()})
    }

    // TODO: mutex
    // TODO: keep the files open
    // TODO: extract http response status code (when available)
    // TODO: flush .cdx to .cdx.gz when enough big \
    //       don't forget to patch list_records()  \
    //       and add the CDX header if it is the first flush
    pub async fn add_warc(&self, record: &WarcRecord) -> anyhow::Result<CDXRecord>{
        info!("writing new record to `{}`: {}", self.get_slug(), record.get_record_id()?);

        let serialized_record = self.compress(record.serialize()).await;

        /* get the first file that can hold the record */
        let mut warc_target = String::new();
        let mut warc_target_size = 0;
        for n in 1.. {
            warc_target = self.gen_warc_filename(n);
            if let Ok(m) = tokio::fs::metadata(format!("{}/{}", self.path, warc_target)).await {
                warc_target_size = m.len();
                if (warc_target_size+(serialized_record.len() as u64)) >= self.manifest.split_threshold {
                    continue;
                }
            } else {
                warc_target_size = 0;
            };
            break;
        }

        let mut cdx = CDXRecord::from_warc(&record)?;
        cdx.set_file(warc_target.clone(), Some(warc_target_size));

        debug!("{} cdx: {}", record.get_record_id()?, cdx);
        
        OpenOptions::new()
            .write(true)
            .append(true)
            .create(true)
            .open(format!("{}/{}", self.path, warc_target)).await?
            .write_all(&serialized_record[..]).await
            .expect("unable to write warc file");

        OpenOptions::new()
            .write(true)
            .append(true)
            .create(true)
            .open(format!("{}/index.cdx", self.path)).await?
            .write_all(format!("{}\n", cdx).as_bytes()).await
            .expect("unable to write cdx file");

        Ok(cdx)
    }

    async fn ensure_dict_loaded(&self) {
        let dict = self.dict.read().await;
        if let None = *dict {
            *self.dict.write().await = self.dict_store.get_zstd_dict(self.manifest.dict_id.unwrap()).await;
            if let None = *self.dict.read().await {
                panic!("unable to load dictionary {}", self.manifest.dict_id.unwrap())
            }
        }
    }

    async fn compress(&self, content: Vec<u8>) -> Vec<u8> {
        if let None = self.manifest.dict_id {
            return content
        }
        
        self.ensure_dict_loaded().await;

        let dict = self.dict.read().await;
        let vec = dict.clone().unwrap();

        let encoder = ZstdEncoder::with_dict(
            BufReader::new(&content[..]),
            async_compression::Level::Precise(self.manifest.compression_level),
            &vec[..]);
        
        let mut ret = Vec::new();
        encoder.unwrap().read_to_end(&mut ret).await.expect("unable to compress record");
        ret
    }

    async fn get_decompressor(&self, fp: fs::File) -> BufReader<Box<dyn AsyncRead + Unpin + Send>> {
        if let None = self.manifest.dict_id {
            return BufReader::new(Box::new(fp));
        }

        self.ensure_dict_loaded().await;

        let dict = self.dict.read().await;
        let vec = dict.clone().unwrap();

        BufReader::new(
            Box::new(
                ZstdDecoder::with_dict(
                    BufReader::new(fp),
                    &vec[..]
                ).expect(&format!("unable to load dictionary {}", self.manifest.dict_id.unwrap()))
            )
        )
    }

    pub async fn iter_cdx(&self) -> anyhow::Result<CDXFileReader> {
        // TODO: cdx.gz
        Ok(CDXFileReader::open(&format!("{}/index.cdx", self.path)).await?)
    }

    pub async fn get_record(&self, filename: &str, offset: i64) -> anyhow::Result<Option<WarcRecord>>{
        let mut fp = fs::File::open(format!("{}/{}", self.path, filename)).await?;

        fp.seek(std::io::SeekFrom::Start(offset as u64)).await?;
        
        Ok(WarcReader::from_bufreader(self.get_decompressor(fp).await).async_next().await)
    }

    // TODO: atomic
    pub async fn delete(&mut self) -> anyhow::Result<()> {
        fs::remove_dir_all(&self.path).await?;
        Ok(())
    }
}

pub async fn load_collection(collection_path: &str, dict_store: Arc<DictStore>) -> Result<Collection> {
    debug!("loading collection: {}", collection_path);
    debug!("reading manifest...");
    let manifest: CollectionManifest = serde_json::from_slice(
        &fs::read(format!("{}/manifest.json", collection_path)).await?)?;

    manifest.validate().await?;

    /* since we support zstd only, i don't check the algorithm */
    if let Some(dict_id) = manifest.dict_id {
        if !dict_store.has_zstd_dict(dict_id).await {
            anyhow::bail!("{}: dictionary {} unknown", manifest.slug, dict_id);
        }
    }

    let collection = Collection{
        path: collection_path.to_string(),
        manifest,
        dict_store, dict: RwLock::new(None)};

    info!("collection {} loaded!", collection.get_slug());

    Ok(collection)
}

pub async fn create_collection(
    repository_path: &str,
    slug: &str,
    dictionary: Option<(String, u32)>,
    dict_store: Arc<DictStore>
    ) -> Result<Collection>{
    debug!("creating collection: {}", slug);
    let collection_uuid = Uuid::new_v4().to_string();
    let collection_path = format!("{}/{}/", repository_path, collection_uuid);

    let manifest = serde_json::to_string(&CollectionManifest{
        uuid: collection_uuid,
        slug: slug.to_string(),
        compression_level: zstd::DEFAULT_COMPRESSION_LEVEL,  // TODO: configure
        split_threshold: (1 << 32) - 1, // TODO: configure
        dict_id: if let Some(x) = &dictionary {
            Some(x.1)
        } else {
            None
        },
        compression: if let Some(x) = dictionary {
            Some(x.0)
        } else {
            None
        }})?;
    
    // TODO: manifest.validate()

    fs::create_dir(&collection_path).await?;
    fs::write(format!("{}/manifest.json", collection_path), &manifest).await?;
    let coll = load_collection(&collection_path, dict_store).await?;

    let mut first_record = WarcRecord::new("warcinfo".to_string());
    first_record.set_header("Content-Type".to_string(), "application/warc-fields".to_string());
    first_record.set_body(
        format!(
            "format: WARC File Format 1.1\r\nsoftware: {}/{} ({})\r\nmasstuffy-collection-manifest: {}\r\n",
            env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"), env!("CARGO_PKG_HOMEPAGE"),
            manifest
        ).as_bytes().to_vec());

    coll.add_warc(&first_record).await?;

    debug!("collection created!");
    Ok(coll)
}