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

use std::{fs::{self, OpenOptions}, io::Write};

use anyhow::Result;
use log::{debug, info};
use serde::{Deserialize, Serialize};

use crate::warc::WarcRecord;


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
    // TODO: "massaged url" in cdx
    // TODO: create a new warc part when the file is too big
    // TODO: extract http response status code (when available)
    // TODO: flush .cdx to .cdx.gz when enough big \
    //       don't forget to patch list_records()  \
    //       and add the CDX header if it is the first flush
    pub fn add_warc(&mut self, record: &WarcRecord) -> anyhow::Result<()>{
        info!("writing new record to `{}` ({})", self.get_slug(), record.get_record_id()?);
        let cdx_line = format!(
            "{} {} {} {} {}\n",
            record.get_target_uri().unwrap_or("-".to_string()),
            record.get_type()?,
            record.get_record_id()?,
            record.get_date()?.format("%Y%m%d"),
            std::fs::metadata(format!("{}/{}.1.warc", self.path, self.get_slug()))?.len()
        );

        debug!("{} cdx: {}", record.get_record_id()?, cdx_line);

        let serialized_record = record.serialize();

        OpenOptions::new()
            .write(true)
            .append(true)
            .create(true)
            .open(format!("{}/{}.1.cdx", self.path, self.get_slug()))?
            .write_all(cdx_line.as_bytes()).expect("unable to write cdx file");

        OpenOptions::new()
            .write(true)
            .append(true)
            .open(format!("{}/{}.1.warc", self.path, self.get_slug()))?
            .write_all(&serialized_record[..]).expect("unable to write warc file");
        

        // std::fs::File::open()?;

        Ok(())
    }
}

pub fn load_collection(manifest_path: &str) -> Result<Collection> {
    debug!("loading collection: {}", manifest_path);
    debug!("reading manifest...");
    let manifest: CollectionManifest = serde_json::from_slice(
        &fs::read(manifest_path)?)?;
    
    // TODO: manifest.validate()
    let collection = Collection{
        path: std::path::Path::new(manifest_path).parent().unwrap().to_str().unwrap().to_string(),
        manifest};

    info!("collection {} loaded!", collection.get_slug());

    Ok(collection)
}

pub fn create_collection(
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
            env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"), env!("CARGO_PKG_REPOSITORY"),
            manifest
        ).as_bytes().to_vec());

    fs::write(format!("{}/{}.1.warc", repository_path, slug), first_record.serialize())?;
    fs::write(&manifest_path, manifest)?;

    debug!("collection created!");
    load_collection(&manifest_path)
}