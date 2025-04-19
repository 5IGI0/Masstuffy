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
use std::io::Write;

use chrono::Utc;
use clap::Parser;
use log::info;
use masstuffy::{database::DBManager, filesystem};

#[derive(Parser)]
struct Args {
    query: String,

    /// search by id instead of uri?
    #[arg(short, long, default_value_t = false)]
    by_id: bool,

    /// date (format: YYYYmmddHHMMSS)
    #[arg(short, long)]
    date: Option<String>,
}

pub async fn main(argv: Vec<String>) -> Result<i32, Box<dyn Error>> {
    let args = Args::parse_from(&argv[1..]);

    let fs = filesystem::init().await?;
    let db = DBManager::new(&fs.get_database_conn_string());

    let record_cdx = if args.by_id {
        db.get_record_from_id(args.query).await?
    } else {
        let date_str = if let Some(d) = args.date {
            d
        } else {
            Utc::now().naive_utc().format("%Y%m%d%H%M%S").to_string()
        };

        db.get_record_from_uri(&date_str, &args.query).await?
    };

    info!("{}", record_cdx.collection);

    let record = fs.get_record(&record_cdx.collection, &record_cdx.filename, record_cdx.offset).await?;

    let mut handle = std::io::stdout().lock();
    handle.write(&record.unwrap().serialize()).unwrap();

    Ok(0)
}