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
 *  See the GNU Affero General Public License for more details.
 *  You should have received a copy of the GNU Affero General Public License
 *  along with Masstuffy. If not, see <https://www.gnu.org/licenses/>. 
 * 
 *  Copyright (C) 2025 5IGI0 / Ethan L. C. Lorenzetti
**/

use tokio::{fs::{self, OpenOptions}, io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader}, sync::RwLock};

use anyhow::{bail, Result};
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use std::{io::SeekFrom, sync::Arc};
use async_compression::tokio::bufread::{ZstdDecoder, ZstdEncoder};

use crate::{database::DBManager, warc::{cdx::{CDXFileReader, CDXRecord}, WarcReader, WarcRecord}};

use super::dict_store::DictStore;

#[derive(Serialize, Deserialize, Debug, Clone)]
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

#[derive(Serialize, Deserialize, Clone)]
pub struct CollectionInfo {
    manifest: CollectionManifest
}

pub struct Collection {
    path: String,
    manifest: RwLock<CollectionManifest>,
    dict_store: Arc<DictStore>,
    dict: RwLock<Option<Arc<Vec<u8>>>>
}

impl Collection {
    pub async fn get_uuid(&self) -> String {
        self.manifest.read().await.uuid.clone()
    }

    pub async fn get_slug(&self) -> String {
        self.manifest.read().await.slug.clone()
    }

    async fn gen_warc_filename(&self, n: i32) -> String{
        let manifest = self.manifest.read().await;
        format!(
            "records.{}{}.warc{}", n,
            if let Some(id) = &manifest.dict_id {
                format!(".{}", id)
            } else {"".to_string()},
            if let Some(alg) = &manifest.compression {
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
        let manifest = self.manifest.read().await;
        info!("writing new record to `{}`: {}", manifest.slug, record.get_record_id()?);

        let serialized_record = self.compress(record.serialize()).await;

        /* get the first file that can hold the record */
        let mut warc_target = String::new();
        let mut warc_target_size = 0;
        for n in 1.. {
            warc_target = self.gen_warc_filename(n).await;
            if let Ok(m) = tokio::fs::metadata(format!("{}/{}", self.path, warc_target)).await {
                warc_target_size = m.len();
                if (warc_target_size+(serialized_record.len() as u64)) >= manifest.split_threshold {
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
        let manifest = self.manifest.read().await;
        let dict = self.dict.read().await;
        if let None = *dict {
            drop(dict);
            *self.dict.write().await = self.dict_store.get_zstd_dict(manifest.dict_id.unwrap()).await;
            if let None = *self.dict.read().await {
                panic!("unable to load dictionary {}", manifest.dict_id.unwrap())
            }
        }
    }

    async fn compress(&self, content: Vec<u8>) -> Vec<u8> {
        let manifest = self.manifest.read().await;
        if let None = manifest.dict_id {
            return content
        }
        
        self.ensure_dict_loaded().await;

        let dict = self.dict.read().await;
        let vec = dict.as_ref().unwrap();

        let encoder = ZstdEncoder::with_dict(
            BufReader::new(&content[..]),
            async_compression::Level::Precise(manifest.compression_level),
            &vec[..]);
        
        let mut ret = Vec::new();
        encoder.unwrap().read_to_end(&mut ret).await.expect("unable to compress record");
        ret
    }

    async fn get_decompressor(&self, fp: fs::File) -> BufReader<Box<dyn AsyncRead + Unpin + Send>> {
        let manifest = self.manifest.read().await;
        if let None = manifest.dict_id {
            return BufReader::new(Box::new(fp));
        }

        self.ensure_dict_loaded().await;

        let dict = self.dict.read().await;
        let vec = dict.as_ref().unwrap();

        BufReader::new(
            Box::new(
                ZstdDecoder::with_dict(
                    BufReader::new(fp),
                    &vec[..]
                ).expect(&format!("unable to load dictionary {}", manifest.dict_id.unwrap()))
            )
        )
    }

    pub async fn iter_cdx(&self) -> anyhow::Result<CDXFileReader> {
        // TODO: cdx.gz
        Ok(CDXFileReader::open(&format!("{}/index.cdx", self.path)).await?)
    }

    pub async fn get_record(&self, filename: &str, offset: i64) -> anyhow::Result<Option<WarcRecord>>{
        let mut fp = fs::File::open(format!("{}/{}", self.path, filename)).await?;

        fp.seek(SeekFrom::Start(offset as u64)).await?;

        Ok(WarcReader::from_bufreader(self.get_decompressor(fp).await).async_next().await)
    }

    // TODO: atomic
    pub async fn delete(&mut self) -> anyhow::Result<()> {
        fs::remove_dir_all(&self.path).await?;
        Ok(())
    }

    // TODO: atomic switch
    // TODO: prevent 2 rebuild at the same time
    // TODO: keep flags
    // TODO: manage rebuilding with the same dict
    // TODO: support not compressed rebuild
    pub async fn rebuild(&self, dict: Option<(String, u32)>, db: &DBManager) -> anyhow::Result<()> {
        let manifest = self.manifest.read().await;
        let dict_id = dict.unwrap().1; // TODO: check Some(dict)

        /*  enumerate records because the underlying file could be corrupted
            since it might be zero'd to delete specific records or whatever reason
            so i prefer to rely on record index */
        info!("enumerating '{}' records...", manifest.slug);
        let mut reader = self.iter_cdx().await?;

        /*  store separately record files to optimise memory 
            and sort speed */
        let mut record_files: Vec<String> = Vec::new();
        let mut records: Vec<(u16,u64)> = Vec::new();

        while let Some(x) = reader.async_next().await {
            let file_id: usize;
            let filename = x.get_file_name().unwrap();
            if let Some(offset) = record_files.iter().position(|f| *f == filename) {
                file_id = offset;
            } else {
                file_id = record_files.len();
                record_files.push(filename);
            }

            records.push((file_id.try_into()?, x.get_file_offset().unwrap() as u64));
        }

        /*  sort records, se we can efficently keep the same fp
            and seek to each records */
        debug!("sorting records");
        records.sort(); // useless as long we don't keep the same fp for each read operation

        /*  delete records with the same dictionary
            in case some partial rebuild got interrupted */
        debug!("cleaning partial build");
        db.delete_records(
            &manifest.slug,
            Some(dict_id as i64),
            Some("zstd")).await?;
        // TODO: delete files too

        // TODO: check Some(dict)
        let dict = self.dict_store.get_zstd_dict(dict_id).await.unwrap();
        let dict_vec = dict.as_ref();

        let mut output_file_name = format!("records.1.{}.warc.zstd", dict_id);
        let mut output_file_id = 1;
        let mut output_index = fs::OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&format!("{}/.index.cdx", self.path)).await
            .expect("unable to open dst index file"); // TODO: generate gzipped index

        let mut output_fp = fs::OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(format!("{}/{}", self.path, output_file_name)).await.expect("unable to open dst file");

        for r in records {
            // TODO: keep fp open
            let mut fp = fs::File::open(format!("{}/{}", self.path, &record_files[r.0 as usize])).await?;
            fp.seek(SeekFrom::Start(r.1)).await?;
            let dec_fp = self.get_decompressor(fp).await;
            
            if let Some(record) = WarcReader::from_bufreader(dec_fp).async_next().await {
                let content = record.serialize();
                let mut cdxr = CDXRecord::from_warc(&record)?;

                let encoder = ZstdEncoder::with_dict(
                    BufReader::new(&content[..]),
                    async_compression::Level::Precise(manifest.compression_level),
                    &&dict_vec[..]);
                
                let mut compressed = Vec::new();
                encoder.unwrap().read_to_end(&mut compressed).await.expect("unable to compress record");
                
                if (output_fp.stream_position().await? + (compressed.len() as u64)) > manifest.split_threshold {
                    output_file_id += 1;
                    output_file_name = format!("records.{}.{}.warc.zstd", output_file_id, dict_id);
                    output_fp = fs::OpenOptions::new()
                        .create_new(true)
                        .append(true)
                        .open(&format!("{}/{}", self.path, output_file_name)).await
                        .expect("unable to open dst file");
                }
                cdxr.set_file(output_file_name.clone(), Some(output_fp.stream_position().await?));
                output_fp.write_all(&compressed).await?;
                output_index.write_all(format!("{}\n", cdxr).as_bytes()).await?;
                db.insert_record(
                    &manifest.uuid,
                    &cdxr, 0,
                    Some(dict_id as i64),
                    Some("zstd")).await?; // TODO: optimise
            } else {
                warn!("unable to read a record while rebuilding, will be dropped") // TODO: find a good way
            }
        }

        info!("commiting rebuld...");
        db.activate_records(&manifest.uuid,
            Some(dict_id as i64),
            Some("zstd")).await?;
        db.delete_records(&manifest.uuid,
            Some(manifest.dict_id.unwrap() as i64),
            manifest.compression.as_deref()).await?;

        let old_dict_id = manifest.dict_id;
        let old_algo = manifest.compression.clone();

        drop(manifest);
        let mut manifest = self.manifest.write().await;
        manifest.dict_id = Some(dict_id);
        manifest.compression = Some("zstd".to_string()); // TODO:
        self.flush_manifest(&manifest).await;

        for i in 1.. {
            let target_file = if let Some(algo) = &old_algo {
                format!(
                        "{}/records.{}.{}.warc.{}",
                        self.path,
                        i,
                        old_dict_id.unwrap(),
                        algo)
            } else {
                format!("{}/records.{}.warc", self.path, i)
            };

            if let Err(_) = fs::metadata(&target_file).await {
                break;
            }

            fs::remove_file(target_file).await?;
        }

        // reset dict to not use the old one with new records
        drop(dict);
        *self.dict.write().await = None;
        
        // TODO: check if the file exist instead of ignoring errors
        let _ = fs::remove_file(format!("{}/index.cdx.gz", self.path)).await;
        fs::rename(
            format!("{}/.index.cdx", self.path),
            format!("{}/index.cdx", self.path)).await?;
        Ok(())
    }

    // must be called when self.manifest is locked
    async fn flush_manifest(&self, manifest: &CollectionManifest) {
        let manifest_str = serde_json::to_string(manifest)
            .expect("unable to serialize collection manifest");

        // TODO: atomicity (if the disk doesn't have enough space, we can get corrupted manifest)
        fs::write(format!("{}/manifest.json", self.path), manifest_str).await
            .expect("failed to write collection manifest");
    }

    pub async fn get_dict(&self) -> (Option<u32>, Option<String>) {
        let manifest = self.manifest.read().await;
        (manifest.dict_id, manifest.compression.clone())
    }

    pub async fn get_info(&self) -> CollectionInfo {
        let manifest = self.manifest.read().await.clone();

        return CollectionInfo{
            manifest
        }
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
        manifest: RwLock::new(manifest),
        dict_store, dict: RwLock::new(None)};

    info!("collection {} loaded!", collection.get_slug().await);

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

    if let Some(x) = &dictionary {
        // assuming it is zstd.
        if !dict_store.has_zstd_dict(x.1).await {
            bail!("no such dictionary");
        }
    }

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