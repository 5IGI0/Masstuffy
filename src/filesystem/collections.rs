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

use tokio::{fs::{self, OpenOptions}, io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader}};

use anyhow::Result;
use log::{debug, info};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use async_compression::tokio::bufread::{ZstdDecoder, ZstdEncoder};

use crate::warc::{cdx::{CDXFileReader, CDXRecord}, WarcReader, WarcRecord};

use super::dict_store::DictStore;

#[derive(Serialize, Deserialize, Debug)]
struct CollectionManifest {
    pub slug: String,
    pub compression: Option<String>,
    pub dict_id: Option<u32>,
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
    dict: Option<Arc<Vec<u8>>>
}

impl Collection {
    pub fn get_slug(&self) -> String {
        self.manifest.slug.clone()
    }

    // TODO: mutex
    // TODO: compression suffix
    // TODO: keep the files open
    // TODO: extract http response status code (when available)
    // TODO: flush .cdx to .cdx.gz when enough big \
    //       don't forget to patch list_records()  \
    //       and add the CDX header if it is the first flush
    pub async fn add_warc(&mut self, record: &WarcRecord) -> anyhow::Result<CDXRecord>{
        info!("writing new record to `{}` ({})", self.get_slug(), record.get_record_id()?);

        let serialized_record = record.serialize();

        // TODO: create a new warc part when the file is too big
        let warc_target = format!("{}/{}.1.warc", self.path, self.get_slug());
        let warc_target_size = if let Ok(m) = tokio::fs::metadata(&warc_target).await {
            m.len()
        } else {
            0
        };

        let mut cdx = CDXRecord::from_warc(&record)?;
        cdx.set_file(format!("{}.1.warc", self.get_slug()), Some(warc_target_size));

        debug!("{} cdx: {}", record.get_record_id()?, cdx);
        
        OpenOptions::new()
            .write(true)
            .append(true)
            .create(true)
            .open(warc_target).await?
            .write_all(&self.compress(serialized_record).await[..]).await
            .expect("unable to write warc file");

        OpenOptions::new()
            .write(true)
            .append(true)
            .create(true)
            .open(format!("{}/{}.cdx", self.path, self.get_slug())).await?
            .write_all(format!("{}\n", cdx).as_bytes()).await
            .expect("unable to write cdx file");

        Ok(cdx)
    }

    async fn ensure_dict_loaded(&mut self) {
        if let None = self.dict {
            self.dict = self.dict_store.get_zstd_dict(self.manifest.dict_id.unwrap()).await;
            if let None = self.dict {
                panic!("unable to load dictionary {}", self.manifest.dict_id.unwrap())
            }
        }
    }

    async fn compress(&mut self, content: Vec<u8>) -> Vec<u8> {
        if let None = self.manifest.dict_id {
            return content
        }
        
        self.ensure_dict_loaded().await;

        let encoder = ZstdEncoder::with_dict(
            BufReader::new(&content[..]),
            async_compression::Level::Default, // TODO: configure
            &self.dict.clone().unwrap()[..]);
        
        let mut ret = Vec::new();
        encoder.unwrap().read_to_end(&mut ret).await.expect("unable to compress record");
        ret
    }

    async fn get_decompressor(&mut self, fp: fs::File) -> BufReader<Box<dyn AsyncRead + Unpin + Send>> {
        if let None = self.manifest.dict_id {
            return BufReader::new(Box::new(fp));
        }

        self.ensure_dict_loaded().await;

        BufReader::new(
            Box::new(
                ZstdDecoder::with_dict(
                    BufReader::new(fp),
                    &self.dict.clone().unwrap()[..]
                ).expect(&format!("unable to load dictionary {}", self.manifest.dict_id.unwrap()))
            )
        )
    }

    pub fn iter_cdx(&self) -> anyhow::Result<CDXFileReader> {
        // TODO: cdx.gz
        // TODO: async
        Ok(CDXFileReader::open(&format!("{}/{}.cdx", self.path, self.get_slug()))?)
    }

    pub async fn get_record(&mut self, filename: &str, offset: i64) -> anyhow::Result<Option<WarcRecord>>{
        let mut fp = fs::File::open(format!("{}/{}", self.path, filename)).await?;

        fp.seek(std::io::SeekFrom::Start(offset as u64)).await?;
        
        Ok(WarcReader::from_bufreader(self.get_decompressor(fp).await).async_next().await)
    }

    // TODO: atomic
    pub async fn delete(&mut self) -> anyhow::Result<()> {
        let slug = self.get_slug();
        for i in 1.. {
            let target = format!("{}/{}.{}.warc", self.path, slug, i);
            if let Err(_) = fs::metadata(&target).await {
                break;
            }
            
            fs::remove_file(target).await?;
        }
        let _ = fs::remove_file(format!("{}/{}.cdx", self.path, slug)).await;
        let _ = fs::remove_file(format!("{}/{}.cdx.gz", self.path, slug)).await;
        let _ = fs::remove_file(format!("{}/{}.json", self.path, slug)).await;
        Ok(())
    }
}

pub async fn load_collection(manifest_path: &str, dict_store: Arc<DictStore>) -> Result<Collection> {
    debug!("loading collection: {}", manifest_path);
    debug!("reading manifest...");
    let manifest: CollectionManifest = serde_json::from_slice(
        &fs::read(manifest_path).await?)?;

    manifest.validate().await?;

    /* since we support zstd only, i don't check the algorithm */
    if let Some(dict_id) = manifest.dict_id {
        if !dict_store.has_zstd_dict(dict_id).await {
            anyhow::bail!("{}: dictionary {} unknown", manifest.slug, dict_id);
        }
    }

    let collection = Collection{
        path: std::path::Path::new(manifest_path).parent().unwrap().to_str().unwrap().to_string(),
        manifest,
        dict_store, dict: None};

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
    let manifest_path = format!("{}/{}.json", repository_path, slug);

    let manifest = serde_json::to_string(&CollectionManifest{
        slug: slug.to_string(),
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

    fs::write(&manifest_path, &manifest).await?;
    let mut coll = load_collection(&manifest_path, dict_store).await?;

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