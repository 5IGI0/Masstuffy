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

use std::error::Error;
use clap::Parser;

use log::error;
use masstuffy::{database::{structs::RECORD_FLAG_ACTIVE, DBManager}, filesystem::{init, CollID}, warc::WarcReader};

#[derive(Parser)]
struct Args {
    /// warc file holding records
    source: String,

    /// collection's slug to put records into
    destination: String
}

pub async fn main(argv: Vec<String>) -> Result<i32, Box<dyn Error>> {
    let args = Args::parse_from(&argv[1..]);

    let fs = init().await
        .expect("unable to initialise fs");

    if !fs.has_collection_slug(&args.destination).await {
        error!("collection `{}` doesn't exist", args.destination);
        return Ok(1);
    }

    let mut dbm: Option<DBManager> = None;

    {
        let db_conn = fs.get_database_conn_string();
        if db_conn != "" {
            let mut db = DBManager::new(&fs.get_database_conn_string());
            db.setup_db().await;
            dbm = Some(db);
        }
    }

    let coll = fs.get_collection(CollID::Slug(args.destination)).await;
    if let None = coll {
        error!("no such collection");
        return Ok(1);
    }
    let coll = coll.unwrap();
    let coll_uuid = coll.read().await.get_uuid().await;

    let (dict_id, dict_algo) = coll.read().await.get_dict().await;
    let dict_id = if let Some(dict_id) = dict_id {
        Some(dict_id as i64)
    } else {
        None
    };

    let mut reader = WarcReader::from_file(&args.source).await?;
    while let Some(record) = reader.async_next().await {
        let cdx = coll.write().await.add_warc(&record).await?;
        if let Some(db) = &dbm {
            db.insert_record(
                &coll_uuid, &cdx,
                RECORD_FLAG_ACTIVE,
                dict_id, dict_algo.as_deref()
            ).await?; // TODO: bulk insert
        }
    }

    Ok(0)
}