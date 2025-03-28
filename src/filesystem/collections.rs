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

use tokio::{fs::{self, OpenOptions}, io::{AsyncSeekExt, AsyncWriteExt}};

use anyhow::Result;
use log::{debug, info};
use serde::{Deserialize, Serialize};

use crate::warc::{cdx::{CDXFileReader, CDXRecord}, WarcReader, WarcRecord};

#[derive(Serialize, Deserialize, Debug)]
struct CollectionManifest {
    pub slug: String,
    pub compression: Option<String>,
    pub dict_id: Option<u16>,
}

pub struct Collection {
    path: String,
    manifest: CollectionManifest
}

impl Collection {
    pub fn get_slug(&self) -> String {
        self.manifest.slug.clone()
    }

    // TODO: mutex
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
        let warc_target_size = std::fs::metadata(&warc_target)?.len();

        let mut cdx = CDXRecord::from_warc(&record)?;
        cdx.set_file(format!("{}.1.warc", self.get_slug()), Some(warc_target_size));

        debug!("{} cdx: {}", record.get_record_id()?, cdx);

        OpenOptions::new()
            .write(true)
            .append(true)
            .create(true)
            .open(format!("{}/{}.cdx", self.path, self.get_slug())).await?
            .write_all(format!("{}\n", cdx).as_bytes()).await
            .expect("unable to write cdx file");

        OpenOptions::new()
            .write(true)
            .append(true)
            .open(warc_target).await?
            .write_all(&serialized_record[..]).await
            .expect("unable to write warc file");

        Ok(cdx)
    }

    pub fn iter_cdx(&self) -> anyhow::Result<CDXFileReader> {
        // TODO: cdx.gz
        // TODO: async
        Ok(CDXFileReader::open(&format!("{}/{}.cdx", self.path, self.get_slug()))?)
    }

    pub async fn get_record(&self, filename: &str, offset: i64) -> anyhow::Result<Option<WarcRecord>>{
        let mut fp = fs::File::open(format!("{}/{}", self.path, filename)).await?;

        fp.seek(std::io::SeekFrom::Start(offset as u64)).await?;
        
        Ok(WarcReader::from_fp(fp).async_next().await)
    }
}

pub async fn load_collection(manifest_path: &str) -> Result<Collection> {
    debug!("loading collection: {}", manifest_path);
    debug!("reading manifest...");
    let manifest: CollectionManifest = serde_json::from_slice(
        &fs::read(manifest_path).await?)?;
    
    // TODO: manifest.validate()
    let collection = Collection{
        path: std::path::Path::new(manifest_path).parent().unwrap().to_str().unwrap().to_string(),
        manifest};

    info!("collection {} loaded!", collection.get_slug());

    Ok(collection)
}

pub async fn create_collection(
    repository_path: &str,
    slug: &str
    ) -> Result<Collection>{
    debug!("creating collection: {}", slug);
    let manifest_path = format!("{}/{}.json", repository_path, slug);

    let manifest = serde_json::to_string(&CollectionManifest{
        slug: slug.to_string(),
        compression: None,
        dict_id: None})?;
    
    // TODO: manifest.validate()

    let mut first_record = WarcRecord::new("warcinfo".to_string());
    first_record.set_header("Content-Type".to_string(), "application/warc-fields".to_string());
    first_record.set_body(
        format!(
            "format: WARC File Format 1.1\r\nsoftware: {}/{} ({})\r\nmasstuffy-collection-manifest: {}\r\n",
            env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"), env!("CARGO_PKG_HOMEPAGE"),
            manifest
        ).as_bytes().to_vec());

    fs::write(format!("{}/{}.1.warc", repository_path, slug), first_record.serialize()).await?;
    fs::write(&manifest_path, manifest).await?;

    debug!("collection created!");
    load_collection(&manifest_path).await
}