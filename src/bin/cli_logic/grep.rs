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

use std::{error::Error, io::stdout};

use clap::Parser;
use masstuffy::{filesystem::{self, CollID}};
use regex::bytes::Regex;

#[derive(Parser)]
struct Args {
    patterns: Vec<String>
}

pub async fn main(argv: Vec<String>) -> Result<i32, Box<dyn Error>> {
    let args = Args::parse_from(&argv[1..]);

    let regs: Vec<Regex> = args.patterns.iter().map(|p| Regex::new(p).expect("failed to compile regex")).collect();
    let mut writer = csv::Writer::from_writer(stdout());

    let fs = filesystem::init().await?;
    for coll_slug in fs.get_collection_list().await {
        let coll = fs.get_collection(CollID::Slug(coll_slug.clone())).await.unwrap();
        let coll = coll.read().await;

        let mut reader = coll.iter_cdx().await?;
        loop {
            let record_cdx = reader.async_next().await;
            if let None = record_cdx {
                break
            }
            let record_cdx = record_cdx.unwrap();
            let record = coll.get_record(
                &record_cdx.get_file_name().unwrap(), 
                record_cdx.get_file_offset().unwrap()).await?.unwrap();

            let record_id = record.get_record_id().unwrap();
            let record_uri = record.get_target_uri().unwrap_or("".to_string());
            let mut body: Vec<u8> = Vec::new();
            record.write_body(&mut body)?;

            for reg in &regs {
                for m in reg.find_iter(&body[..]) {
                    writer.write_record(&[
                        reg.as_str().as_bytes(),
                        coll_slug.as_bytes(),
                        &body[m.start()..m.end()],
                        format!("{}", m.start()).as_bytes(),
                        format!("{}", m.end()).as_bytes(),
                        record_id.as_bytes(),
                        record_uri.as_bytes()
                    ])?;
                }
            }
        }
    }

    Ok(0)
}