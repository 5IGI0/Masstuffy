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

use std::error::Error;
use std::io::Write;

use clap::Parser;
use log::info;
use masstuffy::{database::DBManager, filesystem};

/// Simple program to greet a person
#[derive(Parser)]
struct Args {
    id: String
}

pub async fn main(argv: Vec<String>) -> Result<i32, Box<dyn Error>> {
    let args = Args::parse_from(&argv[1..]);

    let fs = filesystem::init()?;
    let mut db = DBManager::new(&fs.get_database_conn_string());

    let record_cdx= db.get_record_from_id(args.id).await?;

    info!("{}", record_cdx.collection);

    let record = fs.get_record(&record_cdx.collection, &record_cdx.filename, record_cdx.offset)?;

    let mut handle = std::io::stdout().lock();
    handle.write(&record.unwrap().serialize()).unwrap();

    Ok(0)
}